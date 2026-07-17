//! Bounded LRU cache with O(1) get/insert.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;

pub(crate) struct LruCache<K, V> {
    map: HashMap<K, (V, u64)>,
    epoch: u64,
    cap: usize,
}

impl<K: Hash + Eq + Clone, V> LruCache<K, V> {
    pub(crate) fn with_capacity(cap: usize) -> Self {
        Self {
            map: HashMap::with_capacity(cap),
            epoch: 0,
            cap: cap.max(1),
        }
    }

    pub(crate) fn get<Q>(&mut self, k: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.epoch += 1;
        let epoch = self.epoch;
        if let Some((_, g)) = self.map.get_mut(k) {
            *g = epoch;
            // Re-borrow immutably for the return value.
            // The map still holds the entry; we just refreshed its epoch.
            return self.map.get(k).map(|(v, _)| v);
        }
        None
    }

    pub(crate) fn insert(&mut self, k: K, v: V) {
        self.epoch += 1;
        let epoch = self.epoch;
        if let Some(slot) = self.map.get_mut(&k) {
            slot.0 = v;
            slot.1 = epoch;
            return;
        }
        if self.map.len() >= self.cap {
            if let Some(lru_key) = self
                .map
                .iter()
                .min_by_key(|(_, (_, g))| *g)
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&lru_key);
            }
        }
        self.map.insert(k, (v, epoch));
    }
}
