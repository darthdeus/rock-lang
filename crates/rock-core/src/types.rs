use std::fmt::{Display, Formatter};

use crate::prelude::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypeId(pub ModuleId, pub u32);

impl TypeId {
    pub fn new(module_id: ModuleId, local_id: u32) -> Self {
        Self(module_id, local_id)
    }

    // Const method for builtin types
    pub const fn from_u32(id: u32) -> Self {
        Self(ModuleId::BUILTIN, id)
    }

    pub const fn to_u32(&self) -> u32 {
        self.1
    }

    pub fn module_id(&self) -> ModuleId {
        self.0
    }

    pub fn local_id(&self) -> u32 {
        self.1
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Variance {
    Covariant,
    Contravariant,
    Invariant,
}

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub struct TypeVarId(u32);

impl TypeVarId {
    pub fn id(&self) -> u32 {
        self.0
    }

    pub fn from_u32(id: u32) -> Self {
        Self(id)
    }

    pub fn to_ref(self) -> TypeRef {
        TypeRef::Var(self)
    }
}

impl Display for TypeVarId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "?{}", self.0)
    }
}

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub enum TypeVarKind {
    LocalVar,
    Param,
    ArrayElement,
    GenericParam,
    GenericInst,
    FieldAccess(Ustr), // Field name that triggered this type var

    // Type hole for debugging type inference.
    // Carries the hole's positional id plus an optional source name
    // (from `?foo`) so extraction can be keyed by name.
    TypeHole(u32, Option<Ustr>),
}

#[derive(Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub enum TypeRef {
    Var(TypeVarId),
    Concrete(TypeId),

    Nullable(Box<TypeRef>),
    Tuple(Vec<TypeRef>),

    App {
        constructor: TypeId,
        args: Vec<TypeRef>,
    },

    /// Recovery type: stand-in returned when inference for an expression
    /// errored AND a diagnostic was already pushed to `storage.errors`.
    /// Mirrors rustc's `TyKind::Error(ErrorGuaranteed)` and Roslyn's
    /// `ErrorTypeSymbol`. Unification or subtype checks against `Error`
    /// always succeed silently so a single failure doesn't cascade into
    /// dozens of follow-on diagnostics. No public constructor.
    ///
    /// Today this variant has *no constructors*. It exists to define the
    /// match arms that will become live in step 3, so adding the variant
    /// is a pure additive change. Codegen and IR readiness should never
    /// see it; the eventual debug_assert lives in
    /// `type_system::TypeCompleteness::validate_ir_readiness`.
    Error,
}

impl TypeRef {
    /// `true` iff this is the recovery sentinel `TypeRef::Error`.
    /// Diagnostic-emission sites should consult this to suppress
    /// cascade reports; see plan §3.2.
    pub fn is_error(&self) -> bool {
        matches!(self, TypeRef::Error)
    }

    pub fn contains_type_var(&self) -> bool {
        match self {
            TypeRef::Var(_) => true,
            TypeRef::Concrete(_) => false,
            TypeRef::Nullable(inner) => inner.contains_type_var(),
            TypeRef::Tuple(types) => types.iter().any(|t| t.contains_type_var()),
            TypeRef::App { args, .. } => args.iter().any(|t| t.contains_type_var()),
            TypeRef::Error => false,
        }
    }

    pub fn to_ref(&self) -> Self {
        self.clone()
    }

    pub fn is_nullable(&self) -> bool {
        matches!(self, TypeRef::Nullable(_))
    }

    pub fn as_concrete_type_id(&self) -> Option<TypeId> {
        match self {
            TypeRef::Concrete(ty) => Some(*ty),
            _ => None,
        }
    }

    /// Resolve any `TypeRef` to the `TypeId` whose `TypeInfo` describes
    /// its fields/variants for completion and inspection purposes.
    ///
    /// Unlike `as_concrete_type_id`, this also unwraps generic applications
    /// (`App { constructor, args }` -> `constructor`) and nullable wrappers
    /// (`Nullable(inner)` -> recurse). That matters for LSP completion:
    /// the type of `MOBS.filter(_.health > 0)` is
    /// `TypeRef::App { constructor: Vec, args: [Tuple(Entity<Mob>, Mob)] }`
    /// and the user wants to see `Vec`'s methods when they type `.` after
    /// that expression. Without this helper the completion pipeline
    /// skipped every generic return type and walked left until it hit
    /// a scalar subexpression, producing misleading `int`-typed
    /// suggestions.
    ///
    /// Returns `None` for `Var` (unresolved) and `Tuple` (structural, no
    /// single TypeId). Callers should handle those shapes specially if needed.
    pub fn resolve_constructor(&self) -> Option<TypeId> {
        match self {
            TypeRef::Concrete(ty) => Some(*ty),
            TypeRef::App { constructor, .. } => Some(*constructor),
            TypeRef::Nullable(inner) => inner.resolve_constructor(),
            TypeRef::Var(_) | TypeRef::Tuple(_) | TypeRef::Error => None,
        }
    }
}
