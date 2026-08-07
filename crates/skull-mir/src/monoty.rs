//! `MonoTy` -- THE logical type (`docs/monomorphization-plan.md` Stage 1). Every
//! concrete type identity in the compiler is one of these: `SpecKey` slots, the
//! substitutions specialized lowering reads, the symbols the emitters mangle,
//! and the instance graph.
//!
//! There is ONE of these walks, and that is the point: a second, narrower one
//! is free to answer `None` for a type it does not admit, and `None` is not a
//! type -- it is the absence of one, which every binding that lands in it
//! shares. `f<Vec2, unit>`, `f<Vec2, any>` and `f<Vec2, fn(int)->int>` are three
//! instantiations, and a partial walk gives them one identity and one emitted
//! body. So `SpecKey` slots are built from this conversion too
//! ([`reify_type`](crate::lower::shape::reify_type)): WHETHER a call
//! specializes is conditional (the `@inline`/`@pure` opt-in, the storage
//! trigger), WHICH instantiation it names is not. Stage 4 deletes the rest of
//! the policy.
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
//!    change the numeric ids, and nothing else. Cross-process identity is
//!    [`MonoTy::canonical`] / [`MonoTy::structural_hash`], never the integer --
//!    and never [`MonoTy::render`], which is a display spelling and ambiguous.
//!
//! The API takes NO target. Lua, JS and any later projection consume the same
//! arena; only ABI and layout may differ downstream.
//!
//! A [`MonoTy`] OWNS its children, so a type is self-contained and travels into
//! MIR, cross-crate caches and worklists with nothing to thread beside it.
//! [`MonoArena`] is an INDEX over that one tree -- it hands out [`MonoTyId`]s for
//! the instance graph's cheap equality, and never becomes a second
//! representation of a type.

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

/// The total logical type. Owns its children, so it needs no arena to be read,
/// compared, hashed or stored.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum MonoTy {
    Int,
    Float,
    Bool,
    String,
    Any,
    Unit,
    Never,
    Nullable(Box<MonoTy>),
    Tuple(Vec<MonoTy>),
    App { ctor: MonoCtor, params: Vec<MonoTy> },
    Function { params: Vec<MonoTy>, ret: Box<MonoTy> },
}

impl MonoTy {
    /// A readable spelling of the type, for manifests, censuses and messages.
    ///
    /// DISPLAY ONLY. This is a pretty type syntax, so it is NOT injective:
    /// `(fn(int) -> int)?` and `fn(int) -> int?` both spell
    /// `fn(int) -> int?`. Anything that compares, deduplicates, sets, maps or
    /// hashes types must use [`Self::canonical`].
    pub fn render(&self) -> String {
        let mut s = String::new();
        self.write(&mut s);
        s
    }

    fn write(&self, out: &mut String) {
        match self {
            MonoTy::Int => out.push_str("int"),
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
                    MonoCtor::Nominal(n) => out.push_str(n.as_str()),
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

    /// Structural nesting depth of a type: a leaf is 0, `[int]` is 1,
    /// `[[int]]` is 2. Polymorphic recursion grows this without bound
    /// (`f<T> -> f<[T]>`), which is why it -- and not a path length -- is the
    /// sound thing to cap an instantiation walk on.
    pub fn depth(&self) -> u32 {
        match self {
            MonoTy::Int
            | MonoTy::Float
            | MonoTy::Bool
            | MonoTy::String
            | MonoTy::Any
            | MonoTy::Unit
            | MonoTy::Never => 0,
            MonoTy::Nullable(inner) => 1 + inner.depth(),
            MonoTy::Tuple(parts) => 1 + parts.iter().map(MonoTy::depth).max().unwrap_or(0),
            MonoTy::App { params, .. } => {
                1 + params.iter().map(MonoTy::depth).max().unwrap_or(0)
            }
            MonoTy::Function { params, ret } => {
                let p = params.iter().map(MonoTy::depth).max().unwrap_or(0);
                1 + p.max(ret.depth())
            }
        }
    }

    /// The cross-process IDENTITY of a type: a tagged, length-prefixed,
    /// terminated encoding, unambiguous by construction.
    ///
    /// [`Self::render`] cannot serve this. A pretty type syntax is ambiguous
    /// wherever a postfix and an infix constructor meet -- `(fn(int) -> int)?`
    /// and `fn(int) -> int?` both read as "fn(int) -> int" then "?" -- and
    /// bracketing one case moves the collision to its neighbour rather than
    /// removing it. A tag per constructor, a length before every child list and
    /// a terminator after it leaves nothing to parse ambiguously, and a nominal
    /// carries its own length so a struct named `Vec` cannot alias the container
    /// ctor.
    ///
    /// This is also the encoding a specialized body's emitted symbol is built
    /// from ([`crate::mangle::spec_fn_name`]), so identity and symbol cannot
    /// drift apart.
    pub fn canonical(&self) -> String {
        let mut s = String::new();
        self.write_canonical(&mut s);
        s
    }

    pub(crate) fn write_canonical(&self, out: &mut String) {
        let prim = |out: &mut String, name: &str| {
            out.push_str("p_");
            out.push_str(name);
        };
        match self {
            MonoTy::Int => prim(out, "int"),
            MonoTy::Float => prim(out, "float"),
            MonoTy::Bool => prim(out, "bool"),
            MonoTy::String => prim(out, "string"),
            MonoTy::Any => prim(out, "any"),
            MonoTy::Unit => prim(out, "unit"),
            MonoTy::Never => prim(out, "never"),
            // `T = E?` is a DIFFERENT type from `T = E` (its slots carry a
            // presence leaf), so the encoding has to separate them or the two
            // collide on one symbol.
            MonoTy::Nullable(inner) => {
                out.push_str("o_");
                inner.write_canonical(out);
            }
            // Length-prefixed and terminated like `App`, so `(A, B)` cannot
            // encode the same as a one-slot tuple of something that encodes as
            // `A_B`.
            MonoTy::Tuple(parts) => {
                out.push_str("t_");
                out.push_str(&parts.len().to_string());
                for part in parts {
                    out.push('_');
                    part.write_canonical(out);
                }
                out.push_str("_z");
            }
            MonoTy::App { ctor, params } => {
                out.push_str("a_");
                match ctor {
                    MonoCtor::Array => out.push('v'),
                    MonoCtor::Map => out.push('m'),
                    MonoCtor::Nominal(name) => {
                        out.push('n');
                        out.push_str(&name.len().to_string());
                        out.push('_');
                        out.push_str(name.as_str());
                    }
                }
                out.push('_');
                out.push_str(&params.len().to_string());
                for param in params {
                    out.push('_');
                    param.write_canonical(out);
                }
                out.push_str("_z");
            }
            // The result is part of the identity, so it is encoded and
            // terminated like any other child rather than folded in with the
            // parameters.
            MonoTy::Function { params, ret } => {
                out.push_str("f_");
                out.push_str(&params.len().to_string());
                for param in params {
                    out.push('_');
                    param.write_canonical(out);
                }
                out.push('_');
                ret.write_canonical(out);
                out.push_str("_z");
            }
        }
    }

    /// FNV-1a over [`Self::canonical`]. Stable across processes, unlike
    /// anything derived from an arena id or from a `FxHashMap` iteration, and
    /// unambiguous, unlike anything derived from the display rendering.
    pub fn structural_hash(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self.canonical().bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    /// The bound NOMINAL, and `None` for a nullable binding on purpose: every
    /// caller of this asks "is this type exactly this struct", and a
    /// presence-carrying binding is not. The presence-aware callers peel with
    /// [`Self::peel_nullable`] and handle the flag explicitly.
    pub fn nominal(&self) -> Option<Ustr> {
        match self {
            Self::App { ctor: MonoCtor::Nominal(name), .. } => Some(*name),
            _ => None,
        }
    }

    /// The spelling of a PRIMITIVE leaf, and `None` for everything else --
    /// including `any`, unit and `never`, which are leaves of the type tree but
    /// name no primitive a ctype or a storage decision can be read off.
    pub fn prim_name(&self) -> Option<&'static str> {
        match self {
            Self::Int => Some("int"),
            Self::Float => Some("float"),
            Self::Bool => Some("bool"),
            Self::String => Some("string"),
            _ => None,
        }
    }

    /// `(payload, nullable)` -- the type with its `?` stripped.
    pub fn peel_nullable(&self) -> (&MonoTy, bool) {
        match self {
            Self::Nullable(inner) => (inner.as_ref(), true),
            other => (other, false),
        }
    }

    /// Whether any nominal anywhere in the tree satisfies `pred`.
    pub fn any_nominal(&self, pred: &impl Fn(Ustr) -> bool) -> bool {
        match self {
            Self::Int
            | Self::Float
            | Self::Bool
            | Self::String
            | Self::Any
            | Self::Unit
            | Self::Never => false,
            Self::Nullable(inner) => inner.any_nominal(pred),
            Self::Tuple(parts) => parts.iter().any(|part| part.any_nominal(pred)),
            Self::App { ctor, params } => {
                matches!(ctor, MonoCtor::Nominal(name) if pred(*name))
                    || params.iter().any(|param| param.any_nominal(pred))
            }
            Self::Function { params, ret } => {
                params.iter().any(|param| param.any_nominal(pred)) || ret.any_nominal(pred)
            }
        }
    }
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
        }
    }
}

/// A cyclic `Infer` chain would otherwise recurse forever. Not a type-depth
/// bound -- real types nest nowhere near this.
pub const MAX_DEPTH: u32 = 64;

/// Callee generic `VarId` -> its concrete logical type. An instance's own
/// bindings, and what specialized lowering reads as
/// [`crate::lower::SpecSubst`] -- one type, not two spellings of it.
pub type MonoSubst = FxHashMap<VarId, MonoTy>;

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

/// The `VarId` an INFERENCE variable's `TyId` names, read straight out of the
/// body's own arena.
///
/// The `Infer` twin of [`generic_var_of`], for the slots a struct's defaults
/// body publishes. Deliberately does NOT chase `substitutions`: a slot that
/// inference already solved is not a hole a caller may fill, and binding it
/// would overwrite the solution.
pub fn infer_var_of(inf: &BodyInference, ty: TyId) -> Option<VarId> {
    let idx = match ty {
        TyId::Local(idx) => idx,
        TyId::Static(..) => return None,
    };
    match inf.ty_arena[idx] {
        TypeExpr::Infer(var, _) => Some(var),
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

    /// The display rendering of an interned type. Not an identity -- see
    /// [`MonoTy::render`].
    pub fn render(&self, id: MonoTyId) -> String {
        self.get(id).render()
    }

    /// The canonical identity string of an interned type.
    pub fn canonical(&self, id: MonoTyId) -> String {
        self.get(id).canonical()
    }

    /// Structural nesting depth of an interned type.
    pub fn depth(&self, id: MonoTyId) -> u32 {
        self.get(id).depth()
    }

    /// FNV-1a over the canonical encoding of an interned type.
    pub fn structural_hash(&self, id: MonoTyId) -> u64 {
        self.get(id).structural_hash()
    }

    /// Intern `Nullable` over an already-interned child.
    pub fn nullable(&mut self, inner: MonoTyId) -> MonoTyId {
        let inner = self.get(inner).clone();
        self.intern(MonoTy::Nullable(Box::new(inner)))
    }

    /// Intern `Tuple` over already-interned parts.
    pub fn tuple(&mut self, parts: &[MonoTyId]) -> MonoTyId {
        let parts = self.cloned(parts);
        self.intern(MonoTy::Tuple(parts))
    }

    /// Intern `App` over already-interned params.
    pub fn app(&mut self, ctor: MonoCtor, params: &[MonoTyId]) -> MonoTyId {
        let params = self.cloned(params);
        self.intern(MonoTy::App { ctor, params })
    }

    /// Intern `Function` over already-interned params and result.
    pub fn function(&mut self, params: &[MonoTyId], ret: MonoTyId) -> MonoTyId {
        let params = self.cloned(params);
        let ret = self.get(ret).clone();
        self.intern(MonoTy::Function { params, ret: Box::new(ret) })
    }

    fn cloned(&self, ids: &[MonoTyId]) -> Vec<MonoTy> {
        ids.iter().map(|id| self.get(*id).clone()).collect()
    }

    /// [`convert`], interned. The instance graph keys on ids, so it takes the
    /// type through here; nothing else needs to.
    pub fn convert(
        &mut self,
        ws: &Workspace,
        inf: &BodyInference,
        ty: TyId,
        subst: &MonoSubst,
    ) -> Result<MonoTyId, MonoTyReason> {
        Ok(self.intern(convert(ws, inf, ty, subst)?))
    }

    /// [`convert`], interned, reporting how many declared generics the
    /// conversion resolved THROUGH `subst`.
    pub fn convert_counting(
        &mut self,
        ws: &Workspace,
        inf: &BodyInference,
        ty: TyId,
        subst: &MonoSubst,
        bound: &mut usize,
    ) -> Result<MonoTyId, MonoTyReason> {
        Ok(self.intern(convert_counting(ws, inf, ty, subst, bound)?))
    }
}

/// Convert an inferred `TyId` to its total logical type.
///
/// `subst` binds declared generics; a generic absent from it is
/// [`MonoTyReason::UnboundGeneric`] rather than a guess, because the caller is
/// the only one who knows whether that is expected (a shared generic body) or a
/// defect (a missing instantiation).
pub fn convert<S: std::hash::BuildHasher>(
    ws: &Workspace,
    inf: &BodyInference,
    ty: TyId,
    subst: &std::collections::HashMap<VarId, MonoTy, S>,
) -> Result<MonoTy, MonoTyReason> {
    convert_at(ws, inf, ty, subst, 0, &mut 0)
}

/// [`convert`], reporting how many declared generics it resolved THROUGH
/// `subst`.
///
/// A census over converted types cannot otherwise tell "every slot was already
/// concrete" from "the substitution supplied every generic": both report zero
/// failures, and only one of them exercises the substitution at all.
pub fn convert_counting<S: std::hash::BuildHasher>(
    ws: &Workspace,
    inf: &BodyInference,
    ty: TyId,
    subst: &std::collections::HashMap<VarId, MonoTy, S>,
    bound: &mut usize,
) -> Result<MonoTy, MonoTyReason> {
    convert_at(ws, inf, ty, subst, 0, bound)
}

fn convert_at<S: std::hash::BuildHasher>(
    ws: &Workspace,
    inf: &BodyInference,
    ty: TyId,
    subst: &std::collections::HashMap<VarId, MonoTy, S>,
    depth: u32,
    bound: &mut usize,
) -> Result<MonoTy, MonoTyReason> {
    if depth > MAX_DEPTH {
        return Err(MonoTyReason::DepthLimit);
    }
    let node = crate::lower::ty_node(ws, inf, ty).ok_or(MonoTyReason::NoNode)?;
    Ok(match node {
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
            // An unbound var the CALLER can answer. A struct's field default is
            // checked with the struct's declared generics replaced by fresh
            // inference vars, so the type an injection has to resolve is an
            // `Infer`, not a `Generic` -- the caller binds those slots from the
            // literal's arguments (`Builder::with_struct_default`).
            //
            // Safe to consult one map for both arms: `Gamma` mints inference
            // vars from `lower_var_high_water` up, so a `VarId` is a declared
            // generic or an inference var, never both.
            match inf.substitutions.get(var) {
                Some(next) => return convert_at(ws, inf, next, subst, depth + 1, bound),
                None => {
                    let bound_to =
                        subst.get(&var).cloned().ok_or(MonoTyReason::UnresolvedInfer(name))?;
                    *bound += 1;
                    return Ok(bound_to);
                }
            }
        }
        TypeExpr::Generic(var, name) => {
            let bound_to = subst.get(&var).cloned().ok_or(MonoTyReason::UnboundGeneric(name))?;
            *bound += 1;
            return Ok(bound_to);
        }
        TypeExpr::Nullable(inner) => {
            MonoTy::Nullable(Box::new(convert_at(ws, inf, ty.with_idx(inner), subst, depth + 1, bound)?))
        }
        TypeExpr::Tuple { elements } => {
            let mut parts = Vec::new();
            for el in elements {
                parts.push(convert_at(ws, inf, ty.with_idx(el), subst, depth + 1, bound)?);
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
                ps.push(convert_at(ws, inf, ty.with_idx(p), subst, depth + 1, bound)?);
            }
            MonoTy::App { ctor, params: ps }
        }
        TypeExpr::Function { params, ret } => {
            let mut ps = Vec::new();
            for p in params {
                ps.push(convert_at(ws, inf, ty.with_idx(p), subst, depth + 1, bound)?);
            }
            let ret = convert_at(ws, inf, ty.with_idx(ret), subst, depth + 1, bound)?;
            MonoTy::Function { params: ps, ret: Box::new(ret) }
        }
    })
}
