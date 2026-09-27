//! One-time initialisation per key, without a lock across the work.
//!
//! GitLab subgroups and Azure DevOps projects are made the first time a repository needs
//! them, and making one takes seconds to minutes. Eight workers hitting the same new
//! namespace must make it once; workers on other namespaces must not wait for it. A plain
//! mutex around the lookup gave the first and denied the second.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use tokio::sync::OnceCell;

pub struct KeyedOnce<K, V> {
    cells: Mutex<HashMap<K, Arc<OnceCell<V>>>>,
}

impl<K, V> Default for KeyedOnce<K, V> {
    fn default() -> Self {
        KeyedOnce { cells: Mutex::new(HashMap::new()) }
    }
}

impl<K: Eq + Hash + Clone, V: Clone> KeyedOnce<K, V> {
    fn cell(&self, key: &K) -> Arc<OnceCell<V>> {
        let mut cells = self.cells.lock().expect("keyed cells poisoned");
        cells.entry(key.clone()).or_default().clone()
    }

    /// The value for `key`, running `init` once if it is not known yet. Concurrent callers for
    /// the same key wait for that one run; a failure is not remembered, so the next caller
    /// tries again.
    pub async fn get_or_try_init<E, F>(&self, key: &K, init: F) -> Result<V, E>
    where
        F: Future<Output = Result<V, E>>,
    {
        let cell = self.cell(key);
        cell.get_or_try_init(|| init).await.cloned()
    }

    /// The value if it is already known.
    pub fn get(&self, key: &K) -> Option<V> {
        let cells = self.cells.lock().expect("keyed cells poisoned");
        cells.get(key).and_then(|c| c.get().cloned())
    }

    /// Remembers a value without running anything.
    pub fn set(&self, key: &K, value: V) {
        let cell = self.cell(key);
        let _ = cell.set(value);
        if cell.get().is_none() {
            // Already initialised by someone else with another value: replace the cell.
            let mut cells = self.cells.lock().expect("keyed cells poisoned");
            cells.insert(key.clone(), Arc::new(OnceCell::new()));
        }
    }

    /// Forgets `key` and everything below it, so that the next caller looks again.
    pub fn invalidate_where(&self, mut pred: impl FnMut(&K) -> bool) {
        let mut cells = self.cells.lock().expect("keyed cells poisoned");
        cells.retain(|k, _| !pred(k));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn runs_once_per_key_and_forgets_failures() {
        let once: KeyedOnce<String, u32> = KeyedOnce::default();
        let runs = Arc::new(AtomicUsize::new(0));
        let mut tasks = tokio::task::JoinSet::new();
        let once = Arc::new(once);
        for _ in 0..8 {
            let (once, runs) = (once.clone(), runs.clone());
            tasks.spawn(async move {
                once.get_or_try_init::<(), _>(&"a".to_string(), async {
                    runs.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    Ok(7)
                })
                .await
            });
        }
        while let Some(r) = tasks.join_next().await {
            assert_eq!(r.unwrap(), Ok(7));
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        let failed = once.get_or_try_init(&"b".to_string(), async { Err::<u32, &str>("no") }).await;
        assert_eq!(failed, Err("no"));
        let ok = once.get_or_try_init::<&str, _>(&"b".to_string(), async { Ok(1) }).await;
        assert_eq!(ok, Ok(1));

        once.invalidate_where(|k| k.starts_with('a'));
        assert_eq!(once.get(&"a".to_string()), None);
        assert_eq!(once.get(&"b".to_string()), Some(1));
    }
}
