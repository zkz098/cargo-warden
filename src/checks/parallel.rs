//! Bounded thread-pool helpers for deterministic per-file parallel processing.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Apply `f` to every element of `items`, returning outputs in input order.
///
/// Runs sequentially when `parallel` is `false` or when the slice is too small
/// to amortize thread-spawning overhead.
pub(crate) fn map_items<T, R, F>(items: &[T], parallel: bool, f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    if items.is_empty() {
        return Vec::new();
    }

    let workers = if parallel && items.len() >= 4 {
        std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(items.len())
    } else {
        1
    };

    if workers <= 1 {
        return items.iter().map(f).collect();
    }

    let next_idx = AtomicUsize::new(0);
    let batches = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let mut local = Vec::new();
                    loop {
                        let idx = next_idx.fetch_add(1, Ordering::Relaxed);
                        if idx >= items.len() {
                            break;
                        }
                        local.push((idx, f(&items[idx])));
                    }
                    local
                })
            })
            .collect();

        handles
            .into_iter()
            .map(|h| match h.join() {
                Ok(res) => res,
                Err(payload) => std::panic::resume_unwind(payload),
            })
            .collect::<Vec<_>>()
    });

    let mut ordered: Vec<Option<R>> = (0..items.len()).map(|_| None).collect();
    for batch in batches {
        for (idx, val) in batch {
            ordered[idx] = Some(val);
        }
    }

    ordered
        .into_iter()
        .map(|slot| slot.expect("every parallel task slot must be populated"))
        .collect()
}
