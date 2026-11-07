//! Compact sorted collections for the in-memory report (#339): a sorted `Vec` instead of a B-tree
//! saves the node overhead (each B-tree node holds 11 slots and sorted inserts leave them half
//! full). Iteration order is the key order, exactly that of the `BTreeMap`/`BTreeSet` they replace,
//! and so is the serialised form: a JSON object for the map, an array for the set.
//!
//! Inserts are linear, which is fine for the bounded collections of a report. The bounds are
//! configurable: the endpoints of a direction (`MIMIC_STORE_MAX_ENDPOINTS`, 512 by default, at
//! most 10 000), the literals of a template node (`MIMIC_STORE_MAX_LITERALS`, 32 by default, at
//! most 1000), the authorities, the properties of an object schema and the samples of an
//! endpoint. None grows with the traffic. Bulk builds (`collect`, `extend`, reading) sort once.
//! Deserialisation sorts and deduplicates (the last duplicate wins, as in a
//! `BTreeMap`) and drops the spare capacity.

use std::borrow::Borrow;
use std::fmt;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};

pub use compact_str::CompactString as Name;
use litemap::LiteMap;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A map kept in key order in one `Vec`: the subset of the `BTreeMap` API the report uses.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SortedMap<K, V>(LiteMap<K, V>);

impl<K, V> Default for SortedMap<K, V> {
    fn default() -> Self {
        Self(LiteMap::new_vec())
    }
}

impl<K, V> Deref for SortedMap<K, V> {
    type Target = LiteMap<K, V>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<K, V> DerefMut for SortedMap<K, V> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<K, V> SortedMap<K, V> {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &V> {
        self.0.values()
    }
}

impl<K: Ord, V> SortedMap<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    /// The pairs in key order; the last of equal keys wins.
    fn from_pairs(mut pairs: Vec<(K, V)>) -> Self {
        if !pairs.windows(2).all(|w| w[0].0 < w[1].0) {
            // Stable sort, then keep the last of each run of equal keys.
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            let mut out: Vec<(K, V)> = Vec::with_capacity(pairs.len());
            for pair in pairs {
                match out.last_mut() {
                    Some(last) if last.0 == pair.0 => *last = pair,
                    _ => out.push(pair),
                }
            }
            pairs = out;
        }
        pairs.shrink_to_fit();
        Self(LiteMap::from_sorted_store_unchecked(pairs))
    }

    pub fn into_values(self) -> impl Iterator<Item = V> {
        self.into_iter().map(|(_, v)| v)
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.0.iter_mut().map(|(_, v)| v)
    }

    /// Remove and return the entry with the largest key.
    pub fn pop_last(&mut self) -> Option<(K, V)> {
        let mut pairs = std::mem::take(&mut self.0).into_tuple_vec();
        let last = pairs.pop();
        self.0 = LiteMap::from_sorted_store_unchecked(pairs);
        last
    }
}

impl<K: Ord + Borrow<Q>, Q: Ord + ?Sized, V> std::ops::Index<&Q> for SortedMap<K, V> {
    type Output = V;

    /// Panics on a missing key, like `BTreeMap`.
    #[allow(clippy::expect_used)]
    fn index(&self, key: &Q) -> &V {
        self.0.get(key).expect("key present in the map")
    }
}

impl<K: Ord, V> FromIterator<(K, V)> for SortedMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self::from_pairs(iter.into_iter().collect())
    }
}

impl<K: Ord, V, const N: usize> From<[(K, V); N]> for SortedMap<K, V> {
    fn from(pairs: [(K, V); N]) -> Self {
        Self::from_pairs(Vec::from(pairs))
    }
}

impl<K: Ord, V> Extend<(K, V)> for SortedMap<K, V> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        // Collect, sort and deduplicate once (the last value wins), then rebuild: linear inserts
        // would be quadratic on a large unsorted input.
        let mut pairs = std::mem::take(&mut self.0).into_tuple_vec();
        pairs.extend(iter);
        *self = Self::from_pairs(pairs);
    }
}

impl<K, V> IntoIterator for SortedMap<K, V> {
    type Item = (K, V);
    type IntoIter = std::vec::IntoIter<(K, V)>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_tuple_vec().into_iter()
    }
}

impl<'a, K, V> IntoIterator for &'a SortedMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = <&'a LiteMap<K, V> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        (&self.0).into_iter()
    }
}

// Serialised by hand: `LiteMap`'s own impl writes an empty map, or one with a non-string key,
// as an array, which would change the output.
impl<K: Serialize, V: Serialize> Serialize for SortedMap<K, V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

impl<'de, K: Deserialize<'de> + Ord, V: Deserialize<'de>> Deserialize<'de> for SortedMap<K, V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Pairs<K, V>(PhantomData<fn() -> (K, V)>);

        impl<'de, K: Deserialize<'de>, V: Deserialize<'de>> Visitor<'de> for Pairs<K, V> {
            type Value = Vec<(K, V)>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a map")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut pairs = Vec::with_capacity(access.size_hint().unwrap_or(0).min(4096));
                while let Some(pair) = access.next_entry()? {
                    pairs.push(pair);
                }
                Ok(pairs)
            }
        }

        let pairs = deserializer.deserialize_map(Pairs(PhantomData))?;
        Ok(Self::from_pairs(pairs))
    }
}

/// A set kept in order in one `Vec`: the subset of the `BTreeSet` API the report uses.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SortedSet<T>(Vec<T>);

impl<T> Default for SortedSet<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T> Deref for SortedSet<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<T: Ord> SortedSet<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, value: T) -> bool {
        match self.0.binary_search(&value) {
            Ok(_) => false,
            Err(at) => {
                self.0.insert(at, value);
                true
            }
        }
    }

    pub fn contains<Q>(&self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.binary_search_by(|t| t.borrow().cmp(value)).is_ok()
    }

    pub fn pop_last(&mut self) -> Option<T> {
        self.0.pop()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn retain(&mut self, keep: impl FnMut(&T) -> bool) {
        self.0.retain(keep);
    }

    /// The members of both sets, in order.
    pub fn intersection<'a>(&'a self, other: &'a Self) -> impl Iterator<Item = &'a T> {
        self.0.iter().filter(|t| other.0.binary_search(t).is_ok())
    }

    fn from_vec(mut items: Vec<T>) -> Self {
        items.sort();
        items.dedup();
        items.shrink_to_fit();
        Self(items)
    }
}

impl<T: Ord> FromIterator<T> for SortedSet<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self::from_vec(iter.into_iter().collect())
    }
}

impl<T: Ord, const N: usize> From<[T; N]> for SortedSet<T> {
    fn from(items: [T; N]) -> Self {
        Self::from_vec(Vec::from(items))
    }
}

impl<T: Ord> Extend<T> for SortedSet<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        let before = self.0.len();
        self.0.extend(iter);
        if self.0.len() != before {
            self.0.sort();
            self.0.dedup();
        }
    }
}

impl<T> IntoIterator for SortedSet<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a, T> IntoIterator for &'a SortedSet<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<T: Serialize> Serialize for SortedSet<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for item in &self.0 {
            seq.serialize_element(item)?;
        }
        seq.end()
    }
}

impl<'de, T: Deserialize<'de> + Ord> Deserialize<'de> for SortedSet<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Items<T>(PhantomData<fn() -> T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for Items<T> {
            type Value = Vec<T>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a sequence")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut items = Vec::with_capacity(access.size_hint().unwrap_or(0).min(4096));
                while let Some(item) = access.next_element()? {
                    items.push(item);
                }
                Ok(items)
            }
        }

        Ok(Self::from_vec(
            deserializer.deserialize_seq(Items(PhantomData))?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use proptest::prelude::*;

    use super::*;

    proptest! {
        /// SHR-21: the compact map iterates and serialises like the `BTreeMap` it replaces, and the
        /// last of duplicate keys wins when it is read, in any key order.
        #[test]
        fn shr_21_sorted_map_matches_btree_map(
            pairs in prop::collection::vec((0u16..40, any::<u8>()), 0..60),
        ) {
            let expected: BTreeMap<u16, u8> = pairs.iter().copied().collect();
            let built: SortedMap<u16, u8> = pairs.iter().copied().collect();
            prop_assert_eq!(built.iter().collect::<Vec<_>>(), expected.iter().collect::<Vec<_>>());

            let json = serde_json::to_string(&expected).expect("serialises");
            prop_assert_eq!(serde_json::to_string(&built).expect("serialises"), json.clone());
            let object = format!(
                "{{{}}}",
                pairs.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect::<Vec<_>>().join(",")
            );
            let read: SortedMap<u16, u8> = serde_json::from_str(&object).expect("reads");
            prop_assert_eq!(serde_json::to_string(&read).expect("serialises"), json);

            let mut incremental = SortedMap::new();
            let mut model = BTreeMap::new();
            for (k, v) in &pairs {
                prop_assert_eq!(incremental.insert(*k, *v), model.insert(*k, *v));
            }
            prop_assert_eq!(incremental, built);
        }

        /// `extend` on a non-empty map, with unsorted input and duplicate keys, equals the
        /// `BTreeMap`: the last value wins.
        #[test]
        fn shr_21_sorted_map_extend_matches_btree_map(
            first in prop::collection::vec((0u16..40, any::<u8>()), 0..30),
            more in prop::collection::vec((0u16..40, any::<u8>()), 0..60),
        ) {
            let mut model: BTreeMap<u16, u8> = first.iter().copied().collect();
            let mut built: SortedMap<u16, u8> = first.iter().copied().collect();
            model.extend(more.iter().copied());
            built.extend(more.iter().copied());
            prop_assert_eq!(built.iter().collect::<Vec<_>>(), model.iter().collect::<Vec<_>>());
        }

        #[test]
        fn shr_21_sorted_set_matches_btree_set(items in prop::collection::vec(0u16..40, 0..60)) {
            let expected: BTreeSet<u16> = items.iter().copied().collect();
            let built: SortedSet<u16> = items.iter().copied().collect();
            prop_assert_eq!(built.iter().collect::<Vec<_>>(), expected.iter().collect::<Vec<_>>());
            let json = serde_json::to_string(&expected).expect("serialises");
            prop_assert_eq!(serde_json::to_string(&built).expect("serialises"), json);
            let text = serde_json::to_string(&items).expect("serialises");
            let read: SortedSet<u16> = serde_json::from_str(&text).expect("reads");
            prop_assert_eq!(read, built);
        }
    }

    #[test]
    fn shr_21_empty_map_is_an_object_and_empty_set_an_array() {
        let map = SortedMap::<String, u8>::new();
        assert_eq!(serde_json::to_string(&map).expect("json"), "{}");
        assert_eq!(
            serde_json::to_string(&SortedSet::<u8>::new()).expect("json"),
            "[]"
        );
    }
}
