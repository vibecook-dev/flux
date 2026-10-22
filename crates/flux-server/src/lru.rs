//! A small least-recently-used map, bounded by entry count and optionally by
//! weight (bytes). Eviction scans the map (O(n)), which is fine for the few
//! hundred entries the server keeps (plans, prepared assets).

use std::collections::HashMap;
use std::hash::Hash;

pub struct Lru<K, V> {
    cap: usize,
    budget: usize,
    weight: usize,
    tick: u64,
    map: HashMap<K, (u64, usize, V)>,
}

impl<K: Hash + Eq + Clone, V> Lru<K, V> {
    /// At most `cap` entries.
    pub fn new(cap: usize) -> Self {
        Self::with_budget(cap, usize::MAX)
    }

    /// At most `cap` entries whose weights sum to at most `budget` (an
    /// entry heavier than the budget alone is still kept, by itself).
    pub fn with_budget(cap: usize, budget: usize) -> Self {
        Lru {
            cap: cap.max(1),
            budget,
            weight: 0,
            tick: 0,
            map: HashMap::new(),
        }
    }

    pub fn get(&mut self, k: &K) -> Option<&V> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(k).map(|e| {
            e.0 = tick;
            &e.2
        })
    }

    /// Inserts (or replaces) `k` with weight 0.
    pub fn insert(&mut self, k: K, v: V) {
        self.insert_weighted(k, v, 0);
    }

    /// Inserts (or replaces) `k`, evicting least recently used entries
    /// until the count and the weight budget fit.
    pub fn insert_weighted(&mut self, k: K, v: V, w: usize) {
        self.tick += 1;
        self.remove(&k);
        while !self.map.is_empty()
            && (self.map.len() >= self.cap || self.weight.saturating_add(w) > self.budget)
        {
            let Some(oldest) = self.map.iter().min_by_key(|(_, e)| e.0).map(|(k, _)| k.clone()) else {
                break;
            };
            self.remove(&oldest);
        }
        self.weight = self.weight.saturating_add(w);
        self.map.insert(k, (self.tick, w, v));
    }

    pub fn remove(&mut self, k: &K) -> Option<V> {
        self.map.remove(k).map(|e| {
            self.weight -= e.1;
            e.2
        })
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Drops every entry and the table's memory; returns how many there were.
    pub fn clear(&mut self) -> usize {
        let n = self.map.len();
        self.map = HashMap::new();
        self.weight = 0;
        n
    }

    /// Sum of the entries' weights.
    #[cfg(test)]
    pub fn weight(&self) -> usize {
        self.weight
    }
}

#[cfg(test)]
mod tests {
    use super::Lru;

    #[test]
    fn evicts_the_least_recently_used() {
        let mut l = Lru::new(2);
        l.insert(1, "a");
        l.insert(2, "b");
        assert_eq!(l.get(&1), Some(&"a"));
        l.insert(3, "c");
        assert_eq!(l.get(&2), None);
        assert_eq!(l.get(&1), Some(&"a"));
        assert_eq!(l.get(&3), Some(&"c"));
        l.insert(3, "d");
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn evicts_by_weight() {
        let mut l = Lru::with_budget(10, 100);
        l.insert_weighted(1, "a", 40);
        l.insert_weighted(2, "b", 40);
        assert_eq!(l.get(&1), Some(&"a"));
        l.insert_weighted(3, "c", 40);
        assert_eq!(l.get(&2), None, "the least recently used goes first");
        assert_eq!(l.weight(), 80);
        l.insert_weighted(1, "a2", 10);
        assert_eq!(l.weight(), 50);
        l.insert_weighted(4, "huge", 500);
        assert_eq!((l.len(), l.weight()), (1, 500), "an oversize entry stays alone");
        assert_eq!(l.remove(&4), Some("huge"));
        assert_eq!(l.weight(), 0);
        l.insert_weighted(5, "e", 30);
        l.insert_weighted(6, "f", 30);
        assert_eq!(l.clear(), 2);
        assert_eq!((l.len(), l.weight()), (0, 0));
        l.insert_weighted(7, "g", 90);
        assert_eq!(l.get(&7), Some(&"g"), "usable after a clear");
    }
}
