//! `MonoTy` -- the TOTAL logical type (`docs/monomorphization-plan.md` Stage 0c,
//! Stage 1). Stage-0 probe: nothing in production emission calls this. The
//! Stage-0e ablation does -- it is the discriminant for why a binding failed to
//! reify, which is a question only a TOTAL converter can answer.
//!
//! It exists because [`ReifiedType`](crate::lower::shape::ReifiedType) is
//! PARTIAL. That tree answers `None` for valid `any`, unit, never and function
//! types, and its `None` becomes a key slot rather than a rejection -- so
//! `f<Vec2, unit>`, `f<Vec2, any>` and `f<Vec2, fn(int)->int` share one
//! identity. `string` was added to it one observed symptom at a time; this is
//! the replacement rather than the next arm.
//!
//! Three properties, in the order they matter:
//!
//! 1. **Total.** Every valid inferred type converts. An inability to convert is
//!    always a REASON, never a silent hole ([`MonoTyReason`]).
//! 2. **Injective.** Two logical types share a [`MonoTyId`] iff they are
//!    genuinely type-equal. Interning on structural equality gives this by
//!    construction, and nominal identity is carried by name -- two nominals with
//!    identical leaves stay distinct.
//! 3. **Order-independent.** Arena insertion order and the process hash seed
//!    change the numeric ids, and nothing else. Manifests and symbols must read
//!    [`MonoArena::render`] / [`MonoArena::structural_hash`], never the integer.
//!
//! The API takes NO target. Lua, JS and any later projection consume the same
//! arena; only ABI and layout may differ downstream.

use bone_semantic::hir::{BuiltinTy, TypeExpr, TypeCtor, VarId};
use bone_semantic::inference::{BodyInference, Workspace};
use bone_semantic::prelude::TyId;
use rustc_hash::FxHashMap;
use ustr::Ustr;

/// Index into a [`MonoArena`]. Equality is type-equality WITHIN one arena; it
/// carries no meaning across arenas, which is what `render`/`structural_hash`
/// are for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct MonoTyId(u32);

impl MonoTyId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// A container/nominal constructor. `Nominal` keeps the declared NAME, which is
/// what makes two same-layout structs distinct types.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MonoCtor {
    Array,
    Map,
    Nominal(Ustr),
}

/// One node of the total logical type tree. Children are ids into the same
/// arena, so a node is small and comparison is shallow.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum MonoTy {
    Int,
    Float,
    Bool,
    String,
    Any,
    Unit,
    Never,
    Nullable(MonoTyId),
    Tuple(Vec<MonoTyId>),
    App { ctor: MonoCtor, params: Vec<MonoTyId> },
    Function { params: Vec<MonoTyId>, ret: MonoTyId },
}

/// Why a type did not convert. Every variant is a distinct census bucket --
/// `UnboundGeneric` in particular is DATA (it says the reader asked about a
/// shared generic body), not an error to fold into the others.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum MonoTyReason {
    /// `BuiltinTy::Error` -- inference recovery poison. An invalid program.
    RecoveryError,
    /// An inference variable with no substitution: inference did not finish.
    UnresolvedInfer(Ustr),
    /// A declared generic absent from the enclosing substitution. Expected
    /// wherever a shared generic body is read without an instantiation.
    UnboundGeneric(Ustr),
    /// The `TyId` names no node at all.
    NoNode,
    /// Resolution exceeded [`MAX_DEPTH`] -- a cyclic substitution chain.
    DepthLimit,
    /// A body-local type or an unsolved inference variable reached from a
    /// DECLARATION site, which has no body and therefore no inference arena to
    /// resolve one in. A declared signature lives in its own module's arena, so
    /// this is compiler state disagreeing with itself, never a source shape.
    NotADeclarationType,
}

impl MonoTyReason {
    /// Stable bucket name for census output. Deliberately drops the payload so
    /// counts aggregate.
    pub fn bucket(&self) -> &'static str {
        match self {
            Self::RecoveryError => "recovery-error",
            Self::UnresolvedInfer(_) => "unresolved-infer",
            Self::UnboundGeneric(_) => "unbound-generic",
            Self::NoNode => "no-node",
            Self::DepthLimit => "depth-limit",
            Self::NotADeclarationType => "not-a-declaration-type",
        }
    }
}

/// A cyclic `Infer` chain would otherwise recurse forever. Not a type-depth
/// bound -- real types nest nowhere near this.
pub const MAX_DEPTH: u32 = 64;

/// Callee generic `VarId` -> its concrete logical type. The `SpecSubst` analog,
/// over total types.
pub type MonoSubst = FxHashMap<VarId, MonoTyId>;

/// Where a `TyId`'s node is read from.
///
/// A BODY has an inference arena for `TyId::Local` and solved substitutions for
/// `TypeExpr::Infer`. A DECLARATION has neither: a signature's types live in
/// their own module's arena, so either shape reaching [`MonoArena::convert_in`]
/// from a declaration is compiler state disagreeing with itself.
///
/// This is what lets one converter serve both. The alternative -- a second
/// normalization walk for declaration types -- is how the extern-return
/// classifier in `skull::mir_emit` started, and it grew its own partial
/// substitution and its own cycle key before converging back here.
#[derive(Clone, Copy)]
pub enum TyEnv<'a> {
    Body(&'a BodyInference),
    Decl,
}

impl<'a> TyEnv<'a> {
    /// The node a declared `TyId` names, for a caller that needs the type
    /// EXPRESSION rather than its logical type -- reading a declaration's own
    /// generic parameters, say. Same fail-closed resolution the converter uses.
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

    fn solved(self, var: VarId, name: Ustr) -> Result<TyId, MonoTyReason> {
        match self {
            TyEnv::Body(inf) => {
                inf.substitutions.get(var).ok_or(MonoTyReason::UnresolvedInfer(name))
            }
            TyEnv::Decl => Err(MonoTyReason::NotADeclarationType),
        }
    }
}

/// The `VarId` a declared generic's `TyId` names, for building a [`MonoSubst`]
/// against a callee's `Function::generics`. `None` for anything else -- the
/// declaration is the authority on a generic's shape, never the body's own
/// inference.
pub fn generic_var_of(ws: &Workspace, inf: &BodyInference, ty: TyId) -> Option<VarId> {
    match crate::lower::ty_node(ws, inf, ty)? {
        TypeExpr::Generic(var, _) => Some(var),
        _ => None,
    }
}

/// Interning arena. Structural equality is the interning key, so
/// `a == b` iff the two types are equal.
#[derive(Default, Debug)]
pub struct MonoArena {
    tys: Vec<MonoTy>,
    interned: FxHashMap<MonoTy, MonoTyId>,
}

impl MonoArena {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.tys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tys.is_empty()
    }

    pub fn get(&self, id: MonoTyId) -> &MonoTy {
        &self.tys[id.index()]
    }

    pub fn intern(&mut self, ty: MonoTy) -> MonoTyId {
        if let Some(id) = self.interned.get(&ty) {
            return *id;
        }
        let id = MonoTyId(self.tys.len() as u32);
        self.tys.push(ty.clone());
        self.interned.insert(ty, id);
        id
    }

    /// The canonical structural rendering -- the cross-arena identity. Two
    /// arenas that built the same type in different orders render it the same.
    pub fn render(&self, id: MonoTyId) -> String {
        let mut s = String::new();
        self.write(id, &mut s);
        s
    }

    fn write(&self, id: MonoTyId, out: &mut String) {
        match self.get(id) {
            MonoTy::Int => out.push_str("int"),
            MonoTy::Float => out.push_str("float"),
            MonoTy::Bool => out.push_str("bool"),
            MonoTy::String => out.push_str("string"),
            MonoTy::Any => out.push_str("any"),
            MonoTy::Unit => out.push_str("unit"),
            MonoTy::Never => out.push_str("never"),
            MonoTy::Nullable(inner) => {
                self.write(*inner, out);
                out.push('?');
            }
            MonoTy::Tuple(parts) => {
                out.push('(');
                for (i, p) in parts.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    self.write(*p, out);
                }
                out.push(')');
            }
            MonoTy::App { ctor, params } => {
                match ctor {
                    MonoCtor::Array => out.push_str("Vec"),
                    MonoCtor::Map => out.push_str("Map"),
                    MonoCtor::Nominal(n) => out.push_str(n.as_str()),
                }
                if !params.is_empty() {
                    out.push('<');
                    for (i, p) in params.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        self.write(*p, out);
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
                    self.write(*p, out);
                }
                out.push_str(") -> ");
                self.write(*ret, out);
            }
        }
    }

    /// Structural nesting depth of a type: a leaf is 0, `[int]` is 1,
    /// `[[int]]` is 2. Polymorphic recursion grows this without bound
    /// (`f<T> -> f<[T]>`), which is why it -- and not a path length -- is the
    /// sound thing to cap an instantiation walk on.
    pub fn depth(&self, id: MonoTyId) -> u32 {
        match self.get(id) {
            MonoTy::Int
            | MonoTy::Float
            | MonoTy::Bool
            | MonoTy::String
            | MonoTy::Any
            | MonoTy::Unit
            | MonoTy::Never => 0,
            MonoTy::Nullable(inner) => 1 + self.depth(*inner),
            MonoTy::Tuple(parts) => {
                1 + parts.iter().map(|p| self.depth(*p)).max().unwrap_or(0)
            }
            MonoTy::App { params, .. } => {
                1 + params.iter().map(|p| self.depth(*p)).max().unwrap_or(0)
            }
            MonoTy::Function { params, ret } => {
                let p = params.iter().map(|p| self.depth(*p)).max().unwrap_or(0);
                1 + p.max(self.depth(*ret))
            }
        }
    }

    /// FNV-1a over the canonical rendering. Stable across processes and across
    /// insertion orders, unlike anything derived from the integer id or from a
    /// `FxHashMap` iteration.
    pub fn structural_hash(&self, id: MonoTyId) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self.render(id).bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    /// Lift an already-reified binding into the arena.
    ///
    /// Total in this direction by construction: `ReifiedType` is a strict subset
    /// of `MonoTy`. It exists so a classification of why `reify_type` FAILED can
    /// run under a substitution faithful to the one `reify_type` consulted --
    /// without it, an enclosing generic that reifies fine would come back
    /// `UnboundGeneric` and a `(T, unit)` binding would be blamed on `T`.
    pub fn intern_reified(&mut self, t: &crate::lower::ReifiedType) -> MonoTyId {
        use crate::lower::{ReifiedCtor, ReifiedType};
        let mono = match t {
            ReifiedType::Prim("int") => MonoTy::Int,
            ReifiedType::Prim("float") => MonoTy::Float,
            ReifiedType::Prim("bool") => MonoTy::Bool,
            ReifiedType::Prim("string") => MonoTy::String,
            // `reify_type` mints exactly the four above; a fifth would be a new
            // arm on the partial tree, which plan S 19b forbids.
            ReifiedType::Prim(other) => MonoTy::App {
                ctor: MonoCtor::Nominal(Ustr::from(other)),
                params: Vec::new(),
            },
            ReifiedType::Nullable(inner) => MonoTy::Nullable(self.intern_reified(inner)),
            ReifiedType::Tuple(parts) => {
                MonoTy::Tuple(parts.iter().map(|p| self.intern_reified(p)).collect())
            }
            ReifiedType::App { ctor, params } => MonoTy::App {
                ctor: match ctor {
                    ReifiedCtor::Array => MonoCtor::Array,
                    ReifiedCtor::Map => MonoCtor::Map,
                    ReifiedCtor::Nominal(n) => MonoCtor::Nominal(*n),
                },
                params: params.iter().map(|p| self.intern_reified(p)).collect(),
            },
        };
        self.intern(mono)
    }

    /// Convert an inferred `TyId` to its total logical type.
    ///
    /// `subst` binds declared generics; a generic absent from it is
    /// [`MonoTyReason::UnboundGeneric`] rather than a guess, because the caller
    /// is the only one who knows whether that is expected (a shared generic
    /// body) or a defect (a missing instantiation).
    pub fn convert(
        &mut self,
        ws: &Workspace,
        inf: &BodyInference,
        ty: TyId,
        subst: &MonoSubst,
    ) -> Result<MonoTyId, MonoTyReason> {
        self.convert_at(ws, TyEnv::Body(inf), ty, subst, 0)
    }

    /// Convert a type written in a DECLARATION -- a signature, a `newtype`'s
    /// representation -- which has no body and so no inference arena.
    pub fn convert_decl(
        &mut self,
        ws: &Workspace,
        ty: TyId,
        subst: &MonoSubst,
    ) -> Result<MonoTyId, MonoTyReason> {
        self.convert_at(ws, TyEnv::Decl, ty, subst, 0)
    }

    pub fn convert_in(
        &mut self,
        ws: &Workspace,
        env: TyEnv<'_>,
        ty: TyId,
        subst: &MonoSubst,
    ) -> Result<MonoTyId, MonoTyReason> {
        self.convert_at(ws, env, ty, subst, 0)
    }

    fn convert_at(
        &mut self,
        ws: &Workspace,
        env: TyEnv<'_>,
        ty: TyId,
        subst: &MonoSubst,
        depth: u32,
    ) -> Result<MonoTyId, MonoTyReason> {
        if depth > MAX_DEPTH {
            return Err(MonoTyReason::DepthLimit);
        }
        let node = env.node(ws, ty)?;
        let mono = match node {
            TypeExpr::Builtin(b) => match b {
                BuiltinTy::Int => MonoTy::Int,
                BuiltinTy::Float => MonoTy::Float,
                BuiltinTy::Bool => MonoTy::Bool,
                BuiltinTy::String => MonoTy::String,
                BuiltinTy::Any => MonoTy::Any,
                BuiltinTy::Unit => MonoTy::Unit,
                BuiltinTy::Never => MonoTy::Never,
                BuiltinTy::Error => return Err(MonoTyReason::RecoveryError),
            },
            TypeExpr::Infer(var, name) => {
                let next = env.solved(var, name)?;
                return self.convert_at(ws, env, next, subst, depth + 1);
            }
            TypeExpr::Generic(var, name) => {
                return subst.get(&var).copied().ok_or(MonoTyReason::UnboundGeneric(name));
            }
            TypeExpr::Nullable(inner) => {
                let inner = self.convert_at(ws, env, ty.with_idx(inner), subst, depth + 1)?;
                MonoTy::Nullable(inner)
            }
            TypeExpr::Tuple { elements } => {
                let mut parts = Vec::new();
                for el in elements {
                    parts.push(self.convert_at(ws, env, ty.with_idx(el), subst, depth + 1)?);
                }
                MonoTy::Tuple(parts)
            }
            TypeExpr::App { ctor, params } => {
                let ctor = match ctor {
                    TypeCtor::Array => MonoCtor::Array,
                    TypeCtor::Map => MonoCtor::Map,
                    TypeCtor::Nominal(n) => MonoCtor::Nominal(n),
                };
                let mut ps = Vec::new();
                for p in params {
                    ps.push(self.convert_at(ws, env, ty.with_idx(p), subst, depth + 1)?);
                }
                MonoTy::App { ctor, params: ps }
            }
            TypeExpr::Function { params, ret } => {
                let mut ps = Vec::new();
                for p in params {
                    ps.push(self.convert_at(ws, env, ty.with_idx(p), subst, depth + 1)?);
                }
                let ret = self.convert_at(ws, env, ty.with_idx(ret), subst, depth + 1)?;
                MonoTy::Function { params: ps, ret }
            }
        };
        Ok(self.intern(mono))
    }
}
