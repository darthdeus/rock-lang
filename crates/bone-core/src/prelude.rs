pub use rustc_hash::{FxHashMap, FxHashSet};
pub use smallvec::SmallVec;
pub use std::time::{Duration, Instant};
pub use std::{cell::RefCell, sync::LazyLock};
pub use ustr::{ustr, Ustr};

pub use comfy_la_arena::{Arena, ArenaMap, Idx, IdxRange, RawIdx};
pub use rust_lapper::Lapper;
pub use skull_parser::{
    source_file_from_path_and_contents, CompilerError, OrderedFloat, SourceFiles, Span,
};

pub use crate::hir;
pub use crate::workspace::*;
use crate::hir::Function;
pub use crate::hir::{Body, BodyOwner, EnumDef, StructDef, TyId};
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

    pub fn builtin() -> Self {
        ModuleId::new(0)
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
    /// `@host_resource` -- see [`crate::hir::HostResource`].
    pub host_resource: Option<crate::hir::HostResource>,
}

// ---------------------------------------------------------------------------
// Parallelism shim. See `docs/...` -- `sequential` cargo feature swaps rayon's
// `par_iter`/`into_par_iter`/`collect_vec_list` onto std iterators for a clean
// single-threaded sampling profile; call sites stay unchanged.
#[cfg(not(feature = "sequential"))]
pub use rayon::iter::{
    IntoParallelIterator, IntoParallelRefIterator, IntoParallelRefMutIterator, ParallelIterator,
};

#[cfg(feature = "sequential")]
pub use sequential_par::*;

#[cfg(feature = "sequential")]
mod sequential_par {
    use std::collections::{hash_map, HashMap, LinkedList};

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
}
