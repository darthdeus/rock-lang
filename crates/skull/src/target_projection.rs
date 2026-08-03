//! The TARGET SPECS an artifact view is taken through
//! (`docs/monomorphization-plan.md` S 0c final bullet, S 20a, S 20g.1).
//!
//! **Structured capabilities, not a target enum** (plan S 20a). The variants a
//! flat enum would grow -- `LuaDesktop`, `JsWeb`, `CAndroid`, `WasmWeb` -- are
//! cross products of independent axes. [`ExecutionTargetSpec`] carries the axes
//! as fields so a new target is a fixture, not a match arm in every consumer.
//!
//! The COUNT-ONLY projection this module used to return is gone. Counting
//! `g.boundaries` answered a symbol-keyed question -- two `@nomangle` backend
//! siblings share one entry there -- and a count cannot say WHICH declaration a
//! target keeps, which is the only form in which "identity survives
//! realization differing" is checkable. `crate::artifact::realize` returns the
//! realized sets instead; this module keeps the specs and the two per-target
//! predicates they answer.

use crate::instance_graph::BoundaryClass;

/// What a target can supply. Each field is a question some boundary class or
/// ABI decision actually asks; none is a target NAME.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// Can call a foreign C symbol -- decides `extern` (S 1.1).
    pub foreign_symbols: bool,
    /// Has a verbatim-body backend of this flavour -- decides `inline(..)`
    /// (S 1.2).
    pub inline_lua: bool,
    pub inline_js: bool,
    /// The runtime reclaims memory. `false` obliges ownership realization
    /// (plan S 20c), which is explicitly deferred work.
    pub garbage_collected: bool,
    /// A suspendable frame exists in the runtime. `false` obliges the coroutine
    /// frame to be materialized by the backend.
    pub native_coroutines: bool,
    /// 64-bit floats. `false` changes `float`'s representation, never its
    /// logical identity -- which is the separation under test.
    pub float64: bool,
}

/// One execution target's identity and capabilities (plan S 20a's
/// `ExecutionTargetSpec`, reduced to the fields Stage 0 can actually consume).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionTargetSpec {
    pub name: &'static str,
    pub pointer_width: u8,
    /// The `FnBackend` whose declarations this artifact emits, when it has one.
    /// `None` is a target with no Rock-level backend of its own: it realizes
    /// the PORTABLE subset and no verbatim body at all.
    pub backend: Option<bone_semantic::hir::FnBackend>,
    pub caps: Capabilities,
}

/// The luajit artifact.
pub const LUAJIT: ExecutionTargetSpec = ExecutionTargetSpec {
    name: "luajit",
    backend: Some(bone_semantic::hir::FnBackend::Lua),
    pointer_width: 64,
    caps: Capabilities {
        foreign_symbols: true,
        inline_lua: true,
        inline_js: false,
        garbage_collected: true,
        native_coroutines: true,
        float64: true,
    },
};

/// The browser artifact.
pub const JAVASCRIPT: ExecutionTargetSpec = ExecutionTargetSpec {
    name: "javascript",
    backend: Some(bone_semantic::hir::FnBackend::Js),
    pointer_width: 64,
    caps: Capabilities {
        foreign_symbols: false,
        inline_lua: false,
        inline_js: true,
        garbage_collected: true,
        native_coroutines: true,
        float64: true,
    },
};

/// Deliberately hostile synthetic fixture: 16-bit pointers, no GC, no
/// suspendable frame, 32-bit floats. It exists to be maximally UNLIKE the two
/// real targets while sharing their logical graph.
pub const NATIVE_C64: ExecutionTargetSpec = ExecutionTargetSpec {
    name: "native-c64",
    backend: None,
    pointer_width: 16,
    caps: Capabilities {
        foreign_symbols: true,
        inline_lua: false,
        inline_js: false,
        garbage_collected: false,
        native_coroutines: false,
        float64: false,
    },
};

/// Synthetic Wasm32 fixture. Foreign symbols are IMPORTS declared at module
/// instantiation, not a `dlopen` the compiler can assume, so `foreign_symbols`
/// is false -- an `extern` needs an import entry the artifact must carry.
pub const WASM32: ExecutionTargetSpec = ExecutionTargetSpec {
    name: "wasm32",
    backend: None,
    pointer_width: 32,
    caps: Capabilities {
        foreign_symbols: false,
        inline_lua: false,
        inline_js: false,
        garbage_collected: false,
        native_coroutines: false,
        float64: true,
    },
};

pub const ALL: &[ExecutionTargetSpec] = &[LUAJIT, JAVASCRIPT, NATIVE_C64, WASM32];

/// Can `target` supply this boundary class?
///
/// The erased runtime entries are deliberately NOT capability-gated: S 1.4's
/// contract is that the entry exists for EVERY target skull emits, and the
/// compile-time rejection in `stdlib_funcref` is what enforces it. A projection
/// that let one be unavailable would be asserting the opposite of the contract.
pub fn supports(caps: &Capabilities, class: BoundaryClass) -> bool {
    match class {
        BoundaryClass::Extern => caps.foreign_symbols,
        BoundaryClass::InlineBody => caps.inline_lua || caps.inline_js,
        BoundaryClass::Intrinsic => true,
        BoundaryClass::ErasedRuntimeEntry => true,
    }
}

/// The obligations `spec` defers to a later stage. Deferred is not unknown:
/// each names the plan section that owns it.
pub fn deferred_for(spec: &ExecutionTargetSpec) -> Vec<&'static str> {
    let mut deferred = Vec::new();
    if !spec.caps.garbage_collected {
        deferred.push("ownership realization: retain/release + cycle policy (plan S 20c)");
    }
    if !spec.caps.native_coroutines {
        deferred.push("coroutine frames must be materialized by the backend (plan S 13)");
    }
    if spec.pointer_width < 32 {
        deferred.push("pointer width < 32: `int` does not fit a machine word (plan S 5)");
    }
    if !spec.caps.float64 {
        deferred.push("`float` is not 64-bit: a REPRESENTATION change, not an identity one");
    }
    if !spec.caps.foreign_symbols {
        deferred.push("`extern` needs a declared import entry rather than a host lookup");
    }
    deferred
}
