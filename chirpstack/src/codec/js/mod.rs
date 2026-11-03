use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Utc};
use rquickjs::{CatchResultExt, Function, IntoJs, Persistent};
use uuid::Uuid;

use super::{CodecIdentity, convert};
use crate::config;

mod vendor_base64_js;
mod vendor_buffer;
mod vendor_ieee754;

/// QuickJS is not thread-safe. Each blocking thread owns one Runtime and a
/// cache of (Context, compiled function) pairs. The semaphore bounds
/// concurrency -- and therefore the number of live runtimes -- to roughly the
/// number of CPUs.
static JS_SEMAPHORE: LazyLock<tokio::sync::Semaphore> = LazyLock::new(|| {
    let workers = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(4);
    tokio::sync::Semaphore::new(workers)
});

thread_local! {
    static WORKER: RefCell<Option<Worker>> = const { RefCell::new(None) };
}

/// Effective settings for one codec invocation. Read once from the global
/// config so a config reload cannot change a job halfway through.
#[derive(Clone, Debug)]
struct JsSettings {
    max_execution_time: Duration,
    cache_scripts: bool,
    max_cached_scripts: usize,
    max_memory_bytes: usize,
    max_stack_bytes: usize,
    max_calls_per_worker: u64,
    gc_every_calls: u64,
}

impl JsSettings {
    fn from_config() -> Self {
        let conf = config::get();
        let js = &conf.codec.js;
        JsSettings {
            max_execution_time: js.max_execution_time,
            cache_scripts: js.cache_scripts,
            max_cached_scripts: js.max_cached_scripts.max(1),
            max_memory_bytes: js.max_memory_bytes,
            max_stack_bytes: js.max_stack_bytes,
            max_calls_per_worker: js.max_calls_per_worker.max(1),
            gc_every_calls: js.gc_every_calls,
        }
    }

    /// 0 => unlimited memory, so no memory-driven recycling. Otherwise recycle
    /// at 75% of the hard limit, before the runtime starts raising OOM errors.
    fn recycle_memory_bytes(&self) -> usize {
        if self.max_memory_bytes == 0 {
            0
        } else {
            self.max_memory_bytes.saturating_mul(3) / 4
        }
    }

    fn gc_due(&self, calls: u64) -> bool {
        self.gc_every_calls > 0 && calls.is_multiple_of(self.gc_every_calls)
    }
}

/// Reusable equivalent of `rquickjs::loader::BuiltinLoader`.
///
/// `BuiltinLoader::load` calls `self.modules.remove(path)`: it is a ONE-SHOT
/// loader. With several Contexts on the same Runtime the first context takes
/// the `buffer` source and every later context fails with
/// "Error loading module 'buffer'". QuickJS registers modules per JSContext,
/// so it legitimately asks the runtime loader again for each new context.
/// This loader keeps its sources and re-declares the module in the calling
/// context, which is exactly what a multi-realm runtime needs.
struct VendorLoader {
    modules: HashMap<String, Vec<u8>>,
}

impl VendorLoader {
    fn new() -> Self {
        let mut modules = HashMap::new();
        modules.insert(
            "base64-js".to_string(),
            vendor_base64_js::SCRIPT.as_bytes().to_vec(),
        );
        modules.insert(
            "ieee754".to_string(),
            vendor_ieee754::SCRIPT.as_bytes().to_vec(),
        );
        modules.insert(
            "buffer".to_string(),
            vendor_buffer::SCRIPT.as_bytes().to_vec(),
        );
        VendorLoader { modules }
    }
}

impl rquickjs::loader::Loader for VendorLoader {
    fn load<'js>(
        &mut self,
        ctx: &rquickjs::Ctx<'js>,
        name: &str,
        _attributes: Option<rquickjs::loader::ImportAttributes<'js>>,
    ) -> rquickjs::Result<rquickjs::Module<'js, rquickjs::module::Declared>> {
        let source = self
            .modules
            .get(name)
            .ok_or_else(|| rquickjs::Error::new_loading(name))?
            .clone();
        rquickjs::Module::declare(ctx.clone(), name.to_owned(), source)
    }
}

/// A compiled codec is scoped to ONE tenant AND ONE device profile. Two
/// profiles using byte-for-byte identical JS must never share a realm, even
/// though hashing the script alone would give the same key.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct CacheKey {
    tenant_id: Option<Uuid>,
    device_profile_id: Option<Uuid>,
    script_hash: u64,
}

impl CacheKey {
    fn new(identity: CodecIdentity, script: &str) -> Self {
        let mut hasher = DefaultHasher::new();
        script.hash(&mut hasher);
        CacheKey {
            tenant_id: identity.tenant_id,
            device_profile_id: identity.device_profile_id,
            script_hash: hasher.finish(),
        }
    }
}

/// One cache entry = one dedicated JS realm. `Drop` frees the persistent
/// function before the Context; `Worker::drop` frees every Context before the
/// Runtime (rquickjs aborts the process otherwise).
struct CachedScript {
    /// Full script text: makes a hash collision harmless.
    script: String,
    /// `Option` so `Drop` can release it explicitly, ahead of `ctx`.
    entry: Option<Persistent<Function<'static>>>,
    ctx: rquickjs::Context,
}

impl Drop for CachedScript {
    fn drop(&mut self) {
        drop(self.entry.take());
    }
}

struct Worker {
    /// Declared first and explicitly cleared in `Drop`: must die before `rt`.
    cache: HashMap<CacheKey, CachedScript>,
    rt: rquickjs::Runtime,
    /// Unique module names, even after a cache eviction.
    seq: u64,
    /// Calls served by this runtime since it was created.
    calls: u64,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cache.clear();
    }
}

impl Worker {
    fn new(settings: &JsSettings) -> Result<Self> {
        let resolver = rquickjs::loader::BuiltinResolver::default()
            .with_module("base64-js")
            .with_module("ieee754")
            .with_module("buffer");
        let loader = VendorLoader::new();

        let rt = rquickjs::Runtime::new()?;
        // NOTE: QuickJS applies these limits to the *Runtime*, not to an
        // individual Context. All contexts of this worker share the budget;
        // `should_recycle` drops the whole worker as soon as it gets close.
        rt.set_memory_limit(settings.max_memory_bytes);
        rt.set_max_stack_size(settings.max_stack_bytes);
        rt.set_loader(resolver, loader);

        Ok(Worker {
            cache: HashMap::new(),
            rt,
            seq: 0,
            calls: 0,
        })
    }

    fn should_recycle(&self, settings: &JsSettings) -> bool {
        if self.calls >= settings.max_calls_per_worker {
            return true;
        }
        let threshold = settings.recycle_memory_bytes();
        if threshold == 0 {
            return false;
        }
        let used = self.rt.memory_usage().memory_used_size;
        used > 0 && used as usize >= threshold
    }

    fn run(&mut self, job: Job, settings: &JsSettings) -> Result<JobOutput> {
        self.calls += 1;
        let key = CacheKey::new(job.identity(), job.script());
        let script = job.script().to_owned();

        let cache = &mut self.cache;
        let seq = &mut self.seq;
        let runtime = &self.rt;

        // ---- Phase A: get (or create) the dedicated realm for this key. ----
        let deadline = SystemTime::now() + settings.max_execution_time;
        runtime.set_interrupt_handler(Some(Box::new(move || SystemTime::now() > deadline)));

        let needs_compile = match cache.get(&key) {
            Some(entry) => entry.script != script,
            None => true,
        };

        if needs_compile {
            if cache.len() >= settings.max_cached_scripts {
                // Each CachedScript::drop frees its Persistent before its
                // Context, so none of them outlives the Runtime.
                cache.clear();
            }

            // Fresh Context => fresh Object.prototype, JSON, globals, Buffer.
            let ctx = rquickjs::Context::full(runtime)?;
            *seq += 1;
            let module_name = format!("chirpstack-codec-{:016x}-{}", key.script_hash, *seq);

            let persistent = ctx.with(|ctx| -> Result<Persistent<Function<'static>>> {
                // The generated wrapper imports "buffer"; QuickJS resolves and
                // compiles buffer / ieee754 / base64-js in *this* Context
                // (its module registry is per JSContext, the loader is shared).
                let input = build_input(&ctx, &job)?;
                ctx.globals().set("chirpstack_input", input)?;
                let func = compile_entry(&ctx, module_name, &script)?;
                Ok(Persistent::save(&ctx, func))
            })?;

            cache.insert(
                key,
                CachedScript {
                    script,
                    entry: Some(persistent),
                    ctx,
                },
            );
        }

        // ---- Phase B: refresh the deadline OUTSIDE ctx.with. ----
        // Context::with holds the runtime mutex; calling set_interrupt_handler
        // inside it would deadlock. User JS therefore always gets a full
        // max_execution_time, even on a cache miss.
        let deadline = SystemTime::now() + settings.max_execution_time;
        runtime.set_interrupt_handler(Some(Box::new(move || SystemTime::now() > deadline)));

        let cached = cache.get(&key).expect("cache entry was just created");
        let entry = cached.entry.as_ref().expect("compiled entry point").clone();

        let out = cached.ctx.with(|ctx| -> Result<JobOutput> {
            let func = entry
                .restore(&ctx)
                .map_err(|e| anyhow!("Restore compiled codec: {}", e))?;

            // Rebuild the input AND the legacy global on every call so a
            // previous device's data cannot leak into this one.
            let input = build_input(&ctx, &job)?;
            ctx.globals().set("chirpstack_input", input.clone())?;

            let res: rquickjs::Object = func.call((input,)).catch(&ctx).map_err(|e| {
                let msg = e.to_string();
                if msg.contains("out of memory") || msg.contains("Allocation") {
                    anyhow!("JS codec memory limit exceeded: {}", msg)
                } else {
                    anyhow!("JS error: {}", msg)
                }
            })?;

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
        });

        // ---- Phase C: GC (cyclic garbage), then remove the expired deadline. ----
        if settings.gc_due(self.calls) {
            runtime.run_gc();
        }
        runtime.set_interrupt_handler(None);
        out
    }
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// A QuickJS allocation failure can surface as a Rust panic deep inside
/// rquickjs/convert helpers. Never let it take down the process: turn it into
/// an error, and let the caller discard the whole worker.
fn run_guarded(worker: &mut Worker, job: Job, settings: &JsSettings) -> Result<JobOutput> {
    match catch_unwind(AssertUnwindSafe(|| worker.run(job, settings))) {
        Ok(out) => out,
        Err(panic) => Err(anyhow!(
            "JS codec engine aborted (worker discarded): {}",
            panic_message(&panic)
        )),
    }
}

/// Run on an explicit worker slot. Keeping this out of the thread_local makes
/// the recycling / isolation logic directly unit-testable.
fn run_on_slot(slot: &mut Option<Worker>, job: Job, settings: &JsSettings) -> Result<JobOutput> {
    if slot.is_none() {
        *slot = Some(Worker::new(settings)?);
    }
    let out = run_guarded(slot.as_mut().unwrap(), job, settings);
    let recycle = match &out {
        Ok(_) => slot.as_ref().unwrap().should_recycle(settings),
        Err(_) => true,
    };
    if recycle {
        *slot = None;
    }
    out
}

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
    module_name: String,
    script: &str,
) -> Result<Function<'js>> {
    let module = rquickjs::Module::declare(ctx.clone(), module_name, script.to_owned())
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
        identity: CodecIdentity,
        recv_time: DateTime<Utc>,
        f_port: u8,
        variables: HashMap<String, String>,
        script: String,
        bytes: Vec<u8>,
    },
    Encode {
        identity: CodecIdentity,
        f_port: u8,
        variables: HashMap<String, String>,
        script: String,
        data: prost_types::Struct,
    },
}

impl Job {
    fn identity(&self) -> CodecIdentity {
        match self {
            Job::Decode { identity, .. } | Job::Encode { identity, .. } => *identity,
        }
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

async fn execute_with_settings(job: Job, settings: JsSettings) -> Result<JobOutput> {
    let _permit = JS_SEMAPHORE
        .acquire()
        .await
        .map_err(|e| anyhow!("Acquire JS codec worker: {}", e))?;

    tokio::task::spawn_blocking(move || -> Result<JobOutput> {
        if !settings.cache_scripts {
            // Legacy behaviour: a completely fresh Runtime + Context per call.
            let mut worker = Worker::new(&settings)?;
            return run_guarded(&mut worker, job, &settings);
        }

        WORKER.with(|cell| {
            let mut slot = cell.borrow_mut();
            run_on_slot(&mut slot, job, &settings)
        })
    })
    .await
    .context("JS codec worker panicked")?
}

async fn execute(job: Job) -> Result<JobOutput> {
    execute_with_settings(job, JsSettings::from_config()).await
}

pub async fn decode(
    identity: CodecIdentity,
    recv_time: DateTime<Utc>,
    f_port: u8,
    variables: &HashMap<String, String>,
    decode_config: &str,
    b: &[u8],
) -> Result<pbjson_types::Struct> {
    let out = execute(Job::Decode {
        identity,
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
    identity: CodecIdentity,
    f_port: u8,
    variables: &HashMap<String, String>,
    encode_config: &str,
    s: &prost_types::Struct,
) -> Result<(u8, Vec<u8>)> {
    let out = execute(Job::Encode {
        identity,
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
fn pooled_settings() -> JsSettings {
    let mut settings = JsSettings::from_config();
    settings.cache_scripts = true;
    settings
}

#[cfg(test)]
async fn decode_with_settings(
    identity: CodecIdentity,
    f_port: u8,
    script: &str,
    bytes: &[u8],
    settings: JsSettings,
) -> anyhow::Result<pbjson_types::Struct> {
    let job = Job::Decode {
        identity,
        recv_time: Utc::now(),
        f_port,
        variables: HashMap::new(),
        script: make_script(script, "decodeUplink"),
        bytes: bytes.to_vec(),
    };
    match execute_with_settings(job, settings).await? {
        JobOutput::Decode(v) => Ok(v),
        JobOutput::Encode(_) => Err(anyhow!("Unexpected JS codec output for decode")),
    }
}

#[cfg(test)]
pub mod test {
    use super::*;
    use chrono::TimeZone;

    // The production API now requires a caller identity for cache isolation.
    // Keep the upstream test bodies untouched by providing local shims.
    async fn decode(
        recv_time: DateTime<Utc>,
        f_port: u8,
        variables: &HashMap<String, String>,
        decoder: &str,
        b: &[u8],
    ) -> anyhow::Result<pbjson_types::Struct> {
        super::decode(
            crate::codec::CodecIdentity::default(),
            recv_time,
            f_port,
            variables,
            decoder,
            b,
        )
        .await
    }

    async fn encode(
        f_port: u8,
        variables: &HashMap<String, String>,
        encoder: &str,
        s: &prost_types::Struct,
    ) -> anyhow::Result<(u8, Vec<u8>)> {
        super::encode(
            crate::codec::CodecIdentity::default(),
            f_port,
            variables,
            encoder,
            s,
        )
        .await
    }

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

    fn decoder() -> String {
        r#"
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
        .to_string()
    }

    async fn bench_once(label: &str, settings: JsSettings) {
        let script = decoder();
        let identity = CodecIdentity::default();

        // Warm up (also compiles the vendored buffer/ieee754 modules).
        let _ = decode_with_settings(identity, 10, &script, &[1, 2, 3, 4], settings.clone()).await;

        let iterations = 200;
        let start = Instant::now();
        for _ in 0..iterations {
            decode_with_settings(identity, 10, &script, &[1, 2, 3, 4], settings.clone())
                .await
                .unwrap();
        }
        let elapsed = start.elapsed();
        println!(
            "{label}: decode x{iterations}: {elapsed:?} ({:?}/call)",
            elapsed / iterations
        );
    }

    // Run with:
    //   cargo test --offline -p chirpstack codec::js::bench -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "manual benchmark"]
    async fn bench_decode() {
        bench_once("pooled, gc=default", pooled_settings()).await;

        let mut every_call = pooled_settings();
        every_call.gc_every_calls = 1;
        bench_once("pooled, gc=1", every_call).await;

        let mut without_gc = pooled_settings();
        without_gc.gc_every_calls = 0;
        bench_once("pooled, gc=0", without_gc).await;

        let mut legacy = pooled_settings();
        legacy.cache_scripts = false;
        bench_once("legacy (fresh runtime)", legacy).await;
    }

    // Cost of a cache miss: a new Context + vendored modules for a new key.
    #[tokio::test]
    #[ignore = "manual benchmark"]
    async fn bench_cache_miss() {
        let script = "function decodeUplink(i) { return { data: { ok: i.fPort } }; }";
        let settings = pooled_settings();
        let iterations = 40;
        let start = Instant::now();
        for i in 0..iterations {
            let id = CodecIdentity {
                tenant_id: Some(Uuid::from_u128(1)),
                device_profile_id: Some(Uuid::from_u128(i as u128 + 1000)),
            };
            decode_with_settings(id, 1, script, &[1], settings.clone())
                .await
                .unwrap();
        }
        let elapsed = start.elapsed();
        println!(
            "cache miss x{iterations}: {elapsed:?} ({:?}/call)",
            elapsed / iterations
        );
    }
}

/// Security / isolation / resource-limit regression tests for the hardened
/// QuickJS pool. A local `Option<Worker>` slot is used wherever a test needs
/// determinism (Tokio may dispatch blocking jobs to different threads).
#[cfg(test)]
mod pool_test {
    use super::*;
    use std::time::Instant;

    fn identity(tenant: u128, profile: u128) -> CodecIdentity {
        CodecIdentity {
            tenant_id: Some(Uuid::from_u128(tenant)),
            device_profile_id: Some(Uuid::from_u128(profile)),
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

    fn decode_job(id: CodecIdentity, port: u8, bytes: &[u8], script: &str) -> Job {
        Job::Decode {
            identity: id,
            recv_time: Utc::now(),
            f_port: port,
            variables: HashMap::new(),
            script: make_script(script, "decodeUplink"),
            bytes: bytes.to_vec(),
        }
    }

    fn run_decode(
        slot: &mut Option<Worker>,
        id: CodecIdentity,
        port: u8,
        bytes: &[u8],
        script: &str,
        settings: &JsSettings,
    ) -> Result<pbjson_types::Struct> {
        Ok(as_decode(run_on_slot(
            slot,
            decode_job(id, port, bytes, script),
            settings,
        )?))
    }

    #[test]
    fn input_is_refreshed_on_every_call() {
        let decoder = r#"
            function decodeUplink(input) {
                return { data: {
                    from_arg: input.fPort,
                    from_global: chirpstack_input.fPort,
                    first_byte: input.bytes[0]
                }};
            }
        "#;
        let mut slot = None;
        let settings = pooled_settings();

        for (port, byte) in [(5u8, 0x11u8), (9, 0x22), (5, 0x33)] {
            let out =
                run_decode(&mut slot, identity(1, 1), port, &[byte], decoder, &settings).unwrap();
            assert_eq!(number(&out, "from_arg"), port as f64);
            assert_eq!(number(&out, "from_global"), port as f64);
            assert_eq!(number(&out, "first_byte"), byte as f64);
        }
    }

    #[test]
    fn multiple_scripts_can_coexist_in_the_cache() {
        let first = "function decodeUplink(i) { return { data: { v: 1 } }; }";
        let second = "function decodeUplink(i) { return { data: { v: 2 } }; }";
        let mut slot = None;
        let settings = pooled_settings();

        for (script, expected) in [(first, 1.0), (second, 2.0), (first, 1.0), (second, 2.0)] {
            let out = run_decode(&mut slot, identity(1, 1), 1, &[1], script, &settings).unwrap();
            assert_eq!(number(&out, "v"), expected);
        }
        assert_eq!(slot.as_ref().unwrap().cache.len(), 2);
    }

    #[test]
    fn recovers_after_js_error() {
        let bad = "function decodeUplink(i) { return foo; }";
        let good = "function decodeUplink(i) { return { data: { ok: i.fPort } }; }";
        let mut slot = None;
        let settings = pooled_settings();

        assert!(run_decode(&mut slot, identity(1, 1), 1, &[1], bad, &settings).is_err());
        assert!(slot.is_none(), "a poisoned worker must never be reused");
        let out = run_decode(&mut slot, identity(1, 1), 7, &[1], good, &settings).unwrap();
        assert_eq!(number(&out, "ok"), 7.0);
    }

    #[test]
    fn timeout_is_refreshed_for_a_cached_script() {
        let decoder = r#"
            function decodeUplink(input) {
                if (input.bytes[0] === 1) { while (true) {} }
                return { data: { ok: input.fPort } };
            }
        "#;
        let mut slot = None;
        let settings = pooled_settings();

        let out = run_decode(&mut slot, identity(1, 1), 3, &[2], decoder, &settings).unwrap();
        assert_eq!(number(&out, "ok"), 3.0);
        assert!(run_decode(&mut slot, identity(1, 1), 3, &[1], decoder, &settings).is_err());
        assert!(slot.is_none(), "timed-out worker must be discarded");
        let out = run_decode(&mut slot, identity(1, 1), 4, &[2], decoder, &settings).unwrap();
        assert_eq!(number(&out, "ok"), 4.0);
    }

    #[tokio::test]
    async fn concurrent_decodes_are_isolated() {
        let decoder = "function decodeUplink(i) { return { data: { port: i.fPort } }; }";
        let settings = pooled_settings();
        let mut handles = Vec::new();

        for port in 0..16u8 {
            let script = decoder.to_string();
            let settings = settings.clone();
            handles.push(tokio::spawn(async move {
                decode_with_settings(identity(1, 1), port, &script, &[port], settings)
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
        let settings = pooled_settings();
        let job = decode_job(identity(1, 1), 1, &[1], script);
        let mut worker = Worker::new(&settings).unwrap();

        // Documented semantic difference vs upstream: module-level state
        // survives between calls of the SAME (tenant, profile, script).
        for expected in [1.0, 2.0, 3.0] {
            let out = as_decode(worker.run(job.clone(), &settings).unwrap());
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
        let settings = pooled_settings();
        let job = decode_job(identity(1, 1), 1, &[1], script);

        // cache_scripts=false creates a fresh Worker per call in execute().
        for _ in 0..3 {
            let mut worker = Worker::new(&settings).unwrap();
            let out = as_decode(worker.run(job.clone(), &settings).unwrap());
            assert_eq!(number(&out, "counter"), 1.0);
        }
    }

    #[test]
    fn cache_is_bounded() {
        let mut settings = pooled_settings();
        settings.max_cached_scripts = 2;
        let mut worker = Worker::new(&settings).unwrap();

        for i in 0..10u8 {
            let script = format!(
                "function decodeUplink(input) {{ return {{ data: {{ v: {} }} }}; }}",
                i
            );
            let job = decode_job(identity(1, 1), 1, &[1], &script);
            let out = as_decode(worker.run(job, &settings).unwrap());
            assert_eq!(number(&out, "v"), i as f64);
        }
        assert!(worker.cache.len() <= 2);
    }

    /// HARDENING 1: one Context per cache entry, even for identical script
    /// text. A codec poisoning Object.prototype / JSON must not affect the
    /// realm of another device profile.
    #[test]
    fn prototype_and_json_pollution_is_confined_to_one_profile() {
        let polluter = r#"
            function decodeUplink(input) {
                if (input.bytes[0] === 1) {
                    Object.prototype.x = 'poisoned';
                    JSON.parse = function () { return { hijacked: true }; };
                    return { data: { done: 1 } };
                }
                var probe = {};
                var parsed = JSON.parse('{"ok":1}');
                return { data: {
                    leaked_x: (probe.x === undefined) ? 0 : 1,
                    json_ok: (parsed.ok === 1) ? 1 : 0
                }};
            }
        "#;
        let other_script = r#"
            function decodeUplink(input) {
                var probe = {};
                var parsed = JSON.parse('{"ok":1}');
                return { data: {
                    leaked_x: (probe.x === undefined) ? 0 : 1,
                    json_ok: (parsed.ok === 1) ? 1 : 0
                }};
            }
        "#;
        let mut slot = None;
        let settings = pooled_settings();

        // Same text, two different profiles: the second realm must be clean.
        run_decode(&mut slot, identity(1, 1), 1, &[1], polluter, &settings).unwrap();
        for script in [polluter, other_script] {
            let out = run_decode(&mut slot, identity(1, 2), 1, &[2], script, &settings).unwrap();
            assert_eq!(number(&out, "leaked_x"), 0.0, "Object.prototype leaked");
            assert_eq!(number(&out, "json_ok"), 1.0, "JSON.parse was replaced");
        }
        // A third, unrelated tenant/profile is clean too.
        let out = run_decode(&mut slot, identity(2, 3), 1, &[2], other_script, &settings).unwrap();
        assert_eq!(number(&out, "leaked_x"), 0.0);
        assert_eq!(number(&out, "json_ok"), 1.0);
    }

    /// HARDENING 2: the cache key includes tenant + device profile, so the
    /// identical script text cannot share module-level state across tenants.
    #[test]
    fn same_script_is_not_shared_between_tenants_or_profiles() {
        let script = r#"
            var counter = 0;
            function decodeUplink(i) {
                counter++;
                return { data: { counter: counter } };
            }
        "#;
        let a = identity(1, 10);
        let b = identity(2, 20);
        let mut slot = None;
        let settings = pooled_settings();

        for (id, expected) in [(a, 1.0), (a, 2.0), (b, 1.0), (a, 3.0), (b, 2.0), (a, 4.0)] {
            let out = run_decode(&mut slot, id, 1, &[1], script, &settings).unwrap();
            assert_eq!(number(&out, "counter"), expected);
        }
        assert_eq!(slot.as_ref().unwrap().cache.len(), 2);
    }

    /// HARDENING 1 (structural): two entries really own two distinct JS
    /// contexts, while sharing a single Runtime.
    #[test]
    fn each_cache_entry_has_its_own_context() {
        let script = "function decodeUplink(i) { return { data: { ok: 1 } }; }";
        let mut slot = None;
        let settings = pooled_settings();

        run_decode(&mut slot, identity(1, 1), 1, &[1], script, &settings).unwrap();
        run_decode(&mut slot, identity(1, 2), 1, &[1], script, &settings).unwrap();

        let worker = slot.as_ref().unwrap();
        assert_eq!(worker.cache.len(), 2);
        let ptrs: Vec<usize> = worker
            .cache
            .values()
            .map(|e| e.ctx.as_raw().as_ptr() as usize)
            .collect();
        assert_ne!(ptrs[0], ptrs[1], "contexts must not be shared");
    }

    /// HARDENING 3: memory limit is applied to every runtime of the pool.
    #[test]
    fn resource_limits_are_applied_to_the_runtime() {
        let mut settings = pooled_settings();
        settings.max_memory_bytes = 4 * 1024 * 1024;
        settings.max_stack_bytes = 256 * 1024;
        let worker = Worker::new(&settings).unwrap();
        let usage = worker.rt.memory_usage();
        assert_eq!(usage.malloc_limit as usize, settings.max_memory_bytes);
    }

    /// HARDENING 3: runaway allocation produces a clean error (no process
    /// abort), discards the worker, and the next call works again.
    #[test]
    fn memory_limit_returns_error_and_discards_worker() {
        let mut settings = pooled_settings();
        settings.max_memory_bytes = 8 * 1024 * 1024;
        settings.max_execution_time = Duration::from_secs(2);
        settings.gc_every_calls = 0;
        let hog = r#"
            function decodeUplink(input) {
                var sink = [];
                while (true) { sink.push(new Array(20000).fill(7)); }
            }
        "#;
        let healthy = "function decodeUplink(i) { return { data: { ok: 1 } }; }";
        let mut slot = None;

        let err = run_decode(&mut slot, identity(1, 1), 1, &[1], hog, &settings)
            .expect_err("allocation loop must fail");
        println!("memory-limit error: {err:#}");
        assert!(slot.is_none(), "OOM worker must be discarded");

        let out = run_decode(&mut slot, identity(1, 2), 1, &[1], healthy, &settings).unwrap();
        assert_eq!(number(&out, "ok"), 1.0);
    }

    /// HARDENING 3: unbounded recursion is caught by the JS stack limit.
    #[test]
    fn stack_overflow_returns_error_without_crashing() {
        let mut settings = pooled_settings();
        settings.max_stack_bytes = 128 * 1024;
        settings.max_execution_time = Duration::from_secs(2);
        settings.gc_every_calls = 0;
        let recursive = "function decodeUplink(i) { return decodeUplink(i); }";
        let healthy = "function decodeUplink(i) { return { data: { ok: 1 } }; }";
        let mut slot = None;

        let err = run_decode(&mut slot, identity(1, 1), 1, &[1], recursive, &settings)
            .expect_err("infinite recursion must fail");
        println!("stack-limit error: {err:#}");
        assert!(slot.is_none());

        let out = run_decode(&mut slot, identity(1, 2), 1, &[1], healthy, &settings).unwrap();
        assert_eq!(number(&out, "ok"), 1.0);
    }

    /// HARDENING 3: recycling after N calls yields identical results while
    /// resetting module-level state (proof the worker really was replaced).
    #[test]
    fn worker_is_recycled_after_max_calls_per_worker() {
        let mut settings = pooled_settings();
        settings.max_calls_per_worker = 3;
        settings.max_memory_bytes = 0; // unlimited => no memory-driven recycle
        settings.gc_every_calls = 0;
        let script = r#"
            var counter = 0;
            function decodeUplink(i) {
                counter++;
                return { data: { counter: counter } };
            }
        "#;
        let mut slot = None;

        for expected in [1.0, 2.0, 3.0, 1.0, 2.0, 3.0, 1.0, 2.0] {
            let out = run_decode(&mut slot, identity(1, 1), 1, &[1], script, &settings).unwrap();
            assert_eq!(number(&out, "counter"), expected);
        }
    }

    /// One Context per cached script is more expensive than one shared realm;
    /// verify that a full cache still fits inside the configured hard limit.
    #[test]
    fn a_full_cache_stays_within_the_memory_limit() {
        let mut settings = pooled_settings();
        settings.gc_every_calls = 0;
        let script = "function decodeUplink(i) { return { data: { ok: 1 } }; }";
        let mut worker = Worker::new(&settings).unwrap();

        for i in 0..settings.max_cached_scripts as u128 {
            let job = decode_job(identity(1, i), 1, &[1], script);
            let out = as_decode(worker.run(job, &settings).unwrap());
            assert_eq!(number(&out, "ok"), 1.0);
        }

        let used = worker.rt.memory_usage().memory_used_size as usize;
        println!(
            "memory_used_size with {} contexts: {} bytes (hard limit {})",
            worker.cache.len(),
            used,
            settings.max_memory_bytes
        );
        assert_eq!(worker.cache.len(), settings.max_cached_scripts);
        assert!(used < settings.max_memory_bytes);
        assert!(used < settings.recycle_memory_bytes());
    }

    /// JS_RunGC walks every context of the runtime, so its cost must be
    /// measured with a FULL cache, not with a single realm.
    #[test]
    #[ignore = "manual benchmark"]
    fn bench_gc_with_a_full_cache() {
        let script = "function decodeUplink(i) { return { data: { ok: 1 } }; }";
        let mut settings = pooled_settings();
        settings.gc_every_calls = 0;
        let mut worker = Worker::new(&settings).unwrap();

        for i in 0..settings.max_cached_scripts as u128 {
            worker
                .run(decode_job(identity(1, i), 1, &[1], script), &settings)
                .unwrap();
        }

        let job = decode_job(identity(1, 0), 1, &[1], script);
        for gc in [0u64, 1, 100] {
            settings.gc_every_calls = gc;
            let start = Instant::now();
            for _ in 0..200 {
                worker.run(job.clone(), &settings).unwrap();
            }
            println!(
                "{} contexts, gc={gc}: {:?}/call",
                worker.cache.len(),
                start.elapsed() / 200
            );
        }
    }
}
