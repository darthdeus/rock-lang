//! Surface syntax and source spans stored in typed arenas.

use crate::OrderedFloat;
pub use comfy_la_arena::{Arena, Idx};
use rock_core::{CompilerError, Span};
use rock_tokenizer::TokenKind;
use ustr::Ustr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ident {
    pub span: Span,
    pub name: Ustr,
}

impl Ident {
    /// Decode a positional field, such as `.0`. The parser sets its span to the digits.
    pub fn tuple_index(&self) -> Option<usize> {
        self.name.parse::<usize>().ok()
    }
}

/// How a `FunctionCall` was written in source. Mirror of rock's
/// `CallSyntax` — a render hint only, never affecting resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallSyntax {
    Normal,
    At,
    NoParens,
    Macro,
    MethodMacro,
    AtMacro,
    AtMethodMacro,
    TypeArgs,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionCallArg {
    pub name: Option<Ident>,
    pub value: Idx<ExprData>,
    /// Macro splat `...rest` (expanded at macro-expansion time).
    pub is_splat: bool,
    /// Struct spread `..expr` — the operand's fields become named args at this
    /// position. Orthogonal to `is_splat`; the two are never both set.
    pub is_spread: bool,
}

/// Formatting options, including dynamic width and precision expressions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatSpec {
    pub precision: Option<usize>,
    pub width: Option<usize>,
    /// `{v:{w}f}` — dynamic width is a sub-expression (mutually exclusive with
    /// `width`). Lives in the same arena as the enclosing string.
    pub width_expr: Option<Idx<ExprData>>,
    /// `{v:.{p}f}` — dynamic precision (mutually exclusive with `precision`).
    pub precision_expr: Option<Idx<ExprData>>,
    pub alignment: Option<char>,
    pub fill: Option<char>,
    pub sign: Option<char>,
    pub alternate: bool,
    pub zero_pad: bool,
    pub grouping: Option<char>,
    pub format_type: Option<char>,
}

impl FormatSpec {
    pub fn new() -> Self {
        FormatSpec {
            precision: None,
            width: None,
            width_expr: None,
            precision_expr: None,
            alignment: None,
            fill: None,
            sign: None,
            alternate: false,
            zero_pad: false,
            grouping: None,
            format_type: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InterpolatedPart {
    Literal(Ustr),
    Expr(Idx<ExprData>, Option<FormatSpec>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Path {
    /// `::a::b` — anchored at the package root, bypassing local shadowing.
    pub from_root: bool,
    pub segments: Vec<Ident>,
}

impl Path {
    fn surface(&self) -> String {
        self.from_root
            .then_some("")
            .into_iter()
            .chain(self.segments.iter().map(|s| s.name.as_str()))
            .collect::<Vec<_>>()
            .join("::")
    }

    pub fn span(&self) -> Span {
        let first = self.segments.first().expect("path has segments").span;
        let last = self.segments.last().expect("path has segments").span;
        first.merge(&last)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExprData {
    pub span: Span,
    pub kind: ExprKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExprKind {
    Null,
    Unit,

    Hole(u32, Option<Ustr>),

    Ident(Ident),
    Path {
        path: Path,
        generic_args: Vec<Idx<TypeExprData>>,
    },
    PathStructLiteral {
        path: Path,
        bases: Vec<Idx<ExprData>>,
        fields: Vec<(Vec<Ident>, Idx<ExprData>)>,
    },

    Int(i64, Ustr),
    Float(OrderedFloat<f64>),
    String(Ustr),
    InterpolatedString(Vec<InterpolatedPart>),
    Bool(bool),

    Paren(Idx<ExprData>),

    As(Idx<ExprData>, Idx<TypeExprData>),
    Bitcast(Idx<ExprData>, Idx<TypeExprData>),
    ParenAscription(Idx<ExprData>, Idx<TypeExprData>),

    Not(Idx<ExprData>),
    BitNot(Idx<ExprData>),
    UnaryMinus(Idx<ExprData>),

    FieldAccess(Idx<ExprData>, Ident),
    SafeNavigation(Idx<ExprData>, Ident),
    SafeNavigationLet {
        receiver: Idx<ExprData>,
        binding: Ident,
        body: Idx<BlockData>,
    },
    Index(Idx<ExprData>, Idx<ExprData>),

    QuestionMark(Idx<ExprData>),
    PreIncrement(Idx<ExprData>),
    PostIncrement(Idx<ExprData>),
    PreDecrement(Idx<ExprData>),
    PostDecrement(Idx<ExprData>),
    Spread(Idx<ExprData>),

    FunctionCall {
        func: Idx<ExprData>,
        generic_args: Vec<Idx<TypeExprData>>,
        args: Vec<FunctionCallArg>,
        syntax: CallSyntax,
    },

    ArrayLiteral(Vec<Idx<ExprData>>),

    BinOp {
        left: Idx<ExprData>,
        op: TokenKind,
        op_span: Span,
        right: Idx<ExprData>,
    },

    Block(Idx<BlockData>),

    Tuple(Vec<Idx<ExprData>>),

    Match {
        scrutinee: Idx<ExprData>,
        arms: Vec<MatchArm>,
    },

    If {
        branches: Vec<(Idx<ExprData>, Idx<BlockData>)>,
        else_branch: Option<Idx<BlockData>>,
    },

    IfLetChain {
        clauses: Vec<IfLetClause>,
        then_block: Idx<BlockData>,
        else_block: Option<Idx<BlockData>>,
    },

    Assign {
        target: Idx<ExprData>,
        value: Idx<ExprData>,
    },

    ReturnExpr(Option<Idx<ExprData>>),
    BreakExpr,
    ContinueExpr,
    Yield(Option<Idx<ExprData>>),
    YieldDelegate(Idx<ExprData>),

    Lambda {
        params: Vec<LambdaParam>,
        return_ty: Option<Idx<TypeExprData>>,
        body: Idx<BlockData>,
    },

    SiblingFieldRef(Ident),

    FunctionRef(Ident),

    AnaphoricIdent(Ident),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LambdaParam {
    pub span: Span,
    pub name: Ident,
    pub ty: Option<Idx<TypeExprData>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Idx<ExprData>>,
    pub body: Idx<BlockData>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pattern {
    Wildcard,
    Ident(Ident),
    Literal(Idx<ExprData>),
    Tuple(Vec<Pattern>),
    PathVariant {
        path: Path,
        args: Option<Vec<Pattern>>,
    },
    PathRecord {
        path: Path,
        fields: Vec<(Ident, Pattern)>,
        rest: bool,
    },
    Range {
        start: Option<Idx<ExprData>>,
        end: Option<Idx<ExprData>>,
        inclusive: bool,
    },
    Or(Vec<Pattern>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockData {
    pub span: Span,
    /// Delimiters for written blocks. Synthetic blocks have no brace spans.
    pub braces: Option<(Span, Span)>,
    pub statements: Vec<Idx<StatementData>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatementData {
    pub span: Span,
    pub kind: StatementKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatementKind {
    Expression(Idx<ExprData>),
    Let(Let),
    Static(Static),
    Use(UseDecl),
    NestedFn(NestedFn),
    Return {
        keyword_span: Option<Span>,
        value: Option<Idx<ExprData>>,
    },
    Break {
        keyword_span: Option<Span>,
    },
    Continue {
        keyword_span: Option<Span>,
    },
    For(For),
    While(While),
    WhileLet(WhileLet),
    Defer(DeferKind),
    LifecycleHook {
        keyword_span: Option<Span>,
        name: Ustr,
        body: Idx<BlockData>,
    },
    Comment(Ustr),
}

/// One clause of an `if let` chain: a `let pat = value` binding or a plain
/// boolean condition between the `&&`s. Mirrors rock's `IfLetClause`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IfLetClause {
    Binding {
        pattern: Pattern,
        value: Idx<ExprData>,
    },
    Condition(Idx<ExprData>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Let {
    pub span: Span,
    pub keyword_span: Option<Span>,
    /// Any pattern parses here; bone rejects refutable ones without `else`.
    pub pattern: Pattern,
    pub ty: Option<Idx<TypeExprData>>,
    pub value: Option<Idx<ExprData>>,
    /// `let pat = value else { .. }` — taken when the pattern misses; must
    /// diverge (checked in bone-inference).
    pub else_block: Option<Idx<BlockData>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Static {
    pub span: Span,
    pub keyword_span: Option<Span>,
    pub name: Ident,
    pub ty: Option<Idx<TypeExprData>>,
    pub value: Idx<ExprData>,
}

/// A named block-bodied function declared in another function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NestedFn {
    pub span: Span,
    pub keyword_span: Option<Span>,
    pub name: Ident,
    pub params: Vec<Param>,
    /// `(` through `)`, so the formatter can find the comments inside.
    pub params_span: Span,
    pub return_ty: Option<Idx<TypeExprData>>,
    pub body: Idx<BlockData>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct For {
    pub span: Span,
    pub keyword_span: Option<Span>,
    /// Any pattern parses here; bone rejects refutable ones.
    pub pattern: Pattern,
    /// Optional element-type annotation: `for x: T in xs`.
    pub ty: Option<Idx<TypeExprData>>,
    pub iter: Idx<ExprData>,
    pub body: Idx<BlockData>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct While {
    pub span: Span,
    pub keyword_span: Option<Span>,
    pub condition: Idx<ExprData>,
    pub body: Idx<BlockData>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WhileLet {
    pub span: Span,
    pub keyword_span: Option<Span>,
    pub pattern: Pattern,
    pub value: Idx<ExprData>,
    pub body: Idx<BlockData>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeferKind {
    Block(Idx<BlockData>),
    Expression(Idx<ExprData>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeExprData {
    pub span: Span,
    pub kind: TypeExprKind,
    pub is_nullable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeExprKind {
    Any,
    Unit,
    String,
    Int,
    Uint,
    Float,
    F64,
    F32,
    Bool,
    Ref(Idx<TypeExprData>),
    Ident(Ident),
    Path(Path),
    Array(Idx<TypeExprData>),
    App {
        ctor: Ident,
        args: Vec<Idx<TypeExprData>>,
    },
    AppPath {
        ctor: Path,
        args: Vec<Idx<TypeExprData>>,
    },
    Tuple(Vec<Idx<TypeExprData>>),
    Function {
        params: Vec<Idx<TypeExprData>>,
        ret: Idx<TypeExprData>,
    },
    Hole(u32, Option<Ustr>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Param {
    pub span: Span,
    pub name: Ident,
    pub ty: Idx<TypeExprData>,
    pub default: Option<Idx<ExprData>>,
    pub is_variadic: bool,
    /// Leading `@name` / `@name(args)` on the parameter. Preserved for the
    /// bone-side registry pass; no skull consumer reads it yet.
    pub annotations: Vec<Annotation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Generic {
    pub name: Ident,
    pub bound: Option<Idx<TypeExprData>>,
    pub default: Option<Idx<TypeExprData>>,
}

/// Concrete target of an inline or foreign declaration. `Any` is portable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FnBackend {
    Any,
    Lua,
    Js,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FunctionBody {
    Rock(Idx<BlockData>),
    Inline(FnBackend, Ustr),
    ExternLua,
    ExternJs,
    Ffi(Ustr),
}

impl FunctionBody {
    /// The Rock block, if this is a Rock-bodied fn (rock: `defined_block`).
    pub fn rock_block(&self) -> Option<Idx<BlockData>> {
        match self {
            FunctionBody::Rock(b) => Some(*b),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JsonFieldOperation {
    Default,
    Omit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionDef {
    pub span: Span,
    pub keyword_span: Option<Span>,
    pub name: Ident,
    pub generics: Vec<Generic>,
    pub params: Vec<Param>,
    /// `(` through `)`, so the formatter can find the comments inside.
    pub params_span: Span,
    pub return_ty: Option<Idx<TypeExprData>>,
    pub body: FunctionBody,
    pub is_macro: bool,
    pub is_coroutine: bool,
    /// Compiler-generated declaration.
    pub is_synthetic: bool,
    /// A generated default or omission function for one JSON field.
    pub json_field_for: Option<(Ident, Ustr, JsonFieldOperation)>,
    pub macro_items: Vec<Item>,
    /// Written receiver tail and the formatter's split in the generics list.
    pub receiver_surface: Option<(Ident, usize)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructDef {
    pub span: Span,
    pub keyword_span: Option<Span>,
    pub name: Ident,
    pub generics: Vec<Generic>,
    pub fields: Vec<Field>,
    pub is_value: bool,
    pub is_nobox: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub span: Span,
    pub name: Ident,
    pub ty: Idx<TypeExprData>,
    /// Declared visibility (v1). `None` = public read/write.
    pub vis: Option<Visibility>,
    /// `private(set)` — restricted setter on a readable field.
    pub set_private: bool,
    pub default: Option<Idx<ExprData>>,
    /// Leading `@name` / `@name(args)` on the field. Preserved for the
    /// bone-side registry pass; no skull consumer reads it yet.
    pub annotations: Vec<Annotation>,
    pub is_embed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumDef {
    pub span: Span,
    pub keyword_span: Option<Span>,
    pub name: Ident,
    pub generics: Vec<Generic>,
    pub variants: Vec<EnumVariant>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumVariant {
    pub span: Span,
    pub name: Ident,
    pub kind: EnumVariantKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnumVariantKind {
    Unit,
    Tuple(Vec<Idx<TypeExprData>>),
    Record(Vec<Field>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewtypeDef {
    pub span: Span,
    pub name: Ident,
    pub generics: Vec<Generic>,
    pub wrapped: Idx<TypeExprData>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecoratorDef {
    pub span: Span,
    pub name: Ident,
    pub params: Vec<Param>,
    pub body: Idx<BlockData>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalDef {
    pub span: Span,
    pub keyword_span: Option<Span>,
    pub name: Ident,
    pub ty: Option<Idx<TypeExprData>>,
    pub value: Option<Idx<ExprData>>,
    pub is_const: bool,
}

/// `#if "cond"` / `#elseif "cond"` / `#else` / `#end` — each a standalone item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompilerDirective {
    If(Ustr),
    ElseIf(Ustr),
    Else,
    End,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Annotation {
    pub span: Span,
    pub name: Ident,
    pub args: Vec<AnnoArg>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnnoArg {
    pub name: Option<Ident>,
    pub value: Idx<ExprData>,
}

/// Declared visibility (modules-v1 grammar). `None` on an item means
/// "not written" and takes the item-kind's default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Visibility {
    Private,
    Pub,
    PubSuper,
    PubPackage,
    PubIn(Vec<Ident>),
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct VisMods {
    pub vis: Option<Visibility>,
    /// `private(set)` — public read (per `vis`), owner-subtree write.
    pub set_private: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleHeader {
    /// Package-root-relative module path (`mod combat::casting;` -> [combat,
    /// casting]). `mod self;` yields an empty vec (explicit root).
    pub path: Vec<Ident>,
    pub vis: Option<Visibility>,
    pub span: Span,
}

/// `use` declarations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UseDecl {
    pub span: Span,
    pub vis: Option<Visibility>,
    pub tree: UseTree,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UseTree {
    Path {
        from_root: bool,
        segments: Vec<Ident>,
        kind: UseSuffix,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UseSuffix {
    Bind,
    BindAs(Ident),
    Glob,
    Operator { op: Ident, alias: Option<Ident> },
    Nested(Vec<UseTree>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub span: Span,
    pub annotations: Vec<Annotation>,
    pub kind: ItemKind,
    /// Declared visibility (modules-v1). `None` = not written.
    pub vis: Option<Visibility>,
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Function(FunctionDef),
    Struct(StructDef),
    Enum(EnumDef),
    Newtype(NewtypeDef),
    Decorator(DecoratorDef),
    Global(GlobalDef),
    CompilerDirective(CompilerDirective),
    Comment(Ustr),
    Module(ModBlock),
    Use(UseDecl),
    /// A top-level macro invocation `name(args)` / `name!(args)` (sigil enables
    /// type-shaped args). Expansion replaces it with the macro's spliced items
    /// (item-macro) or drops it; it never survives into a cooked module.
    MacroInvocation {
        name: Ident,
        args: Vec<Idx<ExprData>>,
        sigil: bool,
    },
}

/// `pub mod name { .. }` — an inline module block (v1 grammar).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModBlock {
    pub span: Span,
    pub name: Ident,
    pub vis: Option<Visibility>,
    pub items: Vec<Item>,
}

/// One parsed module: the arenas backing every node plus the top-level
/// item list. Children reference each other by `Idx` into these arenas.
#[derive(Clone, Debug, Default)]
pub struct Ast {
    pub file: Ustr,
    /// The owning package (set by the loader from the package graph; `None`
    /// for standalone/editor sources). Package loaders stamp graph facts here.
    pub package: Option<rock_core::source_code::PackageTag>,
    /// The file's `mod a::b;` header (v1), if the first non-comment item was
    /// one. Selects the logical module this file's items contribute to.
    pub module_header: Option<ModuleHeader>,
    pub global_policy: rock_core::source_code::GlobalPolicy,
    pub exprs: Arena<ExprData>,
    pub stmts: Arena<StatementData>,
    pub blocks: Arena<BlockData>,
    pub types: Arena<TypeExprData>,
    pub items: Vec<Item>,
    /// Item and statement errors from recovery. A nonempty list rejects compilation.
    pub recovery_errors: Vec<CompilerError>,
    /// Declaration and use spans from macro templates, before expansion rewrites them.
    pub macro_param_refs: Vec<MacroParamRef>,
}

#[derive(Clone, Debug)]
pub struct MacroParamRef {
    pub name: Ustr,
    pub spans: Vec<Span>,
}

impl Ast {
    pub fn new() -> Self {
        Self::default()
    }

    /// Size the arenas from the token count to avoid growing them during parsing.
    pub fn with_capacity(tokens: usize) -> Self {
        Ast {
            exprs: Arena::with_capacity(tokens / 2),
            stmts: Arena::with_capacity(tokens / 8),
            blocks: Arena::with_capacity(tokens / 32),
            types: Arena::with_capacity(tokens / 16),
            items: Vec::with_capacity(tokens / 16),
            ..Default::default()
        }
    }

    pub fn e(&self, idx: &Idx<ExprData>) -> &ExprData {
        &self.exprs[*idx]
    }

    pub fn s(&self, idx: &Idx<StatementData>) -> &StatementData {
        &self.stmts[*idx]
    }

    pub fn te(&self, idx: &Idx<TypeExprData>) -> &TypeExprData {
        &self.types[*idx]
    }

    pub fn b(&self, idx: &Idx<BlockData>) -> &BlockData {
        &self.blocks[*idx]
    }

    pub fn expr_kind(&self, idx: Idx<ExprData>) -> &ExprKind {
        &self.exprs[idx].kind
    }

    pub fn expr(&mut self, span: Span, kind: ExprKind) -> Idx<ExprData> {
        self.exprs.alloc(ExprData { span, kind })
    }

    pub fn stmt(&mut self, span: Span, kind: StatementKind) -> Idx<StatementData> {
        self.stmts.alloc(StatementData { span, kind })
    }

    pub fn block(
        &mut self,
        span: Span,
        braces: Option<(Span, Span)>,
        statements: Vec<Idx<StatementData>>,
    ) -> Idx<BlockData> {
        self.blocks.alloc(BlockData {
            span,
            braces,
            statements,
        })
    }

    pub fn ty(&mut self, span: Span, kind: TypeExprKind, is_nullable: bool) -> Idx<TypeExprData> {
        self.types.alloc(TypeExprData {
            span,
            kind,
            is_nullable,
        })
    }
}

impl Ast {
    /// Render a type for documentation and editor signatures.
    pub fn type_surface(&self, t: Idx<TypeExprData>) -> String {
        let td = &self.types[t];
        let base = match &td.kind {
            TypeExprKind::Any => "any".to_string(),
            TypeExprKind::Unit => "()".to_string(),
            TypeExprKind::String => "string".to_string(),
            TypeExprKind::Int => "int".to_string(),
            TypeExprKind::Uint => "uint".to_string(),
            TypeExprKind::Float => "float".to_string(),
            TypeExprKind::F64 => "f64".to_string(),
            TypeExprKind::F32 => "f32".to_string(),
            TypeExprKind::Bool => "bool".to_string(),
            TypeExprKind::Ref(inner) => format!("ref {}", self.type_surface(*inner)),
            TypeExprKind::Hole(..) => "?".to_string(),
            TypeExprKind::Ident(id) => id.name.to_string(),
            TypeExprKind::Path(path) => path.surface(),
            TypeExprKind::Array(inner) => format!("[{}]", self.type_surface(*inner)),
            TypeExprKind::AppPath { ctor, args } => format!(
                "{}<{}>",
                ctor.surface(),
                args.iter()
                    .map(|&a| self.type_surface(a))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            TypeExprKind::Tuple(ts) => format!(
                "({})",
                ts.iter()
                    .map(|&t| self.type_surface(t))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            TypeExprKind::App { ctor, args } if args.is_empty() => ctor.name.to_string(),
            TypeExprKind::App { ctor, args } => format!(
                "{}<{}>",
                ctor.name,
                args.iter()
                    .map(|&a| self.type_surface(a))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            TypeExprKind::Function { params, ret } => format!(
                "({}) -> {}",
                params
                    .iter()
                    .map(|&p| self.type_surface(p))
                    .collect::<Vec<_>>()
                    .join(", "),
                self.type_surface(*ret)
            ),
        };
        if td.is_nullable {
            if matches!(td.kind, TypeExprKind::Ref(_)) {
                format!("({base})?")
            } else {
                format!("{base}?")
            }
        } else {
            base
        }
    }
}
