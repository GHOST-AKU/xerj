//! Global-pool fan-out that does not abort the process (#1122).
//!
//! Full-text build and `search_segments` use `par_iter`, which runs on
//! rayon's global pool unless the caller is already a worker. That pool is
//! initialized once. The engine installs it with a non-panicking spawn. When
//! that install cannot start a thread, it sets
//! [`xerj_common::thread_spawn::parallelism_inline`] and a later `par_iter`
//! must not touch the global pool: the failed init left no registry, and
//! `global_registry` panics, which `panic = "abort"` turns into a dead node.
//! A pool that was never initialized is built with `std::thread::Builder`,
//! which panics in the child when the alternate signal stack cannot be mapped.

use std::cell::RefCell;

thread_local! {
    /// A one-thread pool made of the calling thread. Kept so dropping it
    /// does not unregister this thread. A later `par_iter` then stays on
    /// this pool (`current_thread_index` is set for the rest of the thread)
    /// instead of falling through to the global pool.
    static INLINE: RefCell<Option<rayon::ThreadPool>> = const { RefCell::new(None) };
}

/// Run `op`. On the healthy path this is `op()` and `par_iter` uses the
/// global search pool. When that pool could not be started, `op` runs on
/// the calling thread.
pub(crate) fn run<R: Send>(op: impl FnOnce() -> R + Send) -> R {
    run_if(xerj_common::thread_spawn::parallelism_inline(), op)
}

pub(crate) fn run_if<R: Send>(inline: bool, op: impl FnOnce() -> R + Send) -> R {
    if !inline {
        return op();
    }
    // `use_current_thread` leaves this thread registered for its lifetime.
    // A later call just runs `op`; the `par_iter` inside stays on that pool.
    if rayon::current_thread_index().is_some() {
        return op();
    }
    match rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .use_current_thread()
        .build()
    {
        Ok(pool) => {
            let out = pool.install(op);
            INLINE.with(|slot| *slot.borrow_mut() = Some(pool));
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

    #[test]
    fn inline_fanout_runs_par_iter_on_the_caller() {
        let (sum, width) = std::thread::spawn(|| {
            let caller = std::thread::current().id();
            run_if(true, || {
                use rayon::prelude::*;
                let sum: u32 = (0..32u32)
                    .into_par_iter()
                    .map(|i| {
                        assert_eq!(std::thread::current().id(), caller);
                        i
                    })
                    .sum();
                (sum, rayon::current_num_threads())
            })
        })
        .join()
        .expect("fan-out thread");
        assert_eq!(sum, (0..32u32).sum::<u32>());
        assert_eq!(width, 1);
    }
}
