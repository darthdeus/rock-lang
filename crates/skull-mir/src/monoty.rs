//! `MonoTy` -- THE logical type. Every
//! concrete type identity in the compiler is one of these: an `Instance`'s
//! arguments, the
//! substitutions specialized lowering reads, the symbols the emitters mangle,
//! and the instance graph.
//!
//! There is ONE logical-type conversion, and that is the point: a second,
//! narrower walk is free to answer `None` for a type it does not admit, and
//! `None` is not a type -- it is the absence of one, which every binding that
//! lands in it shares. `f<Vec2, unit>`, `f<Vec2, any>` and
//! `f<Vec2, fn(int)->int>` are three instantiations. Instance arguments and
//! concrete lowering substitutions therefore use this conversion directly;
//! representation and optimization policy are decided later by target plans.
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

use bone_core::hir::{BuiltinTy, TypeCtor, TypeExpr, VarId};
use bone_core::prelude::TyId;
use bone_core::workspace::{BodyInference, Workspace};
use rustc_hash::FxHashMap;
use std::rc::Rc;
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
    Ref,
    Nominal(Ustr),
}

/// The total logical type. Owns its children, so it needs no arena to be read,
/// compared, hashed or stored.
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

/// How a nominal constructor's name is spelled inside a canonical encoding.
///
/// A v1 nominal's name is its DefKey (`<package>::<module path>::<Name>`).
/// That IS the type's stable identity, and it is also the text a specialized
/// body's emitted SYMBOL is built from -- two roles for one string, and a
/// DefKey is not an identifier in any backend's grammar. One traversal serves
/// both; this is the only thing the two spellings may disagree about, so a
/// symbol and an identity can differ about a type's NAME but never about its
/// shape.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NominalSpelling {
    /// The DefKey, for a stable identity.
    Key,
    /// The DefKey's identifier form (`<Name>_<digest>`), for an emitted symbol.
    /// A legacy key has no prefix and passes through unchanged, so legacy
    /// symbols are byte-identical under either spelling.
    Symbol,
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
    /// drift apart -- one traversal, with [`NominalSpelling`] the only thing
    /// the two roles disagree about.
    pub fn canonical(&self) -> String {
        let mut s = String::new();
        self.write_canonical(&mut s);
        s
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
            // `T = E?` is a DIFFERENT type from `T = E` (its slots carry a
            // presence leaf), so the encoding has to separate them or the two
            // collide on one symbol.
            MonoTy::Nullable(inner) => {
                out.push_str("o_");
                inner.write_canonical_as(out, spelling);
            }
            // Length-prefixed and terminated like `App`, so `(A, B)` cannot
            // encode the same as a one-slot tuple of something that encodes as
            // `A_B`.
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
                        // A v1 nominal's name is its DefKey. The length prefix
                        // measures the SPELLED text, so the encoding stays
                        // self-delimiting under either spelling.
                        let spelled = match spelling {
                            NominalSpelling::Key => std::borrow::Cow::Borrowed(name.as_str()),
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
            // The result is part of the identity, so it is encoded and
            // terminated like any other child rather than folded in with the
            // parameters.
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
            Self::App {
                ctor: MonoCtor::Nominal(name),
                ..
            } => Some(*name),
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
}

/// Why a type did not convert. Every variant is a distinct census bucket --
/// `UnboundGeneric` in particular is DATA (it says the reader asked about an
/// uninstantiated generic context), not an error to fold into the others.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum MonoTyReason {
    /// `BuiltinTy::Error` -- inference recovery poison. An invalid program.
    RecoveryError,
    /// `BuiltinTy::F32` -- a field's storage type, which no value has.
    StorageOnly,
    /// An inference variable with no substitution: inference did not finish.
    UnresolvedInfer(Ustr),
    /// A declared generic absent from the enclosing substitution. Expected in
    /// source-shaped generic contexts that have not selected an instance.
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
    /// The reason in words, for a diagnostic.
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

/// Resolve the declared representation through newtypes using interned type
/// instantiations as the cycle key. Generic arguments are converted in their
/// current environment before they are bound into the next declaration.
pub fn resolve_newtype_chain(ws: &Workspace, ty: MonoTy) -> Result<MonoTy, NewtypeResolutionError> {
    use bone_core::hir::TypeExpr;

    const MAX_STEPS: usize = 256;

    let mut arena = MonoArena::new();
    let mut current = arena.intern(ty);
    let mut visited: Vec<MonoTyId> = Vec::new();

    for _ in 0..MAX_STEPS {
        let (name, params) = match arena.get(current) {
            MonoTy::App {
                ctor: MonoCtor::Nominal(name),
                params,
            } => (*name, params.clone()),
            _ => return Ok(arena.get(current).clone()),
        };
        let Some(nid) = ws.newtypes_by_name.get(&name).copied() else {
            return Ok(arena.get(current).clone());
        };
        if visited.contains(&current) {
            return Err(NewtypeResolutionError::Cycle(name));
        }
        visited.push(current);

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
            subst.insert(var, arg);
        }
        current = arena
            .convert_decl(ws, def.wrapped, &subst)
            .map_err(NewtypeResolutionError::Conversion)?;
    }

    Err(NewtypeResolutionError::StepLimit)
}

/// A cyclic `Infer` chain would otherwise recurse forever. Not a type-depth
/// bound -- real types nest nowhere near this.
pub const MAX_DEPTH: u32 = 64;

/// Callee generic `VarId` -> its concrete logical type. An instance's own
/// bindings, and what specialized lowering reads as
/// [`crate::lower::SpecSubst`] -- one type, not two spellings of it.
pub type MonoSubst = FxHashMap<VarId, MonoTy>;

/// Where a `TyId`'s node is read from.
///
/// A BODY has an inference arena for `TyId::Local` and solved substitutions for
/// `TypeExpr::Infer`. A DECLARATION has neither: a signature's types live in
/// their own module's arena, so either shape reaching [`MonoArena::convert`]
/// from a declaration is compiler state disagreeing with itself. Declared
/// newtype resolution uses this converter for each instantiated step as well.
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

    /// The type inference solved `var` to, `None` when it left the slot open.
    /// An open slot is not an error here -- a caller's substitution may still
    /// answer it (see the `Infer` arm of `convert_at`).
    fn solved(self, var: VarId) -> Result<Option<TyId>, MonoTyReason> {
        match self {
            TyEnv::Body(inf) => Ok(inf.substitutions.get(var)),
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

/// The `VarId` an INFERENCE variable's `TyId` names, read straight out of the
/// body's own arena.
///
/// The `Infer` twin of [`generic_var_of`], for the slots a struct's defaults
/// body publishes. Deliberately does NOT chase `substitutions`: a slot that
/// inference already solved is not a hole a caller may fill, and binding it
/// would overwrite the solution.
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

/// Interning arena. Structural equality is the interning key, so
/// `a == b` iff the two types are equal.
#[derive(Default, Debug)]
pub struct MonoArena {
    tys: Vec<MonoTy>,
    interned: FxHashMap<MonoTy, MonoTyId>,
    intern_attempts: u64,
    intern_new: u64,
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

    /// The id `ty` already has, without interning it.
    pub fn lookup(&self, ty: &MonoTy) -> Option<MonoTyId> {
        self.interned.get(ty).copied()
    }

    pub fn intern(&mut self, ty: MonoTy) -> MonoTyId {
        self.intern_attempts += 1;
        if let Some(id) = self.interned.get(&ty) {
            return *id;
        }
        self.intern_new += 1;
        let id = MonoTyId(self.tys.len() as u32);
        self.tys.push(ty.clone());
        self.interned.insert(ty, id);
        id
    }

    /// `(attempts, new entries)` across this arena's lifetime, whichever path
    /// reached [`Self::intern`] -- the graph's direct walk and the shared
    /// interner handle alike.
    pub(crate) fn intern_stats(&self) -> (u64, u64) {
        (self.intern_attempts, self.intern_new)
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
        self.intern(MonoTy::Function {
            params,
            ret: Box::new(ret),
        })
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

    /// [`convert_decl`], interned.
    pub fn convert_decl(
        &mut self,
        ws: &Workspace,
        ty: TyId,
        subst: &MonoSubst,
    ) -> Result<MonoTyId, MonoTyReason> {
        Ok(self.intern(convert_decl(ws, ty, subst)?))
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

/// **The one arena a whole build shares**, behind the interior mutability that
/// sharing needs.
///
/// A [`MonoTyId`] means nothing without the arena that minted it, so an
/// [`Instance`](crate::instance::Instance) is only an identity while one arena
/// is reachable from everything that mints or reads one. Body lowering fans out
/// across threads ([`par_map`](../../skull/src/mir_emit/par.rs)) and every body may
/// name a callee instantiation, so the arena has to be shareable rather than
/// owned by one walk -- and giving each walk its own would mint the second
/// identity this stage exists to remove.
///
/// Reads happen throughout parallel lowering, while interning is confined to
/// graph construction and the occasional newly discovered call target. Keep
/// those populations apart so concrete-plan reads do not serialize on writes.
///
/// The arena is MONOTONIC and is never pruned. An entry outliving its last
/// instance is what keeps ids stable across incremental edits, which is the
/// property `InstanceGraph::update_bodies` rests on.
pub struct MonoInterner {
    arena: std::sync::RwLock<MonoArena>,
    /// Process-unique id keying the worker-local read caches: the arena is
    /// monotonic, so a thread may keep private copies of ids it has already
    /// read; the generation keeps two live interners (Lua and shader contexts)
    /// from serving each other's types.
    arena_gen: u64,
    read_acqs: std::sync::atomic::AtomicU64,
    local_read_hits: std::sync::atomic::AtomicU64,
    write_acqs: std::sync::atomic::AtomicU64,
    /// Interns answered without the write lock, from the worker-local cache
    /// or a shared read; the arena's own attempt counter sees only the
    /// write path.
    shared_intern_hits: std::sync::atomic::AtomicU64,
}

#[derive(Default)]
struct MonoReadCache {
    arena_gen: u64,
    tys: Vec<Option<Rc<MonoTy>>>,
    /// Types this worker has interned before, so a repeat intern takes no
    /// lock and touches no shared counter; the arena is monotonic, so an id
    /// learned once stays right for the generation.
    ids: FxHashMap<MonoTy, MonoTyId>,
}

impl MonoReadCache {
    fn reset_for(&mut self, arena_gen: u64) {
        if self.arena_gen != arena_gen {
            self.arena_gen = arena_gen;
            self.tys.clear();
            self.ids.clear();
        }
    }

    fn record(&mut self, id: MonoTyId, ty: Rc<MonoTy>) {
        let idx = id.index();
        if self.tys.len() <= idx {
            self.tys.resize(idx + 1, None);
        }
        self.tys[idx] = Some(ty);
    }
}

thread_local! {
    static MONO_READ_CACHE: std::cell::RefCell<MonoReadCache> =
        std::cell::RefCell::new(MonoReadCache::default());
}

/// Lifetime traffic counters for one [`MonoInterner`]: lock acquisitions by
/// kind, plus the inner arena's intern attempt/new-entry split. Reads clone
/// owned [`MonoTy`] values out, so `read_acqs` bounds the population a frozen
/// indexed view would serve borrow-free.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MonoInternerStats {
    pub read_acqs: u64,
    pub local_read_hits: u64,
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
        Self::from_arena(MonoArena::new())
    }

    /// Adopt an arena an [`InstanceGraph`](../../skull/src/instance_graph.rs)
    /// build already filled. The graph and emission share ONE arena; a second
    /// one would give the same type two ids and the same body two identities.
    pub(crate) fn from_arena(arena: MonoArena) -> Self {
        static ARENA_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            arena: std::sync::RwLock::new(arena),
            arena_gen: ARENA_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            read_acqs: std::sync::atomic::AtomicU64::new(0),
            local_read_hits: std::sync::atomic::AtomicU64::new(0),
            write_acqs: std::sync::atomic::AtomicU64::new(0),
            shared_intern_hits: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn len(&self) -> usize {
        self.with(MonoArena::len)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Intern `ty`, taking the write lock only when it is new. Almost every
    /// intern during a build names a type that is already there (the graph
    /// walk and the concrete-plan conversion run on rayon workers and keep
    /// meeting the same types), so a shared read first keeps those workers
    /// from serializing on the one arena.
    pub fn intern(&self, ty: MonoTy) -> MonoTyId {
        MONO_READ_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache.reset_for(self.arena_gen);
            if let Some(id) = cache.ids.get(&ty) {
                self.shared_intern_hits
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return *id;
            }
            let id = match self.with(|a| a.lookup(&ty)) {
                Some(id) => {
                    self.shared_intern_hits
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    id
                }
                None => {
                    self.write_acqs
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    self.arena
                        .write()
                        .expect("mono arena poisoned")
                        .intern(ty.clone())
                }
            };
            cache.ids.insert(ty, id);
            id
        })
    }

    /// [`Self::intern`] over a whole argument list, which is how an
    /// instance's arguments arrive.
    pub fn intern_all(&self, tys: impl IntoIterator<Item = MonoTy>) -> Vec<MonoTyId> {
        tys.into_iter().map(|ty| self.intern(ty)).collect()
    }

    /// [`MonoArena::convert`] through [`Self::intern`]: the conversion itself
    /// holds no lock.
    pub fn convert(
        &self,
        ws: &Workspace,
        inf: &BodyInference,
        ty: TyId,
        subst: &MonoSubst,
    ) -> Result<MonoTyId, MonoTyReason> {
        Ok(self.intern(convert(ws, inf, ty, subst)?))
    }

    /// [`MonoArena::convert_decl`] through [`Self::intern`].
    pub fn convert_decl(
        &self,
        ws: &Workspace,
        ty: TyId,
        subst: &MonoSubst,
    ) -> Result<MonoTyId, MonoTyReason> {
        Ok(self.intern(convert_decl(ws, ty, subst)?))
    }

    /// [`MonoArena::convert_counting`] through [`Self::intern`].
    pub fn convert_counting(
        &self,
        ws: &Workspace,
        inf: &BodyInference,
        ty: TyId,
        subst: &MonoSubst,
        bound: &mut usize,
    ) -> Result<MonoTyId, MonoTyReason> {
        Ok(self.intern(convert_counting(ws, inf, ty, subst, bound)?))
    }

    /// The type an id names. Returns an OWNED [`MonoTy`] because the arena is
    /// behind a lock; a [`MonoTy`] owns its children, so this is the same
    /// self-contained value every other consumer already passes around.
    /// Served from the worker-local cache when this thread has read `id`
    /// before; a cold id reads through to the lock.
    pub fn get(&self, id: MonoTyId) -> MonoTy {
        (*self.get_shared(id)).clone()
    }

    /// Read an immutable worker-local handle to an interned type. The handle
    /// borrows no arena lock and is valid only within the current worker's
    /// synchronous lowering work; the interner generation resets the cache
    /// before another arena can be observed.
    pub(crate) fn get_shared(&self, id: MonoTyId) -> Rc<MonoTy> {
        self.get_shared_with_status(id).0
    }

    /// As [`Self::get_shared`], also reporting whether this worker had to
    /// materialize the type from the authoritative arena.
    pub(crate) fn get_shared_with_status(&self, id: MonoTyId) -> (Rc<MonoTy>, bool) {
        MONO_READ_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache.reset_for(self.arena_gen);
            if let Some(Some(ty)) = cache.tys.get(id.index()) {
                self.local_read_hits
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return (Rc::clone(ty), false);
            }
            let ty = Rc::new(self.with(|a| a.get(id).clone()));
            cache.record(id, Rc::clone(&ty));
            (ty, true)
        })
    }

    /// The counterpart to [`Self::intern_all`], for reading an instance's
    /// arguments back out as the types they are.
    pub fn get_all(&self, ids: &[MonoTyId]) -> Vec<MonoTy> {
        ids.iter().map(|id| self.get(*id)).collect()
    }

    /// The display rendering of an interned type. Not an identity -- see
    /// [`MonoTy::render`].
    pub fn render(&self, id: MonoTyId) -> String {
        self.with(|a| a.render(id))
    }

    /// The canonical identity string of an interned type.
    pub fn canonical(&self, id: MonoTyId) -> String {
        self.with(|a| a.canonical(id))
    }

    /// Read-only access under one lock, for a caller doing several reads.
    pub fn with<R>(&self, f: impl FnOnce(&MonoArena) -> R) -> R {
        self.read_acqs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        f(&self.arena.read().expect("mono arena poisoned"))
    }

    pub fn stats(&self) -> MonoInternerStats {
        use std::sync::atomic::Ordering;
        let (intern_attempts, intern_new, types) = self.with(|a| {
            let (at, new) = a.intern_stats();
            (at, new, a.len() as u64)
        });
        let intern_attempts = intern_attempts + self.shared_intern_hits.load(Ordering::Relaxed);
        MonoInternerStats {
            // Subtract the read this snapshot itself just took.
            read_acqs: self.read_acqs.load(Ordering::Relaxed).saturating_sub(1),
            local_read_hits: self.local_read_hits.load(Ordering::Relaxed),
            write_acqs: self.write_acqs.load(Ordering::Relaxed),
            intern_attempts,
            intern_new,
            types,
        }
    }
}

/// Convert an inferred `TyId` to its total logical type.
///
/// `subst` binds declared generics; a generic absent from it is
/// [`MonoTyReason::UnboundGeneric`] rather than a guess, because the caller is
/// the only one who knows whether that is expected (an uninstantiated generic
/// context) or a defect (a missing instantiation).
pub fn convert<S: std::hash::BuildHasher>(
    ws: &Workspace,
    inf: &BodyInference,
    ty: TyId,
    subst: &std::collections::HashMap<VarId, MonoTy, S>,
) -> Result<MonoTy, MonoTyReason> {
    convert_at(ws, TyEnv::Body(inf), ty, subst, 0, &mut 0)
}

/// Convert a type written in a DECLARATION -- a signature, a `newtype`'s
/// representation -- which has no body and so no inference arena.
pub fn convert_decl<S: std::hash::BuildHasher>(
    ws: &Workspace,
    ty: TyId,
    subst: &std::collections::HashMap<VarId, MonoTy, S>,
) -> Result<MonoTy, MonoTyReason> {
    convert_at(ws, TyEnv::Decl, ty, subst, 0, &mut 0)
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
    convert_at(ws, TyEnv::Body(inf), ty, subst, 0, bound)
}

fn convert_at<S: std::hash::BuildHasher>(
    ws: &Workspace,
    env: TyEnv<'_>,
    ty: TyId,
    subst: &std::collections::HashMap<VarId, MonoTy, S>,
    depth: u32,
    bound: &mut usize,
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
            match env.solved(var)? {
                Some(next) => return convert_at(ws, env, next, subst, depth + 1, bound),
                None => {
                    let bound_to = subst
                        .get(&var)
                        .cloned()
                        .ok_or(MonoTyReason::UnresolvedInfer(name))?;
                    *bound += 1;
                    return Ok(bound_to);
                }
            }
        }
        TypeExpr::Generic(var, name) => {
            let bound_to = subst
                .get(&var)
                .cloned()
                .ok_or(MonoTyReason::UnboundGeneric(name))?;
            *bound += 1;
            return Ok(bound_to);
        }
        TypeExpr::Nullable(inner) => MonoTy::Nullable(Box::new(convert_at(
            ws,
            env,
            ty.with_idx(inner),
            subst,
            depth + 1,
            bound,
        )?)),
        TypeExpr::Tuple { elements } => {
            let mut parts = Vec::new();
            for el in elements {
                parts.push(convert_at(
                    ws,
                    env,
                    ty.with_idx(el),
                    subst,
                    depth + 1,
                    bound,
                )?);
            }
            MonoTy::Tuple(parts)
        }
        TypeExpr::App { ctor, params } => {
            let ctor = match ctor {
                TypeCtor::Array => MonoCtor::Array,
                TypeCtor::Map => MonoCtor::Map,
                TypeCtor::Ref => MonoCtor::Ref,
                TypeCtor::Nominal(n) => MonoCtor::Nominal(n),
            };
            let mut ps = Vec::new();
            for p in params {
                ps.push(convert_at(
                    ws,
                    env,
                    ty.with_idx(p),
                    subst,
                    depth + 1,
                    bound,
                )?);
            }
            MonoTy::App { ctor, params: ps }
        }
        TypeExpr::Function { params, ret } => {
            let mut ps = Vec::new();
            for p in params {
                ps.push(convert_at(
                    ws,
                    env,
                    ty.with_idx(p),
                    subst,
                    depth + 1,
                    bound,
                )?);
            }
            let ret = convert_at(ws, env, ty.with_idx(ret), subst, depth + 1, bound)?;
            MonoTy::Function {
                params: ps,
                ret: Box::new(ret),
            }
        }
    })
}

/// Polymorphic recursion (`f<T> -> f<[T]>`) does not terminate under
/// demand-driven instantiation (plan S 7). The cap turns divergence into a
/// REPORT: a capped instance is a changing-instance recursion candidate.
///
/// It caps the STRUCTURAL DEPTH of the instantiation types, not the path length
/// from a root. A path length is a property of how the walk arrived, so capping
/// on it makes graph MEMBERSHIP depend on discovery order -- measured: with a
/// path cap, a recursive corpus project produced two different manifests under
/// forward and reversed root order, failing plan S 20g.4. Type depth is a
/// property of the instance itself, so the same instance is capped or not
/// regardless of how it was reached.
///
/// It lives HERE, in the lowest crate that owns [`MonoTy`], so the instance
/// graph and BOTH emitters read one decision. Sharing only the constant is not
/// enough: three call sites each aggregating depth their own way is three
/// implementations that can drift, and the one that lacked the check entirely
/// did not miscompile -- it HUNG.
pub const MAX_INSTANTIATION_DEPTH: u32 = 12;

/// **THE admissibility decision for an instantiation's type arguments.**
///
/// Returns the offending argument -- the deepest one past the cap -- or `None`
/// when the instantiation is admissible. Callers differ only in what they DO
/// with the answer: the instance graph declines to expand and records a
/// changing-instance-recursion candidate, while emission REJECTS, because a call
/// site has already spelled the specialized name and skipping the body would
/// ship a call to a definition nothing writes.
///
/// Takes `&MonoTy` rather than a depth so the aggregation is here too. The graph
/// holds `MonoTyId`s and reaches these through `MonoArena::get`; emission holds
/// the owned trees of an argument list directly.
pub fn too_deep_to_instantiate<'a>(
    args: impl IntoIterator<Item = &'a MonoTy>,
) -> Option<&'a MonoTy> {
    args.into_iter()
        .filter(|t| t.depth() > MAX_INSTANTIATION_DEPTH)
        .max_by_key(|t| t.depth())
}

#[cfg(test)]
mod mono_read_cache_tests {
    use super::*;

    /// Two live interners mint the same numeric ids for different types; the
    /// worker-local cache must never serve one interner's type for the other's
    /// id.
    #[test]
    fn worker_local_reads_stay_interner_scoped() {
        let a = MonoInterner::new();
        let b = MonoInterner::new();
        let ia = a.intern(MonoTy::Int);
        let ib = b.intern(MonoTy::Bool);
        assert_eq!(
            ia, ib,
            "both interners must mint id 0 for the fixture to discriminate"
        );
        for _ in 0..2 {
            assert_eq!(a.get(ia), MonoTy::Int);
            assert_eq!(b.get(ib), MonoTy::Bool);
        }
    }

    /// An intern AFTER this thread warmed its local cache must still be
    /// readable -- a cold id reads through to the lock.
    #[test]
    fn a_post_warm_intern_reads_through() {
        let a = MonoInterner::new();
        let first = a.intern(MonoTy::Int);
        let _ = a.get(first);
        let second = a.intern(MonoTy::Float);
        assert_ne!(first, second);
        assert_eq!(a.get(second), MonoTy::Float);
        assert_eq!(
            a.get_all(&[first, second]),
            vec![MonoTy::Int, MonoTy::Float]
        );
    }

    #[test]
    fn shared_reads_reuse_only_the_current_interner_handle() {
        let a = MonoInterner::new();
        let b = MonoInterner::new();
        let ia = a.intern(MonoTy::Int);
        let ib = b.intern(MonoTy::Bool);

        let (a_first, a_cold) = a.get_shared_with_status(ia);
        let (a_second, a_warm) = a.get_shared_with_status(ia);
        let (b_first, b_cold) = b.get_shared_with_status(ib);

        assert!(a_cold && !a_warm && b_cold);
        assert!(Rc::ptr_eq(&a_first, &a_second));
        assert!(!Rc::ptr_eq(&a_first, &b_first));
        assert_eq!(*a_first, MonoTy::Int);
        assert_eq!(*b_first, MonoTy::Bool);
    }
}
