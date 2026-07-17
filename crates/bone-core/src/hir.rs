use std::sync::OnceLock;

use rust_lapper::Interval;
use skull_parser::Ident;

use crate::prelude::*;

#[derive(Copy, Clone, Debug)]
pub struct Builtins {
    pub int: Idx<TypeExpr>,
    pub float: Idx<TypeExpr>,
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

    pub fn float(&self) -> TyId {
        TyId::Static(ModuleId::builtin(), self.float)
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
    Float,
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
    Nominal(Ustr),
}

impl TypeCtor {
    pub fn for_name(name: Ustr) -> TypeCtor {
        match name.as_str() {
            "Vec" => TypeCtor::Array,
            "Map" => TypeCtor::Map,
            _ => TypeCtor::Nominal(name),
        }
    }
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
    /// Call args written as a struct spread `..op` (keyed by the operand's expr
    /// Idx). The operand's fields expand into named args at this position;
    /// resolved in bone-inference, codegen'd in skull-mir. Never also in
    /// `arg_names`.
    pub spread_args: FxHashSet<Idx<Expr>>,

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
            spread_args: Default::default(),

            root: Default::default(),
            root_scope: None,

            param_bindings: IdxRange::new_empty(),
            param_defaults: Default::default(),

            deco_target: None,

            field_defaults: Default::default(),

            node_index_cell: Default::default(),
            node_intervals: Default::default(),
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
}

impl Default for Module {
    fn default() -> Self {
        Self {
            id: ModuleId::builtin(),

            interface_generation: 0,
            lower_var_high_water: 0,

            file: Ustr::from(""),

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
    pub lua_body: Option<Ustr>,
    pub is_extern: bool,
    pub annotations: Vec<Annotation>,
    pub ffi_lib: Option<Ustr>,
}

#[derive(Clone, Debug)]
pub struct Annotation {
    pub span: Span,
    pub name: Ident,
    pub args: Vec<AnnoArg>,
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
    Not(Idx<Expr>),
    BitNot(Idx<Expr>),

    UnaryMinus(Idx<Expr>),

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

    CompoundAssign {
        target: Idx<Expr>,
        op_fn: Ustr,
        value: Idx<Expr>,
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
    pub span: Span,
    pub param_decls: Vec<ParamDecl>,
    pub num_required: usize,
}

#[derive(Clone, Debug)]
pub struct StructDef {
    pub name: Ustr,
    pub name_span: Span,
    pub is_value: bool,
    pub span: Span,
    pub generics: Vec<TyId>,
    pub fields: Vec<StructFieldDef>,
}

#[derive(Clone, Debug)]
pub struct StructFieldDef {
    pub name: Ustr,
    pub ty: TyId,
    pub span: Span,
    pub has_default: bool,
    pub annotations: Vec<Annotation>,
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
