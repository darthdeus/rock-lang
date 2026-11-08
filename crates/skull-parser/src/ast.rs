//! Arena-allocated surface AST for skull-parser.
//!
//! Every child edge is an arena `Idx` instead of a `Box`/`Vec<T>`, and
//! there is no `AstNodeId` — an `Idx<ExprData>` *is* the node's identity
//! within a module (plan decision 5). One `Ast` per parsed module
//! (decision 6).

use crate::OrderedFloat;
pub use comfy_la_arena::{Arena, Idx};
use rock_core::source_code::Dialect;
use rock_core::{CompilerError, Span};
use rock_tokenizer::TokenKind;
use ustr::Ustr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ident {
    pub span: Span,
    pub name: Ustr,
}

impl Ident {
    /// If this ident names a tuple/newtype positional field (`.0`, `.1`,
    /// ...), return the index. The parser emits `.N`/`?.N` as a
    /// `FieldAccess`/`SafeNavigation` with an all-digit field name; since
    /// Rock identifiers cannot begin with a digit, an all-digit name can
    /// only be such a synthetic index (and is always canonical, no leading
    /// zeros — it round-trips through `to_string`). This is the single
    /// source of truth for "is this a tuple index?" across resolution,
    /// codegen, the formatter, and the LSP.
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
    Binary(TokenKind),
    /// rock's no-parens call surface form (`foo a b` == `foo(a, b)`). Semantically
    /// identical to `Normal` -- every consumer that cares about call semantics
    /// treats it the same -- but kept distinct so the formatter can re-emit the
    /// no-parens spelling instead of normalizing to `foo(a, b)`.
    NoParens,
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

/// Mirror of rock's `FormatSpecifier`. Carries the static spec fields *and*
/// the dynamic width/precision sub-expressions (`{v:{w}.{p}f}`) so no embedded
/// expression is dropped. rock's `to_sexp` ignores the spec, so this data is
/// invisible to the cook oracle — but bone / skull codegen need it to emit the
/// correct formatting and to type-check the embedded width/precision exprs.
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
    /// An embedded `{expr[:spec]}`. The spec is `Some` only when a `:` format
    /// specifier was written; it never affects the s-expr (rock parity).
    Expr(Idx<ExprData>, Option<FormatSpec>),
}

/// A `::` path (modules-v1 grammar): `a::b::c`, `self::x`, `super::super::y`,
/// or the root-anchored `::a::b`. Resolution (owner package, lexical first
/// segment) is a lowering concern; the parser only records the segments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Path {
    /// `::a::b` — anchored at the package root, bypassing local shadowing.
    pub from_root: bool,
    pub segments: Vec<Ident>,
}

impl Path {
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

    /// `?` / `?name` typed hole in value position. `(positional id, optional
    /// name)`, mirroring rock. The id is drawn from the same per-module counter
    /// as `TypeExprKind::Hole` so the rendered `?{id}` matches rock's.
    Hole(u32, Option<Ustr>),

    Ident(Ident),
    QualifiedIdent {
        base: Ident,
        member: Ident,
    },
    /// `p::f`, `E::V`, `p::Type` — a modules-v1 value-position path. The
    /// generic args of `p::Type<T>` (a type used as a value, not followed by
    /// a call's parens) ride here; `p::f<T>(x)` puts them on the FunctionCall
    /// like an unqualified generic call.
    Path {
        path: Path,
        generic_args: Vec<Idx<TypeExprData>>,
    },
    /// `p::Type { f: v }` / `p::E::V { f: v }` — a qualified record literal.
    PathStructLiteral {
        path: Path,
        bases: Vec<Idx<ExprData>>,
        fields: Vec<(Vec<Ident>, Idx<ExprData>)>,
    },

    Int(i64, Ustr),
    Float(OrderedFloat<f64>),
    String(Ustr),
    /// `"text {expr} more"` — literal runs interleaved with embedded
    /// expressions. The format-spec is dropped (it does not affect s-exprs).
    InterpolatedString(Vec<InterpolatedPart>),
    Bool(bool),

    Paren(Idx<ExprData>),

    /// `expr as Type`
    As(Idx<ExprData>, Idx<TypeExprData>),
    /// bitcast<Type>(expr) preserves the low 32 bits between int and uint.
    Bitcast(Idx<ExprData>, Idx<TypeExprData>),
    /// `expr :: Type` type ascription
    TypeAscription(Idx<ExprData>, Idx<TypeExprData>),
    /// `(expr : Type)` — the modules-v1 contextual-ascription form. Same
    /// lowering as [`ExprKind::TypeAscription`]; a separate node because the
    /// spellings format differently (v1 rejects the infix form).
    ParenAscription(Idx<ExprData>, Idx<TypeExprData>),

    Not(Idx<ExprData>),
    BitNot(Idx<ExprData>),
    UnaryMinus(Idx<ExprData>),

    FieldAccess(Idx<ExprData>, Ident),
    SafeNavigation(Idx<ExprData>, Ident),
    /// `recv?.let { body }` — runs `body` with the unwrapped receiver bound to
    /// `binding` (the receiver's ident name, else `value`). The body is not part
    /// of the s-expr (rock parity).
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
        /// Explicit type arguments in `f<A, B>(...)`. Empty for inferred
        /// calls and for every operator/macro call that has no written types.
        generic_args: Vec<Idx<TypeExprData>>,
        args: Vec<FunctionCallArg>,
        syntax: CallSyntax,
    },

    ArrayLiteral(Vec<Idx<ExprData>>),

    /// Non-overloadable infix operators (`== < && ?? ::` ...). Overloadable
    /// ops (`+ - * / %`) are lowered to a `FunctionCall` with
    /// `CallSyntax::Binary`, exactly as rock's parser does.
    BinOp {
        left: Idx<ExprData>,
        op: TokenKind,
        right: Idx<ExprData>,
    },

    Block(Idx<BlockData>),

    Tuple(Vec<Idx<ExprData>>),

    /// `Name { f1: v1, f2 + f3: v, f4 }` — each entry is (field names, value);
    /// shorthand `{ x }` expands to `([x], Ident(x))`, mirroring rock.
    StructLiteral {
        name: Ident,
        /// `..base` operands (functional update), in source order, always
        /// preceding `fields`. Empty for a plain literal.
        bases: Vec<Idx<ExprData>>,
        fields: Vec<(Vec<Ident>, Idx<ExprData>)>,
    },

    /// `Enum::Variant { fields }` — qualified record-variant construction.
    EnumVariantRecord {
        enum_name: Ident,
        variant_name: Ident,
        fields: Vec<(Vec<Ident>, Idx<ExprData>)>,
    },

    Match {
        scrutinee: Idx<ExprData>,
        arms: Vec<MatchArm>,
    },

    If {
        branches: Vec<(Idx<ExprData>, Idx<BlockData>)>,
        else_branch: Option<Idx<BlockData>>,
    },

    /// `if let pat = value { .. } [else { .. }]` (single binding).
    IfLet {
        pattern: Pattern,
        value: Idx<ExprData>,
        then_block: Idx<BlockData>,
        else_block: Option<Idx<BlockData>>,
    },

    /// `if [cond &&]* let pat = value [&& (let .. | cond)]* { .. }` — a binding
    /// chain. rock keeps single bindings as `IfLet` and only uses this when
    /// there is more than one clause.
    IfLetChain {
        clauses: Vec<IfLetClause>,
        then_block: Idx<BlockData>,
        else_block: Option<Idx<BlockData>>,
    },

    Assign {
        target: Idx<ExprData>,
        value: Idx<ExprData>,
        /// `None` for `=`; `Some(op)` for compound `+= -= *= /=`.
        compound_op: Option<TokenKind>,
    },

    /// `return [expr]` / `break` / `continue` / `yield [expr]` in value
    /// position (statement forms are handled in `parse_statement`).
    ReturnExpr(Option<Idx<ExprData>>),
    BreakExpr,
    ContinueExpr,
    Yield(Option<Idx<ExprData>>),
    YieldDelegate(Idx<ExprData>),

    /// `param => body` / `(a, b) => body`. Body is always a block.
    Lambda {
        params: Vec<LambdaParam>,
        return_ty: Option<Idx<TypeExprData>>,
        body: Idx<BlockData>,
    },

    /// Leading `.field` — sibling field reference in a struct default.
    SiblingFieldRef(Ident),

    /// `&name` — function reference (renders `&name`).
    FunctionRef(Ident),

    /// `^name` — anaphoric ident (explicit caller-scope reference in a macro
    /// body). Renders as the bare `name` (rock side-tables the `^`, so the
    /// s-expr is a plain ident), but kept distinct so bone can resolve it to
    /// caller scope rather than hygienically.
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

/// Match pattern. Mirror of rock's `PatternKind` (span dropped — s-exprs
/// don't use it). Sub-patterns nest via `Vec` (no `Box` needed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pattern {
    Wildcard,
    Ident(Ident),
    Literal(Idx<ExprData>),
    Tuple(Vec<Pattern>),
    EnumVariant {
        enum_name: Option<Ident>,
        variant_name: Ident,
        args: Option<Vec<Pattern>>,
    },
    /// `p::E::V(..)` (modules-v1) — a variant pattern behind a path.
    PathVariant {
        path: Path,
        args: Option<Vec<Pattern>>,
    },
    Record {
        enum_name: Option<Ident>,
        name: Option<Ident>,
        fields: Vec<(Ident, Pattern)>,
        rest: bool,
    },
    /// `p::Type { f: pat, .. }` (modules-v1) — a record pattern behind a path.
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
    /// Real `{` / `}` token spans. `None` for synthetic braceless blocks
    /// (lambda `=> expr` bodies, `@`-trailing-lambda bodies, match-arm
    /// `=> expr` bodies, macro-expansion retagged blocks) -- consumers that
    /// mark brace positions (bone-lower's block lapper entries) must emit
    /// nothing for these rather than derive positions from `span`.
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
    /// `use a::b;` inside a block (v1 block scope). Scoped from its
    /// declaration to block end when module resolution lands.
    Use(UseDecl),
    NestedFn(NestedFn),
    /// `return [expr]` — `keyword_span` is the `return` token (rock parity).
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
    /// `on_resume`/`on_killed`/`on_hot_reload { .. }` lifecycle-hook block in a
    /// `co fn`. `name` is the hook keyword.
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

/// A `let` / `for` binding target. Mirrors rock's `VariableBindingKind`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Binding {
    Ident(Ident),
    Tuple(Vec<Binding>),
    Newtype { name: Ident, binding: Ident },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Let {
    pub span: Span,
    pub keyword_span: Option<Span>,
    pub binding: Binding,
    pub ty: Option<Idx<TypeExprData>>,
    pub value: Option<Idx<ExprData>>,
}

/// `static <name>[: T] = <init>` -- a local with private module-level storage
/// that persists across calls, initialized on first reach. The initializer is
/// mandatory (an immutable never-written static should be a `const`). Name is a
/// plain ident; no destructuring binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Static {
    pub span: Span,
    pub keyword_span: Option<Span>,
    pub name: Ident,
    pub ty: Option<Idx<TypeExprData>>,
    pub value: Idx<ExprData>,
}

/// `fn <name>(params) [-> ret] { body }` written inside a fn body -- hoisted to
/// a private top-level function, callable by name and passable as a zero-alloc
/// value. v1: no generics, no annotations,
/// decl-before-use (self-recursion ok, no forward/mutual recursion) -- the last
/// enforced in bone-lower, not here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NestedFn {
    pub span: Span,
    pub keyword_span: Option<Span>,
    pub name: Ident,
    pub params: Vec<Param>,
    pub return_ty: Option<Idx<TypeExprData>>,
    pub body: Idx<BlockData>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct For {
    pub span: Span,
    pub keyword_span: Option<Span>,
    pub var: Binding,
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
    /// `f64`: the explicit wide spelling of `float`, the same type.
    F64,
    /// `f32`: narrow float storage, legal only as a field type.
    F32,
    Bool,
    Ref(Idx<TypeExprData>),
    Ident(Ident),
    /// `a::b::T` (modules-v1) — a qualified type path.
    Path(Path),
    Array(Idx<TypeExprData>),
    App {
        ctor: Ident,
        args: Vec<Idx<TypeExprData>>,
    },
    /// `a::b::T<A, B>` (modules-v1) — a qualified generic application.
    AppPath {
        ctor: Path,
        args: Vec<Idx<TypeExprData>>,
    },
    Tuple(Vec<Idx<TypeExprData>>),
    Function {
        params: Vec<Idx<TypeExprData>>,
        ret: Idx<TypeExprData>,
    },
    /// `?` / `?name` type hole (`let x: ? = 42`) — inferred. `(positional id,
    /// optional name)`, mirroring rock. The s-expr renders a bare `?`
    /// regardless of payload, but bone needs the name and the id keeps the
    /// shared hole counter in lockstep with rock (so value-position `?{id}`
    /// holes render identically).
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

/// A generic parameter `T`, `T = Default`, or `I: Iterator<T>`. Mirrors rock's
/// `GenericParam` plus the bound: the only bound the compiler knows today is
/// the builtin `Iterator<T>` (validated in bone, not here).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Generic {
    pub name: Ident,
    pub bound: Option<Idx<TypeExprData>>,
    pub default: Option<Idx<TypeExprData>>,
}

/// Which backend a function's body / extern binding targets. `Lua` and `Js` are
/// the concrete codegen targets; a backend-specific body (`inline(lua)` /
/// `inline(js)`) or extern (`extern(lua)` / `extern(js)`) is emitted only by its
/// backend and inert on the others. `Any` is not produced by the parser (a plain
/// Rock body / `extern` / `extern(ffi,..)` carries no explicit backend tag here);
/// it exists so the HIR-side [`FnBackend`] the bone layer reuses can mark portable
/// decls that any backend may emit.
/// Declaration order is bone's `backend_rank` (`bone-inference/src/calls.rs`):
/// `Any` < `Lua` < `Js`, the preference order a backend-variant group collapses
/// to its representative by. `Ord` is derived from it so a key that includes a
/// backend cannot introduce a second one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FnBackend {
    Any,
    Lua,
    Js,
}

/// How a function's body is provided. Mirrors rock's `BodySource`: a Rock body
/// IS its block; non-Rock bodies (inline / extern) carry their own data and
/// have no block at all (no placeholder). Only Rock functions have a block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FunctionBody {
    /// `fn f() { .. }` — the Rock block (also a macro's expansion template).
    Rock(Idx<BlockData>),
    /// `inline(<backend>) fn .. = """<src>"""` — raw backend source, emitted
    /// verbatim by that backend. Bare `inline` defaults to [`FnBackend::Lua`].
    Inline(FnBackend, Ustr),
    /// `extern fn` — C cdef.
    ExternC,
    /// `extern(lua) fn` — named global Lua symbol.
    ExternLua,
    /// `extern(js) fn` — JS runtime-provided global symbol.
    ExternJs,
    /// `extern(ffi, "lib") fn` — FFI lib wrapper.
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionDef {
    pub span: Span,
    pub keyword_span: Option<Span>,
    pub name: Ident,
    pub generics: Vec<Generic>,
    pub params: Vec<Param>,
    pub return_ty: Option<Idx<TypeExprData>>,
    pub body: FunctionBody,
    pub is_macro: bool,
    pub is_coroutine: bool,
    /// Parser-synthesized, not user-written: the `@pure @inline fn __op_*(a: NT,
    /// b: NT)` operators derived for a newtype over a primitive (rock parity).
    /// Filtered out of the cook s-expr (the oracle compares user surface only).
    pub is_synthetic: bool,
    /// Item declarations in a macro body (`macro m() { fn f() {..} }`). Empty
    /// for non-macros and expression-only macros. Spliced at module scope when
    /// the macro is invoked as a top-level statement.
    pub macro_items: Vec<Item>,
    /// D-form surface (`fn Mob.take_damage(m, ..)`), set only by the parser:
    /// the receiver path ident as written + how many leading entries of
    /// `generics` came from the path (the print split point). The DESUGARED
    /// truth lives in the normal fields (generics merged, param 0 carries the
    /// receiver type); every consumer except skull-fmt must ignore this.
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
    /// `embed f: Foo` -- the field's leaves live in the parent and the field
    /// has no independently storable aggregate (projection-only).
    /// `embed` is CONTEXTUAL: `embed: int` is still a field named `embed`.
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

/// `@name` / `@name(args)` item annotation. Mirrors rock's `ItemAnnotation`;
/// arg values are parsed as expressions (rock reclassifies simple ones but
/// renders the same).
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
    /// `private` / `pub(self)` — the defining module and its descendants.
    Private,
    /// `pub` — any consumer with an accessible path.
    Pub,
    /// `pub(super)` — the parent module and its descendants.
    PubSuper,
    /// `pub(package)` — every module in the defining package.
    PubPackage,
    /// `pub(in a::b)` — the named ancestor module and its descendants. The
    /// path is package-root-relative.
    PubIn(Vec<Ident>),
}

/// The visibility modifiers one declaration may carry. Items use `vis`;
/// stored fields additionally use `set_private` (`private(set)`).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct VisMods {
    pub vis: Option<Visibility>,
    /// `private(set)` — public read (per `vis`), owner-subtree write.
    pub set_private: bool,
}

/// The file's module header: `mod a::b;` / `pub mod a::b;` — first
/// non-comment item only, `;`-terminated. The header selects which logical
/// module the REST of the file contributes to.
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
    /// `a::b::c` with a trailing shape. `from_root` marks `::a::b`. Segments
    /// may be the contextual words `self`/`super` (resolver's business).
    Path {
        from_root: bool,
        segments: Vec<Ident>,
        kind: UseSuffix,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UseSuffix {
    /// `use a::b;` — bind `b`.
    Bind,
    /// `use a::b as c;`
    BindAs(Ident),
    /// `use a::*;` — immediate accessible members.
    Glob,
    /// `use a::operator(+);` — the glyph family under a nameable selector;
    /// `op` is its internal family name (`__op_add`, or the unary family
    /// name for `!`). `-` uses its binary name as the selector anchor and the
    /// resolver follows its unary sibling. With `as add` the
    /// import binds only `add`, not the glyph.
    Operator { op: Ident, alias: Option<Ident> },
    /// `use a::b::{ .. }` — sub-trees, each relative (`c`, `d::e as f`,
    /// `self`, `operator(+)', nested braces).
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
    /// `pub mod x { .. }` — an inline module block (v1). Reopening a module
    /// adds fragments; the namespace tree coalesces them.
    Module(ModBlock),
    /// `use ...;` at item level (v1 scoping; parses in legacy too, additive).
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
    /// The module's source-file identity, as passed to `parse_module`. The id
    /// for the whole module -- independent of item spans (which macro expansion
    /// can retag) and defined even for an item-less module.
    pub file: Ustr,
    /// The source dialect this file was parsed under (set by the parser).
    pub dialect: Dialect,
    /// The owning package (set by the loader from the package graph; `None`
    /// for standalone/editor sources). Package loaders overwrite the
    /// directive-derived dialect with the graph fact at the same time.
    pub package: Option<rock_core::source_code::PackageTag>,
    /// The file's `mod a::b;` header (v1), if the first non-comment item was
    /// one. Selects the logical module this file's items contribute to.
    pub module_header: Option<ModuleHeader>,
    /// The owning package's mutable-global policy, stamped by the loader with
    /// the other package facts. `Any` keeps standalone sources and packages
    /// without the key on today's behavior.
    pub global_policy: rock_core::source_code::GlobalPolicy,
    pub exprs: Arena<ExprData>,
    pub stmts: Arena<StatementData>,
    pub blocks: Arena<BlockData>,
    pub types: Arena<TypeExprData>,
    pub items: Vec<Item>,
    /// Syntax errors the parser recovered from (item- and statement-level
    /// recovery dropped the malformed span to keep parsing). NON-EMPTY means the
    /// source did not parse cleanly. The parse is TOTAL -- `parse_module` always
    /// returns the best-effort `Ast` with these attached; strictness is a
    /// downstream policy (the CLI compile/test gate in `rock-cli` rejects on the
    /// first, while the interactive `Session`/LSP keep the partial tree and surface
    /// them as diagnostics). Empty for any valid program.
    pub recovery_errors: Vec<CompilerError>,
    /// Per macro parameter, the spans of its declaration and every use inside
    /// the macro's template (body + spliced items) -- recorded by the parser as
    /// it finishes a macro definition (`finish_fn_def` -> `index_macro_param_refs`),
    /// since the parser is the one phase that sees the template with its source
    /// spans (expansion retags clones onto the call site). Drives IDE
    /// references/rename for macro params. Pure metadata: not in the s-expr.
    pub macro_param_refs: Vec<MacroParamRef>,
}

/// One macro parameter's occurrences within its macro definition: the
/// declaration span followed by every use span in the template. Grouped per
/// (macro, parameter); an IDE query finds the group whose any span covers the
/// cursor and returns the lot.
#[derive(Clone, Debug)]
pub struct MacroParamRef {
    pub name: Ustr,
    pub spans: Vec<Span>,
}

impl Ast {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-size the arenas from the module's token count so a parse does a
    /// single up-front allocation per arena instead of the ~log2(N) doubling
    /// reallocations (each a memmove of the live elements) a from-empty grow
    /// incurs. Ratios measured over a corpus of real modules:
    /// exprs ~0.40, stmts ~0.09, types ~0.043, blocks ~0.028 of tokens; the
    /// divisors round up a margin so a typical module never re-grows. `items`
    /// is a plain `Vec` reserved by the same model.
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
