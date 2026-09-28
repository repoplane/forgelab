//! Bounded parallelism over the declared repositories. Everything runs; nothing is abandoned
//! because a neighbour failed; every outcome comes back in input order.

use std::future::Future;

use futures::StreamExt as _;
use tokio_util::sync::CancellationToken;

use super::report::CommandError;

/// Runs `f` over `items`, at most `limit` at a time, and returns each item with its outcome in
/// the order the items came in. A run that has been cancelled does not start further items.
pub async fn for_each_collect<T, F, Fut>(
    items: Vec<T>,
    limit: usize,
    cancel: &CancellationToken,
    f: F,
) -> Vec<(T, Result<(), CommandError>)>
where
    F: Fn(T) -> Fut,
    Fut: Future<Output = (T, Result<(), CommandError>)>,
{
    let limit = limit.max(1);
    let f = &f;
    type Done<T> = Vec<(usize, (T, Result<(), CommandError>))>;
    let mut done: Done<T> = futures::stream::iter(items.into_iter().enumerate())
        .map(|(i, item)| async move {
            if cancel.is_cancelled() {
                return (i, (item, Err(CommandError::Other("interrupted".into()))));
            }
            (i, f(item).await)
        })
        .buffer_unordered(limit)
        .collect()
        .await;
    done.sort_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, r)| r).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn runs_everything_bounded_and_keeps_order() {
        let inflight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let items: Vec<usize> = (0..20).collect();
        let cancel = CancellationToken::new();
        let out = for_each_collect(items, 3, &cancel, |i| {
            let (inflight, peak) = (inflight.clone(), peak.clone());
            async move {
                let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                inflight.fetch_sub(1, Ordering::SeqCst);
                let r = if i % 7 == 0 {
                    Err(CommandError::Other(format!("{i}")))
                } else {
                    Ok(())
                };
                (i, r)
            }
        })
        .await;
        assert_eq!(
            out.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            (0..20).collect::<Vec<_>>()
        );
        assert_eq!(out.iter().filter(|(_, r)| r.is_err()).count(), 3);
        assert!(peak.load(Ordering::SeqCst) <= 3);
    }
}
