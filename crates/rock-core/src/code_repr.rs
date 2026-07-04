use crate::{AstNodeId, FunctionId, GlobalId};
use rustc_hash::FxHashMap;

// Type aliases for the various ID types (these will be properly defined later)
pub type StructId = AstNodeId;
pub type EnumId = AstNodeId;
pub type NewtypeId = AstNodeId;

/// Represents generated code for a single function
#[derive(Clone, Debug)]
pub struct FunctionCode(pub String);

/// Represents generated code for a single struct definition
#[derive(Clone, Debug)]
pub struct StructCode(pub String);

/// Represents generated code for a single enum definition
#[derive(Clone, Debug)]
pub struct EnumCode(pub String);

/// Represents generated code for a single global variable
#[derive(Clone, Debug)]
pub struct GlobalCode(pub String);

/// Represents generated code for an operator overload
#[derive(Clone, Debug)]
pub struct OperatorCode(pub String);

/// Represents generated code for a newtype definition
#[derive(Clone, Debug)]
pub struct NewtypeCode(pub String);

/// Represents all generated code for a single module
#[derive(Debug, Default)]
pub struct ModuleCode {
    /// Generated code for functions, keyed by FunctionId
    pub functions: FxHashMap<FunctionId, FunctionCode>,

    /// Generated code for structs, keyed by StructId
    pub structs: FxHashMap<StructId, StructCode>,

    /// Generated code for enums, keyed by EnumId
    pub enums: FxHashMap<EnumId, EnumCode>,

    /// Generated code for globals, in insertion order
    pub globals: Vec<(GlobalId, GlobalCode)>,

    /// Generated code for operators, keyed by FunctionId (operators are functions)
    pub operators: FxHashMap<FunctionId, OperatorCode>,

    /// Generated code for newtypes, keyed by NewtypeId
    pub newtypes: FxHashMap<NewtypeId, NewtypeCode>,

    /// Generated code for enum constructors (no dependencies, emitted at top)
    pub enum_constructors: Vec<String>,

    /// Top-level singleton values for unit enum variants. Emitted on the
    /// BUILTIN module after FFI defs and before functions. Regenerated
    /// from scratch on every CodegenPass::run.
    pub enum_singletons: Vec<String>,

    /// Standard library or prelude code that needs to be included once
    pub stdlib_code: Option<String>,

    /// struct(C) FFI definitions: ffi.cdef + constructors + metatypes.
    /// Always regenerated from scratch on every CodegenPass::run — never appended to.
    /// Emitted by assembler after stdlib_code, before functions.
    pub c_struct_defs: Option<String>,

    /// Module-scoped constant-field value-struct literals hoisted once at
    /// module init. Lines like `local __rock_const_IRect_1 = IRect_ct(0, 0, 64, 64)`.
    /// Populated during function codegen — when a `StructLiteral` of a value
    /// struct with all-literal fields reaches a cdata-ABI pack site, the pack
    /// is interned and the call site references the hoisted name instead of
    /// re-allocating per iteration. Emitted by the assembler after
    /// `c_struct_defs`, before function bodies (so every `ctor_ct` exists).
    pub value_const_defs: Option<String>,

    /// Module-specific initialization code that needs to run
    pub init_code: Option<String>,

    /// Decorator application code (kept separate from init_code to avoid
    /// duplication during incremental compilation — this field is always
    /// overwritten, never appended to cached values)
    pub decorator_init_code: Option<String>,
}

impl ModuleCode {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a function to this module's code
    pub fn add_function(&mut self, id: FunctionId, code: FunctionCode) {
        self.functions.insert(id, code);
    }

    /// Add a global to this module's code
    pub fn add_global(&mut self, id: GlobalId, code: GlobalCode) {
        self.globals.push((id, code));
    }

    /// Check if this module has any generated code
    pub fn is_empty(&self) -> bool {
        self.functions.is_empty()
            && self.structs.is_empty()
            && self.enums.is_empty()
            && self.globals.is_empty()
            && self.operators.is_empty()
            && self.newtypes.is_empty()
            && self.enum_constructors.is_empty()
            && self.enum_singletons.is_empty()
            && self.stdlib_code.is_none()
            && self.c_struct_defs.is_none()
            && self.init_code.is_none()
    }
}
