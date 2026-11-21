use std::sync::OnceLock;

use rust_lapper::Interval;
pub use skull_parser::FnBackend;
use skull_parser::Ident;

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
    /// Field storage only; reads and expressions use `Float`.
    F32,
    Bool,
    String,
    Never,
    // Inference-recovery poison type.
    Error,
}

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

/// Index-free default for a trailing generic parameter, so each use can
/// synthesize it in its own type arena without cross-arena copying.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GenericDefault {
    Builtin(BuiltinTy),
    Named(Ustr),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TypeExpr {
    Builtin(BuiltinTy),
    // Declared generic parameter.
    Generic(VarId, Ustr),
    // Call-site generic instantiation.
    Infer(VarId, Ustr),
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TypeHead {
    Nullable,
    App(TypeCtor),
    Function,
    Tuple,
}

#[derive(Clone, Copy)]
pub struct TypeShape {
    pub head: TypeHead,
    pub params: IdxRange<TypeExpr>,
    pub tail: Option<Idx<TypeExpr>>,
}

impl TypeShape {
    pub fn rebuild(self) -> TypeExpr {
        let Self { head, params, tail } = self;
        match head {
            TypeHead::Nullable => TypeExpr::Nullable(tail.unwrap()),
            TypeHead::App(ctor) => TypeExpr::App { ctor, params },
            TypeHead::Function => TypeExpr::Function {
                params,
                ret: tail.unwrap(),
            },
            TypeHead::Tuple => TypeExpr::Tuple { elements: params },
        }
    }
}

impl TypeExpr {
    pub fn shape(self) -> Option<TypeShape> {
        let (head, params, tail) = match self {
            Self::Nullable(inner) => (TypeHead::Nullable, IdxRange::default(), Some(inner)),
            Self::App { ctor, params } => (TypeHead::App(ctor), params, None),
            Self::Function { params, ret } => (TypeHead::Function, params, Some(ret)),
            Self::Tuple { elements } => (TypeHead::Tuple, elements, None),
            Self::Builtin(_) | Self::Generic(..) | Self::Infer(..) => return None,
        };
        Some(TypeShape { head, params, tail })
    }

    pub fn equivalent(
        self,
        other: Self,
        same_variable_scope: impl FnOnce() -> bool,
        mut same_child: impl FnMut(Idx<Self>, Idx<Self>) -> bool,
    ) -> bool {
        match (self, other) {
            (Self::Builtin(a), Self::Builtin(b)) => a == b,
            (Self::Generic(a, _), Self::Generic(b, _)) | (Self::Infer(a, _), Self::Infer(b, _)) => {
                a == b && same_variable_scope()
            }
            _ => {
                let (Some(a), Some(b)) = (self.shape(), other.shape()) else {
                    return false;
                };
                a.head == b.head
                    && a.params.len() == b.params.len()
                    && a.tail.zip(b.tail).is_none_or(|(x, y)| same_child(x, y))
                    && a.params.zip(b.params).all(|(x, y)| same_child(x, y))
            }
        }
    }
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

#[derive(Clone, Debug, Default)]
pub struct Body {
    /// Bumped on every re-lower. Batch-lowering leaves 0.
    pub generation: u64,

    pub scope_arena: Arena<Scope>,
    pub lexical_facts: Option<Box<LexicalFacts>>,

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
    /// Name spans kept out of line to keep `Expr` small.
    pub name_spans: Arena<Span>,

    pub lowered_tys: Arena<TypeExpr>,
    pub lowered_ty_spans: ArenaMap<Idx<TypeExpr>, Span>,

    pub arg_names: FxHashMap<Idx<Expr>, Ustr>,
    pub method_call_recvs: FxHashSet<Idx<Expr>>,
    /// Targets shared by several control-flow edges; branch-local narrowing
    /// does not hold on every arrival.
    pub shared_branch_targets: FxHashSet<Idx<Expr>>,
    /// Written method name keyed by receiver; a function-valued field can win
    /// over the resolved callee family.
    pub method_names: FxHashMap<Idx<Expr>, Ustr>,
    /// Struct-spread call operands, keyed by expression. Inference resolves
    /// their fields as named args at this position.
    pub spread_args: FxHashSet<Idx<Expr>>,

    pub root: Option<Idx<Expr>>,
    pub root_scope: Option<Idx<Scope>>,

    pub param_bindings: IdxRange<BindingInfo>,
    pub param_defaults: Vec<Option<Idx<Expr>>>,

    pub deco_target: Option<Idx<BindingInfo>>,

    pub field_defaults: FxHashMap<Ustr, Idx<Expr>>,

    pub node_index_cell: OnceLock<Lapper<usize, NodeRef>>,

    pub node_intervals: Vec<Interval<usize, NodeRef>>,

    pub path_segments: Vec<PathSegmentRef>,
}

#[derive(Clone, Debug, Default)]
pub struct LexicalFacts {
    pub regions: Vec<(Span, Idx<Scope>, bool)>,
    /// Keep earlier declarations after the resolver's binding map overwrites them.
    pub bindings: ArenaMap<Idx<BindingInfo>, (Idx<Scope>, usize)>,
}

/// One resolved path segment, stored so editor features need not re-resolve it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathSegmentRef {
    pub span: Span,
    pub target: SegmentTarget,
    /// This segment is an import alias.
    pub alias: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentTarget {
    /// A module node, identified by package and node key.
    Module {
        package: crate::modules::PackageId,
        node: Ustr,
    },
    Def(Ustr),
    Variant {
        enum_key: Ustr,
        variant: Ustr,
    },
}

impl Body {
    pub fn shrink_to_fit(&mut self) {
        self.scope_arena.shrink_to_fit();
        self.binding_arena.shrink_to_fit();
        self.binding_spans.shrink_to_fit();
        self.pattern_arena.shrink_to_fit();
        self.match_arm_arena.shrink_to_fit();
        self.record_pat_field_arena.shrink_to_fit();
        self.interp_part_arena.shrink_to_fit();
        self.struct_lit_field_arena.shrink_to_fit();
        self.statement_arena.shrink_to_fit();
        self.statement_spans.shrink_to_fit();
        self.expr_arena.shrink_to_fit();
        self.expr_spans.shrink_to_fit();
        self.name_spans.shrink_to_fit();
        self.lowered_tys.shrink_to_fit();
        self.lowered_ty_spans.shrink_to_fit();
        self.arg_names.shrink_to_fit();
        self.method_call_recvs.shrink_to_fit();
        self.method_names.shrink_to_fit();
        self.spread_args.shrink_to_fit();
        self.field_defaults.shrink_to_fit();
    }

    pub fn make_lapper(&mut self, start: usize, stop: usize, val: NodeRef) {
        self.node_intervals.push(Interval { start, stop, val })
    }

    pub fn node_index(&self) -> &Lapper<usize, NodeRef> {
        self.node_index_cell
            .get_or_init(|| Lapper::new(self.node_intervals.clone()))
    }

    pub fn alloc_root_scope(&mut self) -> Idx<Scope> {
        self.alloc_root_scope_with_bindings(Default::default())
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
    StaticTy { mid: ModuleId, idx: Idx<TypeExpr> },
    Expr(Idx<Expr>),
    Statement(Idx<Statement>),
    Binding(Idx<BindingInfo>),
    LocalTy(Idx<TypeExpr>),
}

#[derive(Clone, Debug, Default)]
pub struct Module {
    pub id: ModuleId,
    pub lower_var_high_water: u32,
    /// Previous lowering's types retained for cached inference in other modules.
    pub ty_arena_live_start: u32,
    /// Variable ID floor for this lowering, preserving IDs on re-lower.
    pub lower_var_floor: u32,

    pub file: Ustr,
    pub package: Option<Ustr>,
    /// Package-relative path used in generated code.
    pub logical_file: Ustr,
    pub identity_storage: skull_parser::IdentityStorage,

    pub functions: Arena<Function>,
    pub functions_by_name: FxHashMap<Ustr, FnList>,

    pub structs: Arena<StructDef>,
    pub enums: Arena<EnumDef>,
    pub newtypes: Arena<NewtypeDef>,
    pub nominals: Nominals,

    pub globals: Arena<GlobalDef>,
    pub global_by_name: FxHashMap<Ustr, Idx<GlobalDef>>,

    pub bodies: FxHashMap<BodyOwner, Body>,

    pub ty_arena: Arena<TypeExpr>,
    pub ty_spans: ArenaMap<Idx<TypeExpr>, Span>,

    pub decorators: Arena<DecoratorDef>,

    pub item_index_cell: OnceLock<Lapper<usize, BodyOwner>>,
    pub item_intervals: Vec<Interval<usize, BodyOwner>>,

    /// Body errors keyed so a body-only re-lower replaces that body's errors.
    pub lowering_errors: FxHashMap<BodyOwner, Vec<CompilerError>>,
    pub interface_errors: Vec<CompilerError>,
    /// Namespace facts read by lowering; body-only re-lowers add to the set.
    pub scope_probes: FxHashSet<crate::modules::ScopeProbe>,

    /// Members of merged bare-name call families used by this module.
    pub call_families: FxHashMap<Ustr, Vec<FamilyMember>>,
    /// Families used by implicit iterator-protocol calls.
    pub iterator_families: Option<[Result<CallFamily, String>; 2]>,

    /// Resolved path segments from imports and interface positions.
    pub path_segments: Vec<PathSegmentRef>,
}

impl Module {
    pub fn live_type_exprs(&self) -> impl Iterator<Item = &TypeExpr> {
        self.ty_arena
            .values()
            .skip(self.ty_arena_live_start as usize)
    }

    pub fn make_lapper(&mut self, start: usize, stop: usize, val: BodyOwner) {
        self.item_intervals
            .push(rust_lapper::Interval { start, stop, val })
    }

    pub fn item_index(&self) -> &Lapper<usize, BodyOwner> {
        self.item_index_cell
            .get_or_init(|| Lapper::new(self.item_intervals.clone()))
    }

    pub fn insert_body(&mut self, owner: BodyOwner, mut body: Body) {
        // Bodies survive until emission; discard arena growth slack.
        body.shrink_to_fit();
        self.bodies.insert(owner, body);
    }

    pub fn body_at(&self, offset: usize) -> Option<BodyOwner> {
        self.item_index()
            .find(offset, offset + 1)
            .min_by_key(|x| x.stop - x.start)
            .map(|x| x.val)
    }

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
    /// Hoisted local helper; codegen routes references through `__local_*`.
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
    /// Iterator element type for each generic bound; checked via `next(C)` at
    /// instantiation.
    pub generic_bounds: Vec<Option<TyId>>,
    pub param_decls: Vec<ParamDecl>,

    pub num_required: usize,

    pub return_ty: Option<TyId>,
    /// Target backend and verbatim body for `inline(<backend>)` declarations.
    pub inline_body: Option<(FnBackend, Ustr)>,
    pub is_extern: bool,
    pub lua_extern_path: Option<Ustr>,
    /// Backend used to select the emitted variant. Same-signature declarations
    /// for different backends share one overload candidate.
    pub backend: FnBackend,
    pub annotations: Vec<Annotation>,
    pub ffi_lib: Option<Ustr>,
    /// Parser-derived operator; user-facing lints skip it.
    pub is_synthetic: bool,
    /// A generated default or omission function for one JSON field.
    pub json_field_for: Option<(Ustr, Ustr, skull_parser::JsonFieldOperation)>,
}

impl Function {
    pub fn has_iterator_bounds(&self) -> bool {
        self.generic_bounds.iter().any(Option::is_some)
    }
}

#[derive(Clone, Debug)]
pub struct Annotation {
    pub span: Span,
    pub name: Ident,
    pub args: Vec<AnnoArg>,
    /// Canonical v1 decorator identity when this name resolved through the
    /// module namespace. `None` preserves compiler-owned names.
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

impl Body {
    pub fn pattern_single_bind(&self, p: Idx<Pattern>) -> Option<Idx<BindingInfo>> {
        match self.pattern_arena[p] {
            Pattern::Bind(b) => Some(b),
            _ => None,
        }
    }

    /// Every binding a pattern introduces, in source order.
    pub fn pattern_bindings(&self, p: Idx<Pattern>) -> Vec<Idx<BindingInfo>> {
        use crate::hir_walk::HirChild;

        let mut out = Vec::new();
        let mut collect = |node| {
            let HirChild::Pattern(p) = node else {
                return false;
            };
            if let Pattern::Bind(binding) = self.pattern_arena[p] {
                out.push(binding);
            }
            true
        };
        let root = HirChild::Pattern(p);
        collect(root);
        self.walk_descendants(root, &mut collect);

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

/// Ascription supplies an expected type; conversion infers its source independently.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum AsKind {
    Ascribe,
    Cast,
    Bitcast,
}

// Keep `Expr` small; spans and other wide payloads live in side tables.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<Expr>() <= 56);

#[derive(Copy, Clone, Debug)]
pub enum Expr {
    Unit,
    Null,
    /// A failed name lookup retained for editor queries; invalid HIR is not emitted.
    Error,
    Hole(Option<Ustr>),

    Int(i64),
    Float(OrderedFloat<f64>),
    Bool(bool),
    String(Ustr),
    Var(Idx<BindingInfo>),
    Tuple(IdxRange<Expr>),

    InterpolatedString(IdxRange<InterpPart>),
    /// `!x`; inference reads the family only for a non-bool operand.
    Not(Idx<Expr>, CallFamily),
    BitNot(Idx<Expr>),

    /// `-x`; inference reads the family only for a non-number, non-newtype.
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
        /// Resolved family for the written method name.
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
        /// Explicit type arguments; empty when inferred.
        generic_args: IdxRange<TypeExpr>,
        /// Resolver family; its key matches the unresolved callee name.
        family: CallFamily,
    },

    Array(IdxRange<Expr>),

    StructLiteral {
        name: Ustr,
        fields: IdxRange<StructLitField>,
        /// Functional-update bases, evaluated before fields; empty for a plain literal.
        bases: IdxRange<Expr>,
    },

    FieldAccess {
        base: Idx<Expr>,
        field: Ustr,
    },

    EnumVariant {
        enum_name: Ustr,
        enum_name_span: Idx<Span>,
        variant: Ustr,
        variant_span: Idx<Span>,
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

    /// Compound assignment for targets that cannot be captured as a place
    /// (such as safe-navigation chains).
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
    /// `name_span` indexes [`Body::name_spans`].
    Unresolved {
        name: Ustr,
        name_span: Idx<Span>,
    },
    Local(Idx<BindingInfo>),
    Value(Idx<Expr>),
}

/// One member of a call family: the overloads registered under `key`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FamilyMember {
    pub key: Ustr,
    pub tier: FamilyTier,
}

/// Lookup tier contributing a family member; nearer tiers break exact-fit ties.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FamilyTier {
    Own,
    Imports,
    Globs,
    Std,
}

#[derive(Copy, Clone, Debug)]
pub enum CallFamily {
    /// The definition the tiers bind, by canonical DefKey.
    Resolved(Ustr),
    /// Members are stored by name in the source module; optional lookups may
    /// produce an empty family.
    Merged { name: Ustr, file: Ustr },
}

impl CallFamily {
    pub fn key(&self) -> Ustr {
        match self {
            CallFamily::Resolved(key) => *key,
            CallFamily::Merged { name, .. } => *name,
        }
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

/// Binary declaration names, source symbols, and whether use-site calls can
/// resolve to user functions. Keep total with the parser's operator spellings.
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

pub const UNARY_OPERATOR_FNS: &[(&str, &str)] = &[("__unm", "-"), ("__not", "!")];

pub fn operator_entry(name: &str) -> Option<(&'static str, bool)> {
    OPERATOR_FNS
        .iter()
        .find(|(fn_name, _, _)| *fn_name == name)
        .map(|(_, symbol, overloadable)| (*symbol, *overloadable))
}

pub fn unary_operator_entry(name: &str) -> Option<&'static str> {
    UNARY_OPERATOR_FNS
        .iter()
        .find(|(fn_name, _)| *fn_name == name)
        .map(|(_, symbol)| *symbol)
}

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
    /// `let pattern = value [else { .. }]`. Without `else` the pattern must be
    /// irrefutable; the `else` block runs on a miss and must diverge.
    Let {
        pattern: Idx<Pattern>,
        value: Idx<Expr>,
        else_block: Option<Idx<Expr>>,
    },

    Static {
        name: Idx<BindingInfo>,
        value: Idx<Expr>,
    },

    /// Hoisted helper; params and body use a child scope with no captures.
    Fn {
        name: Idx<BindingInfo>,
        params: IdxRange<BindingInfo>,
        ret_ty: Option<TyId>,
        body: Idx<Expr>,
    },

    Return(Option<Idx<Expr>>),
    Expr(Idx<Expr>),

    Defer(Idx<Expr>),

    /// Nontrivial patterns lower through a hidden element binding and a `let`.
    For {
        pattern: Idx<Pattern>,
        iter: Idx<Expr>,
        body: Idx<Expr>,
    },

    While {
        cond: Idx<Expr>,
        body: Idx<Expr>,
    },

    Comment(Ustr),

    Break,
    Continue,
}

#[derive(Clone, Debug)]
pub struct DecoratorDef {
    pub name: Ustr,
    /// Canonical v1 identity; compiler-owned decorators keep their source name.
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
    pub consume: bool,
    /// `@nomangle`: preserve the declared name in emitted type symbols.
    pub nomangle: bool,
    pub span: Span,
    pub generics: Vec<TyId>,
    pub fields: Vec<StructFieldDef>,
}

#[derive(Clone, Debug)]
pub struct StructFieldDef {
    pub name: Ustr,
    pub ty: TyId,
    /// Stored representation; reads and writes use `ty` instead.
    pub storage_ty: TyId,
    pub span: Span,
    pub has_default: bool,
    pub annotations: Vec<Annotation>,
    /// Embedded fields project leaves from the parent and are never storable.
    pub is_embed: bool,
    /// Getter visibility. Default is public.
    pub read_vis: NsVisibility,
    /// Setter visibility. `private(set)` restricts this to owner subtree.
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

    /// Transparent newtype pattern; binds through without a runtime test.
    Newtype {
        name: Ustr,
        name_span: Span,
        sub: Idx<Pattern>,
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
