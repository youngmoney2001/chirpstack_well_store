use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Utc};
use rquickjs::{CatchResultExt, Function, IntoJs, Persistent};

use super::convert;
use crate::config;

mod vendor_base64_js;
mod vendor_buffer;
mod vendor_ieee754;

/// QuickJS runtimes are not thread-safe. Instead of creating one per codec
/// call, a small set of blocking threads each owns a reusable runtime, context
/// and a cache of already compiled codec entry points.
///
/// The semaphore bounds concurrency (and therefore the number of live
/// runtimes) to roughly the number of CPUs: JS decoding is CPU bound, so more
/// workers would only add contention and memory.
static JS_SEMAPHORE: LazyLock<tokio::sync::Semaphore> = LazyLock::new(|| {
    let workers = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(4);
    tokio::sync::Semaphore::new(workers)
});

thread_local! {
    /// A `thread_local` guarantees that a context is only ever used on the
    /// thread which created it. Rust values never cross that boundary.
    static WORKER: RefCell<Option<Worker>> = const { RefCell::new(None) };
}

/// A compiled codec entry point, detached from the `ctx.with(...)` lifetime.
struct CachedScript {
    /// Stored so that a (very unlikely) hash collision cannot return the
    /// compiled form of a different script.
    script: String,
    entry: Persistent<Function<'static>>,
}

struct Worker {
    /// MUST be dropped before `ctx`/`rt`: a `Persistent` value outliving its
    /// runtime makes rquickjs abort the process. Fields are dropped in
    /// declaration order, and `Drop` clears the cache explicitly as well.
    cache: HashMap<u64, CachedScript>,
    ctx: rquickjs::Context,
    rt: rquickjs::Runtime,
    /// Makes every declared module name unique, even after a cache eviction.
    seq: u64,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cache.clear();
    }
}

impl Worker {
    fn new() -> Result<Self> {
        let resolver = rquickjs::loader::BuiltinResolver::default()
            .with_module("base64-js")
            .with_module("ieee754")
            .with_module("buffer");
        let loader = rquickjs::loader::BuiltinLoader::default()
            .with_module("base64-js", vendor_base64_js::SCRIPT)
            .with_module("ieee754", vendor_ieee754::SCRIPT)
            .with_module("buffer", vendor_buffer::SCRIPT);

        let rt = rquickjs::Runtime::new()?;
        rt.set_loader(resolver, loader);
        let ctx = rquickjs::Context::full(&rt)?;

        Ok(Worker {
            cache: HashMap::new(),
            ctx,
            rt,
            seq: 0,
        })
    }

    fn run(
        &mut self,
        job: Job,
        max_execution_time: Duration,
        cache_scripts: bool,
        max_cached_scripts: usize,
    ) -> Result<JobOutput> {
        let key = job.cache_key();
        let script = job.script().to_owned();

        // Phase 1: resolve or compile the codec. Compilation is protected by
        // the same timeout, but note that runtime/context creation (which used
        // to be inside the deadline) now happens *outside* of it.
        let deadline = SystemTime::now() + max_execution_time;
        self.rt
            .set_interrupt_handler(Some(Box::new(move || SystemTime::now() > deadline)));

        let cache = &mut self.cache;
        let seq = &mut self.seq;
        let context = &self.ctx;
        let runtime = &self.rt;

        let compiled = context.with(|ctx| -> Result<Persistent<Function<'static>>> {
            // Keep the legacy `chirpstack_input` global available while the
            // user's top-level code is evaluated (upstream set it before
            // declaring the module).
            let input = build_input(&ctx, &job)?;
            ctx.globals().set("chirpstack_input", input)?;

            if cache_scripts && let Some(cached) = cache.get(&key).filter(|c| c.script == script) {
                return Ok(cached.entry.clone());
            }

            let func = compile_entry(&ctx, seq, key, &script)?;
            let persistent = Persistent::save(&ctx, func);

            if cache_scripts {
                if cache.len() >= max_cached_scripts {
                    // Drop every persistent reference before declaring new
                    // modules; unique module names keep this safe.
                    cache.clear();
                }
                cache.insert(
                    key,
                    CachedScript {
                        script: script.clone(),
                        entry: persistent.clone(),
                    },
                );
            }

            Ok(persistent)
        });

        // Phase 2: refresh the deadline *outside* `ctx.with` (the runtime
        // mutex is held by `with`, so setting the handler inside would
        // deadlock). User code therefore always gets the full configured
        // budget, even on a cache miss where compilation just happened.
        let deadline = SystemTime::now() + max_execution_time;
        runtime.set_interrupt_handler(Some(Box::new(move || SystemTime::now() > deadline)));

        let out = match compiled {
            Err(e) => Err(e),
            Ok(entry) => context.with(|ctx| -> Result<JobOutput> {
                let func = entry
                    .clone()
                    .restore(&ctx)
                    .map_err(|e| anyhow!("Restore compiled codec: {}", e))?;

                // Rebuild the input for every call and refresh the legacy
                // global so no previous device's data can leak into this one.
                let input = build_input(&ctx, &job)?;
                ctx.globals().set("chirpstack_input", input.clone())?;

                let res: rquickjs::Object = func
                    .call((input,))
                    .catch(&ctx)
                    .map_err(|e| anyhow!("JS error: {}", e))?;

                let errors: Result<Vec<String>, rquickjs::Error> = res.get("errors");
                if let Ok(errors) = errors
                    && !errors.is_empty()
                {
                    return Err(anyhow!(
                        "{} returned errors: {}",
                        job.name(),
                        errors.join(", ")
                    ));
                }

                match &job {
                    Job::Decode { .. } => {
                        let out = convert::rquickjs_to_struct(&res);
                        let data = out.fields.get("data").cloned().unwrap_or_default();
                        if let Some(pbjson_types::value::Kind::StructValue(v)) = data.kind {
                            Ok(JobOutput::Decode(v))
                        } else {
                            Err(anyhow!("decodeUplink did not return 'data'"))
                        }
                    }
                    Job::Encode { f_port, .. } => {
                        let b: Vec<f64> = res.get("bytes")?;
                        let b: Vec<u8> = b.iter().map(|v| *v as u8).collect();
                        let f_port: f64 = res.get("fPort").unwrap_or(*f_port as f64);
                        Ok(JobOutput::Encode((f_port as u8, b)))
                    }
                }
            }),
        };

        // Never leave an already expired deadline attached to a runtime which
        // is about to be reused (or dropped).
        runtime.set_interrupt_handler(None);
        out
    }
}

/// Build the wrapper module. The user script is evaluated once per compiled
/// codec; the exported function is called once per uplink/downlink.
fn make_script(codec_config: &str, fn_name: &str) -> String {
    let mut script = String::with_capacity(codec_config.len() + 160);
    script.push_str("import { Buffer } from \"buffer\";\n\n");
    script.push_str(codec_config);
    script.push_str("\n\nexport default function __chirpstack_codec_entry(input) {\n");
    script.push_str("    return ");
    script.push_str(fn_name);
    script.push_str("(input);\n}\n");
    script
}

fn compile_entry<'js>(
    ctx: &rquickjs::Ctx<'js>,
    seq: &mut u64,
    key: u64,
    script: &str,
) -> Result<Function<'js>> {
    *seq += 1;
    let name = format!("chirpstack-codec-{:016x}-{}", key, *seq);

    let module = rquickjs::Module::declare(ctx.clone(), name, script.to_owned())
        .catch(ctx)
        .map_err(|e| anyhow!("JS error: {}", e))?;
    let (module, promise) = module
        .eval()
        .catch(ctx)
        .map_err(|e| anyhow!("JS error: {}", e))?;
    () = promise
        .finish()
        .catch(ctx)
        .map_err(|e| anyhow!("JS error: {}", e))?;
    let func: Function = module
        .get("default")
        .catch(ctx)
        .map_err(|e| anyhow!("JS error: {}", e))?;
    Ok(func)
}

fn build_input<'js>(ctx: &rquickjs::Ctx<'js>, job: &Job) -> Result<rquickjs::Object<'js>> {
    let input = rquickjs::Object::new(ctx.clone())?;
    match job {
        Job::Decode {
            recv_time,
            f_port,
            variables,
            bytes,
            ..
        } => {
            input.set("bytes", bytes.clone().into_js(ctx)?)?;
            input.set("fPort", (*f_port).into_js(ctx)?)?;
            input.set("recvTime", (*recv_time).into_js(ctx)?)?;
            input.set("variables", variables.into_js(ctx)?)?;
        }
        Job::Encode {
            variables, data, ..
        } => {
            input.set("variables", variables.into_js(ctx)?)?;
            input.set("data", convert::struct_to_rquickjs(ctx, data))?;
        }
    }
    Ok(input)
}

#[derive(Clone)]
enum Job {
    Decode {
        recv_time: DateTime<Utc>,
        f_port: u8,
        variables: HashMap<String, String>,
        script: String,
        bytes: Vec<u8>,
    },
    Encode {
        f_port: u8,
        variables: HashMap<String, String>,
        script: String,
        data: prost_types::Struct,
    },
}

impl Job {
    fn cache_key(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        match self {
            Job::Decode { script, .. } => {
                0u8.hash(&mut hasher);
                script.hash(&mut hasher);
            }
            Job::Encode { script, .. } => {
                1u8.hash(&mut hasher);
                script.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    fn script(&self) -> &str {
        match self {
            Job::Decode { script, .. } | Job::Encode { script, .. } => script,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Job::Decode { .. } => "decodeUplink",
            Job::Encode { .. } => "encodeDownlink",
        }
    }
}

enum JobOutput {
    Decode(pbjson_types::Struct),
    Encode((u8, Vec<u8>)),
}

/// Run a codec job on a pooled blocking worker.
async fn execute(job: Job) -> Result<JobOutput> {
    let conf = config::get();
    let max_execution_time = conf.codec.js.max_execution_time;
    let cache_scripts = conf.codec.js.cache_scripts;
    let max_cached_scripts = conf.codec.js.max_cached_scripts.max(1);

    let _permit = JS_SEMAPHORE
        .acquire()
        .await
        .map_err(|e| anyhow!("Acquire JS codec worker: {}", e))?;

    tokio::task::spawn_blocking(move || -> Result<JobOutput> {
        if !cache_scripts {
            // Legacy behaviour: one completely fresh runtime per call. Useful
            // for codecs relying on top-level state being reset every time.
            let mut worker = Worker::new()?;
            return worker.run(job, max_execution_time, false, max_cached_scripts);
        }

        WORKER.with(|slot| -> Result<JobOutput> {
            let mut slot = slot.borrow_mut();
            if slot.is_none() {
                *slot = Some(Worker::new()?);
            }
            let out = slot
                .as_mut()
                .unwrap()
                .run(job, max_execution_time, true, max_cached_scripts);
            if out.is_err() {
                // A throwing or timed-out script can leave QuickJS in an
                // inconsistent state. Never hand that runtime to another
                // device; rebuild it on the next call.
                *slot = None;
            }
            out
        })
    })
    .await
    .context("JS codec worker panicked")?
}

pub async fn decode(
    recv_time: DateTime<Utc>,
    f_port: u8,
    variables: &HashMap<String, String>,
    decode_config: &str,
    b: &[u8],
) -> Result<pbjson_types::Struct> {
    let out = execute(Job::Decode {
        recv_time,
        f_port,
        variables: variables.clone(),
        script: make_script(decode_config, "decodeUplink"),
        bytes: b.to_vec(),
    })
    .await?;

    match out {
        JobOutput::Decode(v) => Ok(v),
        JobOutput::Encode(_) => Err(anyhow!("Unexpected JS codec output for decode")),
    }
}

pub async fn encode(
    f_port: u8,
    variables: &HashMap<String, String>,
    encode_config: &str,
    s: &prost_types::Struct,
) -> Result<(u8, Vec<u8>)> {
    let out = execute(Job::Encode {
        f_port,
        variables: variables.clone(),
        script: make_script(encode_config, "encodeDownlink"),
        data: s.clone(),
    })
    .await?;

    match out {
        JobOutput::Encode(v) => Ok(v),
        JobOutput::Decode(_) => Err(anyhow!("Unexpected JS codec output for encode")),
    }
}

#[cfg(test)]
pub mod test {
    use super::*;
    use chrono::TimeZone;

    #[tokio::test]
    pub async fn test_decode_timeout() {
        let decoder = r#"
            function decodeUplink(input) {
                while (true) {

                }
            }
        "#
        .to_string();

        let vars: HashMap<String, String> = HashMap::new();
        let out = decode(Utc::now(), 10, &vars, &decoder, &[0x01, 0x02, 0x03]).await;
        assert!(out.is_err());
    }

    #[tokio::test]
    pub async fn test_decode_error() {
        let decoder = r#"
            function decodeUplink(input) {
                return foo;
            }
        "#
        .to_string();

        let vars: HashMap<String, String> = HashMap::new();
        let out = decode(Utc::now(), 10, &vars, &decoder, &[0x01, 0x02, 0x03]).await;

        assert!(
            out.err()
                .unwrap()
                .to_string()
                .starts_with("JS error: Error: foo is not defined")
        );
    }

    #[tokio::test]
    pub async fn test_decode_buffer_ieee754() {
        let recv_time = Utc.with_ymd_and_hms(2014, 7, 8, 9, 10, 11).unwrap();

        let decoder = r#"
            function decodeUplink(input) {
                var buf = new Buffer(input.bytes);

                return {
                    data: {
                        temp: buf.readFloatBE(0).toFixed(3),
                    }
                };
            }
        "#
        .to_string();

        let vars: HashMap<String, String> = HashMap::new();

        // This tests that the buffer module correctly resolves the ieee754 module.
        let _ = decode(recv_time, 10, &vars, &decoder, &[0x00, 0x00, 0x00, 0x00])
            .await
            .unwrap();
    }

    #[tokio::test]
    pub async fn test_decode() {
        let recv_time = Utc.with_ymd_and_hms(2014, 7, 8, 9, 10, 11).unwrap();

        let decoder = r#"
            function decodeUplink(input) {
                var buff = new Buffer(input.bytes);

                return {
                    data: {
                        f_port: input.fPort,
                        variables: input.variables,
                        data_hex: buff.toString('hex'),
                        data: input.bytes,
                        recv_time: input.recvTime.toString()
                    }
                };
            }
        "#
        .to_string();

        let mut vars: HashMap<String, String> = HashMap::new();
        vars.insert("foo".into(), "bar".into());

        let out = decode(recv_time, 10, &vars, &decoder, &[0x01, 0x02, 0x03])
            .await
            .unwrap();

        let expected = pbjson_types::Struct {
            fields: [
                (
                    "f_port".to_string(),
                    pbjson_types::Value {
                        kind: Some(pbjson_types::value::Kind::NumberValue(10.0)),
                    },
                ),
                (
                    "variables".to_string(),
                    pbjson_types::Value {
                        kind: Some(pbjson_types::value::Kind::StructValue(
                            pbjson_types::Struct {
                                fields: [(
                                    "foo".to_string(),
                                    pbjson_types::Value {
                                        kind: Some(pbjson_types::value::Kind::StringValue(
                                            "bar".to_string(),
                                        )),
                                    },
                                )]
                                .iter()
                                .cloned()
                                .collect(),
                            },
                        )),
                    },
                ),
                (
                    "data_hex".to_string(),
                    pbjson_types::Value {
                        kind: Some(pbjson_types::value::Kind::StringValue("010203".to_string())),
                    },
                ),
                (
                    "data".to_string(),
                    pbjson_types::Value {
                        kind: Some(pbjson_types::value::Kind::ListValue(
                            pbjson_types::ListValue {
                                values: vec![
                                    pbjson_types::Value {
                                        kind: Some(pbjson_types::value::Kind::NumberValue(1.0)),
                                    },
                                    pbjson_types::Value {
                                        kind: Some(pbjson_types::value::Kind::NumberValue(2.0)),
                                    },
                                    pbjson_types::Value {
                                        kind: Some(pbjson_types::value::Kind::NumberValue(3.0)),
                                    },
                                ],
                            },
                        )),
                    },
                ),
                (
                    "recv_time".to_string(),
                    pbjson_types::Value {
                        kind: Some(pbjson_types::value::Kind::StringValue(
                            "Tue Jul 08 2014 09:10:11 GMT+0000".to_string(),
                        )),
                    },
                ),
            ]
            .iter()
            .cloned()
            .collect(),
        };

        assert_eq!(expected, out);
    }

    #[tokio::test]
    pub async fn test_encode_timeout() {
        let encoder = r#"
            function encodeDownlink(input) {
                while (true) {

                }
            }
        "#
        .to_string();

        let vars: HashMap<String, String> = HashMap::new();

        let input = prost_types::Struct {
            ..Default::default()
        };

        let out = encode(10, &vars, &encoder, &input).await;
        assert!(out.is_err());
    }

    #[tokio::test]
    pub async fn test_encode_error() {
        let encoder = r#"
            function encodeDownlink(input) {
                return foo;
            }
        "#
        .to_string();

        let vars: HashMap<String, String> = HashMap::new();

        let input = prost_types::Struct {
            ..Default::default()
        };

        let out = encode(10, &vars, &encoder, &input).await;
        assert!(
            out.err()
                .unwrap()
                .to_string()
                .starts_with("JS error: Error: foo is not defined")
        );
    }

    #[tokio::test]
    pub async fn test_encode() {
        let encoder = r#"
            function encodeDownlink(input) {
                if (input.data.enabled) {
                    return {
                        bytes: [0x01] 
                    };
                } else {
                    return {
                        bytes: [0x02]
                    };
                }
            }
        "#
        .to_string();

        let vars: HashMap<String, String> = HashMap::new();

        let mut input = prost_types::Struct::default();
        input.fields.insert(
            "enabled".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::BoolValue(true)),
            },
        );

        let out = encode(10, &vars, &encoder, &input).await.unwrap();
        assert_eq!((10, vec![1]), out);
    }

    #[tokio::test]
    pub async fn test_encode_fport() {
        let encoder = r#"
            function encodeDownlink(input) {
                return {
                    fPort: 20,
                    bytes: [],
                };
            }
            "#
        .to_string();

        let vars: HashMap<String, String> = HashMap::new();

        let input = prost_types::Struct::default();

        let out = encode(10, &vars, &encoder, &input).await.unwrap();
        assert_eq!((20, vec![]), out);
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    // Manual micro-benchmark, run with:
    //   cargo test --offline -p chirpstack codec::js::bench -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "manual benchmark"]
    async fn bench_decode() {
        let decoder = r#"
            function decodeUplink(input) {
                var buf = new Buffer(input.bytes);
                return {
                    data: {
                        temp: buf.readFloatBE(0).toFixed(3),
                        port: input.fPort,
                        hex: buf.toString('hex'),
                    }
                };
            }
        "#
        .to_string();
        let vars: HashMap<String, String> = HashMap::new();

        // Warm up (the first call also compiles the vendored buffer/ieee754 modules).
        let _ = decode(Utc::now(), 10, &vars, &decoder, &[1, 2, 3, 4]).await;

        let iterations = 200;
        let start = Instant::now();
        for _ in 0..iterations {
            decode(Utc::now(), 10, &vars, &decoder, &[1, 2, 3, 4])
                .await
                .unwrap();
        }
        let elapsed = start.elapsed();
        println!(
            "decode x{}: {:?} ({:?}/call)",
            iterations,
            elapsed,
            elapsed / iterations
        );
    }
}

/// Regression tests for the pooled QuickJS runtime / compiled-script cache.
#[cfg(test)]
mod pool_test {
    use super::*;

    fn decode_job(port: u8, bytes: &[u8], script: &str) -> Job {
        Job::Decode {
            recv_time: Utc::now(),
            f_port: port,
            variables: HashMap::new(),
            script: make_script(script, "decodeUplink"),
            bytes: bytes.to_vec(),
        }
    }

    fn number(s: &pbjson_types::Struct, field: &str) -> f64 {
        match s.fields.get(field).and_then(|v| v.kind.as_ref()) {
            Some(pbjson_types::value::Kind::NumberValue(n)) => *n,
            other => panic!("unexpected value for {field}: {other:?}"),
        }
    }

    fn as_decode(out: JobOutput) -> pbjson_types::Struct {
        match out {
            JobOutput::Decode(s) => s,
            JobOutput::Encode(_) => panic!("expected decode output"),
        }
    }

    #[tokio::test]
    async fn input_is_refreshed_on_every_call() {
        let decoder = r#"
            function decodeUplink(input) {
                return { data: {
                    from_arg: input.fPort,
                    from_global: chirpstack_input.fPort,
                    first_byte: input.bytes[0]
                }};
            }
        "#;
        let vars: HashMap<String, String> = HashMap::new();

        // Same compiled script, varying input: neither the function argument
        // nor the legacy global may retain a previous device's data.
        for (port, byte) in [(5u8, 0x11u8), (9, 0x22), (5, 0x33)] {
            let out = decode(Utc::now(), port, &vars, decoder, &[byte])
                .await
                .unwrap();
            assert_eq!(number(&out, "from_arg"), port as f64);
            assert_eq!(number(&out, "from_global"), port as f64);
            assert_eq!(number(&out, "first_byte"), byte as f64);
        }
    }

    #[tokio::test]
    async fn multiple_scripts_can_coexist_in_the_cache() {
        let first = "function decodeUplink(i) { return { data: { v: 1 } }; }";
        let second = "function decodeUplink(i) { return { data: { v: 2 } }; }";
        let vars: HashMap<String, String> = HashMap::new();

        for (script, expected) in [(first, 1.0), (second, 2.0), (first, 1.0), (second, 2.0)] {
            let out = decode(Utc::now(), 1, &vars, script, &[1]).await.unwrap();
            assert_eq!(number(&out, "v"), expected);
        }
    }

    #[tokio::test]
    async fn recovers_after_js_error() {
        let bad = "function decodeUplink(i) { return foo; }";
        let good = "function decodeUplink(i) { return { data: { ok: i.fPort } }; }";
        let vars: HashMap<String, String> = HashMap::new();

        assert!(decode(Utc::now(), 1, &vars, bad, &[1]).await.is_err());
        let out = decode(Utc::now(), 7, &vars, good, &[1]).await.unwrap();
        assert_eq!(number(&out, "ok"), 7.0);
    }

    #[tokio::test]
    async fn timeout_is_refreshed_for_a_cached_script() {
        let decoder = r#"
            function decodeUplink(input) {
                if (input.bytes[0] === 1) {
                    while (true) {}
                }
                return { data: { ok: input.fPort } };
            }
        "#;
        let vars: HashMap<String, String> = HashMap::new();

        // Warm the compiled-script cache with a benign call.
        let out = decode(Utc::now(), 3, &vars, decoder, &[2]).await.unwrap();
        assert_eq!(number(&out, "ok"), 3.0);

        // Same compiled function: the per-call deadline must still interrupt
        // an infinite loop, and the poisoned worker must be discarded.
        assert!(decode(Utc::now(), 3, &vars, decoder, &[1]).await.is_err());

        let out = decode(Utc::now(), 4, &vars, decoder, &[2]).await.unwrap();
        assert_eq!(number(&out, "ok"), 4.0);
    }

    #[tokio::test]
    async fn concurrent_decodes_are_isolated() {
        let decoder = "function decodeUplink(i) { return { data: { port: i.fPort } }; }";
        let mut handles = Vec::new();

        for port in 0..16u8 {
            let script = decoder.to_string();
            handles.push(tokio::spawn(async move {
                let vars: HashMap<String, String> = HashMap::new();
                decode(Utc::now(), port, &vars, &script, &[port])
                    .await
                    .unwrap()
            }));
        }

        for (port, handle) in handles.into_iter().enumerate() {
            let out = handle.await.unwrap();
            assert_eq!(number(&out, "port"), port as f64);
        }
    }

    #[test]
    fn cache_mode_reuses_the_compiled_module() {
        let script = r#"
            var counter = 0;
            function decodeUplink(i) {
                counter++;
                return { data: { counter: counter } };
            }
        "#;
        let job = decode_job(1, &[1], script);
        let mut worker = Worker::new().unwrap();

        // Top-level state survives between calls: this is the (documented)
        // semantic difference introduced by reusing a runtime.
        for expected in [1.0, 2.0, 3.0] {
            let out = as_decode(
                worker
                    .run(job.clone(), Duration::from_millis(100), true, 64)
                    .unwrap(),
            );
            assert_eq!(number(&out, "counter"), expected);
        }
    }

    #[test]
    fn legacy_mode_resets_top_level_state() {
        let script = r#"
            var counter = 0;
            function decodeUplink(i) {
                counter++;
                return { data: { counter: counter } };
            }
        "#;
        let job = decode_job(1, &[1], script);

        // cache_scripts=false mirrors execute()'s legacy branch: a fresh
        // runtime per call, so top-level state starts over each time.
        for _ in 0..3 {
            let mut worker = Worker::new().unwrap();
            let out = as_decode(
                worker
                    .run(job.clone(), Duration::from_millis(100), false, 64)
                    .unwrap(),
            );
            assert_eq!(number(&out, "counter"), 1.0);
        }
    }

    #[test]
    fn cache_is_bounded() {
        let mut worker = Worker::new().unwrap();

        for i in 0..10u8 {
            let script = format!(
                "function decodeUplink(input) {{ return {{ data: {{ v: {} }} }}; }}",
                i
            );
            let job = decode_job(1, &[1], &script);
            let out = as_decode(
                worker
                    .run(job, Duration::from_millis(100), true, 2)
                    .unwrap(),
            );
            assert_eq!(number(&out, "v"), i as f64);
        }

        assert!(worker.cache.len() <= 2);
    }
}
