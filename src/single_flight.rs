//! In-process shared execution of identical in-flight reads. Not a cache: an entry lives only
//! while its call runs and is removed before the outcome is published.
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
};
use tokio::sync::OnceCell;

type Cell<V> = Arc<OnceCell<V>>;

pub(crate) struct SingleFlight<V> {
    inflight: Mutex<HashMap<[u8; 32], Cell<V>>>,
}
/// The leader's map entry. Released when the call finishes, is cancelled or panics; only the
/// entry this registration inserted is removed.
struct Registration<'a, V> {
    flight: &'a SingleFlight<V>,
    key: [u8; 32],
    cell: Cell<V>,
    released: bool,
}
impl<V> Registration<'_, V> {
    fn release(&mut self) {
        if std::mem::replace(&mut self.released, true) {
            return;
        }
        let mut inflight = self
            .flight
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if inflight
            .get(&self.key)
            .is_some_and(|cell| Arc::ptr_eq(cell, &self.cell))
        {
            inflight.remove(&self.key);
        }
    }
}
impl<V> Drop for Registration<'_, V> {
    fn drop(&mut self) {
        self.release();
    }
}
impl<V: Clone + Send + Sync> SingleFlight<V> {
    pub(crate) fn new() -> Self {
        Self {
            inflight: Mutex::new(HashMap::new()),
        }
    }
    /// Concurrent callers with `key` share one execution and clone its outcome, errors included.
    /// A caller that joins waits only until its own `deadline` and then gets `timed_out`. If the
    /// running caller is cancelled, one waiting caller runs its own `fetch` instead; callers
    /// arriving after that start a new execution.
    pub(crate) async fn run<F, Fut>(
        &self,
        key: [u8; 32],
        deadline: tokio::time::Instant,
        timed_out: V,
        fetch: F,
    ) -> V
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V>,
    {
        let (cell, registration) = {
            let mut inflight = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
            match inflight.get(&key) {
                Some(cell) => (cell.clone(), None),
                None => {
                    let cell = Cell::default();
                    inflight.insert(key, cell.clone());
                    let registration = Registration {
                        flight: self,
                        key,
                        cell: cell.clone(),
                        released: false,
                    };
                    (cell, Some(registration))
                }
            }
        };
        match registration {
            // No outer timeout: `fetch` enforces the same deadline itself.
            Some(mut registration) => cell
                .get_or_init(|| async move {
                    let value = fetch().await;
                    // Leave the map before publishing, so no later caller receives an outcome
                    // that was already complete when it arrived.
                    registration.release();
                    value
                })
                .await
                .clone(),
            None => tokio::time::timeout_at(deadline, cell.get_or_init(fetch))
                .await
                .map_or(timed_out, V::clone),
        }
    }
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}
