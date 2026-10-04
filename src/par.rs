//! Work shared among scoped threads, for jobs whose result does not depend
//! on how many threads run them.

use std::sync::atomic::{AtomicUsize, Ordering};

/// The number of threads to use for `threads` (0: the machine's available
/// parallelism).
pub(crate) fn threads(threads: usize) -> usize {
    if threads != 0 {
        return threads;
    }
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// `f(0), f(1), ..., f(tasks - 1)`, computed on up to `threads` threads (0:
/// automatic), in order. Falls back to the calling thread alone where
/// threads cannot be spawned.
pub(crate) fn map<T: Send>(tasks: usize, threads: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let workers = self::threads(threads).min(tasks);
    if workers <= 1 {
        return (0..tasks).map(f).collect();
    }
    let next = AtomicUsize::new(0);
    let run = || {
        let mut done = Vec::new();
        loop {
            let t = next.fetch_add(1, Ordering::Relaxed);
            if t >= tasks {
                break;
            }
            done.push((t, f(t)));
        }
        done
    };
    let mut results: Vec<Option<T>> = (0..tasks).map(|_| None).collect();
    std::thread::scope(|s| {
        let handles: Vec<_> =
            (1..workers).filter_map(|_| std::thread::Builder::new().spawn_scoped(s, run).ok()).collect();
        for (t, r) in run() {
            results[t] = Some(r);
        }
        for h in handles {
            match h.join() {
                Ok(done) => {
                    for (t, r) in done {
                        results[t] = Some(r);
                    }
                }
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }
    });
    results.into_iter().map(|r| r.expect("every task ran")).collect()
}
