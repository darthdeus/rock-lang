//! The TARGET SPECS an artifact view is taken through.
//!
//! **Three independent axes, not a target enum** (plan S 20a). The variants a
//! flat enum would grow -- `LuaDesktop`, `LuaIos`, `JsWeb`, `CAndroid`,
//! `WasmWasi` -- are cross products of things that vary separately:
//! [`PlatformSpec`] is where an artifact RUNS, [`ExecutionTargetSpec`] is what
//! emits and executes its CPU code, and [`ShaderTargetSpec`] is what is packaged
//! beside it. [`TargetSpec`] holds one of each, so a new target is a
//! COMBINATION, not a match arm in every consumer.
//!
//! **Shader packaging cannot affect CPU realization.** Artifact admission takes
//! [`CpuTargetSpec`] -- platform plus execution -- because host capabilities
//! affect available boundaries. It cannot inspect the optional shader target.
//! `shader_packaging_does_not_decide_cpu_realization` measures that separation.
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
    /// Can issue a foreign C call. The platform must also expose host symbols.
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

/// One EXECUTION backend's identity and capabilities (plan S 20a's
/// `ExecutionTargetSpec`, reduced to the fields Stage 2 can actually consume).
///
/// This is one axis of a [`TargetSpec`], not the whole target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionTargetSpec {
    pub kind: ExecutionBackend,
    pub pointer_width: u8,
    /// The `FnBackend` whose declarations this artifact emits, when it has one.
    /// `None` is a target with no Rock-level backend of its own: it realizes
    /// the PORTABLE subset and no verbatim body at all.
    pub backend: Option<bone_semantic::hir::FnBackend>,
    pub caps: Capabilities,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionBackend {
    LuaJit,
    JavaScript,
    NativeC,
    WebAssembly,
}

/// Where an artifact RUNS. An axis of its own: one execution backend is
/// packaged for several platforms, and one
/// platform hosts several backends (the web runs JavaScript and WebAssembly).
///
/// Plan S 20a's detailed payloads (os/arch, browser limits, API level) remain
/// demand-driven. Host-symbol availability is already live in artifact
/// admission, so platform is not decorative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformKind {
    Desktop,
    Web,
    Android,
    Wasi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlatformCapabilities {
    /// The host exposes symbols an execution backend capable of foreign calls
    /// can bind. Browser and WASI artifacts require declared imports instead.
    pub host_symbols: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlatformSpec {
    pub kind: PlatformKind,
    pub caps: PlatformCapabilities,
}

/// What is packaged BESIDE the CPU artifact. Optional, because a target may
/// have no GPU route at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShaderTargetSpec {
    Glsl(skull_mir::shader_ir::GlslProfile),
    SpirV,
    Msl,
}

/// One artifact's target: plan S 20a's `TargetSpec`.
///
/// Three fields, three axes. Composing a new one is a `TargetSpec { .. }`
/// literal, which is what keeps this from becoming the cross-product enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetSpec {
    /// DISPLAY ONLY -- the label a report puts on this combination. It is not
    /// an identity: two `TargetSpec`s with equal axes and different names
    /// realize identically, which `an_artifact_name_decides_nothing` asserts.
    pub name: &'static str,
    pub platform: PlatformSpec,
    pub execution: ExecutionTargetSpec,
    pub shader: Option<ShaderTargetSpec>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuTargetSpec {
    pub platform: PlatformSpec,
    pub execution: ExecutionTargetSpec,
}

impl TargetSpec {
    pub fn cpu(self) -> CpuTargetSpec {
        CpuTargetSpec {
            platform: self.platform,
            execution: self.execution,
        }
    }
}

// --- the axes, declared once each and shared by the combinations below -------

pub const DESKTOP: PlatformSpec = PlatformSpec {
    kind: PlatformKind::Desktop,
    caps: PlatformCapabilities { host_symbols: true },
};
pub const WEB: PlatformSpec = PlatformSpec {
    kind: PlatformKind::Web,
    caps: PlatformCapabilities {
        host_symbols: false,
    },
};
pub const WASI: PlatformSpec = PlatformSpec {
    kind: PlatformKind::Wasi,
    caps: PlatformCapabilities {
        host_symbols: false,
    },
};

pub const GLSL_330: ShaderTargetSpec =
    ShaderTargetSpec::Glsl(skull_mir::shader_ir::GlslProfile::Core330);

/// The luajit execution backend.
pub const LUAJIT: ExecutionTargetSpec = ExecutionTargetSpec {
    kind: ExecutionBackend::LuaJit,
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

/// The browser execution backend.
pub const JAVASCRIPT: ExecutionTargetSpec = ExecutionTargetSpec {
    kind: ExecutionBackend::JavaScript,
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
/// real backends while sharing their logical graph.
pub const NATIVE_C64: ExecutionTargetSpec = ExecutionTargetSpec {
    kind: ExecutionBackend::NativeC,
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
    kind: ExecutionBackend::WebAssembly,
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

// --- the combinations --------------------------------------------------------
//
// Each is a literal over the axes above.

pub const LUAJIT_DESKTOP: TargetSpec = TargetSpec {
    name: "luajit-desktop",
    platform: DESKTOP,
    execution: LUAJIT,
    shader: Some(GLSL_330),
};

pub const JAVASCRIPT_WEB: TargetSpec = TargetSpec {
    name: "javascript-web",
    platform: WEB,
    execution: JAVASCRIPT,
    shader: Some(GLSL_330),
};

pub const NATIVE_C64_DESKTOP: TargetSpec = TargetSpec {
    name: "native-c64-desktop",
    platform: DESKTOP,
    execution: NATIVE_C64,
    shader: Some(GLSL_330),
};

pub const WASM32_WASI: TargetSpec = TargetSpec {
    name: "wasm32-wasi",
    platform: WASI,
    execution: WASM32,
    shader: None,
};

pub const ALL: &[TargetSpec] = &[
    LUAJIT_DESKTOP,
    JAVASCRIPT_WEB,
    NATIVE_C64_DESKTOP,
    WASM32_WASI,
];

/// Can `target` supply this boundary class?
///
/// The erased runtime entries are deliberately NOT capability-gated: S 1.4's
/// contract is that the entry exists for EVERY target skull emits, and the
/// compile-time rejection in `stdlib_funcref` is what enforces it. A projection
/// that let one be unavailable would be asserting the opposite of the contract.
pub fn supports(target: &CpuTargetSpec, class: BoundaryClass) -> bool {
    match class {
        BoundaryClass::Extern => {
            target.execution.caps.foreign_symbols && target.platform.caps.host_symbols
        }
        BoundaryClass::InlineBody => {
            target.execution.caps.inline_lua || target.execution.caps.inline_js
        }
        BoundaryClass::Intrinsic => true,
        BoundaryClass::ErasedRuntimeEntry => true,
        BoundaryClass::Invalid => false,
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
