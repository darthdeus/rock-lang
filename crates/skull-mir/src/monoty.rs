//! The logical type. Ids are type-equality within one arena only; cross-process
//! identity is [`MonoTy::canonical`], never the numeric id or [`MonoTy::render`].
//! The API takes no target: only ABI and layout may differ downstream.

use bone_core::hir::{BuiltinTy, TypeCtor, TypeExpr, VarId};
use bone_core::prelude::TyId;
use bone_core::workspace::{BodyInference, Workspace};
use rustc_hash::FxHashMap;
use ustr::Ustr;

/// Equality is type-equality within one arena; it carries no meaning across
/// arenas, which is what `canonical` is for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct MonoTyId(u32);

impl MonoTyId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// `Nominal` keeps the declared name, which makes two same-layout structs distinct.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MonoCtor {
    Array,
    Map,
    Ref,
    Nominal(Ustr),
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum MonoTy {
    Int,
    Uint,
    Float,
    Bool,
    String,
    Any,
    Unit,
    Never,
    Nullable(Box<MonoTy>),
    Tuple(Vec<MonoTy>),
    App {
        ctor: MonoCtor,
        params: Vec<MonoTy>,
    },
    Function {
        params: Vec<MonoTy>,
        ret: Box<MonoTy>,
    },
}

/// A symbol and an identity may differ about a type's name, never its shape.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NominalSpelling {
    /// The DefKey, for a stable identity.
    Key,
    /// Serialized artifact identity, preserving standalone root spellings.
    Artifact,
    /// The DefKey's identifier form (`<Name>_<digest>`), for an emitted symbol.
    /// Bare compiler-owned keys pass through unchanged.
    Symbol,
}

impl MonoTy {
    /// Display only, and not injective: `(fn(int) -> int)?` and `fn(int) -> int?`
    /// spell alike. Compare, deduplicate and hash with [`Self::canonical`].
    pub fn render(&self) -> String {
        let mut s = String::new();
        self.write(&mut s);
        s
    }

    fn write(&self, out: &mut String) {
        match self {
            MonoTy::Int => out.push_str("int"),
            MonoTy::Uint => out.push_str("uint"),
            MonoTy::Float => out.push_str("float"),
            MonoTy::Bool => out.push_str("bool"),
            MonoTy::String => out.push_str("string"),
            MonoTy::Any => out.push_str("any"),
            MonoTy::Unit => out.push_str("unit"),
            MonoTy::Never => out.push_str("never"),
            MonoTy::Nullable(inner) => {
                inner.write(out);
                out.push('?');
            }
            MonoTy::Tuple(parts) => {
                out.push('(');
                for (i, p) in parts.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    p.write(out);
                }
                out.push(')');
            }
            MonoTy::App { ctor, params } => {
                match ctor {
                    MonoCtor::Array => out.push_str("Vec"),
                    MonoCtor::Map => out.push_str("Map"),
                    MonoCtor::Ref => out.push_str("ref"),
                    MonoCtor::Nominal(n) => {
                        out.push_str(crate::layout_instance::decl_name(*n).as_str())
                    }
                }
                if !params.is_empty() {
                    out.push('<');
                    for (i, p) in params.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        p.write(out);
                    }
                    out.push('>');
                }
            }
            MonoTy::Function { params, ret } => {
                out.push_str("fn(");
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    p.write(out);
                }
                out.push_str(") -> ");
                ret.write(out);
            }
        }
    }

    /// Polymorphic recursion (`f<T> -> f<[T]>`) grows this without bound, so an
    /// instantiation walk caps on it rather than on a path length.
    pub fn depth(&self) -> u32 {
        match self {
            MonoTy::Int
            | MonoTy::Uint
            | MonoTy::Float
            | MonoTy::Bool
            | MonoTy::String
            | MonoTy::Any
            | MonoTy::Unit
            | MonoTy::Never => 0,
            MonoTy::Nullable(inner) => 1 + inner.depth(),
            MonoTy::Tuple(parts) => 1 + parts.iter().map(MonoTy::depth).max().unwrap_or(0),
            MonoTy::App { params, .. } => 1 + params.iter().map(MonoTy::depth).max().unwrap_or(0),
            MonoTy::Function { params, ret } => {
                let p = params.iter().map(MonoTy::depth).max().unwrap_or(0);
                1 + p.max(ret.depth())
            }
        }
    }

    /// Tagged, length-prefixed and terminated, so unambiguous by construction.
    /// Emitted specialized symbols are built from the same encoding.
    pub fn canonical(&self) -> String {
        let mut s = String::new();
        self.write_canonical(&mut s);
        s
    }

    /// Serialized type spelling, retaining standalone root compatibility.
    pub fn artifact_key(&self) -> String {
        let mut key = String::new();
        self.write_canonical_as(&mut key, NominalSpelling::Artifact);
        key
    }

    pub(crate) fn write_canonical(&self, out: &mut String) {
        self.write_canonical_as(out, NominalSpelling::Key);
    }

    pub(crate) fn write_canonical_as(&self, out: &mut String, spelling: NominalSpelling) {
        let prim = |out: &mut String, name: &str| {
            out.push_str("p_");
            out.push_str(name);
        };
        match self {
            MonoTy::Int => prim(out, "int"),
            MonoTy::Uint => prim(out, "uint"),
            MonoTy::Float => prim(out, "float"),
            MonoTy::Bool => prim(out, "bool"),
            MonoTy::String => prim(out, "string"),
            MonoTy::Any => prim(out, "any"),
            MonoTy::Unit => prim(out, "unit"),
            MonoTy::Never => prim(out, "never"),
            // `T = E?` differs from `T = E` (its slots carry a presence leaf); without
            // the tag the two would collide on one symbol.
            MonoTy::Nullable(inner) => {
                out.push_str("o_");
                inner.write_canonical_as(out, spelling);
            }
            // Length-prefixed and terminated, so `(A, B)` cannot encode like a one-slot
            // tuple of something that encodes as `A_B`.
            MonoTy::Tuple(parts) => {
                out.push_str("t_");
                out.push_str(&parts.len().to_string());
                for part in parts {
                    out.push('_');
                    part.write_canonical_as(out, spelling);
                }
                out.push_str("_z");
            }
            MonoTy::App { ctor, params } => {
                out.push_str("a_");
                match ctor {
                    MonoCtor::Array => out.push('v'),
                    MonoCtor::Map => out.push('m'),
                    MonoCtor::Ref => out.push('r'),
                    MonoCtor::Nominal(name) => {
                        let name = &crate::layout_instance::decl_name(*name);
                        // The length prefix measures the spelled text, so the encoding stays
                        // self-delimiting under either spelling.
                        let spelled = match spelling {
                            NominalSpelling::Key => std::borrow::Cow::Borrowed(name.as_str()),
                            NominalSpelling::Artifact => std::borrow::Cow::Borrowed(
                                bone_core::modules::artifact_name(name.as_str()),
                            ),
                            NominalSpelling::Symbol => crate::mangle::ident_of_key(name.as_str()),
                        };
                        out.push('n');
                        out.push_str(&spelled.len().to_string());
                        out.push('_');
                        out.push_str(&spelled);
                    }
                }
                out.push('_');
                out.push_str(&params.len().to_string());
                for param in params {
                    out.push('_');
                    param.write_canonical_as(out, spelling);
                }
                out.push_str("_z");
            }
            // The result is part of the identity, encoded and terminated like any child.
            MonoTy::Function { params, ret } => {
                out.push_str("f_");
                out.push_str(&params.len().to_string());
                for param in params {
                    out.push('_');
                    param.write_canonical_as(out, spelling);
                }
                out.push('_');
                ret.write_canonical_as(out, spelling);
                out.push_str("_z");
            }
        }
    }

    /// `None` for a nullable binding on purpose: a presence-carrying binding is
    /// not exactly this struct. Presence-aware callers use [`Self::peel_nullable`].
    pub fn nominal(&self) -> Option<Ustr> {
        match self {
            Self::App {
                ctor: MonoCtor::Nominal(name),
                ..
            } => Some(*name),
            _ => None,
        }
    }

    /// `?` over a nullable type is that type: both spell absence as one null, so
    /// no type has two layers.
    pub fn nullable(inner: MonoTy) -> MonoTy {
        match inner {
            MonoTy::Nullable(_) => inner,
            other => MonoTy::Nullable(Box::new(other)),
        }
    }

    pub fn peel_nullable(&self) -> (&MonoTy, bool) {
        match self {
            Self::Nullable(inner) => (inner.as_ref(), true),
            other => (other, false),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum MonoTyReason {
    /// Inference recovery poison: an invalid program.
    RecoveryError,
    /// `f32` is a field storage type, which no value has.
    StorageOnly,
    UnresolvedInfer(Ustr),
    /// Expected in generic contexts that have not selected an instance.
    UnboundGeneric(Ustr),
    NoNode,
    /// A cyclic substitution chain.
    DepthLimit,
    /// A body-local or inference type reached from a declaration: compiler state
    /// disagreeing with itself, never a source shape.
    NotADeclarationType,
}

impl MonoTyReason {
    pub fn describe(&self) -> String {
        match self {
            Self::DepthLimit => format!(
                "a type in it is nested too deeply (more than {MAX_DEPTH} levels); name an \
                 inner layer with a struct or newtype"
            ),
            Self::UnboundGeneric(name) => format!("the generic parameter `{name}` is not bound"),
            Self::UnresolvedInfer(name) => format!("the type of `{name}` was not inferred"),
            Self::RecoveryError => "a type could not be recovered".to_string(),
            other => format!("a type the compiler cannot normalize ({})", other.bucket()),
        }
    }

    pub fn bucket(&self) -> &'static str {
        match self {
            Self::RecoveryError => "recovery-error",
            Self::StorageOnly => "storage-only",
            Self::UnresolvedInfer(_) => "unresolved-infer",
            Self::UnboundGeneric(_) => "unbound-generic",
            Self::NoNode => "no-node",
            Self::DepthLimit => "depth-limit",
            Self::NotADeclarationType => "not-a-declaration-type",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NewtypeResolutionError {
    Conversion(MonoTyReason),
    Cycle(Ustr),
    MissingDefinition(Ustr),
    ArityMismatch {
        name: Ustr,
        actual: usize,
        expected: usize,
    },
    InvalidGeneric(Ustr),
    StepLimit,
}

impl std::fmt::Display for NewtypeResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conversion(MonoTyReason::UnboundGeneric(name)) => {
                write!(f, "the unbound generic parameter `{name}`")
            }
            Self::Conversion(MonoTyReason::DepthLimit) => {
                f.write_str("a type that does not bottom out")
            }
            Self::Conversion(MonoTyReason::RecoveryError) => {
                f.write_str("a type inference could not recover")
            }
            Self::Conversion(reason) => {
                write!(
                    f,
                    "a type the compiler cannot normalize ({})",
                    reason.bucket()
                )
            }
            Self::Cycle(_) => f.write_str("a newtype whose representation contains itself"),
            Self::MissingDefinition(name) => {
                write!(f, "a newtype `{name}` whose definition is missing")
            }
            Self::ArityMismatch {
                name,
                actual,
                expected,
            } => write!(
                f,
                "`{name}` applied to {actual} argument(s) where its declaration takes {expected}"
            ),
            Self::InvalidGeneric(name) => {
                write!(f, "`{name}` declares a parameter that is not a generic")
            }
            Self::StepLimit => f.write_str("an alias chain that does not bottom out"),
        }
    }
}

/// Concrete instantiations are the cycle key. Generic arguments convert in
/// their current environment before binding into the next declaration.
pub fn resolve_newtype_chain(ws: &Workspace, ty: MonoTy) -> Result<MonoTy, NewtypeResolutionError> {
    use bone_core::hir::TypeExpr;

    const MAX_STEPS: usize = 256;

    let mut current = ty;
    let mut visited = Vec::new();
    for _ in 0..MAX_STEPS {
        let MonoTy::App {
            ctor: MonoCtor::Nominal(name),
            ref params,
        } = current
        else {
            return Ok(current);
        };
        let Some(nid) = ws.nominals.newtype_id(name) else {
            return Ok(current);
        };
        if visited.contains(&current) {
            return Err(NewtypeResolutionError::Cycle(name));
        }

        let owner = ws
            .modules
            .get(nid.module.index())
            .filter(|module| (nid.def.into_raw().into_u32() as usize) < module.newtypes.len())
            .ok_or(NewtypeResolutionError::MissingDefinition(name))?;
        let def = &owner.newtypes[nid.def];
        if def.generics.len() != params.len() {
            return Err(NewtypeResolutionError::ArityMismatch {
                name,
                actual: params.len(),
                expected: def.generics.len(),
            });
        }

        let mut subst = MonoSubst::default();
        for (generic, arg) in def.generics.iter().zip(params) {
            let (TypeExpr::Generic(var, _) | TypeExpr::Infer(var, _)) = TyEnv::Decl
                .decl_node(ws, *generic)
                .map_err(NewtypeResolutionError::Conversion)?
            else {
                return Err(NewtypeResolutionError::InvalidGeneric(name));
            };
            subst.insert(var, arg.clone());
        }
        let next =
            convert_decl(ws, def.wrapped, &subst).map_err(NewtypeResolutionError::Conversion)?;
        visited.push(current);
        current = next;
    }

    Err(NewtypeResolutionError::StepLimit)
}

/// Guards a cyclic `Infer` chain; not a type-depth bound.
pub const MAX_DEPTH: u32 = 64;

pub type MonoSubst = FxHashMap<VarId, MonoTy>;

/// A declaration has no inference arena: a `TyId::Local` or `Infer` reached
/// from one is compiler state disagreeing with itself.
#[derive(Clone, Copy)]
pub enum TyEnv<'a> {
    Body(&'a BodyInference),
    Decl,
}

impl<'a> TyEnv<'a> {
    pub fn decl_node(self, ws: &Workspace, ty: TyId) -> Result<TypeExpr, MonoTyReason> {
        self.node(ws, ty)
    }

    fn node(self, ws: &Workspace, ty: TyId) -> Result<TypeExpr, MonoTyReason> {
        match (ty, self) {
            (TyId::Static(mid, idx), _) => ws
                .modules
                .get(mid.index())
                .filter(|m| (idx.into_raw().into_u32() as usize) < m.ty_arena.len())
                .map(|m| m.ty_arena[idx])
                .ok_or(MonoTyReason::NoNode),
            (TyId::Local(idx), TyEnv::Body(inf)) => {
                // `Arena` indexes unchecked; bound it rather than panic.
                if idx.into_raw().into_u32() as usize >= inf.ty_arena.len() {
                    return Err(MonoTyReason::NoNode);
                }
                Ok(inf.ty_arena[idx])
            }
            (TyId::Local(_), TyEnv::Decl) => Err(MonoTyReason::NotADeclarationType),
        }
    }

    /// An open slot is not an error: a caller's substitution may still answer it.
    fn solved(self, var: VarId) -> Result<Option<TyId>, MonoTyReason> {
        match self {
            TyEnv::Body(inf) => Ok(inf.substitutions.get(var)),
            TyEnv::Decl => Err(MonoTyReason::NotADeclarationType),
        }
    }
}

/// The declaration is the authority on a generic's shape, never the body's
/// own inference.
pub fn generic_var_of(ws: &Workspace, inf: &BodyInference, ty: TyId) -> Option<VarId> {
    match crate::lower::ty_node(ws, inf, ty)? {
        TypeExpr::Generic(var, _) => Some(var),
        _ => None,
    }
}

/// Does not chase `substitutions`: a solved slot is not a hole a caller may
/// fill, and binding it would overwrite the solution.
pub(crate) fn infer_var_of(inf: &BodyInference, ty: TyId) -> Option<VarId> {
    let idx = match ty {
        TyId::Local(idx) => idx,
        TyId::Static(..) => return None,
    };
    match inf.ty_arena[idx] {
        TypeExpr::Infer(var, _) => Some(var),
        _ => None,
    }
}

/// The one arena a whole build shares: a [`MonoTyId`] means nothing without
/// the arena that minted it. Monotonic and never pruned, so ids stay stable.
pub struct MonoInterner {
    /// Readers index it without a lock.
    types: crate::append_vec::AppendVec<MonoTy>,
    /// Only interning touches it.
    index: std::sync::RwLock<MonoIndex>,
    /// Keys the worker-local intern cache, so two live interners never serve
    /// each other's ids.
    arena_gen: u64,
    write_acqs: std::sync::atomic::AtomicU64,
    /// Each worker counts on a cache line of its own.
    shared_intern_hits: crate::striped::Striped<std::sync::atomic::AtomicU64>,
}

#[derive(Default)]
struct MonoIndex {
    interned: FxHashMap<MonoTy, MonoTyId>,
    intern_attempts: u64,
    intern_new: u64,
}

#[derive(Default)]
struct MonoInternCache {
    arena_gen: u64,
    /// The arena is monotonic, so an id learned once stays right.
    ids: FxHashMap<MonoTy, MonoTyId>,
}

thread_local! {
    static MONO_INTERN_CACHE: std::cell::RefCell<MonoInternCache> =
        std::cell::RefCell::new(MonoInternCache::default());
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MonoInternerStats {
    pub write_acqs: u64,
    pub intern_attempts: u64,
    pub intern_new: u64,
    pub types: u64,
}

impl Default for MonoInterner {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for MonoInterner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MonoInterner({} types)", self.len())
    }
}

impl MonoInterner {
    pub fn new() -> Self {
        static ARENA_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            types: crate::append_vec::AppendVec::new(),
            index: Default::default(),
            arena_gen: ARENA_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            write_acqs: std::sync::atomic::AtomicU64::new(0),
            shared_intern_hits: Default::default(),
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.arena_gen
    }

    pub fn len(&self) -> usize {
        self.types.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Takes the write lock only when `ty` is new.
    pub fn intern(&self, ty: MonoTy) -> MonoTyId {
        MONO_INTERN_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            if cache.arena_gen != self.arena_gen {
                cache.arena_gen = self.arena_gen;
                cache.ids.clear();
            }
            if let Some(id) = cache.ids.get(&ty) {
                self.shared_intern_hits
                    .local()
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return *id;
            }
            let found = self
                .index
                .read()
                .expect("mono arena poisoned")
                .interned
                .get(&ty)
                .copied();
            let id = match found {
                Some(id) => {
                    self.shared_intern_hits
                        .local()
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    id
                }
                None => {
                    self.write_acqs
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let mut index = self.index.write().expect("mono arena poisoned");
                    index.intern_attempts += 1;
                    match index.interned.get(&ty) {
                        Some(id) => *id,
                        None => {
                            index.intern_new += 1;
                            let id = MonoTyId(self.types.push(ty.clone()) as u32);
                            index.interned.insert(ty.clone(), id);
                            id
                        }
                    }
                }
            };
            cache.ids.insert(ty, id);
            id
        })
    }

    /// A type borrowed from this arena is found by address without interning.
    pub fn id_of(&self, ty: &MonoTy) -> MonoTyId {
        if let Some(index) = self.types.index_of(ty) {
            return MonoTyId(index as u32);
        }
        let cached = MONO_INTERN_CACHE.with(|cache| {
            let cache = cache.borrow();
            (cache.arena_gen == self.arena_gen)
                .then(|| cache.ids.get(ty).copied())
                .flatten()
        });
        cached.unwrap_or_else(|| self.intern(ty.clone()))
    }

    pub fn intern_all(&self, tys: impl IntoIterator<Item = MonoTy>) -> Vec<MonoTyId> {
        tys.into_iter().map(|ty| self.intern(ty)).collect()
    }

    pub fn lookup(&self, ty: &MonoTy) -> Option<MonoTyId> {
        self.index
            .read()
            .expect("mono arena poisoned")
            .interned
            .get(ty)
            .copied()
    }

    /// The conversion itself holds no lock.
    pub fn convert(
        &self,
        ws: &Workspace,
        inf: &BodyInference,
        ty: TyId,
        subst: &MonoSubst,
    ) -> Result<MonoTyId, MonoTyReason> {
        Ok(self.intern(convert(ws, inf, ty, subst)?))
    }

    pub fn convert_decl(
        &self,
        ws: &Workspace,
        ty: TyId,
        subst: &MonoSubst,
    ) -> Result<MonoTyId, MonoTyReason> {
        Ok(self.intern(convert_decl(ws, ty, subst)?))
    }

    pub fn iter(&self) -> impl Iterator<Item = &MonoTy> + '_ {
        self.types.iter()
    }

    /// Borrowed from the arena without a lock.
    pub fn get_ref(&self, id: MonoTyId) -> &MonoTy {
        &self.types[id.index()]
    }

    pub fn get(&self, id: MonoTyId) -> MonoTy {
        self.get_ref(id).clone()
    }

    pub fn get_all(&self, ids: &[MonoTyId]) -> Vec<MonoTy> {
        ids.iter().map(|id| self.get(*id)).collect()
    }

    /// Not an identity; see [`MonoTy::render`].
    pub fn render(&self, id: MonoTyId) -> String {
        self.get_ref(id).render()
    }

    pub fn canonical(&self, id: MonoTyId) -> String {
        self.get_ref(id).canonical()
    }

    pub fn stats(&self) -> MonoInternerStats {
        use std::sync::atomic::Ordering;
        let index = self.index.read().expect("mono arena poisoned");
        MonoInternerStats {
            write_acqs: self.write_acqs.load(Ordering::Relaxed),
            intern_attempts: index.intern_attempts
                + self
                    .shared_intern_hits
                    .iter()
                    .map(|hits| hits.load(Ordering::Relaxed))
                    .sum::<u64>(),
            intern_new: index.intern_new,
            types: self.len() as u64,
        }
    }
}

/// A generic absent from `subst` is [`MonoTyReason::UnboundGeneric`] rather
/// than a guess: only the caller knows whether that is expected.
pub fn convert<S: std::hash::BuildHasher>(
    ws: &Workspace,
    inf: &BodyInference,
    ty: TyId,
    subst: &std::collections::HashMap<VarId, MonoTy, S>,
) -> Result<MonoTy, MonoTyReason> {
    convert_at(ws, TyEnv::Body(inf), ty, subst, 0)
}

/// A declaration type has no body and so no inference arena.
pub fn convert_decl<S: std::hash::BuildHasher>(
    ws: &Workspace,
    ty: TyId,
    subst: &std::collections::HashMap<VarId, MonoTy, S>,
) -> Result<MonoTy, MonoTyReason> {
    convert_at(ws, TyEnv::Decl, ty, subst, 0)
}

fn convert_at<S: std::hash::BuildHasher>(
    ws: &Workspace,
    env: TyEnv<'_>,
    ty: TyId,
    subst: &std::collections::HashMap<VarId, MonoTy, S>,
    depth: u32,
) -> Result<MonoTy, MonoTyReason> {
    if depth > MAX_DEPTH {
        return Err(MonoTyReason::DepthLimit);
    }
    let node = env.node(ws, ty)?;
    Ok(match node {
        TypeExpr::Builtin(b) => match b {
            BuiltinTy::Int => MonoTy::Int,
            BuiltinTy::Uint => MonoTy::Uint,
            BuiltinTy::Float => MonoTy::Float,
            BuiltinTy::Bool => MonoTy::Bool,
            BuiltinTy::String => MonoTy::String,
            BuiltinTy::Any => MonoTy::Any,
            BuiltinTy::Unit => MonoTy::Unit,
            BuiltinTy::Never => MonoTy::Never,
            BuiltinTy::Error => return Err(MonoTyReason::RecoveryError),
            BuiltinTy::F32 => return Err(MonoTyReason::StorageOnly),
        },
        // An unsolved var the caller binds (struct-default slots). One map serves
        // both arms: a `VarId` is a declared generic or an inference var, never both.
        TypeExpr::Infer(var, name) => match env.solved(var)? {
            Some(next) => return convert_at(ws, env, next, subst, depth + 1),
            None => {
                let bound_to = subst
                    .get(&var)
                    .cloned()
                    .ok_or(MonoTyReason::UnresolvedInfer(name))?;
                return Ok(bound_to);
            }
        },
        TypeExpr::Generic(var, name) => {
            let bound_to = subst
                .get(&var)
                .cloned()
                .ok_or(MonoTyReason::UnboundGeneric(name))?;
            return Ok(bound_to);
        }
        TypeExpr::Nullable(inner) => {
            MonoTy::nullable(convert_at(ws, env, ty.with_idx(inner), subst, depth + 1)?)
        }
        TypeExpr::Tuple { elements } => {
            let mut parts = Vec::new();
            for el in elements {
                parts.push(convert_at(ws, env, ty.with_idx(el), subst, depth + 1)?);
            }
            MonoTy::Tuple(parts)
        }
        TypeExpr::App { ctor, params } => {
            let mut ps = Vec::new();
            for p in params {
                ps.push(convert_at(ws, env, ty.with_idx(p), subst, depth + 1)?);
            }
            let ctor = match ctor {
                TypeCtor::Array => MonoCtor::Array,
                TypeCtor::Map => MonoCtor::Map,
                TypeCtor::Ref => MonoCtor::Ref,
                TypeCtor::Nominal(n) => {
                    MonoCtor::Nominal(crate::layout_instance::layout_name(ws, n, &ps))
                }
            };
            MonoTy::App { ctor, params: ps }
        }
        TypeExpr::Function { params, ret } => {
            let mut ps = Vec::new();
            for p in params {
                ps.push(convert_at(ws, env, ty.with_idx(p), subst, depth + 1)?);
            }
            let ret = convert_at(ws, env, ty.with_idx(ret), subst, depth + 1)?;
            MonoTy::Function {
                params: ps,
                ret: Box::new(ret),
            }
        }
    })
}

/// Caps structural type depth so admissibility depends on the instance, not
/// the path by which graph traversal discovered it.
pub const MAX_INSTANTIATION_DEPTH: u32 = 12;

/// Emission rejects a too-deep instance: its call site already spelled the
/// specialized name, and skipping the body would ship a call to nothing.
pub fn too_deep_to_instantiate<'a>(
    args: impl IntoIterator<Item = &'a MonoTy>,
) -> Option<&'a MonoTy> {
    args.into_iter()
        .filter(|t| t.depth() > MAX_INSTANTIATION_DEPTH)
        .max_by_key(|t| t.depth())
}
