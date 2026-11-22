//! Yet another index-based arena.

#![warn(rust_2018_idioms, unused_lifetimes)]
#![warn(missing_docs)]

use std::{
    cmp, fmt,
    hash::{Hash, Hasher},
    iter::FusedIterator,
    marker::PhantomData,
    ops::{Index, IndexMut, Range, RangeInclusive},
};

mod map;
pub use map::ArenaMap;

/// The raw index of a value in an arena.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RawIdx(u32);

impl RawIdx {
    /// Constructs a [`RawIdx`] from a u32.
    #[inline]
    pub const fn from_u32(u32: u32) -> Self {
        RawIdx(u32)
    }

    /// Deconstructs a [`RawIdx`] into the underlying u32.
    #[inline]
    pub const fn into_u32(self) -> u32 {
        self.0
    }
}

impl From<RawIdx> for u32 {
    #[inline]
    fn from(raw: RawIdx) -> u32 {
        raw.0
    }
}

impl From<u32> for RawIdx {
    #[inline]
    fn from(idx: u32) -> RawIdx {
        RawIdx(idx)
    }
}

impl fmt::Debug for RawIdx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for RawIdx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The index of a value allocated in an arena that holds `T`s.
pub struct Idx<T> {
    raw: RawIdx,
    _ty: PhantomData<fn() -> T>,
}

impl<T> Ord for Idx<T> {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        self.raw.cmp(&other.raw)
    }
}

impl<T> PartialOrd for Idx<T> {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Clone for Idx<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Idx<T> {}

impl<T> PartialEq for Idx<T> {
    #[inline]
    fn eq(&self, other: &Idx<T>) -> bool {
        self.raw == other.raw
    }
}
impl<T> Eq for Idx<T> {}

impl<T> Hash for Idx<T> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

impl<T> fmt::Debug for Idx<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut type_name = std::any::type_name::<T>();
        if let Some(idx) = type_name.rfind(':') {
            type_name = &type_name[idx + 1..];
        }
        write!(f, "Idx::<{}>({})", type_name, self.raw)
    }
}

impl<T> Idx<T> {
    /// Creates a new index from a [`RawIdx`].
    #[inline]
    pub const fn from_raw(raw: RawIdx) -> Self {
        Idx {
            raw,
            _ty: PhantomData,
        }
    }

    /// Converts this index into the underlying [`RawIdx`].
    #[inline]
    pub const fn into_raw(self) -> RawIdx {
        self.raw
    }
}

/// A range of densely allocated arena values.
pub struct IdxRange<T> {
    range: (u32, u32),
    _p: PhantomData<T>,
}

impl<T> Copy for IdxRange<T> {}
impl<T> Clone for IdxRange<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Default for IdxRange<T> {
    fn default() -> Self {
        Self::new_empty()
    }
}

impl<T> IdxRange<T> {
    /// Creates a new index range
    /// inclusive of the start value and exclusive of the end value.
    pub fn new_exclusive(range: Range<Idx<T>>) -> Self {
        Self {
            range: (range.start.into_raw().into(), range.end.into_raw().into()),
            _p: PhantomData,
        }
    }

    /// Creates a new empty index range
    pub fn new_empty() -> Self {
        let zero = Idx::from_raw(RawIdx::from_u32(0));
        Self::new_exclusive(zero..zero)
    }

    /// Creates a new index range
    /// inclusive of the start value and end value.
    pub fn new_inclusive(range: RangeInclusive<Idx<T>>) -> Self {
        Self {
            range: (
                u32::from(range.start().into_raw()),
                u32::from(range.end().into_raw()) + 1,
            ),
            _p: PhantomData,
        }
    }

    /// Returns whether the index range is empty.
    pub fn is_empty(&self) -> bool {
        self.range.0 >= self.range.1
    }

    /// Returns the start of the index range.
    pub fn start(&self) -> Idx<T> {
        Idx::from_raw(RawIdx::from(self.range.0))
    }

    /// Returns the raw index range.
    pub fn make_range(&self) -> Range<u32> {
        self.range.0..self.range.1
    }
}

impl<T> Iterator for IdxRange<T> {
    type Item = Idx<T>;

    fn next(&mut self) -> Option<Self::Item> {
        if Self::is_empty(self) {
            return None;
        }
        let raw = self.range.0;
        self.range.0 += 1;
        Some(Idx::from_raw(RawIdx::from(raw)))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = (self.range.1 - self.range.0) as usize;
        (n, Some(n))
    }

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        let new_start = self
            .range
            .0
            .saturating_add(u32::try_from(n).unwrap_or(u32::MAX));
        if new_start >= self.range.1 {
            self.range.0 = self.range.1;
            return None;
        }
        self.range.0 = new_start + 1;
        Some(Idx::from_raw(RawIdx::from(new_start)))
    }
}

impl<T> DoubleEndedIterator for IdxRange<T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if Self::is_empty(self) {
            return None;
        }
        self.range.1 -= 1;
        Some(Idx::from_raw(RawIdx::from(self.range.1)))
    }
}

impl<T> ExactSizeIterator for IdxRange<T> {}

impl<T> FusedIterator for IdxRange<T> {}

impl<T> fmt::Debug for IdxRange<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple(&format!("IdxRange::<{}>", std::any::type_name::<T>()))
            .field(&self.range)
            .finish()
    }
}

impl<T> PartialEq for IdxRange<T> {
    fn eq(&self, other: &Self) -> bool {
        self.range == other.range
    }
}

impl<T> Eq for IdxRange<T> {}

/// Yet another index-based arena.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Arena<T> {
    data: Vec<T>,
}

impl<T: fmt::Debug> fmt::Debug for Arena<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Arena")
            .field("len", &self.len())
            .field("data", &self.data)
            .finish()
    }
}

impl<T> Arena<T> {
    /// Creates a new empty arena.
    pub const fn new() -> Arena<T> {
        Arena { data: Vec::new() }
    }

    /// Create a new empty arena with specific capacity.
    pub fn with_capacity(capacity: usize) -> Arena<T> {
        Arena {
            data: Vec::with_capacity(capacity),
        }
    }

    /// Drops every value allocated at or after index `len`; indices below it
    /// stay valid.
    pub fn truncate(&mut self, len: usize) {
        self.data.truncate(len);
    }

    /// Returns the length of the arena.
    #[inline]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// The dense range appended since the arena had `start` values.
    ///
    /// Panics if `start` is greater than the current length.
    pub fn allocated_since(&self, start: usize) -> IdxRange<T> {
        assert!(start <= self.len());
        if start == self.len() {
            IdxRange::new_empty()
        } else {
            IdxRange::new_exclusive(Idx::from_raw((start as u32).into())..self.next_idx())
        }
    }

    /// Returns the allocated backing capacity.
    pub fn capacity(&self) -> usize {
        self.data.capacity()
    }

    /// Reserves capacity for at least `additional` more values.
    pub fn reserve(&mut self, additional: usize) {
        self.data.reserve(additional);
    }

    /// Returns whether the arena contains no elements.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Allocates a new value on the arena, returning the value’s index.
    #[inline]
    pub fn alloc(&mut self, value: T) -> Idx<T> {
        let idx = self.next_idx();
        self.data.push(value);
        idx
    }

    /// Returns an iterator over the arena’s elements.
    pub fn iter(
        &self,
    ) -> impl ExactSizeIterator<Item = (Idx<T>, &T)> + DoubleEndedIterator + Clone {
        self.data
            .iter()
            .enumerate()
            .map(|(idx, value)| (Idx::from_raw(RawIdx(idx as u32)), value))
    }

    /// Returns an iterator over the arena’s mutable elements.
    pub fn iter_mut(
        &mut self,
    ) -> impl ExactSizeIterator<Item = (Idx<T>, &mut T)> + DoubleEndedIterator {
        self.data
            .iter_mut()
            .enumerate()
            .map(|(idx, value)| (Idx::from_raw(RawIdx(idx as u32)), value))
    }

    /// Returns an iterator over the arena’s values.
    pub fn values(&self) -> impl ExactSizeIterator<Item = &T> + DoubleEndedIterator {
        self.data.iter()
    }

    /// Returns an iterator over the arena’s mutable values.
    pub fn values_mut(&mut self) -> impl ExactSizeIterator<Item = &mut T> + DoubleEndedIterator {
        self.data.iter_mut()
    }

    /// Reallocates the arena to make it take up as little space as possible.
    pub fn shrink_to_fit(&mut self) {
        exact_fit(&mut self.data);
    }

    /// Returns the index of the next value allocated on the arena.
    ///
    /// This method should remain private to make creating invalid `Idx`s harder.
    fn next_idx(&self) -> Idx<T> {
        Idx::from_raw(RawIdx(self.data.len() as u32))
    }
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Index<Idx<T>> for Arena<T> {
    type Output = T;
    #[inline]
    fn index(&self, idx: Idx<T>) -> &T {
        let idx = idx.into_raw().0 as usize;
        &self.data[idx]
    }
}

impl<T> IndexMut<Idx<T>> for Arena<T> {
    #[inline]
    fn index_mut(&mut self, idx: Idx<T>) -> &mut T {
        let idx = idx.into_raw().0 as usize;
        &mut self.data[idx]
    }
}

impl<T> FromIterator<T> for Arena<T> {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = T>,
    {
        Arena {
            data: Vec::from_iter(iter),
        }
    }
}

/// Move `v` into an allocation of exactly its length. `Vec::shrink_to_fit`
/// asks the allocator to `realloc`, and mimalloc keeps a block in place when
/// it shrinks by less than half: every vector grown by doubling.
pub fn exact_fit<T>(v: &mut Vec<T>) {
    if v.capacity() > v.len() {
        let mut exact = Vec::with_capacity(v.len());
        exact.append(v);
        *v = exact;
    }
}
