use std::marker::PhantomData;

use crate::Idx;

/// A map from arena indexes to some other type.
/// Space requirement is O(highest index).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArenaMap<IDX, V> {
    v: Vec<Option<V>>,
    _ty: PhantomData<IDX>,
}

impl<T, V> ArenaMap<Idx<T>, V> {
    /// Creates a new empty map.
    pub const fn new() -> Self {
        Self {
            v: Vec::new(),
            _ty: PhantomData,
        }
    }

    /// Removes the values of every index at or after `len`.
    pub fn truncate(&mut self, len: usize) {
        self.v.truncate(len);
    }

    /// Shrinks the capacity of the map as much as possible.
    pub fn shrink_to_fit(&mut self) {
        let min_len = self
            .v
            .iter()
            .rposition(|slot| slot.is_some())
            .map_or(0, |i| i + 1);
        self.v.truncate(min_len);
        crate::exact_fit(&mut self.v);
    }

    /// Returns whether the map contains a value for the specified index.
    pub fn contains_idx(&self, idx: Idx<T>) -> bool {
        matches!(self.v.get(Self::to_idx(idx)), Some(Some(_)))
    }

    /// Removes an index from the map, returning the value at the index if the index was previously in the map.
    pub fn remove(&mut self, idx: Idx<T>) -> Option<V> {
        self.v.get_mut(Self::to_idx(idx))?.take()
    }

    /// Inserts a value associated with a given arena index into the map.
    ///
    /// If the map did not have this index present, None is returned.
    /// Otherwise, the value is updated, and the old value is returned.
    pub fn insert(&mut self, idx: Idx<T>, t: V) -> Option<V> {
        let idx = Self::to_idx(idx);

        self.v.resize_with((idx + 1).max(self.v.len()), || None);
        self.v[idx].replace(t)
    }

    /// Returns a reference to the value associated with the provided index
    /// if it is present.
    pub fn get(&self, idx: Idx<T>) -> Option<&V> {
        self.v.get(Self::to_idx(idx)).and_then(|it| it.as_ref())
    }

    /// Returns an iterator over the values in the map.
    pub fn values(&self) -> impl DoubleEndedIterator<Item = &V> {
        self.v.iter().filter_map(|o| o.as_ref())
    }

    /// Returns an iterator over mutable references to the values in the map.
    pub fn values_mut(&mut self) -> impl DoubleEndedIterator<Item = &mut V> {
        self.v.iter_mut().filter_map(|o| o.as_mut())
    }

    /// Returns an iterator over the arena indexes and values in the map.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (Idx<T>, &V)> {
        self.v
            .iter()
            .enumerate()
            .filter_map(|(idx, o)| Some((Self::from_idx(idx), o.as_ref()?)))
    }

    fn to_idx(idx: Idx<T>) -> usize {
        u32::from(idx.into_raw()) as usize
    }

    fn from_idx(idx: usize) -> Idx<T> {
        Idx::from_raw((idx as u32).into())
    }
}

impl<T, V> std::ops::Index<Idx<V>> for ArenaMap<Idx<V>, T> {
    type Output = T;
    fn index(&self, idx: Idx<V>) -> &T {
        self.v[Self::to_idx(idx)].as_ref().unwrap()
    }
}

impl<T, V> std::ops::IndexMut<Idx<V>> for ArenaMap<Idx<V>, T> {
    fn index_mut(&mut self, idx: Idx<V>) -> &mut T {
        self.v[Self::to_idx(idx)].as_mut().unwrap()
    }
}

impl<T, V> Default for ArenaMap<Idx<V>, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, V> Extend<(Idx<V>, T)> for ArenaMap<Idx<V>, T> {
    fn extend<I: IntoIterator<Item = (Idx<V>, T)>>(&mut self, iter: I) {
        iter.into_iter().for_each(move |(k, v)| {
            self.insert(k, v);
        });
    }
}

impl<T, V> FromIterator<(Idx<V>, T)> for ArenaMap<Idx<V>, T> {
    fn from_iter<I: IntoIterator<Item = (Idx<V>, T)>>(iter: I) -> Self {
        let mut this = Self::new();
        this.extend(iter);
        this
    }
}
