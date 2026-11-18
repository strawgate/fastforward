//! Ordered map used by the pipeline state machines.
//!
//! Normal builds use `alloc::collections::BTreeMap`. Kani builds use
//! `ArrayMap`, a small sorted array with the same observable behavior for
//! the methods the pipeline uses.
//!
//! Why: CBMC cannot bound a B-tree's height, so every `BTreeMap` operation
//! unrolls node-walking loops and drop glue to the full unwind bound. Even a
//! one-batch lifecycle proof ran out of memory (>8 GB). A sorted `Vec` fixed
//! single-batch proofs but not the ack path, where nested heap buffers are
//! moved with `memmove`. `ArrayMap` keeps everything on the stack.
//!
//! The Kani proofs therefore verify the state-machine logic against this
//! model, not `BTreeMap` itself. `ArrayMap` has its own tests that check it
//! against `BTreeMap` (`cargo test -p ffwd-types ord_map`). Its capacity is
//! `KANI_MAP_CAPACITY`; going past it panics, which a proof reports as a
//! failure rather than hiding.

#[cfg(not(kani))]
pub use alloc::collections::BTreeMap as OrdMap;

/// Entries per map in Kani builds. Proofs use at most 4 batches per source
/// and 2 sources.
#[cfg(any(kani, test))]
pub const KANI_MAP_CAPACITY: usize = 4;

/// The Kani-build stand-in for `BTreeMap`.
#[cfg(kani)]
pub type OrdMap<K, V> = ArrayMap<K, V, KANI_MAP_CAPACITY>;

/// Sorted fixed-capacity map with the subset of the `BTreeMap` API the
/// pipeline uses.
///
/// `slots[..len]` are all `Some`, with unique keys in ascending order, so
/// iteration order matches `BTreeMap`.
#[cfg(any(kani, test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayMap<K, V, const N: usize> {
    slots: [Option<(K, V)>; N],
    len: usize,
}

#[cfg(any(kani, test))]
impl<K, V, const N: usize> Default for ArrayMap<K, V, N> {
    fn default() -> Self {
        Self::new()
    }
}

// Indices below are always < len <= N.
#[cfg(any(kani, test))]
#[allow(clippy::indexing_slicing)]
impl<K, V, const N: usize> ArrayMap<K, V, N> {
    /// Create an empty map.
    pub const fn new() -> Self {
        Self {
            slots: [const { None }; N],
            len: 0,
        }
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the map has no entries.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Entries in ascending key order.
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter {
            slots: &self.slots[..self.len],
        }
    }

    /// Keys in ascending order.
    pub fn keys(&self) -> Keys<'_, K, V> {
        Keys { inner: self.iter() }
    }

    /// Values in ascending key order.
    pub fn values(&self) -> Values<'_, K, V> {
        Values { inner: self.iter() }
    }

    /// Keep only the entries for which `keep` returns true.
    pub fn retain(&mut self, mut keep: impl FnMut(&K, &mut V) -> bool) {
        let mut write = 0;
        for read in 0..self.len {
            let kept = match self.slots[read].as_mut() {
                Some((k, v)) => keep(k, v),
                None => false,
            };
            if kept {
                if write != read {
                    self.slots[write] = self.slots[read].take();
                }
                write += 1;
            } else {
                self.slots[read] = None;
            }
        }
        self.len = write;
    }

    /// Open a gap at `index` for a new entry.
    fn insert_at(&mut self, index: usize, key: K, value: V) {
        assert!(
            self.len < N,
            "ArrayMap capacity exceeded; raise KANI_MAP_CAPACITY"
        );
        let mut j = self.len;
        while j > index {
            self.slots[j] = self.slots[j - 1].take();
            j -= 1;
        }
        self.slots[index] = Some((key, value));
        self.len += 1;
    }

    /// Remove the entry at `index`, closing the gap.
    fn remove_at(&mut self, index: usize) -> Option<(K, V)> {
        let out = self.slots[index].take();
        for j in index..self.len - 1 {
            self.slots[j] = self.slots[j + 1].take();
        }
        self.len -= 1;
        out
    }
}

#[cfg(any(kani, test))]
#[allow(clippy::indexing_slicing)]
impl<K: Ord, V, const N: usize> ArrayMap<K, V, N> {
    /// Index of `key`, or the index where it would be inserted.
    fn search(&self, key: &K) -> Result<usize, usize> {
        for i in 0..self.len {
            if let Some((k, _)) = &self.slots[i] {
                match k.cmp(key) {
                    core::cmp::Ordering::Less => {}
                    core::cmp::Ordering::Equal => return Ok(i),
                    core::cmp::Ordering::Greater => return Err(i),
                }
            }
        }
        Err(self.len)
    }

    /// Value for `key`.
    pub fn get(&self, key: &K) -> Option<&V> {
        let i = self.search(key).ok()?;
        self.slots[i].as_ref().map(|(_, v)| v)
    }

    /// Mutable value for `key`.
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        let i = self.search(key).ok()?;
        self.slots[i].as_mut().map(|(_, v)| v)
    }

    /// Insert `value` under `key`, returning the previous value.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        match self.search(&key) {
            Ok(i) => self.slots[i]
                .as_mut()
                .map(|(_, v)| core::mem::replace(v, value)),
            Err(i) => {
                self.insert_at(i, key, value);
                None
            }
        }
    }

    /// Remove `key`, returning its value.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let i = self.search(key).ok()?;
        self.remove_at(i).map(|(_, v)| v)
    }

    /// Entry for `key`, for `or_default` / `or_insert_with`.
    pub fn entry(&mut self, key: K) -> Entry<'_, K, V, N> {
        Entry { map: self, key }
    }
}

/// A key's slot in an [`ArrayMap`], as returned by [`ArrayMap::entry`].
#[cfg(any(kani, test))]
pub struct Entry<'a, K, V, const N: usize> {
    map: &'a mut ArrayMap<K, V, N>,
    key: K,
}

#[cfg(any(kani, test))]
impl<'a, K: Ord, V, const N: usize> Entry<'a, K, V, N> {
    /// Value for the key, inserting `make()` first if absent.
    pub fn or_insert_with(self, make: impl FnOnce() -> V) -> &'a mut V {
        let i = match self.map.search(&self.key) {
            Ok(i) => i,
            Err(i) => {
                self.map.insert_at(i, self.key, make());
                i
            }
        };
        match self.map.slots.get_mut(i).and_then(Option::as_mut) {
            Some((_, v)) => v,
            None => unreachable!("slot {i} was just found or filled"),
        }
    }

    /// Value for the key, inserting `V::default()` first if absent.
    pub fn or_default(self) -> &'a mut V
    where
        V: Default,
    {
        self.or_insert_with(V::default)
    }
}

#[cfg(any(kani, test))]
impl<'a, K, V, const N: usize> IntoIterator for &'a ArrayMap<K, V, N> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

// Concrete iterator types: an `impl Iterator` return would hold the borrow
// for the whole `while let` in `record_ack_and_advance`, which
// `BTreeMap::iter` does not. They are plain structs, not `FilterMap` over a
// function pointer, so CBMC has no indirect calls to resolve.

/// Iterator over `(&K, &V)` in ascending key order.
#[cfg(any(kani, test))]
pub struct Iter<'a, K, V> {
    slots: &'a [Option<(K, V)>],
}

#[cfg(any(kani, test))]
impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        let (first, rest) = self.slots.split_first()?;
        self.slots = rest;
        first.as_ref().map(|(k, v)| (k, v))
    }
}

/// Iterator over keys in ascending order.
#[cfg(any(kani, test))]
pub struct Keys<'a, K, V> {
    inner: Iter<'a, K, V>,
}

#[cfg(any(kani, test))]
impl<'a, K, V> Iterator for Keys<'a, K, V> {
    type Item = &'a K;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(k, _)| k)
    }
}

/// Iterator over values in ascending key order.
#[cfg(any(kani, test))]
pub struct Values<'a, K, V> {
    inner: Iter<'a, K, V>,
}

#[cfg(any(kani, test))]
impl<'a, K, V> Iterator for Values<'a, K, V> {
    type Item = &'a V;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(_, v)| v)
    }
}

#[cfg(test)]
mod tests {
    use super::{ArrayMap, KANI_MAP_CAPACITY};
    use alloc::collections::BTreeMap;
    use alloc::vec::Vec;
    use proptest::prelude::*;

    type Model = ArrayMap<u8, u16, KANI_MAP_CAPACITY>;

    #[derive(Debug, Clone)]
    enum Op {
        Insert(u8, u16),
        Remove(u8),
        EntryAdd(u8, u16),
        Retain(u8),
    }

    // Keys 0..CAPACITY so the model never overflows.
    fn op() -> impl Strategy<Value = Op> {
        let keys = 0u8..KANI_MAP_CAPACITY as u8;
        prop_oneof![
            (keys.clone(), any::<u16>()).prop_map(|(k, v)| Op::Insert(k, v)),
            keys.clone().prop_map(Op::Remove),
            (keys, any::<u16>()).prop_map(|(k, v)| Op::EntryAdd(k, v)),
            (1u8..4).prop_map(Op::Retain),
        ]
    }

    proptest! {
        /// Every operation returns the same result as on a BTreeMap, and the
        /// two maps hold the same entries in the same order afterwards.
        #[test]
        fn array_map_matches_btree_map(ops in proptest::collection::vec(op(), 0..64)) {
            let mut model = Model::new();
            let mut real: BTreeMap<u8, u16> = BTreeMap::new();
            for op in ops {
                match op {
                    Op::Insert(k, v) => prop_assert_eq!(model.insert(k, v), real.insert(k, v)),
                    Op::Remove(k) => prop_assert_eq!(model.remove(&k), real.remove(&k)),
                    Op::EntryAdd(k, v) => {
                        *model.entry(k).or_default() ^= v;
                        *real.entry(k).or_default() ^= v;
                    }
                    Op::Retain(m) => {
                        model.retain(|k, _| k % m == 0);
                        real.retain(|k, _| k % m == 0);
                    }
                }
                for k in 0u8..=KANI_MAP_CAPACITY as u8 {
                    prop_assert_eq!(model.get(&k), real.get(&k));
                }
                prop_assert_eq!(model.len(), real.len());
                prop_assert_eq!(model.is_empty(), real.is_empty());
                let a: Vec<_> = model.iter().collect();
                let b: Vec<_> = real.iter().collect();
                prop_assert_eq!(&a, &b);
                let a: Vec<_> = (&model).into_iter().collect();
                prop_assert_eq!(&a, &b);
                prop_assert!(model.keys().eq(real.keys()));
                prop_assert!(model.values().eq(real.values()));
            }
        }
    }

    #[test]
    fn get_mut_updates_in_place() {
        let mut m = Model::new();
        m.insert(3, 1);
        *m.get_mut(&3).expect("present") = 9;
        assert_eq!(m.get(&3), Some(&9));
        assert!(m.get_mut(&2).is_none());
    }

    #[test]
    #[should_panic(expected = "capacity exceeded")]
    fn insert_past_capacity_panics() {
        let mut m = ArrayMap::<u8, u8, 2>::new();
        m.insert(0, 0);
        m.insert(1, 0);
        m.insert(2, 0);
    }
}
