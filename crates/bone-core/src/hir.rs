use std::sync::OnceLock;

use rust_lapper::Interval;
use skull_parser::Ident;
pub use skull_parser::FnBackend;

use crate::modules::NsVisibility;
use crate::prelude::*;

#[derive(Copy, Clone, Debug)]
pub struct Builtins {
    pub int: Idx<TypeExpr>,
    pub uint: Idx<TypeExpr>,
    pub float: Idx<TypeExpr>,
    pub f32: Idx<TypeExpr>,
    pub bool: Idx<TypeExpr>,
    pub string: Idx<TypeExpr>,
    pub unit: Idx<TypeExpr>,
    pub any: Idx<TypeExpr>,
    pub never: Idx<TypeExpr>,
    pub error: Idx<TypeExpr>,
}

impl Builtins {
    pub fn int(&self) -> TyId {
        TyId::Static(ModuleId::builtin(), self.int)
    }

    pub fn uint(&self) -> TyId {
        TyId::Static(ModuleId::builtin(), self.uint)
    }

    pub fn float(&self) -> TyId {
        TyId::Static(ModuleId::builtin(), self.float)
    }

    pub fn f32(&self) -> TyId {
        TyId::Static(ModuleId::builtin(), self.f32)
    }

    pub fn bool(&self) -> TyId {
        TyId::Static(ModuleId::builtin(), self.bool)
    }

    pub fn string(&self) -> TyId {
        TyId::Static(ModuleId::builtin(), self.string)
    }

    pub fn unit(&self) -> TyId {
        TyId::Static(ModuleId::builtin(), self.unit)
    }

    pub fn any(&self) -> TyId {
        TyId::Static(ModuleId::builtin(), self.any)
    }

    pub fn never(&self) -> TyId {
        TyId::Static(ModuleId::builtin(), self.never)
    }

    pub fn error(&self) -> TyId {
        TyId::Static(ModuleId::builtin(), self.error)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum TyId {
    Static(ModuleId, Idx<TypeExpr>),
    Local(Idx<TypeExpr>),
}

impl TyId {
    pub fn unwrap_in_mod(self, err_span: Span) -> Result<(ModuleId, Idx<TypeExpr>), CompilerError> {
        match self {
            TyId::Static(mid, idx) => Ok((mid, idx)),

            TyId::Local(idx) => comp_error!(
                err_span,
                "Tried to unwrap a in-module, but got local {:?}",
                idx
            ),
        }
    }

    pub fn with_idx(self, idx: Idx<TypeExpr>) -> TyId {
        match self {
            TyId::Static(mid, _) => TyId::Static(mid, idx),
            TyId::Local(_) => TyId::Local(idx),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BuiltinTy {
    Any,
    Unit,
    Int,
    Uint,
    Float,
    /// Narrow float STORAGE: only ever a field's `storage_ty`. Expressions,
    /// and the field's `ty`, see `Float`; layout reads the storage type.
    F32,
    Bool,
    String,
    Never,
    // Inference-recovery poison type. Failed inference (e.g. RHS)
    // get assigned this, and it absorbs in unify/coerce/join.
    Error,
}

// #[derive(Copy, Clone, Debug, PartialEq, Eq)]
// pub enum NamedTy {
//     Struct { name: Ustr },
// }

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TypeCtor {
    Array,
    Map,
    Ref,
    Nominal(Ustr),
}

impl TypeCtor {
    pub fn for_name(name: Ustr) -> TypeCtor {
        if crate::modules::is_intrinsic_key(name, "Vec") {
            TypeCtor::Array
        } else if crate::modules::is_intrinsic_key(name, "Map") {
            TypeCtor::Map
        } else if crate::modules::is_intrinsic_key(name, "Ref") {
            TypeCtor::Ref
        } else {
            TypeCtor::Nominal(name)
        }
    }
}

/// A default for a trailing generic param (`struct Pair<T, U = int>`).
/// Deliberately index-free (a builtin or a bare named type) so a use site in
/// ANY module can synthesize the padded `TypeExpr` into its own arena without
/// cross-arena copying; richer default shapes are rejected at the decl.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GenericDefault {
    Builtin(BuiltinTy),
    Named(Ustr),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TypeExpr {
    Builtin(BuiltinTy),
    // declared generic param, from <T>
    Generic(VarId, Ustr),
    // call-site generic instantiation
    Infer(VarId, Ustr),
    // Named(NamedTy),
    Nullable(Idx<TypeExpr>),

    App {
        ctor: TypeCtor,
        params: IdxRange<TypeExpr>,
    },

    Function {
        params: IdxRange<TypeExpr>,
        ret: Idx<TypeExpr>,
    },

    Tuple {
        elements: IdxRange<TypeExpr>,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum BodyOwner {
    Function(Idx<Function>),
    Decorator(Idx<DecoratorDef>),
    Global(Idx<GlobalDef>),
    StructDefaults(Idx<StructDef>),
    Enum(Idx<EnumDef>),
    Newtype(Idx<NewtypeDef>),
}

#[derive(Clone, Debug)]
pub struct Body {
    /// Bumped on every re-lower. Batch-lowering leaves 0.
    pub generation: u64,

    pub scope_arena: Arena<Scope>,

    pub binding_arena: Arena<BindingInfo>,
    pub binding_spans: ArenaMap<Idx<BindingInfo>, Span>,

    pub pattern_arena: Arena<Pattern>,
    pub match_arm_arena: Arena<MatchArm>,
    pub record_pat_field_arena: Arena<RecordPatField>,
    pub interp_part_arena: Arena<InterpPart>,
    pub struct_lit_field_arena: Arena<StructLitField>,

    pub statement_arena: Arena<Statement>,
    pub statement_spans: ArenaMap<Idx<Statement>, Span>,

    pub expr_arena: Arena<Expr>,
    pub expr_spans: ArenaMap<Idx<Expr>, Span>,

    pub lowered_tys: Arena<TypeExpr>,
    pub lowered_ty_spans: ArenaMap<Idx<TypeExpr>, Span>,

    // TODO: make this IdxMap instead and A/B benchmark?
    pub arg_names: FxHashMap<Idx<Expr>, Ustr>,
    pub method_call_recvs: FxHashSet<Idx<Expr>>,
    /// The method name as written at `recv.name(..)` / `recv?.name(..)`,
    /// keyed by the receiver expr. A fn-typed FIELD of that name wins over the
    /// callee family (spec 8.4), and in v1 the family's key is a DefKey or an
    /// alias target, not the written name.
    pub method_names: FxHashMap<Idx<Expr>, Ustr>,
    /// Call args written as a struct spread `..op` (keyed by the operand's expr
    /// Idx). The operand's fields expand into named args at this position;
    /// resolved in bone-inference, codegen'd in skull-mir. Never also in
    /// `arg_names`.
    pub spread_args: FxHashSet<Idx<Expr>>,
    /// In a v1 fragment, an `Expr::Global` naming a legacy callable reached
    /// through the std view or a legacy provider's view, taken as a value:
    /// the view's package (`FamilyMember::owner`), since the bare key is
    /// shared by every legacy file defining the name.
    pub value_owners: FxHashMap<Idx<Expr>, crate::modules::PackageId>,

    // Owner's body expression:
    // - fn body (none for lua/extern)
    // - deco body
    // - global initializer
    // - struct default bodies use field_defaults
    pub root: Option<Idx<Expr>>,
    pub root_scope: Option<Idx<Scope>>,

    // Param bindings in decl order, from ParamDecls at lowering
    pub param_bindings: IdxRange<BindingInfo>,
    pub param_defaults: Vec<Option<Idx<Expr>>>,

    // Decorator only `target` binding
    pub deco_target: Option<Idx<BindingInfo>>,

    // StructDefaults bodies
    pub field_defaults: FxHashMap<Ustr, Idx<Expr>>,

    pub node_index_cell: OnceLock<Lapper<usize, NodeRef>>,

    pub node_intervals: Vec<Interval<usize, NodeRef>>,

    /// Each segment of every modules-v1 path this body spells, as the
    /// resolver resolved it at lowering ([`PathSegmentRef`]).
    pub path_segments: Vec<PathSegmentRef>,
}

/// One segment of a modules-v1 path as the resolver resolved it at lowering
/// (plan 2.3: resolver outputs are recorded on the HIR; consumers do not
/// re-resolve). The node the path lowered to carries its final target; this
/// record adds where each segment is spelled and what the segments before
/// the last walked through, so an editor can navigate and rename by segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathSegmentRef {
    pub span: Span,
    pub target: SegmentTarget,
    /// The local name an import binds (`use a::f as g;` -- the `g`), not a
    /// spelling of the target's own name.
    pub alias: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentTarget {
    /// A module the path walked into: its package and node key (`""` for a
    /// package root).
    Module { package: crate::modules::PackageId, node: Ustr },
    /// A definition, by DefKey.
    Def(Ustr),
    /// An enum variant, by its enum's DefKey.
    Variant { enum_key: Ustr, variant: Ustr },
}

impl Default for Body {
    fn default() -> Self {
        Self {
            generation: 0,

            scope_arena: Default::default(),

            binding_arena: Default::default(),
            binding_spans: Default::default(),

            pattern_arena: Default::default(),
            match_arm_arena: Default::default(),
            record_pat_field_arena: Default::default(),
            interp_part_arena: Default::default(),
            struct_lit_field_arena: Default::default(),

            statement_arena: Default::default(),
            statement_spans: Default::default(),

            expr_arena: Default::default(),
            expr_spans: Default::default(),

            lowered_tys: Default::default(),
            lowered_ty_spans: Default::default(),

            arg_names: Default::default(),
            method_call_recvs: Default::default(),
            method_names: Default::default(),
            spread_args: Default::default(),
            value_owners: Default::default(),

            root: Default::default(),
            root_scope: None,

            param_bindings: IdxRange::new_empty(),
            param_defaults: Default::default(),

            deco_target: None,

            field_defaults: Default::default(),

            node_index_cell: Default::default(),
            node_intervals: Default::default(),

            path_segments: Default::default(),
        }
    }
}

impl Body {
    pub fn make_lapper(&mut self, start: usize, stop: usize, val: NodeRef) {
        self.node_intervals.push(Interval { start, stop, val })
    }

    pub fn node_index(&self) -> &Lapper<usize, NodeRef> {
        self.node_index_cell
            .get_or_init(|| Lapper::new(self.node_intervals.clone()))
    }

    pub fn alloc_root_scope(&mut self) -> Idx<Scope> {
        self.scope_arena.alloc(hir::Scope {
            parent: None,
            bindings: Default::default(),
        })
    }

    pub fn alloc_root_scope_with_bindings(
        &mut self,
        bindings: FxHashMap<Ustr, Idx<BindingInfo>>,
    ) -> Idx<Scope> {
        self.scope_arena.alloc(hir::Scope {
            parent: None,
            bindings,
        })
    }

    pub fn alloc_child_scope(&mut self, parent: Idx<Scope>) -> Idx<Scope> {
        self.scope_arena.alloc(Scope {
            parent: Some(parent),
            bindings: Default::default(),
        })
    }

    pub fn alloc_statement(&mut self, statement: Statement, span: Span) -> Idx<Statement> {
        let idx = self.statement_arena.alloc(statement);
        self.statement_spans.insert(idx, span);
        idx
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NodeRef {
    // Same as TyId::Static, but more explicit
    StaticTy { mid: ModuleId, idx: Idx<TypeExpr> },
    Expr(Idx<Expr>),
    Statement(Idx<Statement>),
    Binding(Idx<BindingInfo>),
    LocalTy(Idx<TypeExpr>),
}

#[derive(Clone, Debug, Default)]
pub struct ModuleData {}

#[derive(Clone, Debug)]
pub struct Module {
    pub id: ModuleId,
    /// Bumped on any interface rebuild, batch leaves 0.
    pub interface_generation: u64,

    pub lower_var_high_water: u32,

    pub file: Ustr,
    /// The owning package's manifest name, stamped by the host graph. `None`
    /// for stdlib files and for programs outside any package.
    pub package: Option<Ustr>,

    pub functions: Arena<Function>,
    pub functions_by_name: FxHashMap<Ustr, FnList>,

    pub structs: Arena<StructDef>,
    pub struct_by_name: FxHashMap<Ustr, Idx<StructDef>>,

    pub enums: Arena<EnumDef>,
    pub enums_by_name: FxHashMap<Ustr, Idx<EnumDef>>,

    pub newtypes: Arena<NewtypeDef>,
    pub newtypes_by_name: FxHashMap<Ustr, Idx<NewtypeDef>>,

    pub globals: Arena<GlobalDef>,
    pub global_by_name: FxHashMap<Ustr, Idx<GlobalDef>>,

    pub bodies: FxHashMap<BodyOwner, Body>,

    pub ty_arena: Arena<TypeExpr>,
    pub ty_spans: ArenaMap<Idx<TypeExpr>, Span>,

    pub decorators: Arena<DecoratorDef>,

    pub item_index_cell: OnceLock<Lapper<usize, BodyOwner>>,
    pub item_intervals: Vec<Interval<usize, BodyOwner>>,

    /// Body-lowering errors, keyed by the body they came from, so the
    /// incremental body relower can REPLACE one body's errors instead of
    /// appending (a flat Vec leaked + went stale: a body-only re-lower mutates
    /// the module in place, so old errors for the edited body would accumulate
    /// and survive even after the body is fixed). The full rebuild path rebuilds
    /// a fresh module, so it is naturally correct either way.
    pub lowering_errors: FxHashMap<BodyOwner, Vec<CompilerError>>,
    pub interface_errors: Vec<CompilerError>,
    /// Every namespace fact this module's lowering read (plan 2.6). A full
    /// lowering replaces the set; the one-body fast path only adds to it, so
    /// a superset is kept and an extra re-lower is the worst it costs.
    pub scope_probes: FxHashSet<crate::modules::ScopeProbe>,

    /// The members of every `CallFamily::Merged` this module's bodies call,
    /// by bare name. A bare name's family is a fact of the file (its imports
    /// and the namespace tree), so every body that calls it records the same
    /// members, and a body-only re-lower overwrites them in place.
    pub call_families: FxHashMap<Ustr, Vec<FamilyMember>>,

    /// Path segments spelled outside any body: `use` items and the
    /// interface positions (signatures, fields, globals) of a v1 fragment
    /// (see [`PathSegmentRef`]).
    pub path_segments: Vec<PathSegmentRef>,
}

impl Default for Module {
    fn default() -> Self {
        Self {
            id: ModuleId::builtin(),

            interface_generation: 0,
            lower_var_high_water: 0,

            file: Ustr::from(""),
            package: None,

            functions: Default::default(),
            functions_by_name: Default::default(),

            structs: Default::default(),
            struct_by_name: Default::default(),

            enums: Default::default(),
            enums_by_name: Default::default(),

            newtypes: Default::default(),
            newtypes_by_name: Default::default(),

            globals: Default::default(),
            global_by_name: Default::default(),

            bodies: Default::default(),

            ty_arena: Default::default(),
            ty_spans: Default::default(),

            decorators: Default::default(),

            item_index_cell: Default::default(),
            item_intervals: Default::default(),

            lowering_errors: Default::default(),
            interface_errors: Default::default(),
            scope_probes: Default::default(),

            call_families: Default::default(),

            path_segments: Default::default(),
        }
    }
}

impl Module {
    pub fn make_lapper(&mut self, start: usize, stop: usize, val: BodyOwner) {
        self.item_intervals
            .push(rust_lapper::Interval { start, stop, val })
    }

    pub fn item_index(&self) -> &Lapper<usize, BodyOwner> {
        self.item_index_cell
            .get_or_init(|| Lapper::new(self.item_intervals.clone()))
    }

    pub fn insert_body(&mut self, owner: BodyOwner, body: Body) {
        self.bodies.insert(owner, body);
    }

    pub fn body_at(&self, offset: usize) -> Option<BodyOwner> {
        self.item_index()
            .find(offset, offset + 1)
            .min_by_key(|x| x.stop - x.start)
            .map(|x| x.val)
    }

    // TODO: inline this later
    pub fn module_id(&self) -> ModuleId {
        self.id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingInfo {
    pub name: Ustr,
    pub kind: BindingKind,
    pub declared_ty: Option<TyId>,
    pub span: Span,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BindingKind {
    Let,
    Param,
    Static,
    /// A nested `fn` name -- a hoisted local helper. Block-scoped like `Let`,
    /// but its value is a zero-alloc reference to the hoisted top-level fn, and
    /// codegen routes calls/value-refs to the `__local_*` name.
    Fn,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct VarId(pub u32);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub parent: Option<Idx<Scope>>,
    pub bindings: FxHashMap<Ustr, Idx<BindingInfo>>,
}

#[derive(Clone, Debug)]
pub struct Function {
    pub span: Span,
    pub name: Ustr,
    pub name_span: Span,

    pub is_coroutine: bool,
    pub is_variadic: bool,
    pub generics: Vec<TyId>,
    /// Parallel to `generics`: `Some(elem)` == the param is declared
    /// `I: Iterator<elem>` (the only bound the compiler knows). Checked
    /// structurally at instantiation: `C` satisfies `Iterator<E>` iff
    /// `next(C) -> (bool, E)` resolves.
    pub generic_bounds: Vec<Option<TyId>>,
    pub param_decls: Vec<ParamDecl>,

    pub num_required: usize,

    pub return_ty: Option<TyId>,
    /// An `inline(<backend>)` body: its target backend + the verbatim source. The
    /// backend disambiguates which codegen emits it (`inline(lua)` vs
    /// `inline(js)`); it is always concrete (`Lua`/`Js`, never `Any`) when set.
    pub inline_body: Option<(FnBackend, Ustr)>,
    pub is_extern: bool,
    /// Which backend this decl targets, the single source of truth for
    /// backend-variant selection at codegen. `Any` = a portable Rock body (any
    /// backend emits it from HIR). `Lua` = luajit-native binding/body
    /// (`inline(lua)` / `extern(lua)` / `extern(ffi,..)` / plain `extern` C).
    /// `Js` = `inline(js)` / `extern(js)`. Two same-name same-signature decls that
    /// differ only in `backend` are collapsed to one overload candidate
    /// (`select_overload`); each backend then emits its own variant.
    pub backend: FnBackend,
    pub annotations: Vec<Annotation>,
    pub ffi_lib: Option<Ustr>,
    /// Parser-derived rather than user-written: the `__op_*` operators
    /// synthesized for a newtype over a primitive. Lints report on user surface
    /// only -- an unreached derived operator has no available action.
    pub is_synthetic: bool,
}

#[derive(Clone, Debug)]
pub struct Annotation {
    pub span: Span,
    pub name: Ident,
    pub args: Vec<AnnoArg>,
    /// Canonical v1 decorator identity when this name resolved through the
    /// module namespace. `None` preserves legacy and compiler-owned names.
    pub resolved_name: Option<Ustr>,
    /// Canonical v1 identities for the identifiers in a builtin structured
    /// argument such as `@wire(onchange=[CONST])`.
    pub resolved_ident_list: Option<Vec<Ustr>>,
}

impl Annotation {
    /// Compiler-owned behavior is attached only to an un-resolved spelling.
    /// A user decorator may legally have the same source name as a builtin.
    pub fn is_builtin(&self, name: &str) -> bool {
        self.name.name.as_str() == name && self.resolved_name.is_none()
    }
}

#[derive(Clone, Debug)]
pub struct AnnoArg {
    pub name: Option<Ident>,
    pub value: AnnoValue,
}

#[derive(Clone, Debug)]
pub enum AnnoValue {
    Int(i64),
    Float(f64),
    String(Ustr),
    Ident(Ident),
    IdentList(Vec<Ident>),
    Bool(bool),
    Other,
}

#[derive(Clone, Debug)]
pub struct ParamDecl {
    pub name: Ustr,
    pub ty: TyId,
    pub has_default: bool,
    pub annotations: Vec<Annotation>,
}

#[derive(Clone, Debug)]
pub enum BindingPattern {
    Leaf(Idx<BindingInfo>),
    Tuple(Vec<BindingPattern>),
}

impl BindingPattern {
    pub fn leaves(&self) -> Vec<Idx<BindingInfo>> {
        let mut out = Vec::new();

        fn go(p: &BindingPattern, out: &mut Vec<Idx<BindingInfo>>) {
            match p {
                BindingPattern::Leaf(b) => out.push(*b),
                BindingPattern::Tuple(subs) => subs.iter().for_each(|s| go(s, out)),
            }
        }

        go(self, &mut out);
        out
    }
}

#[derive(Copy, Clone, Debug)]
pub enum InterpPart {
    Literal(Ustr),
    Value {
        expr: Idx<Expr>,
        spec: Option<InterpSpec>,
    },
}

#[derive(Copy, Clone, Debug)]
pub struct InterpSpec {
    pub precision: Option<usize>,
    pub precision_expr: Option<Idx<Expr>>,
    pub width: Option<usize>,
    pub width_expr: Option<Idx<Expr>>,
    pub alignment: Option<char>,
    pub fill: Option<char>,
    pub sign: Option<char>,
    pub alternate: bool,
    pub zero_pad: bool,
    pub grouping: Option<char>,
    pub format_type: Option<char>,
}

/// Which surface form produced an `Expr::As`. `::` checks its operand against
/// the target as an expected type; `as` converts an independently inferred
/// source type through the closed conversion relation.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum AsKind {
    Ascribe,
    Cast,
    Bitcast,
}

#[derive(Copy, Clone, Debug)]
pub enum Expr {
    Unit,
    Null,
    Hole(Option<Ustr>),

    Int(i64),
    Float(OrderedFloat<f64>),
    Bool(bool),
    String(Ustr),
    Var(Idx<BindingInfo>),
    Tuple(IdxRange<Expr>),

    InterpolatedString(IdxRange<InterpPart>),
    /// `!x`. The family is `__not`'s, decided at lowering like a binary
    /// operator's; inference reads it only for a non-bool operand.
    Not(Idx<Expr>, CallFamily),
    BitNot(Idx<Expr>),

    /// `-x`. The family is `__unm`'s, decided at lowering; inference reads
    /// it only for an operand that is not a number or a newtype.
    UnaryMinus(Idx<Expr>, CallFamily),

    IncDec {
        target: Idx<Expr>,
        decrement: bool,
        postfix: bool,
    },

    Return(Option<Idx<Expr>>),

    Break,
    Continue,

    Yield(Option<Idx<Expr>>),
    YieldDelegate(Idx<Expr>),

    Spread(Idx<Expr>),

    As {
        value: Idx<Expr>,
        target: TyId,
        kind: AsKind,
    },

    Index {
        base: Idx<Expr>,
        index: Idx<Expr>,
    },

    Lambda {
        params: IdxRange<BindingInfo>,
        body: Idx<Expr>,
    },

    SafeNav {
        base: Idx<Expr>,
        field: Ustr,
    },

    SafeNavLet {
        receiver: Idx<Expr>,
        binding: Idx<BindingInfo>,
        body: Idx<Expr>,
    },

    SafeNavCall {
        receiver: Idx<Expr>,
        method: Ustr,
        args: IdxRange<Expr>,
        /// Resolver family for `method`, same shape as `Expr::Call` (spec 8.4:
        /// UFCS and the bare call select the same family); `family.key()` is
        /// `method`.
        family: CallFamily,
    },

    TupleIndex {
        tuple: Idx<Expr>,
        index: usize,
    },

    BinOp {
        op: BinOp,
        lhs: Idx<Expr>,
        rhs: Idx<Expr>,
    },
    Block {
        statements: IdxRange<Statement>,
        tail: Option<Idx<Expr>>,
    },

    If {
        cond: Idx<Expr>,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
    },

    IfLet {
        pattern: Idx<Pattern>,
        value: Idx<Expr>,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
    },

    Call {
        callee: Callee,
        args: IdxRange<Expr>,
        /// Explicit type arguments from `f<A, B>(...)`; empty when generic
        /// inference supplies the callee's instance.
        generic_args: IdxRange<TypeExpr>,
        /// Resolver family (plan 2.3). Legacy calls use `CallFamily::Legacy`;
        /// v1 calls carry the DefKey plus the shadowed losing-tier origin.
        /// `family.key()` is the same key as `Callee::Unresolved.name`, which
        /// the MIR backend still reads; M4 moves backend identity onto the
        /// family and drops the second copy.
        family: CallFamily,
    },

    Array(IdxRange<Expr>),

    StructLiteral {
        name: Ustr,
        fields: IdxRange<StructLitField>,
        /// Functional-update bases (`Ty { ..base, .. }`), evaluated before
        /// fields. Empty for a plain literal. Each base has the same nominal
        /// type as the literal; unwritten fields are copied from it.
        bases: IdxRange<Expr>,
    },

    FieldAccess {
        base: Idx<Expr>,
        field: Ustr,
    },

    EnumVariant {
        enum_name: Ustr,
        enum_name_span: Span,
        variant: Ustr,
        variant_span: Span,
        args: VariantArgs,
    },

    Global(Ustr),
    SiblingRef(Ustr),

    Range {
        low: Idx<Expr>,
        high: Idx<Expr>,
    },

    Assign {
        target: Idx<Expr>,
        value: Idx<Expr>,
    },

    /// `target op= value` where `target` has no single place to rewrite as
    /// `target = target op value` (a safe-nav chain). `op_fn` is the
    /// operator's glyph function, which scalar arithmetic reads; `family` is
    /// the overload family `target op value` resolves through, decided at
    /// lowering exactly as for the binary operator.
    CompoundAssign {
        target: Idx<Expr>,
        op_fn: Ustr,
        value: Idx<Expr>,
        family: CallFamily,
    },

    Match {
        scrutinee: Idx<Expr>,
        arms: IdxRange<MatchArm>,
    },
}

#[derive(Copy, Clone, Debug)]
pub enum Callee {
    Unresolved { name: Ustr, name_span: Span },
    Local(Idx<BindingInfo>),
    Value(Idx<Expr>),
}

/// One member of a v1 call family: the overloads registered under `key`.
/// A legacy definition's key is its bare name, shared by every legacy file
/// that defines the name and by the standard library, so a member a v1
/// fragment reached through the std view or a legacy provider's view names
/// that view's package in `owner`, and only the overloads its own files
/// define belong to the member (spec 3.2, 5). `None` for a v1 DefKey, which
/// only its definition carries.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FamilyMember {
    pub key: Ustr,
    pub owner: Option<crate::modules::PackageId>,
    pub tier: FamilyTier,
}

/// The lookup tier that contributed a family member (spec 8.3 tiers 2-5).
/// A v1 callee family spans every tier from the first one that binds the
/// name; fit ranks the whole family and the tier breaks only an exact
/// best-fit tie, nearer tier first. A legacy family is one flat tier.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FamilyTier {
    Legacy,
    Own,
    Imports,
    Globs,
    Std,
}

/// Overload family selected by the resolver (plan 2.3 / M2 step 5).
/// `resolve_callee` consumes this rather than a spelled last segment.
#[derive(Copy, Clone, Debug)]
pub enum CallFamily {
    /// Legacy compilation unit: workspace-map lookup by bare name.
    Legacy(Ustr),
    /// v1: the one definition the tiers bind, by canonical DefKey. `owner`
    /// is as for [`FamilyMember`].
    Resolved { key: Ustr, owner: Option<crate::modules::PackageId> },
    /// A bare name whose overload family spans several definitions, in
    /// either dialect: legacy callables merged with projected or imported v1
    /// ones (spec 3.2), or a v1 family spanning the lookup tiers (spec 8.3,
    /// 8.5). Lowering decides the member keys and records them under `name`
    /// in `Module::call_families` of the module lowered from `file`; `Expr`
    /// is `Copy`, so the family carries only that address. A v1 unary
    /// operator no tier binds is a family with no members: a scalar operand
    /// never reads it, any other has no overload.
    Merged { name: Ustr, file: Ustr },
}

impl CallFamily {
    /// The family's spelling: its one key, or the bare name of a merged
    /// family.
    pub fn key(&self) -> Ustr {
        match self {
            CallFamily::Legacy(n) => *n,
            CallFamily::Resolved { key, .. } => *key,
            CallFamily::Merged { name, .. } => *name,
        }
    }

    pub fn from_name(name: Ustr) -> Self {
        CallFamily::Legacy(name)
    }

    pub fn resolved(key: Ustr, owner: Option<crate::modules::PackageId>) -> Self {
        CallFamily::Resolved { key, owner }
    }

    pub fn merged(name: Ustr, file: Ustr) -> Self {
        CallFamily::Merged { name, file }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum BinOp {
    Eq,
    NotEq,
    Lt,
    Leq,
    Gt,
    Geq,
    And,
    Or,
    NullCoalesce,

    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

/// Every binary/operator declaration name the parser can mint, as
/// `(fn name, symbol as written, may a user overload it)`. One-parameter `-`
/// and `!` declarations select the prefix names in `UNARY_OPERATOR_FNS`
/// instead.
///
/// Overloadable means exactly one thing: the operator's USE lowers to a call
/// resolved by name, so a user fn can be found there. `+ - * / % ./` do.
/// Everything else becomes a `BinOp` variant, a dedicated `Expr`, or native
/// codegen and never consults a fn -- so a definition for one could not be
/// reached however it was written. `pass_lower_functions` rejects those instead
/// of emitting a fn nothing calls; before it did, `operator ==` silently left
/// structural equality in place with no diagnostic at any stage.
///
/// This table must stay total over the parser's symbol match for binary
/// declarations (`skull-parser/src/parser.rs`, `parse_operator`). The
/// `operator_table_matches_the_parser` test spells every symbol with its
/// binary arity.
pub const OPERATOR_FNS: &[(&str, &str, bool)] = &[
    ("__op_add", "+", true),
    ("__op_sub", "-", true),
    ("__op_mul", "*", true),
    ("__op_div", "/", true),
    ("__op_mod", "%", true),
    ("__op_idiv", "./", true),
    ("__op_eq", "==", false),
    ("__op_ne", "!=", false),
    ("__op_lt", "<", false),
    ("__op_le", "<=", false),
    ("__op_gt", ">", false),
    ("__op_ge", ">=", false),
    ("__op_and", "&&", false),
    ("__op_or", "||", false),
    ("__op_not", "!", false),
    ("__op_bitnot", "~", false),
    ("__op_bitand", "&", false),
    ("__op_bitor", "|", false),
    ("__op_bitxor", "^", false),
    ("__op_shl", "<<", false),
    ("__op_shr", ">>", false),
    ("__op_index", "[]", false),
];

/// The function names selected by the prefix forms of the two glyphs that
/// have a unary spelling. They are separate from `OPERATOR_FNS`: the latter
/// remains the table for binary/operator declarations and non-overloadable
/// reserved symbols, while these names are ordinary callable families too.
pub const UNARY_OPERATOR_FNS: &[(&str, &str)] = &[("__unm", "-"), ("__not", "!")];


/// `None` for a name that is not an operator fn at all.
pub fn operator_entry(name: &str) -> Option<(&'static str, bool)> {
    OPERATOR_FNS
        .iter()
        .find(|(fn_name, _, _)| *fn_name == name)
        .map(|(_, symbol, overloadable)| (*symbol, *overloadable))
}

/// `None` for a name that is not the prefix family of `-` or `!`.
pub fn unary_operator_entry(name: &str) -> Option<&'static str> {
    UNARY_OPERATOR_FNS
        .iter()
        .find(|(fn_name, _)| *fn_name == name)
        .map(|(_, symbol)| *symbol)
}

/// The symbols a user may write an `operator` definition for, for diagnostics.
pub fn overloadable_operator_symbols() -> String {
    OPERATOR_FNS
        .iter()
        .filter(|(_, _, overloadable)| *overloadable)
        .map(|(_, symbol, _)| *symbol)
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone, Debug)]
pub enum Statement {
    Let {
        name: Idx<BindingInfo>,
        value: Idx<Expr>,
    },

    LetTuple {
        vars: BindingPattern,
        value: Idx<Expr>,
    },

    LetNewtype {
        newtype_name: Ustr,
        var: Idx<BindingInfo>,
        value: Idx<Expr>,
    },

    Static {
        name: Idx<BindingInfo>,
        value: Idx<Expr>,
    },

    /// A nested `fn` -- a hoisted local helper. `name` binds in the enclosing
    /// scope (kind `Fn`); `params` are its own child-scope bindings; `body` is
    /// the block, lowered in that child scope. No capture of enclosing
    /// params/locals (rejected in inference).
    Fn {
        name: Idx<BindingInfo>,
        params: IdxRange<BindingInfo>,
        ret_ty: Option<TyId>,
        body: Idx<Expr>,
    },

    Return(Option<Idx<Expr>>),
    Expr(Idx<Expr>),

    Defer(Idx<Expr>),

    For {
        vars: BindingPattern,
        iter: Idx<Expr>,
        body: Idx<Expr>,
    },

    While {
        cond: Idx<Expr>,
        body: Idx<Expr>,
    },

    WhileLet {
        pattern: Idx<Pattern>,
        value: Idx<Expr>,
        body: Idx<Expr>,
    },

    Comment(Ustr),

    Break,
    Continue,
}

#[derive(Clone, Debug)]
pub struct DecoratorDef {
    pub name: Ustr,
    /// Canonical v1 identity used by registries and generated wrappers. Bare
    /// legacy decorators keep their source name here.
    pub def_key: Ustr,
    pub span: Span,
    pub param_decls: Vec<ParamDecl>,
    pub num_required: usize,
    /// Compiler-known metadata on the decorator declaration itself.
    pub annotations: Vec<Annotation>,
}

#[derive(Clone, Debug)]
pub struct StructDef {
    pub name: Ustr,
    pub name_span: Span,
    pub is_value: bool,
    pub is_nobox: bool,
    /// `@nomangle`: the emitted type symbol (C typedef, `__ty` tag) is the
    /// declared name even under modules v1, for inline Lua that spells it.
    pub nomangle: bool,
    pub span: Span,
    pub generics: Vec<TyId>,
    pub fields: Vec<StructFieldDef>,
}

#[derive(Clone, Debug)]
pub struct StructFieldDef {
    pub name: Ustr,
    pub ty: TyId,
    /// The type the field is STORED as: `ty`, except that an `f32` field has
    /// `ty` = `float` (what reads and writes see) and `storage_ty` = `f32`.
    /// Layout and serialization read this; inference never does.
    pub storage_ty: TyId,
    pub span: Span,
    pub has_default: bool,
    pub annotations: Vec<Annotation>,
    /// `embed f: Foo` -- the field's leaves live in the parent, so it has no
    /// independently storable aggregate: PROJECTION-ONLY at every use site.
    /// Enum record variants cannot carry it -- always false there.
    pub is_embed: bool,
    /// Getter visibility (spec 9/10). Default is public.
    pub read_vis: NsVisibility,
    /// Setter visibility (spec 10). `private(set)` restricts this to owner subtree.
    pub write_vis: NsVisibility,
}

#[derive(Copy, Clone, Debug)]
pub struct StructLitField {
    pub name: Ustr,
    pub name_span: Span,
    pub value: Idx<Expr>,
}

#[derive(Clone, Debug)]
pub struct EnumDef {
    pub name: Ustr,
    pub name_span: Span,
    pub span: Span,
    pub generics: Vec<TyId>,
    pub variants: Vec<EnumVariantDef>,
}

#[derive(Clone, Debug)]
pub struct EnumVariantDef {
    pub name: Ustr,
    pub name_span: Span,
    pub payload: VariantPayload,
}

#[derive(Clone, Debug)]
pub enum VariantPayload {
    Unit,
    Tuple(Vec<TyId>),
    Record(Vec<StructFieldDef>),
}

#[derive(Copy, Clone, Debug)]
pub enum VariantArgs {
    Unit,
    Tuple(IdxRange<Expr>),
    Record(IdxRange<StructLitField>),
}

#[derive(Copy, Clone, Debug)]
pub struct GlobalDef {
    pub name: Ustr,
    pub name_span: Span,
    pub ty: TyId,
    pub span: Span,
    pub is_const: bool,
}

#[derive(Copy, Clone, Debug)]
pub enum Pattern {
    Wildcard,
    Bind(Idx<BindingInfo>),
    Literal(Idx<Expr>),

    Range {
        start: Option<Idx<Expr>>,
        end: Option<Idx<Expr>>,
        inclusive: bool,
    },

    EnumVariant {
        enum_name: Option<Ustr>,
        enum_name_span: Option<Span>,
        variant: Ustr,
        variant_span: Span,
        sub: IdxRange<Pattern>,
    },

    Record {
        enum_name: Option<Ustr>,
        enum_name_span: Option<Span>,
        name: Option<Ustr>,
        name_span: Option<Span>,
        fields: IdxRange<RecordPatField>,
        rest: bool,
    },

    Tuple {
        elements: IdxRange<Pattern>,
    },

    Or {
        alternatives: IdxRange<Pattern>,
    },
}

#[derive(Copy, Clone, Debug)]
pub struct MatchArm {
    pub pattern: Idx<Pattern>,
    pub body: Idx<Expr>,
    pub guard: Option<Idx<Expr>>,
}

#[derive(Copy, Clone, Debug)]
pub struct RecordPatField {
    pub name: Ustr,
    pub name_span: Span,
    pub pat: Idx<Pattern>,
}
