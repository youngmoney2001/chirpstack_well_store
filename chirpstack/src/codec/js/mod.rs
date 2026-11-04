use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime};

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use rquickjs::{CatchResultExt, Function, IntoJs, Persistent};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::timeout;
use tracing::error;
use uuid::Uuid;

use super::{CodecIdentity, convert};
use crate::config;
use crate::monitoring::prometheus;

mod vendor_base64_js;
mod vendor_buffer;
mod vendor_ieee754;

/// Panics caught inside QuickJS. On such a panic we deliberately leak the
/// runtimes instead of dropping them (see run_slot), so this counter is the
/// only way to observe the leak.
static CODEC_PANIC_COUNTER: LazyLock<Counter> = LazyLock::new(|| {
    let counter = Counter::default();
    prometheus::register(
        "codec_js_panics_total",
        "Number of panics caught inside the JS codec engine (runtimes leaked on purpose)",
        counter.clone(),
    );
    counter
});

static CODEC_QUEUE_FULL_COUNTER: LazyLock<Counter> = LazyLock::new(|| {
    let counter = Counter::default();
    prometheus::register(
        "codec_js_queue_full_total",
        "Number of JS codec jobs rejected because the bounded worker queue was full",
        counter.clone(),
    );
    counter
});

/// Cache hits / misses on the per-thread LRU.
static CODEC_CACHE_HITS: LazyLock<Counter> = LazyLock::new(|| {
    let counter = Counter::default();
    prometheus::register(
        "codec_js_cache_hits_total",
        "Number of JS codec calls served by a cached realm",
        counter.clone(),
    );
    counter
});

static CODEC_CACHE_MISSES: LazyLock<Counter> = LazyLock::new(|| {
    let counter = Counter::default();
    prometheus::register(
        "codec_js_cache_misses_total",
        "Number of JS codec calls requiring a new realm (Runtime + modules + compile)",
        counter.clone(),
    );
    counter
});

/// Evictions, labelled by reason: lru / budget / error.
static CODEC_CACHE_EVICTIONS: LazyLock<Family<Labels, Counter>> = LazyLock::new(|| {
    let family = Family::default();
    prometheus::register(
        "codec_js_cache_evictions_total",
        "Number of JS codec realms evicted, by reason (lru / budget / error)",
        family.clone(),
    );
    family
});

/// Routing observability: detects a hot profile concentrating load on one
/// dedicated thread (hash(key) % N is deterministic, not load-aware).
static CODEC_JOBS_BY_WORKER: LazyLock<Family<Labels, Counter>> = LazyLock::new(|| {
    let family = Family::default();
    prometheus::register(
        "codec_js_jobs_by_worker_total",
        "Number of JS codec jobs handled by each dedicated worker thread",
        family.clone(),
    );
    family
});

/// A FIXED number of dedicated OS threads, each owning exactly one Worker and
/// ONE bounded channel. Jobs are routed by hash(tenant, profile, script) % N so
/// that a given script is compiled and cached exactly once in the process.
///
/// Hard bounds:
///   threads        = `codec.js.workers` (default available_parallelism)
///   waiting jobs   = N * `codec.js.queue_capacity` (default 256 per thread)
///   JS memory      <= N * `codec.js.max_cache_memory_bytes`
///                  (each realm also capped by `max_memory_bytes`,
///                   and at most `max_cached_scripts` realms per thread)
static CODEC_POOL: LazyLock<CodecPool> = LazyLock::new(CodecPool::new);

type Labels = Vec<(String, String)>;

fn reason_labels(reason: &str) -> Labels {
    vec![("reason".to_owned(), reason.to_owned())]
}

fn worker_labels(worker: usize) -> Labels {
    vec![("worker".to_owned(), worker.to_string())]
}

struct JobMessage {
    job: Job,
    settings: JsSettings,
    reply: tokio::sync::oneshot::Sender<Result<JobOutput>>,
}

struct CodecPool {
    /// ONE bounded channel per worker thread; no shared mutex on the receive
    /// side, so workers never serialise on channel access.
    senders: Vec<Sender<JobMessage>>,
    /// Lightweight per-thread job counters used by the routing tests and the
    /// imbalance benchmark (also exported as a Prometheus metric).
    jobs_per_worker: Vec<Arc<AtomicU64>>,
    workers: usize,
    /// Capacity of EACH per-thread channel; total queued = N * queue_capacity.
    queue_capacity: usize,
    queue_wait_timeout: Duration,
}

impl CodecPool {
    fn new() -> Self {
        let conf = config::get();
        let workers = if conf.codec.js.workers == 0 {
            std::thread::available_parallelism()
                .map(|v| v.get())
                .unwrap_or(4)
        } else {
            conf.codec.js.workers
        }
        .max(1);
        let queue_capacity = if conf.codec.js.queue_capacity == 0 {
            256
        } else {
            conf.codec.js.queue_capacity.max(1)
        };
        CodecPool::start(workers, queue_capacity, conf.codec.js.queue_wait_timeout)
    }

    /// Build N channels. `start` moves the receivers into the dedicated OS
    /// threads; tests can keep them alive instead to simulate a full queue.
    fn channels(
        workers: usize,
        queue_capacity: usize,
        queue_wait_timeout: Duration,
    ) -> (Self, Vec<Receiver<JobMessage>>) {
        let mut senders = Vec::with_capacity(workers);
        let mut receivers = Vec::with_capacity(workers);
        let mut jobs_per_worker = Vec::with_capacity(workers);
        for _ in 0..workers {
            let (tx, rx) = tokio::sync::mpsc::channel(queue_capacity.max(1));
            senders.push(tx);
            receivers.push(rx);
            jobs_per_worker.push(Arc::new(AtomicU64::new(0)));
        }
        (
            CodecPool {
                senders,
                jobs_per_worker,
                workers,
                queue_capacity,
                queue_wait_timeout,
            },
            receivers,
        )
    }

    fn start(workers: usize, queue_capacity: usize, queue_wait_timeout: Duration) -> Self {
        let (pool, receivers) = CodecPool::channels(workers, queue_capacity, queue_wait_timeout);
        pool.spawn_threads(receivers);
        pool
    }

    fn spawn_threads(&self, receivers: Vec<Receiver<JobMessage>>) {
        for (i, rx) in receivers.into_iter().enumerate() {
            let counter = Arc::clone(&self.jobs_per_worker[i]);
            if let Err(e) = std::thread::Builder::new()
                .name(format!("chirpstack-js-codec-{i}"))
                .spawn(move || codec_thread(i, rx, counter))
            {
                error!(error = %e, "Unable to start a JS codec worker thread");
            }
        }
    }
}

fn codec_thread(index: usize, mut rx: Receiver<JobMessage>, jobs: Arc<AtomicU64>) {
    // ONE Worker per OS thread: its LRU holds one Runtime+Context per script.
    let mut slot: Option<Worker> = None;
    loop {
        // A dedicated OS thread, never an async context: blocking_recv is safe.
        let Some(message) = rx.blocking_recv() else {
            break;
        };
        jobs.fetch_add(1, Ordering::Relaxed);
        CODEC_JOBS_BY_WORKER
            .get_or_create(&worker_labels(index))
            .inc();

        let JobMessage {
            job,
            settings,
            reply,
        } = message;
        let out = run_on_slot(&mut slot, job, &settings);
        // The caller may have been cancelled; that is not a worker failure.
        let _ = reply.send(out);
    }
}

/// Effective settings for one codec invocation.
#[derive(Clone, Debug)]
struct JsSettings {
    max_execution_time: Duration,
    cache_scripts: bool,
    max_cached_scripts: usize,
    max_script_bytes: usize,
    /// Real per-thread budget: sum of `memory_usage()` over cached realms.
    max_cache_memory_bytes: usize,
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
            max_script_bytes: js.max_script_bytes.max(1),
            max_cache_memory_bytes: js.max_cache_memory_bytes,
            max_memory_bytes: js.max_memory_bytes,
            max_stack_bytes: js.max_stack_bytes,
            max_calls_per_worker: js.max_calls_per_worker.max(1),
            gc_every_calls: js.gc_every_calls,
        }
    }

    /// Per-script memory watermark. 0 (unlimited) disables memory-driven
    /// eviction. With one Runtime per script this is now a real per-script
    /// budget, not a budget shared between tenants.
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

/// Deterministic routing: the same cache key always reaches the same thread,
/// so a script is compiled once instead of once per worker.
fn route_worker(key: &CacheKey, workers: usize) -> usize {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    (hasher.finish() % workers as u64) as usize
}

/// Reusable equivalent of rquickjs' one-shot BuiltinLoader.
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

/// Importing "buffer" transitively loads ieee754 + base64-js. This module is
/// evaluated ONCE per new realm, BEFORE any interrupt deadline is armed: it is
/// internal, fixed-size code whose cost must not be charged to the user's
/// max_execution_time budget.
const VENDOR_BOOTSTRAP_MODULE: &str = "__chirpstack_vendor_bootstrap__";
const VENDOR_BOOTSTRAP: &str = "import { Buffer } from \"buffer\";\nexport default 1;\n";

/// ONE cache entry = ONE complete QuickJS realm: its own Runtime, Context,
/// loader, memory/stack limits and compiled entry point. Nothing is shared
/// with another script/tenant.
struct CachedScript {
    script: String,
    last_used: u64,
    calls: u64,
    entry: Option<Persistent<Function<'static>>>,
    ctx: rquickjs::Context,
    rt: rquickjs::Runtime,
}

impl Drop for CachedScript {
    fn drop(&mut self) {
        // Persistent -> Context -> Runtime. rquickjs aborts if a Persistent
        // outlives its runtime.
        drop(self.entry.take());
    }
}

impl CachedScript {
    /// Create the realm and preload the vendored modules. NO interrupt handler
    /// is armed here (see the deadline ordering in Worker::run).
    fn new(settings: &JsSettings) -> Result<Self> {
        let rt = rquickjs::Runtime::new()?;
        rt.set_memory_limit(settings.max_memory_bytes);
        rt.set_max_stack_size(settings.max_stack_bytes);
        let resolver = rquickjs::loader::BuiltinResolver::default()
            .with_module("base64-js")
            .with_module("ieee754")
            .with_module("buffer");
        rt.set_loader(resolver, VendorLoader::new());
        let ctx = rquickjs::Context::full(&rt)?;

        ctx.with(|ctx| -> Result<()> {
            let module =
                rquickjs::Module::declare(ctx.clone(), VENDOR_BOOTSTRAP_MODULE, VENDOR_BOOTSTRAP)
                    .catch(&ctx)
                    .map_err(|e| anyhow!("JS error loading vendored modules: {}", e))?;
            let (_, promise) = module
                .eval()
                .catch(&ctx)
                .map_err(|e| anyhow!("JS error loading vendored modules: {}", e))?;
            () = promise
                .finish()
                .catch(&ctx)
                .map_err(|e| anyhow!("JS error loading vendored modules: {}", e))?;
            Ok(())
        })?;

        Ok(CachedScript {
            script: String::new(),
            last_used: 0,
            calls: 0,
            entry: None,
            ctx,
            rt,
        })
    }
}

/// One LRU of per-script realms per dedicated OS thread. Contains no Runtime
/// itself: every Runtime lives in a CachedScript and is bounded by
/// max_cached_scripts.
struct Worker {
    cache: HashMap<CacheKey, CachedScript>,
    seq: u64,
    calls: u64,
    clock: u64,
    /// Small step log used by the deadline-ordering test. Cleared every run so
    /// it cannot grow.
    trace: Vec<&'static str>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Every CachedScript::drop frees its Persistent/Context/Runtime safely.
        self.cache.clear();
    }
}

impl Worker {
    fn new() -> Self {
        Worker {
            cache: HashMap::new(),
            seq: 0,
            calls: 0,
            clock: 0,
            trace: Vec::new(),
        }
    }

    fn trace(&mut self, step: &'static str) {
        self.trace.push(step);
    }

    fn should_recycle(&self, settings: &JsSettings) -> bool {
        // Memory is now per script (evicted in run); only long-lived state
        // accumulation justifies discarding the whole thread worker.
        self.calls >= settings.max_calls_per_worker
    }

    /// Sum of QuickJS `memory_usage()` over every cached realm. This is the
    /// accounting used for the per-thread budget (not the process RSS).
    fn cache_bytes(&self) -> usize {
        self.cache.values().fold(0usize, |acc, e| {
            acc.saturating_add(e.rt.memory_usage().memory_used_size as usize)
        })
    }

    fn remove_entry(&mut self, key: &CacheKey, reason: &str) {
        if self.cache.remove(key).is_some() {
            CODEC_CACHE_EVICTIONS
                .get_or_create(&reason_labels(reason))
                .inc();
        }
    }

    fn evict_oldest(&mut self, reason: &str) {
        let oldest = self
            .cache
            .iter()
            .min_by_key(|(_, e)| e.last_used)
            .map(|(k, _)| *k);
        if let Some(key) = oldest {
            self.remove_entry(&key, reason);
        }
    }

    fn evict_lru(&mut self, max: usize) {
        if self.cache.len() >= max {
            self.evict_oldest("lru");
        }
    }

    /// Evict LRU realms until the sum of their real memory usage fits the
    /// per-thread budget. `max_memory_bytes` remains the per-script ceiling.
    fn enforce_budget(&mut self, budget: usize) {
        if budget == 0 {
            return;
        }
        while self.cache_bytes() > budget && !self.cache.is_empty() {
            self.evict_oldest("budget");
        }
    }

    fn run(&mut self, job: Job, settings: &JsSettings) -> Result<JobOutput> {
        if job.panic_probe() {
            panic!("codec panic probe");
        }

        self.calls += 1;
        self.clock += 1;
        self.trace.clear();

        let key = CacheKey::new(job.identity(), job.script());
        let script = job.script();

        // User-controlled input: bound it before any work is done.
        if script.len() > settings.max_script_bytes {
            return Err(anyhow!(
                "JS codec script is too large ({} > {} bytes)",
                script.len(),
                settings.max_script_bytes
            ));
        }
        let script = script.to_owned();

        let needs_compile = match self.cache.get(&key) {
            Some(entry) => entry.script != script,
            None => true,
        };
        if needs_compile {
            CODEC_CACHE_MISSES.inc();
        } else {
            CODEC_CACHE_HITS.inc();
        }

        if needs_compile {
            // A changed script under the same key must free its old realm
            // before the LRU bound is evaluated.
            self.remove_entry(&key, "error");
            self.evict_lru(settings.max_cached_scripts);
            self.seq += 1;
            let module_name = format!("chirpstack-codec-{:016x}-{}", key.script_hash, self.seq);

            // (1) Runtime + Context + limits + vendored modules: internal cost,
            //     NO deadline armed yet.
            let mut cached = CachedScript::new(settings)?;
            self.trace("vendor_loaded");

            // (2) NOW arm the deadline, before compiling user-controlled text.
            let deadline = SystemTime::now() + settings.max_execution_time;
            cached
                .rt
                .set_interrupt_handler(Some(Box::new(move || SystemTime::now() > deadline)));
            self.trace("deadline_armed");

            self.trace("user_compile");
            let persistent = cached
                .ctx
                .with(|ctx| -> Result<Persistent<Function<'static>>> {
                    let input = build_input(&ctx, &job)?;
                    ctx.globals().set("chirpstack_input", input)?;
                    let func = compile_entry(&ctx, module_name, &script)?;
                    Ok(Persistent::save(&ctx, func))
                })?;

            cached.entry = Some(persistent);
            cached.script = script.clone();
            cached.last_used = self.clock;
            self.cache.insert(key, cached);
        }

        // (3) Refresh the deadline OUTSIDE ctx.with (Context::with holds the
        //     runtime mutex; setting the handler inside would deadlock).
        self.trace("deadline_refreshed");

        let (out, evict_entry) = {
            let cached = self.cache.get_mut(&key).expect("cache entry just created");
            cached.last_used = self.clock;
            cached.calls += 1;

            let deadline = SystemTime::now() + settings.max_execution_time;
            cached
                .rt
                .set_interrupt_handler(Some(Box::new(move || SystemTime::now() > deadline)));
            let entry = cached.entry.as_ref().expect("compiled entry point").clone();

            let out = cached.ctx.with(|ctx| -> Result<JobOutput> {
                let func = entry
                    .restore(&ctx)
                    .map_err(|e| anyhow!("Restore compiled codec: {}", e))?;
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

            // GC is now per script: one Runtime, one Context.
            if settings.gc_due(cached.calls) {
                cached.rt.run_gc();
            }
            cached.rt.set_interrupt_handler(None);

            let threshold = settings.recycle_memory_bytes();
            let over_memory =
                threshold > 0 && cached.rt.memory_usage().memory_used_size as usize >= threshold;
            (out, over_memory)
        };

        // On error or memory pressure, drop ONLY this script's realm. Every
        // other tenant's realm on this thread stays alive and untouched.
        if out.is_err() || evict_entry {
            self.remove_entry(&key, "error");
        }
        self.enforce_budget(settings.max_cache_memory_bytes);
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

/// Run a job on an explicit worker slot. Keeping this out of the OS thread
/// makes the isolation/recycling/panic behaviour directly unit-testable.
fn run_slot(slot: &mut Option<Worker>, job: Job, settings: &JsSettings) -> Result<JobOutput> {
    if slot.is_none() {
        *slot = Some(Worker::new());
    }

    // A panic can leave QuickJS C state inconsistent. catch_unwind stops it
    // from reaching the process, but we must NOT run the Drop of a possibly
    // corrupt Runtime/Context: mem::forget leaks it on purpose instead.
    let result = catch_unwind(AssertUnwindSafe(|| {
        let worker = slot.as_mut().unwrap();
        worker.run(job, settings)
    }));

    match result {
        Ok(out) => {
            if slot.as_ref().unwrap().should_recycle(settings) {
                // Normal path only: runtimes are known-good, Drop is safe.
                drop(slot.take());
            }
            out
        }
        Err(panic) => {
            if let Some(worker) = slot.take() {
                std::mem::forget(worker);
            }
            let msg = panic_message(&panic);
            CODEC_PANIC_COUNTER.inc();
            error!(
                panic = %msg,
                "JS codec engine panicked; leaked its runtimes and discarded the worker"
            );
            Err(anyhow!(
                "JS codec engine aborted (runtimes leaked, worker discarded): {}",
                msg
            ))
        }
    }
}

fn run_on_slot(slot: &mut Option<Worker>, job: Job, settings: &JsSettings) -> Result<JobOutput> {
    if !settings.cache_scripts {
        // Upstream behaviour: a brand new realm for every single call.
        let mut fresh: Option<Worker> = None;
        return run_slot(&mut fresh, job, settings);
    }
    run_slot(slot, job, settings)
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
        panic_probe: bool,
    },
    Encode {
        identity: CodecIdentity,
        f_port: u8,
        variables: HashMap<String, String>,
        script: String,
        data: prost_types::Struct,
        panic_probe: bool,
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

    fn panic_probe(&self) -> bool {
        match self {
            Job::Decode { panic_probe, .. } | Job::Encode { panic_probe, .. } => *panic_probe,
        }
    }
}

#[derive(Debug)]
enum JobOutput {
    Decode(pbjson_types::Struct),
    Encode((u8, Vec<u8>)),
}

/// Enqueue with a BOUNDED wait. A burst is absorbed by the queue; rejection
/// only happens at real saturation (the per-thread channel stayed full for
/// `queue_wait_timeout`), and is a clean, observable error.
async fn submit(
    pool: &CodecPool,
    job: Job,
    settings: JsSettings,
) -> Result<tokio::sync::oneshot::Receiver<Result<JobOutput>>> {
    let key = CacheKey::new(job.identity(), job.script());
    let index = route_worker(&key, pool.workers);
    let (reply, rx) = tokio::sync::oneshot::channel();
    let message = JobMessage {
        job,
        settings,
        reply,
    };
    match timeout(pool.queue_wait_timeout, pool.senders[index].send(message)).await {
        Ok(Ok(())) => Ok(rx),
        Ok(Err(_)) => Err(anyhow!("JS codec worker pool is unavailable")),
        Err(_) => {
            CODEC_QUEUE_FULL_COUNTER.inc();
            Err(anyhow!(
                "JS codec queue saturated: waited {:?} for worker {index}/{} \
                 (per-thread capacity {}); request rejected",
                pool.queue_wait_timeout,
                pool.workers,
                pool.queue_capacity
            ))
        }
    }
}

async fn execute_on(pool: &CodecPool, job: Job, settings: JsSettings) -> Result<JobOutput> {
    let rx = submit(pool, job, settings).await?;
    rx.await
        .map_err(|e| anyhow!("JS codec worker dropped the reply: {}", e))?
}

async fn execute_with_settings(job: Job, settings: JsSettings) -> Result<JobOutput> {
    execute_on(&CODEC_POOL, job, settings).await
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
        panic_probe: false,
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
        panic_probe: false,
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
fn test_pool(
    workers: usize,
    capacity: usize,
    wait: Duration,
    spawn_threads: bool,
) -> (CodecPool, Vec<Receiver<JobMessage>>) {
    let (pool, receivers) = CodecPool::channels(workers, capacity, wait);
    if spawn_threads {
        pool.spawn_threads(receivers);
        (pool, Vec::new())
    } else {
        // Caller keeps the receivers alive: channels stay open but nobody
        // consumes them, which is a deterministic saturated queue.
        (pool, receivers)
    }
}

#[cfg(test)]
async fn decode_on(
    pool: &CodecPool,
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
        panic_probe: false,
    };
    match execute_on(pool, job, settings).await? {
        JobOutput::Decode(v) => Ok(v),
        JobOutput::Encode(_) => Err(anyhow!("Unexpected JS codec output for decode")),
    }
}

#[cfg(test)]
async fn decode_with_settings(
    identity: CodecIdentity,
    f_port: u8,
    script: &str,
    bytes: &[u8],
    settings: JsSettings,
) -> anyhow::Result<pbjson_types::Struct> {
    decode_on(&CODEC_POOL, identity, f_port, script, bytes, settings).await
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

    fn profile() -> &'static str {
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    }

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

        // Warm up (also creates the realm + vendored modules).
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
            "[{}] {label}: decode x{iterations}: {elapsed:?} ({:?}/call)",
            profile(),
            elapsed / iterations
        );
    }

    // Run with (add --release for production numbers):
    //   cargo test --offline -p chirpstack codec::js::bench -- --ignored --nocapture --test-threads=1
    #[tokio::test]
    #[ignore = "manual benchmark"]
    async fn bench_decode() {
        println!(
            "[{}] panic strategy: unwind={} abort={}",
            profile(),
            cfg!(panic = "unwind"),
            cfg!(panic = "abort")
        );
        bench_once("pooled, gc=default", pooled_settings()).await;

        let mut every_call = pooled_settings();
        every_call.gc_every_calls = 1;
        bench_once("pooled, gc=1", every_call).await;

        let mut no_gc = pooled_settings();
        no_gc.gc_every_calls = 0;
        bench_once("pooled, gc=0", no_gc).await;

        let mut legacy = pooled_settings();
        legacy.cache_scripts = false;
        bench_once("legacy (fresh realm)", legacy).await;
    }

    /// Cost of one cache miss: a new Runtime + Context + vendored modules +
    /// user script, once per (tenant, profile, script) key.
    #[tokio::test]
    #[ignore = "manual benchmark"]
    async fn bench_cache_miss() {
        let script = "function decodeUplink(i) { return { data: { ok: i.fPort } }; }";
        let settings = pooled_settings();
        let iterations = 20;
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
            "[{}] cache miss x{iterations}: {elapsed:?} ({:?}/call)",
            profile(),
            elapsed / iterations
        );
    }
}

/// Security / isolation / resource-limit regression tests. A local
/// `Option<Worker>` slot is used wherever determinism is needed (the real pool
/// may dispatch jobs to any of its dedicated threads).
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
            panic_probe: false,
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

    fn key_of(id: CodecIdentity, script: &str) -> CacheKey {
        CacheKey::new(id, &make_script(script, "decodeUplink"))
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
        // Only the offending realm is dropped; the thread worker stays usable.
        assert!(slot.is_some());
        assert!(slot.as_ref().unwrap().cache.is_empty());
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
        assert!(slot.is_some());
        assert!(slot.as_ref().unwrap().cache.is_empty());
        let out = run_decode(&mut slot, identity(1, 1), 4, &[2], decoder, &settings).unwrap();
        assert_eq!(number(&out, "ok"), 4.0);
    }

    #[tokio::test]
    async fn concurrent_decodes_are_isolated() {
        // Keep concurrency <= queue_capacity so no job is legitimately rejected.
        let decoder = "function decodeUplink(i) { return { data: { port: i.fPort } }; }";
        let settings = pooled_settings();
        let jobs = CODEC_POOL.queue_capacity.min(8);
        let mut handles = Vec::new();

        for port in 0..jobs as u8 {
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
        let mut worker = Worker::new();

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

        for _ in 0..3 {
            let mut worker = Worker::new();
            let out = as_decode(worker.run(job.clone(), &settings).unwrap());
            assert_eq!(number(&out, "counter"), 1.0);
        }
    }

    #[test]
    fn cache_is_bounded() {
        let mut settings = pooled_settings();
        settings.max_cached_scripts = 2;
        let mut worker = Worker::new();

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

    #[test]
    fn lru_evicts_the_least_recently_used_entry() {
        let mut settings = pooled_settings();
        settings.max_cached_scripts = 2;
        let script = "function decodeUplink(i) { return { data: { ok: 1 } }; }";
        let a = identity(1, 1);
        let b = identity(1, 2);
        let c = identity(1, 3);
        let mut slot = None;

        run_decode(&mut slot, a, 1, &[1], script, &settings).unwrap();
        run_decode(&mut slot, b, 1, &[1], script, &settings).unwrap();
        run_decode(&mut slot, a, 1, &[1], script, &settings).unwrap(); // touch a
        run_decode(&mut slot, c, 1, &[1], script, &settings).unwrap(); // evict b

        let cache = &slot.as_ref().unwrap().cache;
        assert_eq!(cache.len(), 2);
        assert!(cache.contains_key(&key_of(a, script)));
        assert!(cache.contains_key(&key_of(c, script)));
        assert!(
            !cache.contains_key(&key_of(b, script)),
            "b was the LRU entry"
        );
    }

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

        run_decode(&mut slot, identity(1, 1), 1, &[1], polluter, &settings).unwrap();
        for script in [polluter, other_script] {
            let out = run_decode(&mut slot, identity(1, 2), 1, &[2], script, &settings).unwrap();
            assert_eq!(number(&out, "leaked_x"), 0.0, "Object.prototype leaked");
            assert_eq!(number(&out, "json_ok"), 1.0, "JSON.parse was replaced");
        }
        let out = run_decode(&mut slot, identity(2, 3), 1, &[2], other_script, &settings).unwrap();
        assert_eq!(number(&out, "leaked_x"), 0.0);
        assert_eq!(number(&out, "json_ok"), 1.0);
    }

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

    /// B: one Runtime AND one Context per cache entry.
    #[test]
    fn each_cache_entry_has_its_own_runtime_and_context() {
        let script = "function decodeUplink(i) { return { data: { ok: 1 } }; }";
        let mut slot = None;
        let settings = pooled_settings();

        run_decode(&mut slot, identity(1, 1), 1, &[1], script, &settings).unwrap();
        run_decode(&mut slot, identity(1, 2), 1, &[1], script, &settings).unwrap();

        let worker = slot.as_ref().unwrap();
        assert_eq!(worker.cache.len(), 2);
        let ctxs: Vec<usize> = worker
            .cache
            .values()
            .map(|e| e.ctx.as_raw().as_ptr() as usize)
            .collect();
        let rts: Vec<usize> = worker
            .cache
            .values()
            .map(|e| e.ctx.get_runtime_ptr() as usize)
            .collect();
        assert_ne!(ctxs[0], ctxs[1], "contexts must not be shared");
        assert_ne!(rts[0], rts[1], "runtimes must not be shared");
    }

    #[test]
    fn resource_limits_are_applied_per_script() {
        let mut settings = pooled_settings();
        settings.max_memory_bytes = 4 * 1024 * 1024;
        let cached = CachedScript::new(&settings).unwrap();
        assert_eq!(
            cached.rt.memory_usage().malloc_limit as usize,
            settings.max_memory_bytes
        );
        // The vendored modules were preloaded into this realm without any
        // interrupt deadline being armed.
        assert!(cached.entry.is_none());
    }

    /// B: a script saturating its OWN runtime cannot affect another script on
    /// the same thread -- its realm stays cached and keeps working.
    #[test]
    fn memory_exhaustion_in_one_script_does_not_affect_another_on_the_same_thread() {
        let mut settings = pooled_settings();
        settings.max_execution_time = Duration::from_secs(2);
        settings.gc_every_calls = 0;
        let hog = r#"
            function decodeUplink(input) {
                var sink = [];
                while (true) { sink.push(new Array(20000).fill(7)); }
            }
        "#;
        let healthy = "function decodeUplink(i) { return { data: { ok: 1 } }; }";
        let a = identity(1, 1);
        let b = identity(1, 2);
        let mut slot = None;

        run_decode(&mut slot, b, 1, &[1], healthy, &settings).unwrap();
        assert_eq!(slot.as_ref().unwrap().cache.len(), 1);

        let err = run_decode(&mut slot, a, 1, &[1], hog, &settings)
            .expect_err("allocation loop must fail");
        println!("per-script OOM: {err:#}");
        assert!(err.to_string().contains("memory"));

        // B's realm is still cached on the SAME worker, untouched.
        assert_eq!(slot.as_ref().unwrap().cache.len(), 1);
        assert!(
            slot.as_ref()
                .unwrap()
                .cache
                .contains_key(&key_of(b, healthy))
        );
        let out = run_decode(&mut slot, b, 1, &[1], healthy, &settings).unwrap();
        assert_eq!(number(&out, "ok"), 1.0);
    }

    #[test]
    fn memory_limit_returns_a_clean_error() {
        let mut settings = pooled_settings();
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
        assert!(err.to_string().contains("memory"));
        assert!(slot.as_ref().unwrap().cache.is_empty());
        let out = run_decode(&mut slot, identity(1, 2), 1, &[1], healthy, &settings).unwrap();
        assert_eq!(number(&out, "ok"), 1.0);
    }

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
        println!("stack overflow: {err:#}");
        assert!(slot.is_some());
        let out = run_decode(&mut slot, identity(1, 2), 1, &[1], healthy, &settings).unwrap();
        assert_eq!(number(&out, "ok"), 1.0);
    }

    #[test]
    fn worker_is_recycled_after_max_calls_per_worker() {
        let mut settings = pooled_settings();
        settings.max_calls_per_worker = 3;
        settings.max_memory_bytes = 0;
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

    /// Total memory is bounded by workers * max_cached_scripts *
    /// max_memory_bytes. Verify that a full LRU fits inside that budget.
    #[test]
    fn a_full_cache_stays_within_the_memory_bound() {
        let mut settings = pooled_settings();
        settings.gc_every_calls = 0;
        let script = "function decodeUplink(i) { return { data: { ok: 1 } }; }";
        let mut worker = Worker::new();

        for i in 0..settings.max_cached_scripts as u128 {
            let job = decode_job(identity(1, i), 1, &[1], script);
            let out = as_decode(worker.run(job, &settings).unwrap());
            assert_eq!(number(&out, "ok"), 1.0);
        }

        let per_script: Vec<usize> = worker
            .cache
            .values()
            .map(|e| e.rt.memory_usage().memory_used_size as usize)
            .collect();
        let total = worker.cache_bytes();
        println!(
            "one thread: {} realms, {} bytes used, budget {} bytes \
             (bound = workers * max_cache_memory_bytes)",
            worker.cache.len(),
            total,
            settings.max_cache_memory_bytes
        );
        assert_eq!(worker.cache.len(), settings.max_cached_scripts);
        assert!(per_script.iter().all(|v| *v < settings.max_memory_bytes));
        assert!(total <= settings.max_cache_memory_bytes);
    }

    /// 2. The real per-thread budget evicts LRU realms even when the count is
    /// still far below max_cached_scripts.
    #[test]
    fn budget_eviction_keeps_the_sum_under_the_per_thread_budget() {
        let mut settings = pooled_settings();
        settings.gc_every_calls = 0;
        settings.max_cached_scripts = 64;
        settings.max_cache_memory_bytes = 1024 * 1024; // 1 MiB
        let script = "function decodeUplink(i) { return { data: { ok: 1 } }; }";
        let mut worker = Worker::new();
        let before = CODEC_CACHE_EVICTIONS
            .get_or_create(&reason_labels("budget"))
            .get();

        for i in 0..20u128 {
            worker
                .run(decode_job(identity(1, i), 1, &[1], script), &settings)
                .unwrap();
        }

        println!(
            "budget eviction: {} realms left, {} bytes used (budget {})",
            worker.cache.len(),
            worker.cache_bytes(),
            settings.max_cache_memory_bytes
        );
        assert!(worker.cache_bytes() <= settings.max_cache_memory_bytes);
        assert!(worker.cache.len() < 20);
        assert!(
            CODEC_CACHE_EVICTIONS
                .get_or_create(&reason_labels("budget"))
                .get()
                > before
        );
    }

    /// 2. Every removal is observable: hits/misses and eviction reasons.
    #[test]
    fn cache_metrics_expose_hits_misses_and_eviction_reasons() {
        let hits = CODEC_CACHE_HITS.get();
        let misses = CODEC_CACHE_MISSES.get();
        let lru = CODEC_CACHE_EVICTIONS
            .get_or_create(&reason_labels("lru"))
            .get();
        let mut settings = pooled_settings();
        settings.max_cached_scripts = 1;
        settings.max_cache_memory_bytes = 0;
        let script = "function decodeUplink(i) { return { data: { ok: 1 } }; }";
        let mut worker = Worker::new();

        worker
            .run(decode_job(identity(1, 1), 1, &[1], script), &settings)
            .unwrap(); // miss
        worker
            .run(decode_job(identity(1, 1), 2, &[1], script), &settings)
            .unwrap(); // hit
        worker
            .run(decode_job(identity(1, 2), 1, &[1], script), &settings)
            .unwrap(); // miss + LRU eviction

        assert_eq!(CODEC_CACHE_HITS.get() - hits, 1);
        assert_eq!(CODEC_CACHE_MISSES.get() - misses, 2);
        assert!(
            CODEC_CACHE_EVICTIONS
                .get_or_create(&reason_labels("lru"))
                .get()
                > lru
        );
    }

    /// 2. hash(key) % N is deterministic: one script is cached once.
    #[tokio::test]
    async fn routing_keeps_one_key_on_one_worker() {
        let (pool, _receivers) = test_pool(2, 256, Duration::from_secs(1), true);
        let settings = pooled_settings();
        let script = "function decodeUplink(i) { return { data: { ok: 1 } }; }";
        let wrapped = make_script(script, "decodeUplink");
        let before: Vec<u64> = pool
            .jobs_per_worker
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect();

        for port in 0..20u8 {
            decode_on(&pool, identity(1, 1), port, script, &[1], settings.clone())
                .await
                .unwrap();
        }

        let after: Vec<u64> = pool
            .jobs_per_worker
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect();
        let delta: Vec<u64> = after.iter().zip(&before).map(|(a, b)| a - b).collect();
        assert_eq!(delta.iter().sum::<u64>(), 20);
        assert!(
            delta.iter().any(|v| *v == 20),
            "all same-key jobs must land on ONE worker: {delta:?}"
        );

        // And a realistic mix of keys must reach both workers.
        let mut seen = [false; 2];
        for i in 0..100u128 {
            let key = CacheKey::new(identity(1, i), &wrapped);
            seen[route_worker(&key, 2)] = true;
        }
        assert_eq!(seen, [true, true], "hash routing must use both threads");
    }

    /// 3. After mem::forget, the OS thread is still alive and picks up a fresh
    /// Worker (no lost thread => no lost capacity).
    #[tokio::test]
    async fn a_panic_does_not_kill_the_worker_thread() {
        let (pool, _receivers) = test_pool(1, 256, Duration::from_secs(1), true);
        let settings = pooled_settings();
        let script = r#"
            var counter = 0;
            function decodeUplink(i) {
                counter++;
                return { data: { counter: counter } };
            }
        "#;
        let jobs_before = pool.jobs_per_worker[0].load(Ordering::Relaxed);

        let out = decode_on(&pool, identity(1, 1), 1, script, &[1], settings.clone())
            .await
            .unwrap();
        assert_eq!(number(&out, "counter"), 1.0);

        let panics_before = CODEC_PANIC_COUNTER.get();
        let mut job = decode_job(identity(1, 1), 1, &[1], script);
        if let Job::Decode { panic_probe, .. } = &mut job {
            *panic_probe = true;
        }
        let err = execute_on(&pool, job, settings.clone())
            .await
            .expect_err("panic must produce a clean error");
        assert!(err.to_string().contains("engine aborted"), "{err:#}");
        assert_eq!(CODEC_PANIC_COUNTER.get(), panics_before + 1);

        // Fresh Worker: module-level state starts from zero again, and the
        // SAME thread keeps serving (its job counter keeps increasing).
        for expected in [1.0, 2.0, 3.0] {
            let out = decode_on(&pool, identity(1, 1), 1, script, &[1], settings.clone())
                .await
                .unwrap();
            assert_eq!(number(&out, "counter"), expected);
        }
        assert!(
            pool.jobs_per_worker[0].load(Ordering::Relaxed) >= jobs_before + 5,
            "the worker thread must not have died"
        );
    }

    #[test]
    fn deadline_order_is_vendor_then_deadline_then_user_script() {
        let settings = pooled_settings();
        let mut slot = None;
        run_decode(
            &mut slot,
            identity(9, 9),
            1,
            &[1],
            "function decodeUplink(i) { return { data: { ok: 1 } }; }",
            &settings,
        )
        .unwrap();

        let steps = &slot.as_ref().unwrap().trace;
        let pos = |s: &str| steps.iter().position(|x| *x == s).expect(s);
        assert!(pos("vendor_loaded") < pos("deadline_armed"));
        assert!(pos("deadline_armed") < pos("user_compile"));
    }

    /// The deadline covers user-controlled top-level code, not just the
    /// decodeUplink body.
    #[test]
    fn top_level_infinite_loop_is_interrupted() {
        let mut settings = pooled_settings();
        settings.max_execution_time = Duration::from_millis(200);
        let script = r#"
            while (true) {}
            function decodeUplink(i) { return { data: { ok: 1 } }; }
        "#;
        let mut slot = None;
        let err = run_decode(&mut slot, identity(1, 1), 1, &[1], script, &settings)
            .expect_err("top-level infinite loop must be interrupted");
        println!("top-level timeout: {err:#}");
        assert!(slot.as_ref().unwrap().cache.is_empty());
    }

    #[test]
    fn oversized_script_is_rejected_before_compilation() {
        let mut settings = pooled_settings();
        settings.max_script_bytes = 256;
        let big = "x".repeat(4096);
        let mut slot = None;
        let err = run_decode(&mut slot, identity(1, 1), 1, &[1], &big, &settings)
            .expect_err("oversized script must be rejected");
        assert!(err.to_string().contains("too large"), "{err:#}");
        assert!(slot.as_ref().unwrap().cache.is_empty());
    }

    /// C1: catch_unwind only works with panic=unwind. The workspace release
    /// profile does NOT set panic="abort", so this holds in --release too.
    #[test]
    fn catch_unwind_is_effective_in_this_build() {
        assert!(
            cfg!(panic = "unwind"),
            "panic=abort makes catch_unwind inoperative; see docs/OPTIMIZATIONS.md"
        );
        assert!(catch_unwind(|| panic!("probe")).is_err());
    }

    /// C1: after a caught panic we must NOT drop a possibly corrupt Runtime.
    #[test]
    fn caught_panic_leaks_the_runtimes_and_discards_the_worker() {
        let settings = pooled_settings();
        let script = "function decodeUplink(i) { return { data: { ok: 1 } }; }";
        let mut slot = None;

        run_decode(&mut slot, identity(1, 1), 1, &[1], script, &settings).unwrap();
        assert_eq!(slot.as_ref().unwrap().cache.len(), 1);

        let before = CODEC_PANIC_COUNTER.get();
        let mut job = decode_job(identity(1, 2), 1, &[1], script);
        if let Job::Decode { panic_probe, .. } = &mut job {
            *panic_probe = true;
        }
        let err = match run_on_slot(&mut slot, job, &settings) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(err.to_string().contains("engine aborted"), "{err:#}");
        assert!(slot.is_none(), "the whole worker must be discarded");
        assert_eq!(CODEC_PANIC_COUNTER.get(), before + 1);

        let out = run_decode(&mut slot, identity(1, 3), 1, &[1], script, &settings).unwrap();
        assert_eq!(number(&out, "ok"), 1.0);
    }

    /// A: a bounded channel rejects instead of waiting forever.
    /// A burst is absorbed by the bounded queue; only real saturation
    /// (channel still full after queue_wait_timeout) produces a clean error.
    #[tokio::test]
    async fn real_queue_saturation_waits_then_rejects_cleanly() {
        // No worker threads: the receiver is kept alive by `_receivers`, so the
        // channel is open but nobody drains it -> deterministic saturation.
        let (pool, _receivers) = test_pool(1, 1, Duration::from_millis(80), false);
        let job = decode_job(
            identity(1, 1),
            1,
            &[1],
            "function decodeUplink(i) { return { data: { ok: 1 } }; }",
        );
        let settings = pooled_settings();
        let before = CODEC_QUEUE_FULL_COUNTER.get();

        let _first = submit(&pool, job.clone(), settings.clone()).await.unwrap();
        let start = Instant::now();
        let err = submit(&pool, job, settings)
            .await
            .expect_err("saturated queue must be rejected");
        let waited = start.elapsed();

        assert!(err.to_string().contains("queue saturated"), "{err:#}");
        assert!(
            waited >= Duration::from_millis(50),
            "the submit must actually WAIT before rejecting (waited {waited:?})"
        );
        assert_eq!(CODEC_QUEUE_FULL_COUNTER.get(), before + 1);
    }

    /// 1. A legitimate burst of 200 simultaneous uplinks with workers=2 must
    /// not lose a single frame to queue rejection.
    #[tokio::test]
    async fn a_burst_of_200_concurrent_decodes_is_not_rejected() {
        use futures::future::join_all;

        let (pool, _receivers) = test_pool(2, 256, Duration::from_secs(1), true);
        let settings = pooled_settings();
        let script = "function decodeUplink(i) { return { data: { ok: i.fPort } }; }";
        let before = CODEC_QUEUE_FULL_COUNTER.get();

        // Two realms -> both per-thread channels are exercised.
        let jobs: Vec<_> = (0..200u16)
            .map(|i| {
                let id = identity(1, (i % 2) as u128);
                decode_on(&pool, id, (i % 256) as u8, script, &[1], settings.clone())
            })
            .collect();
        let results = join_all(jobs).await;
        for (i, r) in results.into_iter().enumerate() {
            let out = r.unwrap_or_else(|e| panic!("job {i} was rejected: {e:#}"));
            assert_eq!(number(&out, "ok"), ((i % 256) as f64));
        }
        assert_eq!(
            CODEC_QUEUE_FULL_COUNTER.get(),
            before,
            "0 rejection expected"
        );
    }

    /// A: exactly N dedicated threads, not Tokio's 512-thread blocking pool.
    #[test]
    fn dedicated_pool_has_a_bounded_number_of_threads() {
        let conf = config::get();
        let expected = if conf.codec.js.workers == 0 {
            std::thread::available_parallelism()
                .map(|v| v.get())
                .unwrap_or(4)
        } else {
            conf.codec.js.workers
        }
        .max(1);
        assert_eq!(CODEC_POOL.workers, expected);
        assert_eq!(CODEC_POOL.senders.len(), expected, "one channel per thread");
        assert_eq!(CODEC_POOL.jobs_per_worker.len(), expected);
        assert_eq!(
            CODEC_POOL.queue_capacity,
            if conf.codec.js.queue_capacity == 0 {
                256
            } else {
                conf.codec.js.queue_capacity.max(1)
            }
        );
    }

    /// GC is now per script: one Runtime, one Context.
    #[test]
    #[ignore = "manual benchmark"]
    fn bench_gc_with_a_full_cache() {
        let script = "function decodeUplink(i) { return { data: { ok: 1 } }; }";
        let mut settings = pooled_settings();
        settings.gc_every_calls = 0;
        let mut worker = Worker::new();

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
                "[gc benchmark] {} realms, gc={gc}: {:?}/call",
                worker.cache.len(),
                start.elapsed() / 200
            );
        }
    }
    /// 2. Quantify the imbalance when one very active profile pins all jobs to
    /// a single worker (hash routing is deterministic, not load-aware).
    #[tokio::test]
    #[ignore = "manual benchmark"]
    async fn bench_routing_imbalance() {
        use futures::future::join_all;

        let (pool, _receivers) = test_pool(2, 256, Duration::from_secs(1), true);
        let settings = pooled_settings();
        let script = "function decodeUplink(i) { return { data: { ok: i.fPort } }; }";
        let wrapped = make_script(script, "decodeUplink");

        let mut keys: [Option<u128>; 2] = [None, None];
        for i in 0..1000u128 {
            let key = CacheKey::new(identity(1, i), &wrapped);
            let w = route_worker(&key, 2);
            if keys[w].is_none() {
                keys[w] = Some(i);
            }
            if keys.iter().all(Option::is_some) {
                break;
            }
        }
        let [Some(k0), Some(k1)] = keys else {
            panic!("could not find a key for each worker");
        };

        // Warm both realms so the measurement only exercises cache hits.
        decode_on(&pool, identity(1, k0), 1, script, &[1], settings.clone())
            .await
            .unwrap();
        decode_on(&pool, identity(1, k1), 1, script, &[1], settings.clone())
            .await
            .unwrap();

        let n = 200u32;
        let before: Vec<u64> = pool
            .jobs_per_worker
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect();

        let start = Instant::now();
        let hot: Vec<_> = (0..n)
            .map(|i| {
                decode_on(
                    &pool,
                    identity(1, k0),
                    (i % 256) as u8,
                    script,
                    &[1],
                    settings.clone(),
                )
            })
            .collect();
        for r in join_all(hot).await {
            r.unwrap();
        }
        let hot_time = start.elapsed();

        let start = Instant::now();
        let spread: Vec<_> = (0..n)
            .map(|i| {
                let k = if i % 2 == 0 { k0 } else { k1 };
                decode_on(
                    &pool,
                    identity(1, k),
                    (i % 256) as u8,
                    script,
                    &[1],
                    settings.clone(),
                )
            })
            .collect();
        for r in join_all(spread).await {
            r.unwrap();
        }
        let spread_time = start.elapsed();

        let after: Vec<u64> = pool
            .jobs_per_worker
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect();
        let delta: Vec<u64> = after.iter().zip(&before).map(|(a, b)| a - b).collect();
        println!(
            "[routing] hot key  x{n}: {hot_time:?} ({:?}/call)",
            hot_time / n
        );
        println!(
            "[routing] 2 keys   x{n}: {spread_time:?} ({:?}/call)",
            spread_time / n
        );
        println!("[routing] jobs/worker: {delta:?}");
        println!(
            "[routing] hot/spread = {:.2}x",
            hot_time.as_secs_f64() / spread_time.as_secs_f64()
        );
    }
}
