use crate::prelude::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ScopeId {
    Root(ModuleId),
    Block(AstNodeId),
    Type(AstNodeId),
}

impl ScopeId {
    /// The module this scope lives in.
    pub fn module_id(&self) -> ModuleId {
        match self {
            ScopeId::Root(module_id) => *module_id,
            ScopeId::Block(node_id) | ScopeId::Type(node_id) => node_id.module_id(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum ScopeSpan {
    Root,
    Span(Span),
}

#[derive(Clone, Debug)]
pub enum ScopeEntry {
    /// Every binding of the name still reachable here, in declaration order,
    /// so a shadowed binding is never lost.
    Var(Vec<SymbolId>),
    FnOverloads(Vec<SymbolId>),
}

impl ScopeEntry {
    pub fn contains(&self, sym_id: &SymbolId) -> bool {
        match self {
            ScopeEntry::Var(ids) => ids.contains(sym_id),
            ScopeEntry::FnOverloads(list) => list.contains(sym_id),
        }
    }
}

/// One lexical scope: the names it declares and the scope it is nested in.
#[derive(Clone, Debug)]
pub struct ScopeInfo {
    pub id: ScopeId,
    pub symbols: FxHashMap<Ustr, ScopeEntry>,
    pub parent: Option<ScopeId>,
    pub span: ScopeSpan,
}

impl ScopeInfo {
    /// The scope that holds a module's top-level names.
    pub fn new_root(module_id: ModuleId) -> Self {
        Self {
            id: ScopeId::Root(module_id),
            symbols: Default::default(),
            parent: None,
            span: ScopeSpan::Root,
        }
    }

    /// A scope nested in `parent`, covering `span` of the source.
    pub fn new_child(id: ScopeId, parent: ScopeId, span: Span) -> Self {
        Self {
            id,
            symbols: Default::default(),
            parent: parent.into(),
            span: ScopeSpan::Span(span),
        }
    }
}

/// The AST nodes that can carry a type.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TypedAstNodeId {
    VariableBinding(AstNodeId),
    TupleBinding(AstNodeId),
    NewtypeBinding(AstNodeId),

    Struct(AstNodeId),
    StructField(AstNodeId),
    StructLiteral(AstNodeId),

    ExternTable(AstNodeId),
    ExternTableField(AstNodeId),

    Expr(AstNodeId),
    TypeExpr(AstNodeId),
    Pattern(AstNodeId),

    Ident(AstNodeId),

    FunctionName(AstNodeId),
    FunctionParam(AstNodeId),

    Comment(AstNodeId),
    Return(AstNodeId),
    Break(AstNodeId),
    Continue(AstNodeId),

    For(AstNodeId),
    While(AstNodeId),
    WhileLet(AstNodeId),

    Global(AstNodeId),
    Let(AstNodeId),

    Synthetic(AstNodeId),
}

impl TypedAstNodeId {
    pub fn id(&self) -> AstNodeId {
        match self {
            TypedAstNodeId::VariableBinding(id) => *id,
            TypedAstNodeId::TupleBinding(id) => *id,
            TypedAstNodeId::NewtypeBinding(id) => *id,

            TypedAstNodeId::Struct(id) => *id,
            TypedAstNodeId::StructField(id) => *id,
            TypedAstNodeId::StructLiteral(id) => *id,

            TypedAstNodeId::ExternTable(id) => *id,
            TypedAstNodeId::ExternTableField(id) => *id,

            TypedAstNodeId::Expr(id) => *id,
            TypedAstNodeId::TypeExpr(id) => *id,
            TypedAstNodeId::Pattern(id) => *id,

            TypedAstNodeId::Ident(id) => *id,

            TypedAstNodeId::FunctionName(id) => *id,
            TypedAstNodeId::FunctionParam(id) => *id,

            TypedAstNodeId::Comment(id) => *id,
            TypedAstNodeId::Return(id) => *id,
            TypedAstNodeId::Break(id) => *id,
            TypedAstNodeId::Continue(id) => *id,

            TypedAstNodeId::For(id) => *id,
            TypedAstNodeId::While(id) => *id,
            TypedAstNodeId::WhileLet(id) => *id,

            TypedAstNodeId::Global(id) => *id,
            TypedAstNodeId::Let(id) => *id,

            TypedAstNodeId::Synthetic(id) => *id,
        }
    }
}
