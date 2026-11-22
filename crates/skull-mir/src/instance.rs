//! An [`Instance`]'s `args` are ids in the instance graph's
//! [`crate::monoty::MonoInterner`] and have no meaning without it.
use bone_core::workspace::Workspace;

use crate::mir::FuncRef;
use crate::monoty::{MonoInterner, MonoSubst, MonoTy, MonoTyId};

/// Non-generic functions get an instance too, with empty `args`. Empty `args`
/// means non-generic and nothing else; it is not a shared-body sentinel.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Instance {
    pub func: FuncRef,
    pub args: Vec<MonoTyId>,
}

impl Instance {
    pub fn base(func: FuncRef) -> Self {
        Self {
            func,
            args: Vec::new(),
        }
    }

    /// Reads the declaration's generics, not `self.args`: an argument list that
    /// disagreed would be a defect, not a different answer.
    pub fn is_generic(&self, ws: &Workspace) -> bool {
        !ws.module(self.func.module).functions[self.func.func]
            .generics
            .is_empty()
    }

    /// In declaration order. Ids in, owned types out is always safe; the other
    /// direction ([`Self::intern`]) must go through the build's single interner.
    pub fn arg_types(&self, interner: &MonoInterner) -> Vec<MonoTy> {
        interner.get_all(&self.args)
    }

    /// The only way an instance is minted outside the graph's own walk.
    pub fn intern(interner: &MonoInterner, func: FuncRef, args: Vec<MonoTy>) -> Self {
        Self {
            func,
            args: interner.intern_all(args),
        }
    }

    /// The declaration supplies the generics' order. Empty for a non-generic
    /// instance.
    pub fn subst(&self, ws: &Workspace, interner: &MonoInterner) -> MonoSubst {
        if self.args.is_empty() {
            return MonoSubst::default();
        }
        crate::lower::spec_subst_from_key(ws, self.func, &self.arg_types(interner))
    }

    /// A non-generic instance is the base mangling. A generic one always has
    /// concrete arguments; falling back to the base name would miscompile.
    pub fn symbol(&self, ws: &Workspace, interner: &MonoInterner) -> String {
        let f = &ws.module(self.func.module).functions[self.func.func];
        if self.args.is_empty() {
            return crate::mangle::mangle_fn(ws, f);
        }
        crate::mangle::spec_fn_name(ws, f, &self.arg_types(interner))
            .expect("an instance with arguments always has a specialized symbol")
    }

    /// The graph silently caps a too-deep instance, so whoever needs its body
    /// raises this. Only recursion that grows its type arguments diverges;
    /// same-instance recursion is legal.
    pub fn too_deep_error(
        &self,
        ws: &Workspace,
        interner: &MonoInterner,
    ) -> Option<skull_parser::CompilerError> {
        let args = self.arg_types(interner);
        let at = crate::monoty::too_deep_to_instantiate(args.iter())?.render();
        let f = &ws.module(self.func.module).functions[self.func.func];
        Some(skull_parser::CompilerError::new_with_label(
            f.name_span,
            format!(
                "polymorphic recursion: instantiating `{}` at `{}` exceeds the \
                 instantiation depth limit of {}. A recursive call whose type \
                 arguments GROW never terminates -- each call needs a new body -- so it \
                 is rejected rather than expanded. Same-instance recursion (type \
                 arguments unchanged) is unaffected.",
                f.name,
                at,
                crate::monoty::MAX_INSTANTIATION_DEPTH,
            ),
        ))
    }

    /// Raised for an instance whose call sites resolve its `Iterator` bounds
    /// differently.
    pub fn bound_clash_error(
        &self,
        ws: &Workspace,
        interner: &MonoInterner,
    ) -> skull_parser::CompilerError {
        let f = &ws.module(self.func.module).functions[self.func.func];
        skull_parser::CompilerError::new_with_label(
            f.name_span,
            format!(
                "internal: two call sites demand `{}` with different `Iterator` \
                 bound resolutions, so one emitted definition cannot serve both",
                self.symbol(ws, interner)
            ),
        )
    }
}

/// `(module, function index)`: stable within a build and used only to make
/// iteration deterministic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeclKey(pub FuncRef);

impl DeclKey {
    pub fn raw(self) -> (u32, u32) {
        (self.0.module.raw(), self.0.func.into_raw().into_u32())
    }
}

impl Ord for DeclKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.raw().cmp(&other.raw())
    }
}

impl PartialOrd for DeclKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Stable within a build and only used for deterministic iteration; the manifest
/// is sorted by rendered symbol, which is stable across builds.
fn ord_key(i: &Instance) -> (u32, u32, &[MonoTyId]) {
    (
        i.func.module.raw(),
        i.func.func.into_raw().into_u32(),
        &i.args,
    )
}

/// Not an identity across builds or indexes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct InstanceId(u32);

impl InstanceId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Ids are dense in first-asked order and never reused or reassigned, so a table
/// indexed by them only grows.
pub struct InstanceIndex {
    generation: u64,
    ids: std::sync::RwLock<rustc_hash::FxHashMap<Instance, InstanceId>>,
}

struct InstanceIdCache {
    generation: u64,
    ids: rustc_hash::FxHashMap<Instance, InstanceId>,
}

thread_local! {
    /// Ids this worker has seen, so a repeat question takes no shared lock.
    static INSTANCE_ID_CACHE: std::cell::RefCell<InstanceIdCache> =
        std::cell::RefCell::new(InstanceIdCache {
            generation: 0,
            ids: Default::default(),
        });
}

impl Default for InstanceIndex {
    fn default() -> Self {
        static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            generation: GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            ids: Default::default(),
        }
    }
}

impl std::fmt::Debug for InstanceIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstanceIndex")
            .field("len", &self.len())
            .finish()
    }
}

impl InstanceIndex {
    fn len(&self) -> usize {
        self.ids.read().expect("instance index poisoned").len()
    }

    pub fn id(&self, inst: &Instance) -> InstanceId {
        INSTANCE_ID_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            if cache.generation != self.generation {
                cache.generation = self.generation;
                cache.ids.clear();
            }
            if let Some(id) = cache.ids.get(inst) {
                return *id;
            }
            let known = self
                .ids
                .read()
                .expect("instance index poisoned")
                .get(inst)
                .copied();
            let id = known.unwrap_or_else(|| {
                let mut ids = self.ids.write().expect("instance index poisoned");
                let next = InstanceId(ids.len() as u32);
                *ids.entry(inst.clone()).or_insert(next)
            });
            cache.ids.insert(inst.clone(), id);
            id
        })
    }
}

impl Ord for Instance {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        ord_key(self).cmp(&ord_key(other))
    }
}

impl PartialOrd for Instance {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(interner: &MonoInterner, func: u32, args: usize) -> Instance {
        Instance {
            func: FuncRef {
                module: bone_core::prelude::ModuleId::new(0),
                func: comfy_la_arena::Idx::from_raw(comfy_la_arena::RawIdx::from_u32(func)),
            },
            args: vec![interner.intern(MonoTy::Int); args],
        }
    }

    #[test]
    fn instance_ids_are_dense_stable_and_per_index() {
        let interner = MonoInterner::new();
        let index = InstanceIndex::default();
        let a = instance(&interner, 1, 0);
        let b = instance(&interner, 1, 1);
        assert_eq!(index.id(&a).index(), 0);
        assert_eq!(index.id(&b).index(), 1);
        assert_eq!(index.id(&a).index(), 0);

        let other = InstanceIndex::default();
        assert_eq!(other.id(&b).index(), 0);
        assert_eq!(index.id(&b).index(), 1);

        let seen_elsewhere = std::thread::spawn(move || index.id(&b).index())
            .join()
            .expect("worker");
        assert_eq!(seen_elsewhere, 1);
    }
}
