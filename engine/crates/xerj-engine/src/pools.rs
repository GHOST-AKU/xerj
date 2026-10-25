//! Where the engine's thread pools get their widths.
//!
//! Every pool in this crate used to size itself inline from
//! `available_parallelism()` with its own magic fraction, while
//! `EngineConfig::{flush_workers, merge_workers, search_workers}` — declared,
//! defaulted, documented and validated in `xerj-common` — were read by nothing
//! at all. An operator who set `engine.search_workers = 2` to keep XERJ off
//! their laptop's cores got no effect and no warning: the accepted-and-ignored
//! class tracked in #204, and half of #240.
//!
//! This module is the single seam between configuration and pool construction:
//! [`resolve`] turns an [`EngineConfig`] plus the machine policy in
//! [`xerj_common::resource`] into concrete widths, [`init`] installs them once
//! per process, and the pools in `lib.rs` read them. Defaults reproduce the
//! widths the pools already used — the measured ingest/read tuning is
//! unchanged; what changes is that the knobs now do what they say.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use rayon::{ThreadPool, ThreadPoolBuildError, ThreadPoolBuilder};
use xerj_common::config::EngineConfig;
use xerj_common::resource::{self, Workload};

/// Concrete thread counts for one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolSizing {
    /// `ingest_pool`: bulk parse/analyze/insert. Latency-critical for writers.
    pub ingest: usize,
    /// `background_pool`: flush side-cars and segment finalisation.
    pub background: usize,
    /// `merge_pool`: merge re-encode. Pure maintenance.
    pub merge: usize,
    /// `flush_finalize_gate`: how many shard finalizes may run at once.
    pub flush_finalize: usize,
    /// Rayon's global pool, which is what search segment fan-out runs on.
    pub search: usize,
}

/// What [`resolve`] decided, plus anything the operator asked for that could
/// not be honoured verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub sizing: PoolSizing,
    pub warnings: Vec<String>,
}

/// Turn configuration into pool widths.
///
/// `env_flush_finalize` is `XERJ_FIN_CONC`, kept as the tuning override it has
/// always been; it wins over the config value, and a value that cannot be used
/// is reported rather than dropped on the floor.
pub fn resolve(cfg: &EngineConfig, env_flush_finalize: Option<&str>) -> Resolved {
    let mut warnings = Vec::new();

    let mut flush_finalize = cfg.flush_workers.max(1);
    if let Some(raw) = env_flush_finalize.map(str::trim).filter(|s| !s.is_empty()) {
        match raw.parse::<usize>() {
            Ok(n) if n >= 1 => flush_finalize = n,
            _ => warnings.push(format!(
                "XERJ_FIN_CONC={raw} is not a positive integer; using \
                 engine.flush_workers={flush_finalize} instead"
            )),
        }
    }

    Resolved {
        sizing: PoolSizing {
            ingest: resource::threads_for(Workload::Latency),
            background: resource::threads_for(Workload::Background),
            merge: cfg.merge_workers.max(1),
            flush_finalize,
            search: cfg.search_workers.max(1),
        },
        warnings,
    }
}

static SIZING: std::sync::OnceLock<PoolSizing> = std::sync::OnceLock::new();

/// Install the process-wide pool sizing from config. Idempotent, like
/// [`crate::governor::init`]: the first caller wins, because rayon pools are
/// process-global and already-running threads cannot be resized.
///
/// A second [`Engine`](crate::Engine) built in the same process with *different*
/// widths logs a warning naming the values that were ignored — silently serving
/// one engine's config to another is exactly the bug this module exists to end.
pub fn init(cfg: &EngineConfig) {
    let resolved = resolve(cfg, std::env::var("XERJ_FIN_CONC").ok().as_deref());
    for warning in &resolved.warnings {
        tracing::warn!("{warning}");
    }
    let installed = *SIZING.get_or_init(|| resolved.sizing);
    if installed != resolved.sizing {
        tracing::warn!(
            "engine pool sizing was already fixed for this process ({installed:?}); ignoring \
             {:?} from this engine's config",
            resolved.sizing
        );
    }
    install_search_pool(installed.search);
}

/// The installed sizing, or the policy defaults when no engine has been built
/// yet (engine-only unit tests that touch a pool directly).
pub fn sizing() -> PoolSizing {
    *SIZING.get_or_init(|| resolve(&EngineConfig::default(), None).sizing)
}

/// Search fan-out runs on rayon's *global* pool, so honouring
/// `engine.search_workers` means building that pool explicitly — rayon
/// otherwise defaults it to every core on first use.
///
/// `build_global` can only succeed once per process (rayon-core 1.13,
/// `registry.rs` `set_global_registry`: the `Once` runs even when the
/// builder returns `Err`, and `THE_REGISTRY` stays empty). The next
/// `par_iter` then hits `global_registry`'s `.expect`, or, if nothing
/// called `build_global`, `DefaultSpawn` which is `std::thread::Builder`.
/// Either one aborts the node under `panic = "abort"` (#1122). A failed
/// spawn is probed on a *local* pool first, so the once-only global init
/// is attempted only at a width that just started. If even one thread
/// cannot be started, fan-out runs on the calling thread.
///
/// A probe that succeeds is dropped before `build_global`, so the process
/// does not hold two copies of the pool. The retry is only on failure: a
/// transient `EAGAIN` at boot must not pin search to one thread for the
/// life of the process, and this init cannot be repeated later.
fn install_search_pool(threads: usize) {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| {
        let requested = threads.max(1);
        let width = choose_search_width(requested, local_pool_starts);
        if width == 0 {
            degrade_search_fanout_inline(&format!(
                "could not start the search pool with {requested} threads or with one thread; \
                 search and bulk fan-out run on the calling thread (#1122)"
            ));
            return;
        }
        match ThreadPoolBuilder::new()
            .num_threads(width)
            .thread_name(|i| format!("xerj-search-{i}"))
            .spawn_handler(safe_rayon_spawn)
            .build_global()
        {
            Ok(()) => {
                if width != requested {
                    tracing::error!(
                        "could not start the search pool with {requested} threads; using {width} \
                         (#1122)"
                    );
                }
            }
            Err(err) if global_pool_already_initialized(&err) => {
                // A `par_iter` won the race and the registry exists. Do not
                // call `current_num_threads` on any other error: with an
                // empty registry that call panics.
                let actual = rayon::current_num_threads();
                if actual != requested {
                    tracing::warn!(
                        "engine.search_workers={requested} could not be applied ({err}); search \
                         fan-out runs on the already-initialised global pool of {actual} threads"
                    );
                }
            }
            Err(err) => degrade_search_fanout_inline(&format!(
                "could not install the search pool ({err}); fan-out runs on the calling thread \
                 (#1122)"
            )),
        }
    });
}

/// How many search workers to ask `build_global` for.
///
/// `0` means not even one thread could be started. The caller must not
/// pass that to a spawning `build_global`: the failure consumes rayon's
/// once-only init.
pub(crate) fn choose_search_width(
    requested: usize,
    mut can_start: impl FnMut(usize) -> bool,
) -> usize {
    let requested = requested.max(1);
    if can_start(requested) {
        return requested;
    }
    if requested > 1 && can_start(1) {
        return 1;
    }
    0
}

/// Whether a local pool of `threads` workers starts.
///
/// This does not touch rayon's global registry. Three attempts, because a
/// miss here is permanent for the process. The pool is dropped before the
/// function returns, which joins the workers it started.
fn local_pool_starts(threads: usize) -> bool {
    for attempt in 0..3 {
        if ThreadPoolBuilder::new()
            .num_threads(threads.max(1))
            .spawn_handler(safe_rayon_spawn)
            .build()
            .is_ok()
        {
            return true;
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    false
}

/// Rayon-core 1.13 `ThreadPoolBuildError` display for
/// `ErrorKind::GlobalPoolAlreadyInitialized`. The kind is private;
/// `source()` is `None` for that error and also for
/// `CurrentThreadAlreadyInPool`, so the text is what distinguishes a
/// registry that exists from one that does not.
fn global_pool_already_initialized(err: &rayon::ThreadPoolBuildError) -> bool {
    err.to_string() == "The global thread pool has already been initialized."
}

/// Fan-out that goes through [`search_fanout`] must not touch the global
/// pool. Occupy the once-only init on a fallback thread that keeps
/// pulling work, so a later bare `par_iter` does not fall through to
/// `std::thread::Builder` (that child panics, and `panic = "abort"` kills
/// the node) and does not wait forever for a worker loop that never starts.
///
/// Occupying the global pool on *this* thread and returning would mark it
/// as the only worker and then leave the worker loop (rayon-core 1.13
/// `Registry::new`, the `use_current_thread` arm). A later `par_iter` on
/// any other thread injects into that pool and waits forever
/// (`Registry::in_worker_cold`). Search, aggregation, full-text build, and
/// the `xerj index` scan go through [`search_fanout`] or the full-text
/// helper. A missed `par_iter` is run by a fallback thread that stays in
/// [`rayon::yield_now`], so the wait has a worker. If that thread cannot
/// be started, the registry is left alone: the missed iterator can still
/// abort, and it does not wedge every caller.
fn degrade_search_fanout_inline(message: &str) {
    xerj_common::thread_spawn::set_parallelism_inline(true);
    tracing::error!("{message}");
    let (tx, rx) = std::sync::mpsc::channel();
    // Not `std::thread::Builder`: that panics in the child when the
    // alternate signal stack cannot be mapped, which is the failure this
    // path exists for. 8 MiB matches the in-process shard stack; the
    // fallback thread executes injected jobs on its own stack.
    match xerj_common::thread_spawn::spawn_detached(
        "xerj-search-inline",
        Some(8 * 1024 * 1024),
        move || occupy_global_pool(tx),
    ) {
        Ok(()) => match rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::error!(
                "could not occupy rayon's global pool ({err}); a parallel iterator that does \
                 not go through search fan-out can still abort the process (#1122)"
            ),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => tracing::error!(
                "timed out waiting for the search-pool fallback thread; a parallel iterator \
                 that does not go through search fan-out can still abort or wait (#1122)"
            ),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => tracing::error!(
                "the search-pool fallback thread ended before occupying rayon's global pool; \
                 a parallel iterator that does not go through search fan-out can still abort \
                 the process (#1122)"
            ),
        },
        Err(err) => tracing::error!(
            "could not start a thread to occupy rayon's global pool ({err}); a parallel \
             iterator that does not go through search fan-out can still abort the process (#1122)"
        ),
    }
}

/// Become rayon's one global worker and keep pulling injected jobs.
///
/// `use_current_thread` does not enter the worker loop, so without the
/// `yield_now` poll a `par_iter` from any other thread waits forever.
/// The 10 ms idle sleep is only taken when the pool could not be started
/// at all; a healthy node never calls this.
fn occupy_global_pool(tx: std::sync::mpsc::Sender<Result<(), String>>) {
    match ThreadPoolBuilder::new()
        .num_threads(1)
        .use_current_thread()
        .build_global()
    {
        Ok(()) => {
            let _ = tx.send(Ok(()));
        }
        Err(err) if global_pool_already_initialized(&err) => {
            let _ = tx.send(Ok(()));
            return;
        }
        Err(err) => {
            let _ = tx.send(Err(err.to_string()));
            return;
        }
    }
    loop {
        match rayon::yield_now() {
            Some(rayon::Yield::Executed) => {}
            _ => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
}

/// Run `op`, whose `par_iter` uses the global search pool.
///
/// When that pool could not be started, `op` runs on a one-thread pool
/// made of the calling thread ([`inline_install`]). A `par_iter` inside
/// it does not initialize rayon's global pool, so it cannot take the
/// `std::thread::Builder` path that aborts the process (#1122). On the
/// healthy path this is `op()` itself: same pool, same width.
pub fn search_fanout<R: Send>(op: impl FnOnce() -> R + Send) -> R {
    search_fanout_if(xerj_common::thread_spawn::parallelism_inline(), op)
}

pub(crate) fn search_fanout_if<R: Send>(inline: bool, op: impl FnOnce() -> R + Send) -> R {
    if inline {
        inline_install(op)
    } else {
        op()
    }
}

// ── Building the named pools without aborting (#1122) ────────────────────────
//
// `panic = "abort"` in the release profile turns a rayon `build()` failure
// into a dead node. `build()` fails when `pthread_create` returns `EAGAIN`
// (thread or memory limit) — the same moment the node is already degraded.
// A failed build is not cached, so the next call tries again. A wide pool
// that cannot start is retried at one thread and that success is cached.
// When even one thread cannot be started, `install` runs on the calling
// thread and `spawn` runs the job inline rather than aborting.

static POOL_BUILD_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    /// A one-thread pool made of the calling thread. Kept for the thread's
    /// lifetime so a later `par_iter` on this thread does not fall through to
    /// rayon's global pool, which panics if it cannot start its own threads.
    static INLINE_POOL: RefCell<Option<ThreadPool>> = const { RefCell::new(None) };
}

/// A named engine pool. Callers keep writing `ingest_pool().install(...)`.
/// The pool itself is started on first use, not when this handle is built.
#[derive(Clone, Copy)]
pub(crate) struct PoolCall {
    slot: &'static OnceLock<ThreadPool>,
    label: &'static str,
    threads: fn() -> usize,
    build: fn(usize) -> Result<ThreadPool, ThreadPoolBuildError>,
}

impl PoolCall {
    pub(crate) const fn new(
        slot: &'static OnceLock<ThreadPool>,
        label: &'static str,
        threads: fn() -> usize,
        build: fn(usize) -> Result<ThreadPool, ThreadPoolBuildError>,
    ) -> Self {
        Self {
            slot,
            label,
            threads,
            build,
        }
    }

    pub fn install<R: Send>(&self, op: impl FnOnce() -> R + Send) -> R {
        install_or_inline(self.slot, self.label, (self.threads)(), self.build, op)
    }

    pub fn spawn(&self, op: impl FnOnce() + Send + 'static) {
        spawn_or_inline(self.slot, self.label, (self.threads)(), self.build, op)
    }
}

pub(crate) fn install_or_inline<R: Send>(
    slot: &OnceLock<ThreadPool>,
    label: &str,
    threads: usize,
    build: fn(usize) -> Result<ThreadPool, ThreadPoolBuildError>,
    op: impl FnOnce() -> R + Send,
) -> R {
    match ensure_pool(slot, label, threads, build) {
        Some(pool) => pool.install(op),
        None => inline_install(op),
    }
}

pub(crate) fn spawn_or_inline(
    slot: &OnceLock<ThreadPool>,
    label: &str,
    threads: usize,
    build: fn(usize) -> Result<ThreadPool, ThreadPoolBuildError>,
    op: impl FnOnce() + Send + 'static,
) {
    match ensure_pool(slot, label, threads, build) {
        Some(pool) => pool.spawn(op),
        None => match xerj_common::thread_spawn::try_spawn(label, op) {
            Ok(_) => {}
            Err(failure) => {
                static LOGGED: AtomicBool = AtomicBool::new(false);
                xerj_common::thread_spawn::warn_best_effort_degraded(
                    &LOGGED,
                    label,
                    &failure.error,
                );
                if let Some(job) = failure.job {
                    job();
                }
            }
        },
    }
}

/// The pool in `slot`, built at `threads` or, if that fails, at one thread.
/// `None` when neither could start. A failure is not stored, so the next
/// call tries again.
fn ensure_pool<'a>(
    slot: &'a OnceLock<ThreadPool>,
    label: &str,
    threads: usize,
    build: fn(usize) -> Result<ThreadPool, ThreadPoolBuildError>,
) -> Option<&'a ThreadPool> {
    if let Some(pool) = slot.get() {
        return Some(pool);
    }
    let _guard = POOL_BUILD_LOCK
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    if let Some(pool) = slot.get() {
        return Some(pool);
    }
    let width = threads.max(1);
    let pool = match build(width) {
        Ok(pool) => pool,
        Err(err) if width > 1 => match build(1) {
            Ok(pool) => {
                log_pool_degraded_once(|| {
                    format!(
                        "could not start the {label} pool with {width} threads ({err}); \
                         using one thread (#1122)"
                    )
                });
                pool
            }
            Err(retry) => {
                log_pool_degraded_once(|| {
                    format!(
                        "could not start the {label} pool ({err}; one-thread retry: {retry}); \
                         this call runs on the calling thread (#1122)"
                    )
                });
                return None;
            }
        },
        Err(err) => {
            log_pool_degraded_once(|| {
                format!(
                    "could not start the {label} pool ({err}); \
                     this call runs on the calling thread (#1122)"
                )
            });
            return None;
        }
    };
    Some(slot.get_or_init(|| pool))
}

/// One line per process. A pool that cannot start is retried on the next
/// call; logging every call floods the node that is already out of threads.
fn log_pool_degraded_once(message: impl FnOnce() -> String) {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if LOGGED.swap(true, Ordering::Relaxed) {
        return;
    }
    tracing::error!("{}", message());
}

/// Rayon worker spawn that cannot abort the process (#1122).
///
/// Rayon's default handler is `std::thread::Builder::spawn`. That returns
/// `Ok` before the child maps its alternate signal stack, and the mmap
/// failure panics in the child. `panic = "abort"` then kills the node
/// during pool build, so the `Err` fallback in [`ensure_pool`] never runs.
/// This handler uses the same non-panicking spawn as the other best-effort
/// threads. The watermark gate is not consulted: these pools are built once
/// and a soft RSS watermark must not refuse them the way it refuses a
/// drain-free thread. A real `pthread_create` failure is an `Err`, which
/// fails the build and lets the caller run inline.
pub(crate) fn safe_rayon_spawn(thread: rayon::ThreadBuilder) -> std::io::Result<()> {
    let name = thread.name().unwrap_or("xerj-rayon").to_owned();
    xerj_common::thread_spawn::spawn_detached(name.as_str(), thread.stack_size(), move || {
        thread.run()
    })
}

/// Run `op`, and every `par_iter` inside it, on the calling thread.
///
/// Calling `op()` directly would not do that: a `par_iter` on a thread that
/// is not a rayon worker goes to the global pool, which panics if its threads
/// cannot start. A `use_current_thread()` pool of width 1 spawns nothing.
fn inline_install<R: Send>(op: impl FnOnce() -> R + Send) -> R {
    if rayon::current_thread_index().is_some() {
        return op();
    }
    // Build, run, then keep the pool alive. Dropping it would unregister this
    // thread, and the next `par_iter` would go to the global pool.
    match ThreadPoolBuilder::new()
        .num_threads(1)
        .use_current_thread()
        .build()
    {
        Ok(pool) => {
            let out = pool.install(op);
            INLINE_POOL.with(|slot| *slot.borrow_mut() = Some(pool));
            out
        }
        Err(err) => {
            tracing::error!("could not register the calling thread with rayon ({err})");
            op()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(flush: usize, merge: usize, search: usize) -> EngineConfig {
        EngineConfig {
            flush_workers: flush,
            merge_workers: merge,
            search_workers: search,
            ..EngineConfig::default()
        }
    }

    #[test]
    fn configured_workers_reach_the_pools() {
        // The #240 §4 defect: these three were read by nothing.
        let r = resolve(&cfg(3, 5, 7), None);
        assert_eq!(r.sizing.flush_finalize, 3);
        assert_eq!(r.sizing.merge, 5);
        assert_eq!(r.sizing.search, 7);
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn defaults_reproduce_the_measured_widths() {
        let cores = xerj_common::resource::cores();
        let r = resolve(&EngineConfig::default(), None);
        // Latency-critical pools keep the whole machine ...
        assert_eq!(r.sizing.ingest, cores);
        assert_eq!(r.sizing.background, cores);
        assert_eq!(r.sizing.search, cores);
        // ... maintenance keeps the measured max(2, cores/8).
        assert_eq!(r.sizing.merge, (cores / 8).max(2).min(cores.max(1)));
        assert_eq!(r.sizing.flush_finalize, r.sizing.merge);
    }

    #[test]
    fn the_env_override_wins_and_a_bad_one_is_reported() {
        assert_eq!(resolve(&cfg(3, 2, 4), Some("9")).sizing.flush_finalize, 9);
        assert_eq!(resolve(&cfg(3, 2, 4), Some("")).sizing.flush_finalize, 3);
        let r = resolve(&cfg(3, 2, 4), Some("lots"));
        assert_eq!(r.sizing.flush_finalize, 3);
        assert!(r.warnings[0].contains("XERJ_FIN_CONC=lots"));
        let r = resolve(&cfg(3, 2, 4), Some("0"));
        assert_eq!(r.sizing.flush_finalize, 3);
        assert_eq!(r.warnings.len(), 1);
    }

    /// `pthread_create` returning `EAGAIN`. Injected through rayon's spawn
    /// hook, so `build()` takes its real failure path.
    fn failing_build(n: usize) -> Result<ThreadPool, ThreadPoolBuildError> {
        ThreadPoolBuilder::new()
            .num_threads(n)
            .spawn_handler(|_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "injected EAGAIN",
                ))
            })
            .build()
    }

    fn wide_fails_one_succeeds(n: usize) -> Result<ThreadPool, ThreadPoolBuildError> {
        if n > 1 {
            failing_build(n)
        } else {
            ThreadPoolBuilder::new().num_threads(1).build()
        }
    }

    /// #1122: `.build().expect(..)` aborted the process. A pool that cannot
    /// start must still produce the job's result, on the calling thread, and
    /// must not cache the failure (the next call tries again).
    #[test]
    fn a_pool_that_cannot_spawn_runs_install_on_the_caller() {
        let slot = OnceLock::new();
        let caller = std::thread::current().id();
        let seen = install_or_inline(&slot, "xerj-test", 4, failing_build, || {
            std::thread::current().id()
        });
        assert_eq!(seen, caller);
        assert!(slot.get().is_none(), "a failed build must not be cached");

        let sum: u32 = install_or_inline(&slot, "xerj-test", 4, failing_build, || {
            use rayon::prelude::*;
            (0..32u32).into_par_iter().sum()
        });
        assert_eq!(sum, (0..32u32).sum::<u32>());
    }

    /// The wide pool cannot start, one thread can. That one-thread pool is
    /// what later calls use — the work still runs, and the process stays up.
    #[test]
    fn a_wide_pool_that_cannot_spawn_falls_back_to_one_thread() {
        let slot = OnceLock::new();
        let width = install_or_inline(&slot, "xerj-test", 8, wide_fails_one_succeeds, || {
            rayon::current_num_threads()
        });
        assert_eq!(width, 1);
        assert_eq!(slot.get().unwrap().current_num_threads(), 1);
    }

    /// `spawn` is fire-and-forget on the live pools. When the pool cannot
    /// start, the job still runs (on a thread, or inline if that spawn fails)
    /// instead of aborting at pool construction.
    #[test]
    fn a_pool_that_cannot_spawn_still_runs_a_spawned_job() {
        let slot = OnceLock::new();
        let (tx, rx) = std::sync::mpsc::channel();
        spawn_or_inline(&slot, "xerj-test", 4, failing_build, move || {
            tx.send(std::thread::current().id()).unwrap();
        });
        let ran = rx.recv_timeout(std::time::Duration::from_secs(2));
        assert!(ran.is_ok(), "the spawned job must run");
        assert!(slot.get().is_none());
    }

    /// #1122: production pools spawn workers through `safe_rayon_spawn`,
    /// not `std::thread::Builder`. The work has to actually run — a spawn
    /// handler that detaches wrong, or that never calls `ThreadBuilder::run`,
    /// would hang `install` or drop the job.
    #[test]
    fn the_safe_rayon_spawner_runs_pool_work() {
        let pool = ThreadPoolBuilder::new()
            .num_threads(2)
            .thread_name(|i| format!("xerj-t1122-{i}"))
            .spawn_handler(super::safe_rayon_spawn)
            .build()
            .expect("safe spawner starts a pool");
        let sum = pool.install(|| {
            use rayon::prelude::*;
            (0..64u32).into_par_iter().sum::<u32>()
        });
        assert_eq!(sum, (0..64u32).sum::<u32>());
    }

    #[test]
    fn search_width_follows_what_can_actually_start() {
        assert_eq!(super::choose_search_width(8, |_| true), 8);
        assert_eq!(super::choose_search_width(0, |_| true), 1);
        assert_eq!(super::choose_search_width(8, |n| n == 1), 1);
        assert_eq!(super::choose_search_width(8, |_| false), 0);
    }

    /// `pthread_create` returning `EAGAIN`, the way a search-pool probe
    /// observes it. The width is 0, so `build_global` is not called at a
    /// width that would consume rayon's once-only init.
    #[test]
    fn a_spawn_failure_is_not_a_search_width() {
        let width = super::choose_search_width(4, |n| {
            ThreadPoolBuilder::new()
                .num_threads(n)
                .spawn_handler(|_| {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "injected EAGAIN",
                    ))
                })
                .build()
                .is_ok()
        });
        assert_eq!(width, 0);
    }

    /// #1122: with the global pool unusable, fan-out still returns the
    /// parallel result, on the calling thread, and does not build rayon's
    /// global pool with `std::thread::Builder`.
    #[test]
    fn an_inline_search_fanout_runs_par_iter_on_the_caller() {
        let (sum, on_caller, width) = std::thread::spawn(|| {
            let caller = std::thread::current().id();
            super::search_fanout_if(true, || {
                use rayon::prelude::*;
                let sum: u32 = (0..64u32)
                    .into_par_iter()
                    .map(|i| {
                        assert_eq!(std::thread::current().id(), caller);
                        i
                    })
                    .sum();
                (
                    sum,
                    std::thread::current().id() == caller,
                    rayon::current_num_threads(),
                )
            })
        })
        .join()
        .expect("fan-out thread");
        assert_eq!(sum, (0..64u32).sum::<u32>());
        assert!(on_caller);
        assert_eq!(width, 1);
    }

    /// #1122: `build_global().use_current_thread()` on the caller, then
    /// returning, leaves no worker loop. A `par_iter` on any other thread
    /// injects and waits forever (measured: still blocked after 3s). The
    /// fallback thread has to keep pulling that work. This runs in a child
    /// process because it consumes rayon's once-only global init; the parent
    /// kills the child if that wait comes back.
    #[test]
    fn a_degraded_search_pool_does_not_wait_forever() {
        const CHILD: &str = "XERJ_TEST_DEGRADED_SEARCH_POOL_CHILD";
        if std::env::var_os(CHILD).is_some() {
            super::degrade_search_fanout_inline(
                "probe: search pool cannot start; fan-out stays on the caller (#1122)",
            );
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                use rayon::prelude::*;
                let bare: u32 = (0..32u32).into_par_iter().sum();
                let via_fanout: u32 = super::search_fanout(|| (0..32u32).into_par_iter().sum());
                let _ = tx.send((bare, via_fanout));
            });
            let (bare, via_fanout) = rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("par_iter on another thread waited for a worker that never loops");
            assert_eq!(bare, (0..32u32).sum::<u32>());
            assert_eq!(via_fanout, (0..32u32).sum::<u32>());
            return;
        }

        let log_path = std::env::temp_dir().join(format!(
            "xerj-degraded-search-pool-{}.log",
            std::process::id()
        ));
        let log = std::fs::File::create(&log_path).expect("create degrade-probe log");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "pools::tests::a_degraded_search_pool_does_not_wait_forever",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdout(log.try_clone().expect("clone log"))
            .stderr(log)
            .spawn()
            .expect("spawn degrade-probe child");
        let started = std::time::Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() > std::time::Duration::from_secs(20) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let logged = std::fs::read_to_string(&log_path).unwrap_or_default();
                    let _ = std::fs::remove_file(&log_path);
                    panic!(
                        "degraded search pool still blocked after 20s; a par_iter waited \
                         forever\n{logged}"
                    );
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
                Err(err) => panic!("wait for degrade-probe child: {err}"),
            }
        };
        let logged = std::fs::read_to_string(&log_path).unwrap_or_default();
        let _ = std::fs::remove_file(&log_path);
        assert!(
            status.success(),
            "degrade-probe child failed ({status}):\n{logged}"
        );
        assert!(
            logged.contains("a_degraded_search_pool_does_not_wait_forever ... ok"),
            "child did not run the probe:\n{logged}"
        );
    }
}
