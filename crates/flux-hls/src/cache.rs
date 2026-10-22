//! Byte-budgeted LRU caches: produced segments, prepared assets, WebVTT.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

use bytes::Bytes;

/// Least-recently-used cache with a byte budget. Each entry has a weight
/// (its approximate memory); inserting or re-weighing evicts the least
/// recently used entries until the total fits the budget.
pub(crate) struct LruCache<K, V> {
    map: HashMap<K, (V, usize, u64)>,
    /// Recency tick → key (oldest first).
    order: BTreeMap<u64, K>,
    tick: u64,
    bytes: usize,
    budget: usize,
    evicted: u64,
}

impl<K: Hash + Eq + Clone, V: Clone> LruCache<K, V> {
    pub(crate) fn new(budget: usize) -> Self {
        LruCache {
            map: HashMap::new(),
            order: BTreeMap::new(),
            tick: 0,
            bytes: 0,
            budget,
            evicted: 0,
        }
    }

    fn touch(&mut self, k: &K) {
        if let Some(e) = self.map.get_mut(k) {
            self.order.remove(&e.2);
            self.tick += 1;
            e.2 = self.tick;
            self.order.insert(self.tick, k.clone());
        }
    }

    /// The value, marking it most recently used.
    pub(crate) fn get(&mut self, k: &K) -> Option<V> {
        self.touch(k);
        self.map.get(k).map(|e| e.0.clone())
    }

    pub(crate) fn contains(&self, k: &K) -> bool {
        self.map.contains_key(k)
    }

    /// Inserts (or replaces) an entry of `weight` bytes.
    pub(crate) fn put(&mut self, k: K, v: V, weight: usize) {
        self.remove(&k);
        self.tick += 1;
        self.order.insert(self.tick, k.clone());
        self.map.insert(k, (v, weight, self.tick));
        self.bytes += weight;
        self.evict();
    }

    /// Updates an entry's weight (e.g. once a lazily built value is known).
    pub(crate) fn reweigh(&mut self, k: &K, weight: usize) {
        if let Some(e) = self.map.get_mut(k) {
            self.bytes = self.bytes - e.1 + weight;
            e.1 = weight;
        }
        self.touch(k);
        self.evict();
    }

    pub(crate) fn remove(&mut self, k: &K) -> Option<V> {
        let (v, w, t) = self.map.remove(k)?;
        self.order.remove(&t);
        self.bytes -= w;
        Some(v)
    }

    fn evict(&mut self) {
        while self.bytes > self.budget {
            let Some((&t, _)) = self.order.iter().next() else {
                break;
            };
            let Some(k) = self.order.remove(&t) else { break };
            if let Some((_, w, _)) = self.map.remove(&k) {
                self.bytes -= w;
                self.evicted += 1;
            }
        }
    }

    /// (entries, bytes, evictions so far).
    pub(crate) fn usage(&self) -> (usize, usize, u64) {
        (self.map.len(), self.bytes, self.evicted)
    }

    /// Drops every entry and the tables' memory (not counted as
    /// evictions); returns the bytes the entries weighed.
    pub(crate) fn clear(&mut self) -> usize {
        let freed = self.bytes;
        self.map = HashMap::new();
        self.order = BTreeMap::new();
        self.bytes = 0;
        freed
    }
}

/// Produced segments by key, weighed by their length.
pub(crate) struct ByteCache<K>(LruCache<K, Bytes>);

impl<K: Hash + Eq + Clone> ByteCache<K> {
    pub(crate) fn new(budget: usize) -> Self {
        ByteCache(LruCache::new(budget))
    }

    pub(crate) fn get(&mut self, k: &K) -> Option<Bytes> {
        self.0.get(k)
    }

    pub(crate) fn contains(&self, k: &K) -> bool {
        self.0.contains(k)
    }

    pub(crate) fn put(&mut self, k: K, v: Bytes) {
        if self.0.contains(&k) {
            return;
        }
        let w = v.len();
        self.0.put(k, v, w);
    }

    pub(crate) fn usage(&self) -> (usize, usize, u64) {
        self.0.usage()
    }

    pub(crate) fn clear(&mut self) -> usize {
        self.0.clear()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used_by_bytes() {
        let mut c = LruCache::new(10);
        c.put("a", 1, 4);
        c.put("b", 2, 4);
        assert_eq!(c.get(&"a"), Some(1)); // a is now newer than b
        c.put("c", 3, 4); // 12 > 10: evict b
        assert!(c.contains(&"a") && !c.contains(&"b") && c.contains(&"c"));
        c.reweigh(&"a", 9); // 13 > 10: evict c (older than a)
        assert!(c.contains(&"a") && !c.contains(&"c"));
        assert_eq!(c.usage(), (1, 9, 2));
        c.put("big", 0, 50); // larger than the budget: evicts everything, itself last
        assert_eq!(c.usage().0, 0);
    }

    #[test]
    fn clear_drops_everything_and_keeps_working() {
        let mut c = ByteCache::new(100);
        c.put(1, Bytes::from_static(b"abcd"));
        c.put(2, Bytes::from_static(b"ef"));
        assert_eq!(c.clear(), 6);
        assert_eq!(c.usage(), (0, 0, 0), "a clear is not an eviction");
        assert!(c.get(&1).is_none());
        c.put(3, Bytes::from_static(b"g"));
        assert_eq!(c.usage(), (1, 1, 0));
    }
}
