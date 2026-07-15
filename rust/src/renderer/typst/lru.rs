//! Bounded LRU cache used by the renderer's per-frame memoization tables.
//!
//! The renderer memoizes three things that are expensive to recompute per
//! frame: the parsed Typst source (`WorldState::source_cache`), the compiled
//! `PagedDocument` (`Renderer::body_cache`), and the rasterized object sprite
//! (`Renderer::sprite_cache`). For *static / paused* content these keys are
//! stable and re-touched every frame, so caching is a big win. But for
//! *animated* content every frame produces a **distinct** key (different
//! `dx/dy/scale/opacity`, counter value, morph polygon, …), so an unbounded
//! `HashMap` would accumulate one entry per frame and blow up memory — exactly
//! the OOM the streaming pipeline was meant to prevent.
//!
//! A bounded LRU keeps the cache beneficial for static content (its keys stay
//! resident because they are refreshed on every touch) while evicting the
//! per-frame churn of moving objects, capping peak memory at `capacity` entries
//! **independent of the total frame count `N`**. This is the missing half of
//! the OOM fix: the bounded channel bounds in-flight RGBA, and the bounded LRU
//! bounds the renderer's internal memo tables.

use std::borrow::Borrow;
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

/// A least-recently-used cache with a fixed capacity. Insertion beyond
/// `capacity` evicts the least-recently-used entry. Touching an entry via
/// [`get`](LruCache::get) refreshes its recency so frequently-used keys are
/// retained through the eviction window.
pub(crate) struct LruCache<K, V> {
    map: HashMap<K, V>,
    recency: VecDeque<K>,
    cap: usize,
}

impl<K: Hash + Eq + Clone, V> LruCache<K, V> {
    /// Create an empty cache that holds at most `cap` entries (`cap` is
    /// clamped up to 1 so a misconfigured capacity can never disable eviction).
    pub(crate) fn with_capacity(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            recency: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// Look up `k`, refreshing its recency on a hit. Accepts any borrowed form
    /// of the key (e.g. `&str` for a `String` key), mirroring `HashMap::get`.
    /// Returns a borrow of the value (the caller clones it out), or `None` on a
    /// miss.
    pub(crate) fn get<Q: ?Sized>(&mut self, k: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        if !self.map.contains_key(k) {
            return None;
        }
        // Refresh recency: move the existing key to the back (most-recent).
        if let Some(pos) = self.recency.iter().position(|x| (*x).borrow() == k) {
            let key = self.recency[pos].clone();
            self.recency.remove(pos);
            self.recency.push_back(key);
        }
        self.map.get(k)
    }

    /// Insert or replace `k` → `v`, evicting the least-recently-used entry if
    /// the cache is over capacity.
    pub(crate) fn insert(&mut self, k: K, v: V) {
        if self.map.contains_key(&k) {
            if let Some(pos) = self.recency.iter().position(|x| x == &k) {
                self.recency.remove(pos);
            }
            self.recency.push_back(k.clone());
        } else {
            self.recency.push_back(k.clone());
        }
        self.map.insert(k, v);
        while self.map.len() > self.cap {
            if let Some(old) = self.recency.pop_front() {
                self.map.remove(&old);
            } else {
                break;
            }
        }
    }
}
