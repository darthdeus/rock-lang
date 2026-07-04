use crate::prelude::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SymbolId(pub SymbolAstNodeId);

impl SymbolId {
    pub fn ast_id(self) -> AstNodeId {
        self.0.id()
    }

    /// The module the symbol was declared in.
    pub fn module_id(self) -> ModuleId {
        let ast_node_id = match self.0 {
            SymbolAstNodeId::Struct(id)
            | SymbolAstNodeId::Newtype(id)
            | SymbolAstNodeId::GenericTypeParameter(id)
            | SymbolAstNodeId::Enum(id)
            | SymbolAstNodeId::EnumVariant(id)
            | SymbolAstNodeId::Field(id)
            | SymbolAstNodeId::VariableBinding(id)
            | SymbolAstNodeId::Function(id)
            | SymbolAstNodeId::Coroutine(id)
            | SymbolAstNodeId::FunctionCall(id)
            | SymbolAstNodeId::StrippedMethodAlias(id)
            | SymbolAstNodeId::Synthetic(id) => id,

            SymbolAstNodeId::DecoratorParam(block, _) => block,

            SymbolAstNodeId::GlobalVariableBinding(id) => id.ast_id(),
        };
        ast_node_id.module_id()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SymbolAstNodeId {
    Struct(AstNodeId),
    Newtype(AstNodeId),

    GenericTypeParameter(AstNodeId),

    Enum(AstNodeId),
    EnumVariant(AstNodeId),

    Field(AstNodeId),

    GlobalVariableBinding(GlobalId),
    VariableBinding(AstNodeId),

    Function(AstNodeId),
    Coroutine(AstNodeId),

    FunctionCall(AstNodeId),

    StrippedMethodAlias(AstNodeId),

    Synthetic(AstNodeId),

    /// Symbol for a decorator parameter (or `target` binding) inside a
    /// decorator body block. Two-AstNodeId key keeps each (block, param)
    /// pair unique so they cannot collide with real-AST-id synthetics.
    /// `block` is the decorator-body block AST id; `param` is the
    /// parameter's `name.id`, or the block's own id for the synthesized
    /// `target` binding.
    DecoratorParam(AstNodeId, AstNodeId),
}

/// Represents the binding state of a symbol during two-phase compilation
#[derive(Clone, Debug, PartialEq, Eq, Copy, Default)]
pub enum BindingState {
    /// Symbol has been declared but its initializer hasn't been evaluated yet
    /// (e.g., we're currently evaluating the RHS of `let x = ...`)
    Pending,
    /// Symbol is fully bound and available for use
    #[default]
    Active,
}

/// What a symbol table knows about one AST node, usually an identifier: the
/// text it was written as, where it is, and which construct it declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolInfo {
    /// The identifier as it appears in the source. Diagnostics quote it, but it
    /// does not identify the symbol on its own: two declarations of one name
    /// share the text and differ only by their ids.
    pub ident_text: Ustr,
    pub kind: SymbolInfoKind,
    pub span: Span,
    /// Binding state for two-phase compilation
    /// Only relevant for LocalVariable symbols
    pub binding_state: BindingState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SymbolInfoKind {
    /// A placeholder for a declaration whose fields are not registered yet.
    ///
    /// The name is registered on the first pass and its fields on the second,
    /// so that nothing depends on the order the declarations are written in.
    EnumWithoutFields,

    /// A local variable declared in the current scope.
    LocalVariable,
    /// A global variable declaration.
    GlobalVariable,

    /// A function declaration. `params` holds one symbol id per argument, in
    /// the order the parameters are written.
    Function {
        fn_id: FunctionId,
        params: Vec<SymbolId>,
        is_extern: bool,
    },

    /// Externally defined table
    ExternTable,
    ExternTableField,

    GenericTypeParameter {
        name: Ustr,
        index: usize,
    },

    Struct,
    StructField,

    Newtype,

    Enum,
    EnumVariant,

    ExternType,
}

impl SymbolInfoKind {
    /// Whether the kind names a type rather than a value or a field.
    pub fn is_named_type(&self) -> bool {
        matches!(
            self,
            SymbolInfoKind::Struct
                | SymbolInfoKind::Newtype
                | SymbolInfoKind::Enum
                | SymbolInfoKind::Function { .. }
                | SymbolInfoKind::ExternTable
                | SymbolInfoKind::ExternType
        )
    }

    /// Whether the symbol is reachable from another module.
    pub fn is_global_symbol(&self) -> bool {
        matches!(
            self,
            SymbolInfoKind::Struct
                | SymbolInfoKind::Newtype
                | SymbolInfoKind::Enum
                | SymbolInfoKind::Function { .. }
                | SymbolInfoKind::ExternTable
                | SymbolInfoKind::ExternType
                | SymbolInfoKind::GlobalVariable
        )
    }

    pub fn is_function(&self) -> bool {
        matches!(self, SymbolInfoKind::Function { .. })
    }

    /// Whether only one such declaration may exist in the whole program, all
    /// modules counted together. Functions are not in this set because they
    /// overload across modules; the cross-module duplicate check in
    /// rock-optimizations uses this predicate.
    pub fn is_unique_top_level_decl(&self) -> bool {
        matches!(
            self,
            SymbolInfoKind::Struct
                | SymbolInfoKind::Enum
                | SymbolInfoKind::Newtype
                | SymbolInfoKind::GlobalVariable
        )
    }

    /// The noun a diagnostic uses for this kind of symbol.
    pub fn display_label(&self) -> &'static str {
        match self {
            SymbolInfoKind::Struct => "struct",
            SymbolInfoKind::Enum => "enum",
            SymbolInfoKind::Newtype => "newtype",
            SymbolInfoKind::GlobalVariable => "global",
            SymbolInfoKind::Function { .. } => "function",
            _ => "symbol",
        }
    }
}

impl SymbolAstNodeId {
    /// The AST node the symbol was minted from. A decorator-parameter symbol
    /// belongs to the decorator body it was declared in.
    pub fn id(&self) -> AstNodeId {
        match self {
            SymbolAstNodeId::Struct(id) => *id,
            SymbolAstNodeId::Newtype(id) => *id,
            SymbolAstNodeId::GenericTypeParameter(id) => *id,
            SymbolAstNodeId::Enum(id) => *id,
            SymbolAstNodeId::EnumVariant(id) => *id,
            SymbolAstNodeId::Field(id) => *id,
            SymbolAstNodeId::GlobalVariableBinding(id) => id.ast_id(),
            SymbolAstNodeId::VariableBinding(id) => *id,
            SymbolAstNodeId::Function(id) => *id,
            SymbolAstNodeId::Coroutine(id) => *id,
            SymbolAstNodeId::FunctionCall(id) => *id,
            SymbolAstNodeId::StrippedMethodAlias(id) => *id,
            SymbolAstNodeId::Synthetic(id) => *id,
            SymbolAstNodeId::DecoratorParam(block, _) => *block,
        }
    }
}
