pub use rock_core::clock::Instant;
pub use rustc_hash::{FxHashMap, FxHashSet};
pub use smallvec::SmallVec;
pub use std::time::Duration;
pub use std::{cell::RefCell, sync::LazyLock};
pub use ustr::{Ustr, ustr};

pub use comfy_la_arena::{Arena, ArenaMap, Idx, IdxRange, RawIdx};
pub(crate) use rust_lapper::Lapper;
pub use skull_parser::{
    CompilerError, CompilerErrorKind, OrderedFloat, SourceFiles, Span,
    source_file_from_path_and_contents,
};

pub use crate::hir;
use crate::hir::Function;
pub use crate::hir::{Body, BodyOwner, EnumDef, StructDef, TyId};
pub use crate::workspace::*;
pub use itertools::Itertools;

use nonmax::NonMaxU32;

#[macro_export]
macro_rules! comp_error {
    ($span:expr, $($arg:tt)*) => {{
        return Err(CompilerError::new_with_label($span, format!($($arg)*)));
    }}
}

#[macro_export]
macro_rules! or_fail {
    ($expr:expr, $span:expr, $($arg:tt)*) => {
        ($expr).ok_or_else(|| CompilerError::new_with_label($span, format!($($arg)*)))
    }
}

// The message arguments are evaluated only when the error can be reported, so
// keep `render` calls inside the macro: quiet trials discard the message.
#[macro_export]
macro_rules! unify_fail {
    ($gamma:expr, $span:expr, $($arg:tt)*) => {{
        if $gamma.quiet {
            return Err(CompilerError::silent($span));
        }

        comp_error!($span, $($arg)*)
    }}
}

pub use crate::{comp_error, or_fail, unify_fail};

pub type FnList = SmallVec<[(ModuleId, Idx<Function>); 1]>;

pub trait TryCollectVec<T, E>: Iterator<Item = Result<T, E>> + Sized {
    fn try_vec(self) -> Result<Vec<T>, E> {
        self.collect()
    }
}

impl<I, T, E> TryCollectVec<T, E> for I where I: Iterator<Item = Result<T, E>> {}

pub fn alloc_contiguous<T>(
    arena: &mut Arena<T>,
    nodes: impl IntoIterator<Item = T>,
) -> IdxRange<T> {
    let mut iter = nodes.into_iter();

    if let Some(first) = iter.next() {
        let first = arena.alloc(first);

        let mut last = first;
        for n in iter {
            last = arena.alloc(n);
        }

        IdxRange::new_inclusive(first..=last)
    } else {
        IdxRange::new_empty()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct StructId {
    pub module: ModuleId,
    pub def: Idx<StructDef>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct EnumId {
    pub module: ModuleId,
    pub def: Idx<EnumDef>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ModuleId(pub NonMaxU32);

impl ModuleId {
    #[inline]
    pub fn new(v: u32) -> ModuleId {
        ModuleId(NonMaxU32::new(v).expect("ModuleId u32::MAX is reserved as the TyId niche"))
    }

    #[inline]
    pub fn raw(self) -> u32 {
        self.0.get()
    }

    #[inline]
    pub fn index(self) -> usize {
        self.0.get() as usize
    }

    /// The synthetic `$builtin` module `Workspace::new` creates to own the
    /// builtin type nodes. It is permanent: see `Workspace::source_modules`.
    pub fn builtin() -> Self {
        ModuleId::new(0)
    }

    #[inline]
    pub fn is_builtin(self) -> bool {
        self == Self::builtin()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct NewtypeId {
    pub module: ModuleId,
    pub def: Idx<NewtypeDef>,
}

#[derive(Clone, Debug)]
pub struct NewtypeDef {
    pub name: Ustr,
    pub name_span: Span,
    pub span: Span,
    pub generics: Vec<TyId>,
    pub wrapped: TyId,
}

// ---------------------------------------------------------------------------
// Parallelism shim. `sequential` cargo feature swaps rayon's
// `par_iter`/`into_par_iter`/`collect_vec_list` onto std iterators for a clean
// single-threaded sampling profile; call sites stay unchanged.
#[cfg(not(feature = "sequential"))]
pub use rayon::iter::{
    IntoParallelIterator, IntoParallelRefIterator, IntoParallelRefMutIterator, ParallelIterator,
};

#[cfg(not(feature = "sequential"))]
pub use rayon::join as par_join;

/// Free `value` on the pool instead of the calling thread.
#[cfg(not(any(feature = "sequential", target_arch = "wasm32")))]
pub fn drop_in_background<T: Send + 'static>(value: T) {
    rayon::spawn(move || drop(value));
}

#[cfg(any(feature = "sequential", target_arch = "wasm32"))]
pub fn drop_in_background<T>(value: T) {
    drop(value);
}

/// `items.into_par_iter().map(f).collect()`, in the same order, but each
/// worker takes the costliest item left. Split-based scheduling can start a
/// large item last and leave it running alone after the others finish.
#[cfg(not(any(feature = "sequential", target_arch = "wasm32")))]
pub fn par_map_largest_first<T: Send, R: Send>(
    items: Vec<T>,
    cost: impl Fn(&T) -> usize,
    f: impl Fn(T) -> R + Sync + Send,
) -> Vec<R> {
    use rayon::iter::ParallelBridge;
    let n = items.len();
    let mut queue: Vec<(usize, T)> = items.into_iter().enumerate().collect();
    queue.sort_by_cached_key(|(_, item)| std::cmp::Reverse(cost(item)));
    let mut done: Vec<(usize, R)> = queue
        .into_iter()
        .par_bridge()
        .map(|(index, item)| (index, f(item)))
        .collect();
    done.sort_unstable_by_key(|(index, _)| *index);
    debug_assert_eq!(done.len(), n);
    done.into_iter().map(|(_, result)| result).collect()
}

#[cfg(any(feature = "sequential", target_arch = "wasm32"))]
pub fn par_map_largest_first<T, R>(
    items: Vec<T>,
    _cost: impl Fn(&T) -> usize,
    f: impl Fn(T) -> R,
) -> Vec<R> {
    items.into_iter().map(f).collect()
}

#[cfg(feature = "sequential")]
pub use sequential_par::*;

#[cfg(feature = "sequential")]
mod sequential_par {
    use std::collections::{HashMap, LinkedList, hash_map};

    pub trait SeqParSlice<T> {
        fn par_iter(&self) -> std::slice::Iter<'_, T>;
        fn par_iter_mut(&mut self) -> std::slice::IterMut<'_, T>;
    }
    impl<T> SeqParSlice<T> for [T] {
        fn par_iter(&self) -> std::slice::Iter<'_, T> {
            self.iter()
        }
        fn par_iter_mut(&mut self) -> std::slice::IterMut<'_, T> {
            self.iter_mut()
        }
    }

    pub trait SeqParMap<K, V> {
        fn par_iter(&self) -> hash_map::Iter<'_, K, V>;
    }
    impl<K, V, S> SeqParMap<K, V> for HashMap<K, V, S> {
        fn par_iter(&self) -> hash_map::Iter<'_, K, V> {
            self.iter()
        }
    }

    pub trait SeqIntoPar: IntoIterator + Sized {
        fn into_par_iter(self) -> Self::IntoIter {
            self.into_iter()
        }
    }
    impl<I: IntoIterator> SeqIntoPar for I {}

    pub trait SeqCollectVecList: Iterator + Sized {
        fn collect_vec_list(self) -> LinkedList<Vec<Self::Item>> {
            let mut list = LinkedList::new();
            list.push_back(self.collect());
            list
        }
    }
    impl<I: Iterator> SeqCollectVecList for I {}

    pub trait SeqFindMapFirst: Iterator + Sized {
        fn find_map_first<B>(mut self, f: impl FnMut(Self::Item) -> Option<B>) -> Option<B> {
            self.find_map(f)
        }
    }
    impl<I: Iterator> SeqFindMapFirst for I {}

    pub fn par_join<RA, RB>(a: impl FnOnce() -> RA, b: impl FnOnce() -> RB) -> (RA, RB) {
        (a(), b())
    }
}
