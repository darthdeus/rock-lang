//! skull — the Claude-managed code-generation backend for `bone`'s HIR.
//!
//! skull consumes `bone` as a library (`parse_rock` -> `semantic_pass` ->
//! `Workspace`), lowers the typed HIR to a **basic-block CFG** ([`mir`]), and
//! emits **luajit** from it ([`emit_lua`]) — independent of rock's
//! `rock-codegen`. It is a separate verification layer driving the user's manual
//! `bone` work; bone will eventually pull from skull.
//!
//! Pipeline: bone HIR --[`lower`]--> MIR (CFG, light SSA) --[`emit_lua`]--> Lua.
//! The CFG substrate is the foundation for real codegen passes (const-fold,
//! copy-prop, DCE, and — once bone models `value` types — arg-SROA) and for
//! retargeting (a JS backend would reloop/dispatch the same MIR). The long-term
//! goal is `rock test tests --backend skull` at 100% as bone grows. See
//! `docs/skull-plan.md`.

pub mod allocation_report;
pub mod coroutine_emit;
pub mod inline_stats;
pub mod mem2reg_stats;
pub mod graph_snapshot;
pub mod shader_artifact;
pub mod site_join;
pub mod mir_emit;

/// Emitted callee shape, shared by CLI sessions and the Lua printer.
pub use skull_codegen_lua::{callee_binding_is_bound, callee_binding_mode, CalleeBindingMode};

// The shared graph layer, re-exported under its original paths. It moved BELOW
// the backends (`skull-graph`) so the JS and GLSL emitters can realize the same
// graph the luajit driver does; consumers name it the same way either side of
// that move.
pub use skull_graph::{artifact, concrete_body, expr_subtree, instance_graph, shader, target_projection};
pub mod artifact_fingerprint;

use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use skull_parser::CompilerError;

/// Parse, type-check (via bone), lower to MIR, and emit Lua for the single user
/// module.
pub fn compile_source(src: &str) -> Result<String, CompilerError> {
    let module = skull_parser::parse_module(src, ustr::Ustr::from("<skull>"))?;
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut [module])?;
    // `semantic_pass` Ok means "lowered and checked", not "no type errors" —
    // body-level errors are collected per BodyOwner. Refuse to emit from a
    // workspace with broken bodies (their inference is missing).
    let mut body_errs: Vec<&CompilerError> = ws
        .inferences
        .iter().flatten()
        .flat_map(|inf| inf.errors.values().flatten())
        .collect();
    body_errs.sort_by_key(|e| e.location.offset);
    if let Some(e) = body_errs.first() {
        return Err((*e).clone());
    }
    let mid = user_module_id(&ws)?;
    // The one emission path (same as rock-cli): workspace emit includes the
    // runtime prelude, enum singletons, value-struct cdefs, and sorted global
    // inits — a single-module emit that skips those produces Lua that
    // references undefined globals (e.g. `__rock_enum_<E>_<V>`).
    Ok(mir_emit::emit_workspace_mir(&ws, mid)?.lua)
}

/// Per-nominal-type layout hash, keyed by type name (unique workspace-wide).
pub type ShapeHashes = rustc_hash::FxHashMap<ustr::Ustr, u64>;

/// Hash every nominal type's RUNTIME LAYOUT for the hot-reload schema-drift
/// restart trigger (docs/hot-reload-schema-drift-plan.md): live data outlives
/// a reload, so a layout change means the process must restart. The hash
/// covers exactly what determines how live values are laid out -- ordered
/// (field name, field type) pairs for structs (+ the value/cdata flag),
/// ordered (variant name, payload shape) for enums, the wrapped type for
/// newtypes. Bodies, spans, defaults, and comments do not participate, so a
/// hot body edit or a doc tweak never restarts. A BOXED field needs no
/// transitive hash: the parent holds a pointer either way, so a nested type's
/// OWN entry changing is the whole signal. An `embed` field is the exception --
/// see [`struct_shape_hash`]. Known over-trigger (accepted,
/// uniform-predictable): reordering a boxed enum's variants restarts even
/// though its live tags are name strings.
pub fn type_shape_hashes(ws: &Workspace) -> Result<ShapeHashes, CompilerError> {
    use bone_semantic::hir::VariantPayload;
    use std::hash::{Hash, Hasher};

    let mut out = ShapeHashes::default();
    for module in ws.modules.iter() {
        for (_, s) in module.structs.iter() {
            out.insert(s.name, struct_shape_hash(ws, s, &mut Vec::new())?);
        }
        for (_, e) in module.enums.iter() {
            let mut h = rustc_hash::FxHasher::default();
            "enum".hash(&mut h);
            for v in e.variants.iter() {
                v.name.as_str().hash(&mut h);
                match &v.payload {
                    VariantPayload::Unit => "unit".hash(&mut h),
                    VariantPayload::Tuple(tys) => {
                        "tuple".hash(&mut h);
                        for t in tys.iter() {
                            ws.render(None, *t).hash(&mut h);
                        }
                    }
                    VariantPayload::Record(fields) => {
                        "record".hash(&mut h);
                        for f in fields.iter() {
                            f.name.as_str().hash(&mut h);
                            ws.render(None, f.ty).hash(&mut h);
                        }
                    }
                }
            }
            out.insert(e.name, h.finish());
        }
        for (_, nt) in module.newtypes.iter() {
            let mut h = rustc_hash::FxHasher::default();
            "newtype".hash(&mut h);
            ws.render(None, nt.wrapped).hash(&mut h);
            out.insert(nt.name, h.finish());
        }
    }
    Ok(out)
}

/// One struct's layout hash. An `embed` field contributes the embedded type's
/// SHAPE HASH, not just its rendered name: the child has no box of its own, so
/// its leaves ARE this struct's physical keys and widening the child relayouts
/// the PARENT.
///
/// That extra contribution also covers the flag flip, since an embed field
/// hashes strictly more than a boxed one: flipping `f: Task?` to
/// `embed f: Task?` leaves every rendered field type identical and touches no
/// other type, so before this the whole workspace hashed clean and NO restart
/// fired. The planned inline/direct decision is hashed explicitly, so the
/// layout bit remains load-bearing on its own and does not depend on how the
/// child happens to contribute.
///
/// `visiting` breaks a cycle with a stable sentinel. bone rejects a cyclic
/// embed graph (`check_embed_site_legality`), so this is unreachable by
/// construction; it exists so a malformed workspace reaching codegen produces a
/// wrong-but-finite hash instead of hanging the compiler.
fn struct_shape_hash(
    ws: &Workspace,
    s: &bone_semantic::hir::StructDef,
    visiting: &mut Vec<ustr::Ustr>,
) -> Result<u64, CompilerError> {
    use std::hash::{Hash, Hasher};

    let mut h = rustc_hash::FxHasher::default();
    "struct".hash(&mut h);
    s.is_value.hash(&mut h);
    s.is_nobox.hash(&mut h);
    for f in s.fields.iter() {
        f.name.as_str().hash(&mut h);
        ws.render(None, f.ty).hash(&mut h);
        let plan = skull_mir::lower::struct_field_plan(
            ws,
            bone_semantic::hir::FnBackend::Lua,
            s.name,
            f.name,
            false,
        )
        .map_err(|e| CompilerError::new_no_location(e.message))?;
        plan.embed().is_some().hash(&mut h);
        if let Some(child_name) = plan.inline_child() {
            let sid = ws.struct_by_name.get(&child_name).ok_or_else(|| {
                CompilerError::new_no_location(format!(
                    "struct `{}` field `{}` has no planned child `{child_name}`",
                    s.name, f.name
                ))
            })?;
            if !visiting.contains(&s.name) {
                let child = &ws.modules[sid.module.index()].structs[sid.def];
                visiting.push(s.name);
                struct_shape_hash(ws, child, visiting)?.hash(&mut h);
                visiting.pop();
            }
        }
    }
    Ok(h.finish())
}

/// The type names whose shape hash differs between two compiles: changed,
/// added, or removed entries all count -- live data under a REMOVED type is
/// just as stale as under a changed one.
pub fn shape_diff(old: &ShapeHashes, new: &ShapeHashes) -> Vec<String> {
    let mut changed: Vec<String> = new
        .iter()
        .filter(|(name, hash)| old.get(*name) != Some(hash))
        .map(|(name, _)| name.to_string())
        .collect();
    changed.extend(old.keys().filter(|n| !new.contains_key(*n)).map(|n| n.to_string()));
    changed.sort();
    changed
}

/// The single user module id (everything in the workspace but the builtin one).
pub fn user_module_id(ws: &Workspace) -> Result<ModuleId, CompilerError> {
    ws.modules
        .iter()
        .map(|m| m.id)
        .find(|m| *m != ModuleId::builtin())
        .ok_or_else(|| CompilerError::new_no_location("skull: no user module after semantic_pass"))
}
/// A `@test` function discovered in a module: its LOGICAL name (the DefKey --
/// what a report labels and what the value backends look the fn up by), the
/// SYMBOL the artifact defines it under, and the expected stdout of
/// `@test(output = "...")`.
///
/// The two differ for a v1 package. `@test` is mangling-exempt, so the emitted
/// global is the key's declared segment, while the key itself (`pkg::mod::name`)
/// is not an identifier in any backend's grammar. A harness that must CALL the
/// test uses `symbol`; text a human reads uses `name`.
#[derive(Clone, Debug)]
pub struct SkullTest {
    pub name: String,
    pub symbol: String,
    pub output: Option<String>,
    /// `@test(returns = <literal>)`: the fn's return value is captured by the
    /// backend harness and compared HOST-side against this pinned literal.
    pub returns: Option<ValueExpect>,
}

/// The pinned literal of a `@test(returns = ...)` value test. Comparison policy
/// (int/bool exact, float relative tolerance) lives host-side in
/// `rock-cli/src/test_protocol.rs`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ValueExpect {
    Int(i64),
    Float(f64),
    Bool(bool),
}

/// Unescape a `@test(output = "...")` string the way rock's test runner does:
/// the source had `\\n`, so the parsed annotation value carries a literal
/// backslash-n that must become a real newline before comparison.
fn unescape_expected(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('0') => out.push('\0'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// The `@test` functions in `mid`, with their expected outputs (if any).
/// `Err` = a malformed `@test` annotation (bad `returns` literal, or
/// `returns` + `expect_failure` together) -- a spanned rejection, never a
/// silently-ignored annotation.
pub fn module_tests(ws: &Workspace, mid: ModuleId) -> Result<Vec<SkullTest>, CompilerError> {
    Ok(module_tests_with_ids(ws, mid)?.into_iter().map(|(_, t)| t).collect())
}

/// [`module_tests`] carrying each test's `Idx<Function>` too -- the JS test
/// executor needs the `FuncRef` to lower/emit the fn, not just its Lua name.
pub fn module_tests_with_ids(
    ws: &Workspace,
    mid: ModuleId,
) -> Result<Vec<(bone_semantic::prelude::Idx<bone_semantic::hir::Function>, SkullTest)>, CompilerError>
{
    let Some(module) = ws.modules.get(mid.index()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (fid, f) in module.functions.iter() {
        let Some(test_ann) = f.annotations.iter().find(|a| a.is_builtin("test")) else {
            continue;
        };
        // Honor `@ignore` (same convention as rock-cli/src/test.rs): a test
        // marked ignored is not run.
        if f.annotations.iter().any(|a| a.is_builtin("ignore")) {
            continue;
        }
        let returns = match test_ann
            .args
            .iter()
            .find(|arg| arg.name.as_ref().map_or(false, |n| n.name.as_str() == "returns"))
        {
            Some(arg) => Some(match &arg.value {
                bone_semantic::hir::AnnoValue::Int(i) => ValueExpect::Int(*i),
                bone_semantic::hir::AnnoValue::Float(f) => ValueExpect::Float(*f),
                bone_semantic::hir::AnnoValue::Bool(b) => ValueExpect::Bool(*b),
                _ => {
                    return Err(CompilerError::new_with_label(
                        test_ann.span,
                        format!(
                            "@test on `{}`: returns expects an int/float/bool literal",
                            f.name.as_str()
                        ),
                    ));
                }
            }),
            None => None,
        };
        // A `@test(expect_failure = true)` asserts a COMPILE error, not runtime
        // output — the execution oracle can't verify rejection (and an accepted
        // one is a type-checker gap, not a codegen issue). Skip it here; the
        // compile-fail / backend-diff harnesses cover rejection. Combining it
        // with `returns` (a runtime-value assertion) is contradictory.
        if test_ann.args.iter().any(|arg| {
            arg.name.as_ref().map_or(false, |n| n.name.as_str() == "expect_failure")
                && matches!(arg.value, bone_semantic::hir::AnnoValue::Bool(true))
        }) {
            if returns.is_some() {
                return Err(CompilerError::new_with_label(
                    test_ann.span,
                    format!(
                        "@test on `{}`: returns and expect_failure are contradictory \
                         (expect_failure asserts a compile error; the fn never runs)",
                        f.name.as_str()
                    ),
                ));
            }
            continue;
        }
        let output = match test_ann
            .args
            .iter()
            .find(|arg| arg.name.as_ref().map_or(false, |n| n.name.as_str() == "output"))
        {
            Some(arg) => match &arg.value {
                bone_semantic::hir::AnnoValue::String(s) => Some(unescape_expected(s.as_str())),
                // A lone `{` makes the literal an interpolation expression
                // (AnnoValue::Other) -- dropping it to None silently un-pins
                // the expectation, so reject instead.
                _ => {
                    return Err(CompilerError::new_with_label(
                        test_ann.span,
                        format!(
                            "@test on `{}`: output expects a plain string literal \
                             (use {{{{ and }}}} for literal braces -- a lone brace \
                             is string interpolation)",
                            f.name.as_str()
                        ),
                    ));
                }
            },
            None => None,
        };
        out.push((
            fid,
            SkullTest {
                name: f.name.as_str().to_string(),
                // The one emitted-name oracle, so the harness cannot drift from
                // the definition the artifact actually writes.
                symbol: skull_mir::mangle::mangle_fn(ws, f),
                output,
                returns,
            },
        ));
    }
    Ok(out)
}

/// Emit the whole workspace: every stdlib module (leniently) followed by the
/// user module (strict). skull emits only function definitions, so definition
/// order doesn't matter for loading — all globals exist before anything runs.
// --- emit-name helpers (pure; shared by the new-path `mir_emit` + scaffolding) ---

/// The interned singleton global for a unit enum variant (`Color::Red` ->
/// `__rock_enum_Color_Red`), so the value is reference-equal across uses and
/// usable as a Lua map key.
pub fn enum_singleton_name(enum_name: &str, variant: &str) -> String {
    format!("__rock_enum_{enum_name}_{variant}")
}

/// Lua name for a global's init fn (`<global> = __skull_ginit_<global>()`).
pub fn global_init_fn_name(global: &str) -> String {
    format!("__skull_ginit_{}", global)
}

/// Lua name for a decorator fn (`@register` -> `__deco_register`).
pub fn decorator_fn_name(deco: &str) -> String {
    format!("__deco_{}", skull_mir::mangle::symbol_of_key(ustr::Ustr::from(deco)))
}

/// A `@deco`-annotation argument (or `deco` param default) as a Lua literal.
pub fn annotation_value_lua(v: &bone_semantic::hir::AnnoValue) -> Option<String> {
    use bone_semantic::hir::AnnoValue as A;
    Some(match v {
        A::Int(i) => i.to_string(),
        A::Float(f) => f.to_string(),
        A::Bool(b) => b.to_string(),
        A::String(s) => format!("{:?}", s.as_str()),
        A::Ident(id) => id.name.as_str().to_string(),
        A::IdentList(_) => return None,
        A::Other => return None,
    })
}

/// The Lua argument list for one `@deco` application: each `deco` param gets its
/// named annotation arg if present, else its default (`nil` — filled callee-side
/// in `__deco_<name>`'s prologue). Required params without an arg, and
/// non-literal args/defaults, error. The selection (named/default/required) is
/// the backend-agnostic `skull_mir::decorator::select_decorator_args`, shared
/// with the JS backend's `decorator_call_args_js`; only the literal rendering
/// (`annotation_value_lua`) and the `nil` default token are Lua-specific.
pub fn decorator_call_args(
    d: &bone_semantic::hir::DecoratorDef,
    ann: &bone_semantic::hir::Annotation,
) -> Result<Vec<String>, CompilerError> {
    use skull_mir::decorator::DecoArg;
    let mut args = Vec::new();
    for choice in skull_mir::decorator::select_decorator_args(d, ann)? {
        args.push(match choice {
            DecoArg::Value(v) => annotation_value_lua(v).ok_or_else(|| {
                CompilerError::new_no_location(format!(
                    "skull: decorator `{}` arg is a non-literal expression (unsupported)",
                    d.name
                ))
            })?,
            DecoArg::Default => "nil".to_string(),
        });
    }
    Ok(args)
}

/// Returns the Lua plus the list of skipped stdlib functions.
/// Runtime globals the stdlib expects pre-seeded (rock's codegen emits an init
/// that does this; skull emits this small prelude instead). Empty tables are
/// enough — the stdlib's accessors (`__rock_create_struct`, `vec_new`)
/// nil-guard per-key, so they self-heal off a present-but-empty registry.
fn runtime_prelude() -> &'static str {
    // Runtime setup has two parts:
    //   1. LuaJIT mcode/trace tuning. skull (like rock-codegen) emits more Lua
    //      than LuaJIT's small-script defaults assume (32 KB mcode blocks /
    //      512 KB total / 1000 traces). A real game's hot traces overflow the
    //      mcode arena -> "failed to allocate mcode memory" -> a GLOBAL trace
    //      flush wipes every good trace -> the hot loop drops to the interpreter
    //      and re-traces on a cliff/spiral (mag floors_walls: ~2.6 ms vs ~0.5
    //      with this). A trace must fit one `sizemcode` block. On arm64 macOS,
    //      dyld leaves room for the first block near the VM but later mappings
    //      prevent a second one, so that first block must hold the hot set --
    //      which is why size == max on both arches: one block, not several.
    //      The budget is arch-split because the jump range is: x64 reaches
    //      +/-2GB, arm64 only +/-128MB, so arm64 keeps the 16 MB figure from
    //      the W1 decision in docs/luajit-mcode-flush-plan.md while x64 takes
    //      64 MB. 8 MB was sized against `mag floors_walls` and is far too
    //      small for the live game: a 60 s session of real play (summons,
    //      combat, multiple spaces) flushed 31 times -- once every ~0.7 s --
    //      and flush-plus-recovery accounted for 94% of all frame time over
    //      budget. Evidence and the per-frame join are in
    //      docs/trace-pressure-work-plan.md.
    //      ROCK_JIT_MCODE_KB overrides both, so the budget can be swept
    //      without a rebuild; this prelude runs at chunk load and beats any
    //      -O flag passed on the command line.
    //      `hotexit` is raised from LuaJIT's default 10 to 100 because a side
    //      trace that FAILS is never penalised or blacklisted
    //      (lj_trace.c:596-606) and snap->count only saturates on success, so a
    //      doomed exit is re-recorded ~245 times for nothing. MAG's hottest
    //      exits are doomed: they die in asm_head_side and never appear as a
    //      parent for any other trace. Measured -57% aborts and -22% mean frame
    //      on two independent workloads (a scripted combat sim and a recorded
    //      play session), both disjoint across interleaved arms.
    //      NOT validated on a workload whose side traces mostly SUCCEED, where
    //      a higher threshold would cost real compiled coverage; and 250 beat
    //      100 on every metric without the curve flattening, so this is a
    //      conservative pick rather than a tuned one. ROCK_JIT_HOTEXIT
    //      overrides it, so a regression reverts without a rebuild.
    //      `maxtrace` is 32000 (LuaJIT default 1000, previously 4000 here).
    //      A MAG combat session compiles ~24,600 traces; at 4000 the trace
    //      array filled six times per benchmark run and every fill flushed
    //      the whole cache -- all compiled code, every side-exit counter and
    //      every abort penalty -- so the working set was re-recorded from
    //      scratch six times. At 32000 the benchmark never flushes: aborts
    //      11.1-11.5k -> 3.9k, mean frame 7.1 -> 4.7-5.1 ms, frames over
    //      16.7 ms 37-39 -> 3-4, two interleaved pairs, peak RSS unchanged.
    //      The trace array is 16 bytes per slot, so the fixed cost is 512 KB.
    //      ROCK_JIT_MAXTRACE overrides it. `maxirconst` stays at LuaJIT's 500
    //      (ROCK_JIT_MAXIRCONST): every "trace too long" in the census is the
    //      constant limit, not maxrecord, but raising it only converts the
    //      aborts into tail-call unroll aborts at the same starts.
    //      `hotloop` and `loopunroll` stay at LuaJIT's defaults (56, 15) but
    //      are exposed as ROCK_JIT_HOTLOOP / ROCK_JIT_LOOPUNROLL: MAG's inner
    //      loops (glyphs per string, points per ribbon) run fewer than 56
    //      iterations, so they never become loop traces and get unrolled by
    //      whichever function-entry trace reaches them, which is where the
    //      "trace too long" aborts come from. Sweepable without a rebuild.
    //   2. 32-bit RNG (stdlib `random_*` inline(lua) bodies call
    //      `__rock_rng_next`/`__rock_rng_seed` by name); seeded from
    //      ROCK_TEST_SEED for deterministic test runs.
    // The CodeRef runtime is appended by assembly only when this artifact has an
    // escaping Rock function value. This keeps direct-call-only output free of
    // slot machinery while retaining one canonical runtime definition.
    static PRELUDE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let out = PRELUDE_TEMPLATE
            .replace("{FN_MT}", "")
            .replace("{SESSION_CELL_BINDING}\n", SESSION_CELL_BINDING)
            .replace("{LEAF_VIEWS}", &skull_mir::lower::leaf_views_lua_table())
            .replace("{NB_PRIM_STORAGE}", &skull_mir::lower::nb_prim_storage_lua());
        // `str::replace` on a placeholder that drifted out of the template is a
        // NO-OP, and the survivor is valid Lua: `= {LEAF_VIEWS}` reads an unset
        // global, so the table is EMPTY rather than a syntax error. `pairs({})`
        // then makes `__nb_refresh_views` do nothing and every view dangles
        // after a grow -- silently, which is the class that killed mag's
        // spell_damages once already. Fail loudly at build time instead.
        assert!(
            !out.contains("{LEAF_VIEWS}"),
            "prelude template still contains an unsubstituted {{LEAF_VIEWS}} placeholder"
        );
        assert!(
            !out.contains("{NB_PRIM_STORAGE}"),
            "prelude template still contains an unsubstituted {{NB_PRIM_STORAGE}} placeholder"
        );
        out
    });
    &PRELUDE
}

/// Session activation must reseat cells in full chunks and incremental overlays.
pub(crate) const SESSION_CELL_BINDING: &str = r#"if not _G.__rock_state_cell_sink then
  _G.__rock_state_cell_sink = function(session)
    __rock_state_cell = session and session.__rock_state or nil
    __rock_initialized_cell = session and session.__rock_initialized or nil
  end
end
if not _G.__rock_active_session then
  _G.__rock_session_activate(_G.__rock_session_new(nil))
end
__rock_state_cell = _G.__rock_active_session.__rock_state
__rock_initialized_cell = _G.__rock_active_session.__rock_initialized
"#;

const PRELUDE_TEMPLATE: &str =
    r#"if jit and jit.opt then local __mc = tonumber(os.getenv("ROCK_JIT_MCODE_KB") or "") or (jit.arch == "arm64" and 16384 or 65536) local __he = tonumber(os.getenv("ROCK_JIT_HOTEXIT") or "") or 100 local __hl = tonumber(os.getenv("ROCK_JIT_HOTLOOP") or "") or 56 local __lu = tonumber(os.getenv("ROCK_JIT_LOOPUNROLL") or "") or 15 local __mt = tonumber(os.getenv("ROCK_JIT_MAXTRACE") or "") or 32000 local __mk = tonumber(os.getenv("ROCK_JIT_MAXIRCONST") or "") or 500 jit.opt.start("maxmcode=" .. __mc, "sizemcode=" .. __mc, "maxtrace=" .. __mt, "maxrecord=32000", "maxsnap=4000", "maxirconst=" .. __mk, "hotexit=" .. __he, "hotloop=" .. __hl, "loopunroll=" .. __lu) end
-- Everything up to the entry call runs inside one block, so the block's locals
-- -- the Session cells first of all -- are CLOSED upvalues (UREFC: one guard,
-- no constant) by the time an emitted function runs under the JIT. Left open,
-- every inlined frame reading a cell would intern its own 2-slot i64 guard
-- constant (docs/trace-pressure-work-plan.md, "Session cells"). The matching
-- `end` is the last line the full emitter writes; the entry call the CLI
-- appends runs after it.
do
-- Chunk-level cells every bound function reads a mutable global through
-- (docs/backends.md, "Session cells"). Seated by __rock_session_publish below.
local __rock_state_cell, __rock_initialized_cell
local __rock_table_new = require("table.new")
if not _G.ffi then _G.ffi = require("ffi") end
if not _G.__ROCK_C_STRUCT then _G.__ROCK_C_STRUCT = {} end
if not _G.__ROCK_TYPE_TABLE then _G.__ROCK_TYPE_TABLE = {} end
-- Compiler-minted NativeBuffer leaf views: field -> ffi.cast type. Generated
-- from skull_mir::lower::LEAF_VIEWS. Views are DERIVED (pointers over the
-- buffer's own `ptr`), so refresh reseats them and serialize/inspect skip them.
_G.__ROCK_LEAF_VIEWS = {LEAF_VIEWS}
-- Never legitimately empty: an empty table means the generator emitted nothing,
-- which would make view refresh a silent no-op and leave every view dangling.
assert(next(_G.__ROCK_LEAF_VIEWS), "__ROCK_LEAF_VIEWS is empty; leaf-view refresh would silently do nothing")
-- Compiler-described NativeBuffer STORAGE, keyed by the ctype LuaJIT gives the
-- allocation itself. Generated from skull_mir::lower (NB_PRIM_ELEMS for the
-- primitive/sized element types, value_leaf_layout_checked + LEAF_VIEWS for the
-- value-struct ones), so a holder of a storage cdata can ask what its bytes mean
-- -- element ctype, element width, and the views minted over it -- rather than
-- reading a `__ty` string or re-deriving a layout. An element array whose ctype
-- is absent here is storage the compiler does not describe, and every consumer
-- must fail closed on it.
--
-- Keyed by `tostring(ffi.typeof(...))` because a ctype cdata hashes by identity
-- as a table key, and because a typedef'd anonymous struct only gets its
-- `struct <n>` spelling from LuaJIT. Registering through the same `ffi.typeof`
-- the lookup uses is what makes the two agree.
_G.__ROCK_NB_STORAGE = {}
local function __rock_nb_storage(ctype, elem_size, views)
  local ok, ct = pcall(ffi.typeof, ctype .. "[?]")
  if not ok then error("skull: __ROCK_NB_STORAGE names undeclared ctype `" .. ctype .. "`", 0) end
  if ffi.sizeof(ctype) ~= elem_size then
    error("skull storage drift: sizeof(" .. ctype .. ") = " .. ffi.sizeof(ctype)
          .. ", expected " .. elem_size, 0)
  end
  _G.__ROCK_NB_STORAGE[tostring(ct)] = { ctype = ctype, elem_size = elem_size, views = views }
end
_G.__rock_nb_storage = __rock_nb_storage
{NB_PRIM_STORAGE}
if not _G.__rock_on_hot_reload then _G.__rock_on_hot_reload = {} end
-- A Session's state tables are sized to the schema's rehash fixpoint
-- (`_G.__ROCK_STATE_CAPACITY`, emitted by the compiler; `__rock_state_reserve`
-- below) so their array part never moves after load. LuaJIT specialises a
-- constant-key read to the table's CURRENT array/hash split, and a table that
-- grows lazily from `{}` rehashes once per hash-part overflow -- each time
-- recomputing the array part from the keys populated so far -- which left
-- every trace recorded before a move with a permanently failing guard
-- (`docs/backends.md`, "Session cells"). Defined outside the define-once block
-- so a reload with a newer artifact never keeps an older constructor.
_G.__rock_session_new = function(schema_id)
  local capacity = rawget(_G, "__ROCK_STATE_CAPACITY")
  local narr, nhash = 0, 0
  if capacity then narr, nhash = capacity[1], capacity[2] end
  return {
    __rock_schema_id = schema_id,
    __rock_state = __rock_table_new(narr, nhash),
    __rock_initialized = __rock_table_new(narr, nhash),
    __rock_initializer_keys = {},
    __rock_rng_state = nil,
    __rock_presentation_rng_state = nil,
    __rock_state_capacity = { narr, nhash }
  }
end
-- Grow a Session's tables to at least (narr, nhash) by rebuilding them at that
-- size; values and keys are preserved. The first Session is created above
-- before the compiler's capacity line runs, and a full reload may raise the
-- capacity, so this is what seats the fixpoint on a live Session.
_G.__rock_state_reserve = function(session, narr, nhash)
  local have = session.__rock_state_capacity
  if have and have[1] >= narr and have[2] >= nhash then return end
  local state = __rock_table_new(narr, nhash)
  for key, value in pairs(session.__rock_state) do state[key] = value end
  local initialized = __rock_table_new(narr, nhash)
  for key, value in pairs(session.__rock_initialized) do initialized[key] = value end
  session.__rock_state = state
  session.__rock_initialized = initialized
  session.__rock_state_capacity = { narr, nhash }
  if rawget(_G, "__rock_active_session") == session then _G.__rock_session_publish(session) end
end
_G.__rock_state_reserve_active = function()
  local capacity = rawget(_G, "__ROCK_STATE_CAPACITY")
  if not capacity then return end
  _G.__rock_state_reserve(_G.__rock_active_session, capacity[1], capacity[2])
end
if not _G.__rock_session_new_defined_once then
  _G.__rock_session_new_defined_once = true
  _G.__rock_session_current = function()
    local session = _G.__rock_active_session
    if not session then error("no active Rock Session", 0) end
    return session
  end
  _G.__rock_state_has = function(key)
    local initialized = _G.__rock_active_initialized
    if not initialized then error("no active Rock Session", 0) end
    return initialized[key] == true
  end
  _G.__rock_state_get = function(key)
    local state = _G.__rock_active_state
    if not state then error("no active Rock Session", 0) end
    return state[key]
  end
  _G.__rock_state_set = function(key, value)
    local state, initialized = _G.__rock_active_state, _G.__rock_active_initialized
    if not state or not initialized then error("no active Rock Session", 0) end
    state[key] = value
    initialized[key] = true
    return value
  end
  _G.__rock_state_prepare_static = function(value_key, done_key, initializer_key)
    local session = _G.__rock_session_current()
    local old = session.__rock_initializer_keys[value_key]
    if old ~= nil and old ~= initializer_key then
      session.__rock_state[value_key] = nil
      session.__rock_initialized[value_key] = nil
      session.__rock_state[done_key] = false
      session.__rock_initialized[done_key] = true
    end
    session.__rock_initializer_keys[value_key] = initializer_key
  end
  _G.__rock_state_publish_schema = function(schema_id)
    local session = _G.__rock_session_current()
    if session.__rock_schema_id ~= nil and session.__rock_schema_id ~= schema_id then
      error("Rock StateSchema changed; restart is required", 0)
    end
    session.__rock_schema_id = schema_id
    rawset(_G, "__rock_state_schema_id", schema_id)
    rawset(_G, "__rock_state_name_index", nil)
  end
end
-- Every Session swap publishes through the canonical pair of state cells,
-- including state-image candidates and transaction aborts.
_G.__rock_session_publish = function(session)
  rawset(_G, "__rock_active_session", session)
  rawset(_G, "__rock_active_state", session and session.__rock_state or nil)
  rawset(_G, "__rock_active_initialized", session and session.__rock_initialized or nil)
  local sink = rawget(_G, "__rock_state_cell_sink")
  if sink then sink(session) end
  return session
end
_G.__rock_session_activate = function(session)
  if type(session) ~= "table" or type(session.__rock_state) ~= "table"
      or type(session.__rock_initialized) ~= "table" then
    error("invalid Rock Session", 0)
  end
  local published = _G.__rock_state_schema_id
  if published ~= nil and session.__rock_schema_id ~= published then
    error("Rock StateSchema does not match the active Session", 0)
  end
  return _G.__rock_session_publish(session)
end
{SESSION_CELL_BINDING}
-- Join before a factory creates its body, so all later closures inherit these
-- same two cells. Match table identity: stripped bytecode has no upvalue names.
_G.__rock_bind_state_cells = function(body)
  local session = _G.__rock_active_session
  local sink = _G.__rock_state_cell_sink
  -- Initial full-chunk factories already share these cells. Avoid a redundant
  -- join there; overlays and later full chunks still join their distinct cells.
  local state_id = debug.upvalueid(sink, 1)
  local initialized_id = debug.upvalueid(sink, 2)
  local i = 1
  while true do
    local name, value = debug.getupvalue(body, i)
    if name == nil then break end
    if value == session.__rock_state then
      if debug.upvalueid(body, i) ~= state_id then
        debug.upvaluejoin(body, i, sink, 1)
      end
    elseif value == session.__rock_initialized then
      if debug.upvalueid(body, i) ~= initialized_id then
        debug.upvaluejoin(body, i, sink, 2)
      end
    end
    i = i + 1
  end
end
_G.__rock_state_clear = function(key)
  local session = _G.__rock_session_current()
  session.__rock_state[key] = nil
  session.__rock_initialized[key] = nil
end
_G.__rock_state_transaction_begin = function()
  if rawget(_G, "__rock_state_transaction") ~= nil then return end
  local session = _G.__rock_session_current()
  local state, initialized, initializer_keys = {}, {}, {}
  for key, value in pairs(session.__rock_state) do state[key] = value end
  for key, value in pairs(session.__rock_initialized) do initialized[key] = value end
  for key, value in pairs(session.__rock_initializer_keys) do initializer_keys[key] = value end
  rawset(_G, "__rock_state_transaction", {
    session = session,
    schema_id = session.__rock_schema_id,
    schema = rawget(_G, "__ROCK_STATE_SCHEMA"),
    metadata = rawget(_G, "__ROCK_STATE_METADATA"),
    nobox = rawget(_G, "__ROCK_STATE_NOBOX"),
    artifact_binding_plan = rawget(_G, "__ROCK_ARTIFACT_BINDING_PLAN"),
    name_index = rawget(_G, "__rock_state_name_index"),
    bridge_physical = rawget(_G, "__rock_state_bridge_physical"),
    state = state,
    initialized = initialized,
    initializer_keys = initializer_keys,
    published_schema_id = rawget(_G, "__rock_state_schema_id")
  })
end
_G.__rock_state_transaction_abort = function()
  local transaction = rawget(_G, "__rock_state_transaction")
  if not transaction then return end
  local session = transaction.session
  _G.__rock_session_publish(session)
  session.__rock_schema_id = transaction.schema_id
  for key in pairs(session.__rock_state) do session.__rock_state[key] = nil end
  for key, value in pairs(transaction.state) do session.__rock_state[key] = value end
  for key in pairs(session.__rock_initialized) do session.__rock_initialized[key] = nil end
  for key, value in pairs(transaction.initialized) do session.__rock_initialized[key] = value end
  for key in pairs(session.__rock_initializer_keys) do session.__rock_initializer_keys[key] = nil end
  for key, value in pairs(transaction.initializer_keys) do session.__rock_initializer_keys[key] = value end
  rawset(_G, "__ROCK_STATE_SCHEMA", transaction.schema)
  rawset(_G, "__ROCK_STATE_METADATA", transaction.metadata)
  rawset(_G, "__ROCK_STATE_NOBOX", transaction.nobox)
  rawset(_G, "__ROCK_ARTIFACT_BINDING_PLAN", transaction.artifact_binding_plan)
  rawset(_G, "__rock_artifact_binding_plan_pending", nil)
  rawset(_G, "__rock_state_name_index", transaction.name_index)
  rawset(_G, "__rock_state_bridge_physical", transaction.bridge_physical)
  rawset(_G, "__rock_state_schema_id", transaction.published_schema_id)
  rawset(_G, "__rock_state_transaction", nil)
end
_G.__rock_state_transaction_commit = function()
  local publish_binding = rawget(_G, "__rock_artifact_binding_plan_publish")
  if publish_binding then publish_binding() end
  rawset(_G, "__rock_state_transaction", nil)
end
_G.__rock_state_name_index_build = function()
  local index = {}
  local function add(key, entry)
    if not key then return end
    local old = index[key]
    if old == nil or old == entry then index[key] = entry else index[key] = false end
  end
  for _, entry in ipairs(rawget(_G, "__ROCK_STATE_SCHEMA") or {}) do
    add(entry.name, entry)
    add(entry.identity, entry)
    if entry.kind == "global" then
      for _, physical in ipairs(entry.physical or {}) do add(physical, entry) end
    end
  end
  rawset(_G, "__rock_state_name_index", index)
  return index
end
_G.__rock_state_entry_for_name = function(name)
  local index = rawget(_G, "__rock_state_name_index")
  if not index then index = _G.__rock_state_name_index_build() end
  local entry = index[name]
  return entry ~= false and entry or nil
end
_G.__rock_state_entry_for_identity = function(identity)
  for _, entry in ipairs(rawget(_G, "__ROCK_STATE_SCHEMA") or {}) do
    if entry.identity == identity then return entry end
  end
  return nil
end
_G.__rock_state_entry_get = function(entry)
  local adapters = rawget(_G, "__ROCK_STATE_NOBOX")
  local adapter = adapters and adapters[entry.identity]
  if adapter then return adapter.get() end
  local keys = entry.session_keys or {}
  if #keys == 1 then return _G.__rock_state_get(keys[1]) end
  error("StateSchema entry has no logical realization " .. tostring(entry.identity), 0)
end
_G.__rock_state_entry_try_get = function(entry)
  for _, key in ipairs(entry.session_keys or {}) do
    if _G.__rock_state_has(key) then
      return true, _G.__rock_state_entry_get(entry)
    end
  end
  return false, nil
end
_G.__rock_state_entry_set = function(entry, value)
  local adapters = rawget(_G, "__ROCK_STATE_NOBOX")
  local adapter = adapters and adapters[entry.identity]
  if adapter then return adapter.set(value) end
  local keys = entry.session_keys or {}
  if #keys == 1 then return _G.__rock_state_set(keys[1], value) end
  error("StateSchema entry has no logical realization " .. tostring(entry.identity), 0)
end
_G.__rock_state_entry_clear = function(entry)
  for _, key in ipairs(entry.session_keys or {}) do _G.__rock_state_clear(key) end
end
_G.__rock_state_install_global_bridge = function()
  local schema = rawget(_G, "__ROCK_STATE_SCHEMA") or {}
  local mt = getmetatable(_G) or {}
  local old_index, old_newindex = mt.__rock_state_original_index, mt.__rock_state_original_newindex
  if not mt.__rock_state_bridge then
    old_index, old_newindex = mt.__index, mt.__newindex
    mt.__rock_state_original_index = old_index
    mt.__rock_state_original_newindex = old_newindex
  end
  -- Which of the three pre-bridge fallback shapes applies is decided HERE, once,
  -- rather than re-decided inside the metamethod on every miss. `_G` normally
  -- carries no metatable at all, so both resolve to nil and the miss path is a
  -- plain `return nil` / `rawset` with no Lua-level TAIL CALL -- and a miss is
  -- the common case: a read of any global that is not a Rock state global and
  -- holds nil (an unset feature flag, say) reaches `__index` every time, and
  -- cannot be cached raw, because rawsetting nil leaves the key absent.
  local index_fn = type(old_index) == "function" and old_index or nil
  local index_tab = type(old_index) == "table" and old_index or nil
  local newindex_fn = type(old_newindex) == "function" and old_newindex or nil
  local newindex_tab = type(old_newindex) == "table" and old_newindex or nil
  local previous_physical = rawget(_G, "__rock_state_bridge_physical") or {}
  for _, physical in ipairs(previous_physical) do rawset(_G, physical, nil) end
  local current_physical = {}
  for _, entry in ipairs(schema) do
    if entry.kind == "global" then
      for _, physical in ipairs(entry.physical or {}) do
        rawset(_G, physical, nil)
        current_physical[#current_physical + 1] = physical
      end
    end
  end
  rawset(_G, "__rock_state_bridge_physical", current_physical)
  mt.__index = function(t, key)
    local entry = _G.__rock_state_entry_for_name(key)
    if entry and entry.kind == "global" then return _G.__rock_state_entry_get(entry) end
    if index_fn then return index_fn(t, key) end
    if index_tab then return index_tab[key] end
    return nil
  end
  mt.__newindex = function(t, key, value)
    local entry = _G.__rock_state_entry_for_name(key)
    if entry and entry.kind == "global" then return _G.__rock_state_entry_set(entry, value) end
    if newindex_fn then return newindex_fn(t, key, value) end
    if newindex_tab then newindex_tab[key] = value; return end
    rawset(t, key, value)
  end
  mt.__rock_state_bridge = true
  setmetatable(_G, mt)
end
-- Shared metatable of capturing-lambda env records (docs/closure-env-records-plan.md):
-- a record { __slot = <CodeRef>, <captures> } is callable as f(...) via __call.
-- Preserved across reload so pre- and post-reload records share metatable identity.
{FN_MT}
if not _G.__rock_rng_next then
  local __rock_bit = require("bit")
  local __rock_rng_state = 0
  local function active_rng_state()
    local session = _G.__rock_active_session
    if session and session.__rock_rng_state ~= nil then return session.__rock_rng_state end
    return __rock_rng_state
  end
  local function store_rng_state(state)
    local session = _G.__rock_active_session
    if session then session.__rock_rng_state = state else __rock_rng_state = state end
  end
  _G.__rock_rng_seed = function(s)
    local state = __rock_bit.tobit(tonumber(s))
    store_rng_state(state)
  end
  -- Whole RNG state is this one counter; get/set serialize it for replay
  -- keyframe restore (docs/replay-scrub-plan.md). It is a plain 32-bit number.
  _G.__rock_rng_state_get = function() return active_rng_state() end
  _G.__rock_rng_state_set = function(s)
    local state = __rock_bit.tobit(tonumber(s))
    store_rng_state(state)
  end
  _G.__rock_rng_next = function()
    local state = __rock_bit.tobit(active_rng_state() + 0x9E3779B9)
    state = __rock_bit.bxor(state, __rock_bit.lshift(state, 13))
    state = __rock_bit.bxor(state, __rock_bit.rshift(state, 17))
    state = __rock_bit.bxor(state, __rock_bit.lshift(state, 5))
    store_rng_state(state)
    local output = __rock_bit.bxor(state, __rock_bit.rshift(state, 16))
    if output < 0 then return output + 4294967296 end
    return output
  end

  local __rock_presentation_rng_state = 0
  local function active_presentation_rng_state()
    local session = _G.__rock_active_session
    if session and session.__rock_presentation_rng_state ~= nil then return session.__rock_presentation_rng_state end
    return __rock_presentation_rng_state
  end
  local function store_presentation_rng_state(state)
    local session = _G.__rock_active_session
    if session then session.__rock_presentation_rng_state = state else __rock_presentation_rng_state = state end
  end
  _G.__rock_presentation_rng_seed = function(s)
    local state = __rock_bit.tobit(tonumber(s))
    store_presentation_rng_state(state)
  end
  _G.__rock_presentation_rng_state_get = function() return active_presentation_rng_state() end
  _G.__rock_presentation_rng_state_set = function(s)
    local state = __rock_bit.tobit(tonumber(s))
    store_presentation_rng_state(state)
  end
  _G.__rock_presentation_rng_next = function()
    local state = __rock_bit.tobit(active_presentation_rng_state() + 0x9E3779B9)
    state = __rock_bit.bxor(state, __rock_bit.lshift(state, 13))
    state = __rock_bit.bxor(state, __rock_bit.rshift(state, 17))
    state = __rock_bit.bxor(state, __rock_bit.lshift(state, 5))
    store_presentation_rng_state(state)
    local output = __rock_bit.bxor(state, __rock_bit.rshift(state, 16))
    if output < 0 then return output + 4294967296 end
    return output
  end

  local __seed = tonumber(os.getenv("ROCK_TEST_SEED"))
  if __seed then
    _G.__rock_rng_seed(__seed)
    _G.__rock_presentation_rng_seed(__seed + 10007)
  else
    _G.__rock_rng_seed(os.time())
    _G.__rock_presentation_rng_seed(os.time() + 10007)
  end
end
"#;

/// Map a value-struct field type to a C type for `ffi.cdef`, or `None` when the
/// field is not C-layout-mappable. Delegates to the single mappability oracle
/// `skull_mir::lower::value_field_c_type` (shared with the GC-tier
/// shared `ValueStorage` classification, so the cdef view and the pack-site
/// `_ct`/`_vt` choice can never diverge).
fn c_field_type(ws: &Workspace, ty: bone_semantic::prelude::TyId) -> Option<String> {
    skull_mir::lower::value_field_c_type(ws, ty)
}

/// The emitted name of a nominal type. A modules-v1 definition's name is its
/// DefKey, which is neither a Lua nor a C identifier, so every minted name --
/// the cdef typedef, `<N>_ct`/`<N>_lct`/`<N>_vt`, an `ffi.typeof`/`metatype`
/// argument, an `__rock_enum_<E>` global -- takes its identifier form. A legacy
/// name has no `::` and passes through byte-identical. DISPLAY text keeps the
/// DefKey: it reads as the definition's path, and nothing parses it.
fn type_symbol(name: ustr::Ustr) -> ustr::Ustr {
    skull_mir::mangle::symbol_of_key(name)
}

/// Whether a struct named `name` is declared `value` (C-layout cdata).
fn is_value_struct(ws: &Workspace, name: ustr::Ustr) -> bool {
    ws.struct_by_name
        .get(&name)
        .and_then(|sid| ws.modules.get(sid.module.index()).map(|m| m.structs[sid.def].is_value))
        .unwrap_or(false)
}

/// Canonical (deduped, name-sorted) list of `value` structs in the workspace,
/// via the same `struct_by_name` view name resolution uses. Sorted so the
/// emitted Lua is deterministic across runs (module maps hash-iterate).
fn canonical_value_structs(ws: &Workspace) -> Vec<bone_semantic::hir::StructDef> {
    let mut v: Vec<bone_semantic::hir::StructDef> = ws
        .struct_by_name
        .values()
        .filter_map(|sid| {
            let s = &ws.modules.get(sid.module.index())?.structs[sid.def];
            s.is_value.then(|| s.clone())
        })
        .collect();
    v.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
    v
}


/// `<Name>_lct(<one param per primitive leaf, in `value_leaf_paths` order>)`:
/// allocate the cdata and write each leaf through a hoisted nested reference.
/// The call site stays a single expression, so const-hoisting and the
/// read-only-use analysis are untouched.
fn leaf_ctor_def(name: ustr::Ustr, paths: &[ustr::Ustr]) -> String {
    // Names are POSITIONAL indices, never derived from the path. A
    // dot->underscore mapping is not injective: sibling fields `pos` (holding
    // `inner`) and `pos_inner` both render `l_pos_inner`, and Lua silently lets
    // the later duplicate shadow the earlier, writing one leaf's value into the
    // other. Dotted keys are injective precisely because `.` cannot appear in an
    // identifier; deriving identifiers back from them throws that away.
    let params: Vec<String> = (0..paths.len()).map(|i| format!("l{i}")).collect();
    let mut body = String::new();
    let _ = std::fmt::Write::write_fmt(&mut body, format_args!("  local t = {name}_ct()\n"));
    // One local per distinct nested prefix (EVERY ancestor, shortest first, so
    // `b` is bound before `b.lo`), so a leaf write never re-indexes an aggregate
    // field -- each such index allocates a reference cdata.
    let mut refs: Vec<String> = Vec::new();
    for p in paths {
        let comps: Vec<&str> = p.as_str().split('.').collect();
        for n in 1..comps.len() {
            let prefix = comps[..n].join(".");
            if !refs.iter().any(|r| *r == prefix) {
                refs.push(prefix);
            }
        }
    }
    refs.sort_by_key(|r| r.matches('.').count());
    let ref_local = |r: &str| -> String {
        format!("r{}", refs.iter().position(|x| x == r).expect("prefix registered"))
    };
    for r in &refs {
        let base = match r.rsplit_once('.') {
            Some((pre, last)) => format!("{}.{last}", ref_local(pre)),
            None => format!("t.{r}"),
        };
        let _ = std::fmt::Write::write_fmt(
            &mut body,
            format_args!("  local {} = {base}\n", ref_local(r)),
        );
    }
    for (i, p) in paths.iter().enumerate() {
        let target = match p.as_str().rsplit_once('.') {
            Some((prefix, last)) => format!("{}.{last}", ref_local(prefix)),
            None => format!("t.{p}"),
        };
        let _ = std::fmt::Write::write_fmt(&mut body, format_args!("  {target} = {}\n", params[i]));
    }
    format!("{name}_lct = function({})\n{body}  return t\nend\n", params.join(", "))
}

/// Emit the `ffi.cdef` typedefs + `<Name>_ct = ffi.typeof(...)` constructors for
/// every FFI-clean `value` struct, so value-struct construction (`V2_ct(x, y)`)
/// and `NativeBuffer<V2>` (`ffi.new("V2[?]", n)`) work. Returns
/// `(lua, ffi_emitted, gc_emitted)`:
/// - `ffi_emitted` -- names a cdef typedef was emitted for (the gate
///   `value_struct_metatypes` / `c_struct_inspect_defs` share: an
///   `ffi.metatype`/`ffi.istype` on an undeclared ctype errors).
/// - `gc_emitted` -- the GC-carrying (table-repr) names, materialized by
///   `value_struct_gc_defs` as `<Name>_vt` table ctors (the tier is decided by
///   `skull_mir::lower::ValueStorage`; this replaces the old silent
///   skip-then-`Name_ct`-nil-crash, unfuck backlog #51).
///
/// C requires a nested struct's typedef to precede its use, so FFI-clean
/// emission is dependency-ordered (mirrors rock-codegen/src/backend/lua/
/// structs.rs): each round emits the structs whose value-struct fields are all
/// emitted. A round with no progress is a definition cycle: rejected, not
/// emitted broken. A GC-carrying struct never enters the cdef ordering (a table
/// has no C layout to sequence).
fn value_struct_defs(
    ws: &Workspace,
    rep_arena: &skull_mir::lower::RepArena,
) -> Result<(String, rustc_hash::FxHashSet<ustr::Ustr>, rustc_hash::FxHashSet<ustr::Ustr>), CompilerError>
{
    let vstructs = canonical_value_structs(ws);
    let mut emitted: rustc_hash::FxHashSet<ustr::Ustr> = Default::default();
    let mut gc: rustc_hash::FxHashSet<ustr::Ustr> = Default::default();
    if vstructs.is_empty() {
        return Ok((String::new(), emitted, gc));
    }

    // Partition by tier. GC-carrying value structs materialize as Lua tables
    // (`_vt`), never as cdef, so only FFI-clean structs enter the ordering.
    let mut ffi_structs = Vec::new();
    for s in vstructs {
        let storage = rep_arena
            .nominal_value_storage_required(ws, bone_semantic::hir::FnBackend::Lua, s.name)
            .map_err(|e| CompilerError::new_no_location(e.message))?;
        if storage != skull_mir::lower::ValueStorage::CData {
            gc.insert(s.name);
        } else {
            ffi_structs.push(s);
        }
    }
    let ffi_names: rustc_hash::FxHashSet<ustr::Ustr> = ffi_structs.iter().map(|s| s.name).collect();

    let mut typedefs = String::new();
    let mut ctors = String::new();
    let mut remaining = ffi_structs;

    while !remaining.is_empty() {
        let mut next = Vec::new();
        let mut progress = false;
        for s in remaining {
            let mut body = String::new();
            let mut blocked = false;
            for f in &s.fields {
                // An FFI-clean struct has every field C-mappable by construction
                // (that is what the `CData` storage plan means).
                let ct = c_field_type(ws, f.ty).expect(
                    "FFI-clean value-struct field must be C-mappable (CData storage invariant)",
                );
                let dep = ustr::Ustr::from(ct.as_str());
                if ffi_names.contains(&dep) && !emitted.contains(&dep) {
                    blocked = true;
                }
                // The dependency key above is the LOGICAL name (what the
                // workspace sets hold); the cdef text is the C one.
                body.push_str(&format!("  {} {};\n", type_symbol(dep), f.name));
            }
            if blocked {
                next.push(s);
            } else {
                let sym = type_symbol(s.name);
                typedefs.push_str(&format!("typedef struct {{\n{}}} {};\n", body, sym));
                ctors.push_str(&format!("{n}_ct = ffi.typeof(\"{n}\")\n", n = sym));
                // Leaf-taking ctor: LuaJIT has no brace elision, so a nested
                // field cannot be passed as loose scalars to `_ct`. Allocate and
                // write leaves instead, hoisting each nested reference once.
                // Used by the normalization that keeps a `nobox` field from
                // materializing when its parent boxes.
                let paths = rep_arena
                    .nominal_value_leaf_paths_required(
                        ws,
                        bone_semantic::hir::FnBackend::Lua,
                        s.name,
                    )
                    .map_err(|e| CompilerError::new_no_location(e.message))?;
                if paths.iter().any(|p| p.contains('.')) {
                    ctors.push_str(&leaf_ctor_def(sym, &paths));
                }
                emitted.insert(s.name);
                progress = true;
            }
        }
        if !progress {
            let names: Vec<&str> = next.iter().map(|s| s.name.as_str()).collect();
            return Err(CompilerError::new_no_location(format!(
                "skull: value-struct cdef ordering made no progress — \
                 cyclic value-struct fields among: {}",
                names.join(", ")
            )));
        }
        remaining = next;
    }

    if typedefs.is_empty() {
        return Ok((String::new(), emitted, gc));
    }
    // Direct ffi.cdef: a skull chunk loads once per process, so a failure here
    // is a genuine emit bug and must abort with LuaJIT's own message — the old
    // pcall swallowed it and every `<Name>_ct` use downstream failed cryptically.
    let (checks, storage) = value_leaf_layout_checks(ws, &emitted, rep_arena)?;
    Ok((format!("ffi.cdef[[\n{}]]\n{}{}{}", typedefs, ctors, checks, storage), emitted, gc))
}

/// Assert, once at chunk load, that skull's C layout model agrees with LuaJIT's
/// for every leaf of every FFI-clean value struct -- and, from that same checked
/// layout, register each struct as `NativeBuffer` storage in
/// `_G.__ROCK_NB_STORAGE` (whose primitive/sized half the prelude declares).
///
/// One walk, two emitted views. The registry states what a buffer of this
/// element is made of; the assertions state that the model producing it agrees
/// with LuaJIT. Deriving both from the same owned RepPlan C-layout result
/// is what keeps them from describing different layouts.
///
/// Leaf addressing (`docs/native-buffer-leaf-addressing-plan.md`) indexes typed
/// views with COMPILER-COMPUTED byte offsets — a literal constant per leaf, which
/// is the whole point, since a runtime offset lookup would cost what the boxing
/// cost. If that model ever disagreed with LuaJIT's, every element access would
/// read and write the wrong address: silent memory corruption, not a failure.
/// So the two are cross-checked against `ffi.offsetof` composed along each leaf's
/// field chain, and a mismatch aborts loudly at load.
///
/// Every name here is already in the FFI-clean emitted set, so a missing layout
/// (or a disagreement between the two leaf walkers) is an INVARIANT FAILURE, not
/// an optional check to skip: skipping would emit a type whose offsets nothing
/// validated, and leaf addressing would then read and write unvalidated
/// addresses. Rejected with the type named.
fn value_leaf_layout_checks(
    ws: &Workspace,
    emitted: &rustc_hash::FxHashSet<ustr::Ustr>,
    rep_arena: &skull_mir::lower::RepArena,
) -> Result<(String, String), CompilerError> {
    use std::fmt::Write as _;
    let mut names: Vec<ustr::Ustr> = emitted.iter().copied().collect();
    names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let mut out = String::new();
    let mut storage = String::new();
    for name in names {
        let (leaves, size, _align) = rep_arena
            .nominal_leaf_layout(ws, bone_semantic::hir::FnBackend::Lua, name)
            .map_err(|e| {
                CompilerError::new_no_location(format!(
                    "skull: cannot validate the C layout of FFI-clean value struct `{name}`: {}",
                    e.message
                ))
            })?;
        let sym = type_symbol(name);
        let _ = writeln!(
            out,
            "if ffi.sizeof(\"{sym}\") ~= {size} then error(\"skull layout drift: \
             sizeof({sym}) = \" .. ffi.sizeof(\"{sym}\") .. \", expected {size}\", 0) end"
        );
        for l in &leaves {
            let _ = writeln!(
                out,
                "if ({expr}) ~= {off} then error(\"skull layout drift: {sym}.{path} at \" \
                 .. ({expr}) .. \", expected {off}\", 0) end",
                expr = l.offsetof_expr,
                off = l.byte_off,
                path = l.path,
            );
        }
        storage.push_str(&skull_mir::lower::nb_value_storage_lua(name, &leaves, size));
    }
    Ok((out, storage))
}

/// Emit the table-repr materialization defs for every GC-carrying value struct:
/// a `<Name>_vt(...)` ctor building an immutable Lua table tagged `__ty`, plus a
/// shared per-type metatable (`__rock_vt_<Name>`) carrying value `__eq`
/// (scalar/value fields structural, GC-ref fields by reference) and
/// `__tostring`. The single-call wire shape matches `<Name>_ct`, so pack sites
/// choose a ctor NAME by tier, not a new code path.
///
/// The metatable table is preserved across hot reloads (`= X or {}`, then its
/// fields reassigned) so tables built before a reload keep the SAME metatable
/// identity -- LuaJIT only fires `__eq` when both operands share the metamethod.
/// Rendering is handled by the generic table `inspect` path (a `__ty`-tagged
/// table), so no `__ROCK_C_STRUCT` (ffi.istype) entry is needed.
fn value_struct_gc_defs(
    ws: &Workspace,
    gc: &rustc_hash::FxHashSet<ustr::Ustr>,
    rep_arena: &skull_mir::lower::RepArena,
) -> Result<String, CompilerError> {
    let mut out = String::new();
    for s in canonical_value_structs(ws) {
        if !gc.contains(&s.name) {
            continue;
        }
        let n = s.name;
        // `n` stays the readable path for DISPLAY text; every name the chunk
        // defines or tags with is the type's symbol.
        let sym = type_symbol(n);
        let mt = skull_codegen_lua::value_struct_metatable_name(sym.as_str());
        out.push_str(&format!("{mt} = {mt} or {{}}\n"));
        // A leaf-table struct has no declared-field keys to compare or print, so
        // both metamethods and its ctor run off the leaf paths instead.
        let storage = rep_arena
            .nominal_value_storage_required(ws, bone_semantic::hir::FnBackend::Lua, n)
            .map_err(|e| CompilerError::new_no_location(e.message))?;
        if storage == skull_mir::lower::ValueStorage::LeafTable {
            let paths = rep_arena
                .nominal_value_leaf_paths_required(ws, bone_semantic::hir::FnBackend::Lua, n)
                .map_err(|e| CompilerError::new_no_location(e.message))?;
            out.push_str(&format!("{mt}.__eq = function(a, b)\n"));
            out.push_str(
                "  if type(a) ~= \"table\" or type(b) ~= \"table\" then return false end\n",
            );
            let cmps: Vec<String> =
                paths.iter().map(|p| format!("a[\"{p}\"] == b[\"{p}\"]")).collect();
            out.push_str(&format!(
                "  return {}\n",
                if cmps.is_empty() { "true".to_string() } else { cmps.join(" and ") }
            ));
            out.push_str("end\n");
            out.push_str(&format!("{mt}.__tostring = function(v)\n"));
            let ts: Vec<String> =
                paths.iter().map(|p| format!("\"{p}: \" .. tostring(v[\"{p}\"])")).collect();
            out.push_str(&format!(
                "  return \"{n} {{ \" .. {} .. \" }}\"\nend\n",
                if ts.is_empty() { "\"\"".to_string() } else { ts.join(" .. \", \" .. ") }
            ));
            let params: Vec<String> = (0..paths.len()).map(|i| format!("l{i}")).collect();
            // An absent tag canonicalizes to `nil` -- the form a `= null` write
            // and an absent literal both store. The scalar ABI spells absence
            // `false` on the wire, and two logically-equal absent values then
            // compared unequal (`__eq` is a per-key compare, and `false ~= nil`).
            let presence = rep_arena
                .nominal_value_leaf_presence_flags_required(ws, bone_semantic::hir::FnBackend::Lua, n)
                .map_err(|e| CompilerError::new_no_location(e.message))?;
            let assigns: Vec<String> = paths
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let tag = presence.get(i).copied().unwrap_or(false);
                    let v = if tag { format!("l{i} or nil") } else { format!("l{i}") };
                    format!("[\"{p}\"] = {v}, ")
                })
                .collect();
            out.push_str(&format!(
                "{sym}_lct = function({p})\n{r}\nend\n",
                p = params.join(", "),
                r = skull_codegen_lua::wrapper_return(&format!(
                    "setmetatable({{ {b}__ty = \"{sym}\" }}, {mt})",
                    b = assigns.concat(),
                )),
            ));
            continue;
        }
        // value __eq
        out.push_str(&format!("{mt}.__eq = function(a, b)\n"));
        out.push_str("  if type(a) ~= \"table\" or type(b) ~= \"table\" then return false end\n");
        if s.fields.is_empty() {
            out.push_str("  return true\n");
        } else {
            let cmps: Vec<String> = s
                .fields
                .iter()
                .map(|f| {
                    if skull_mir::lower::gc_eq_is_structural(ws, f.ty) {
                        format!("a.{n} == b.{n}", n = f.name)
                    } else {
                        format!("rawequal(a.{n}, b.{n})", n = f.name)
                    }
                })
                .collect();
            out.push_str(&format!("  return {}\n", cmps.join(" and ")));
        }
        out.push_str("end\n");
        // __tostring: field-wise, mirroring the cdata tier's `Name { f: v, .. }`.
        out.push_str(&format!("{mt}.__tostring = function(v)\n"));
        if s.fields.is_empty() {
            out.push_str(&format!("  return \"{n} {{ }}\"\n"));
        } else {
            let ts: Vec<String> = s
                .fields
                .iter()
                .map(|f| format!("\"{}: \" .. tostring(v.{})", f.name, f.name))
                .collect();
            out.push_str(&format!(
                "  return \"{n} {{ \" .. {} .. \" }}\"\n",
                ts.join(" .. \", \" .. ")
            ));
        }
        out.push_str("end\n");
        // `<Name>_vt(...)` positional ctor in declared field order.
        let params: Vec<String> = s.fields.iter().map(|f| f.name.as_str().to_string()).collect();
        let assigns: Vec<String> =
            s.fields.iter().map(|f| format!("{n} = {n}", n = f.name)).collect();
        let mut body = assigns.join(", ");
        if !body.is_empty() {
            body.push_str(", ");
        }
        out.push_str(&format!(
            "{sym}_vt = function({p})\n{r}\nend\n",
            p = params.join(", "),
            r = skull_codegen_lua::wrapper_return(&format!(
                "setmetatable({{ {b}__ty = \"{sym}\" }}, {mt})",
                b = body,
            )),
        ));
    }
    Ok(out)
}

/// Emit the `ffi.cdef` typedef + `<Enum>_ct` ctor for every cdata-eligible enum
/// (`skull_mir::lower::enum_value_shape`): a `value`-struct-style cdata
/// `{ int __tag; <T> _0; }` carrying the variant tag + its single scalar payload.
/// Scalar payloads only (cut 1), so there is no inter-enum dependency; emitted
/// BEFORE value structs so a future enum-typed value-struct field resolves. Name-
/// sorted for deterministic output. Reflection is sacrificed: there is no
/// `{__variant}` table -- see docs/skull-enum-increment2-plan.md.
fn enum_cdata_defs(ws: &Workspace) -> String {
    let mut names: Vec<ustr::Ustr> = ws.enums_by_name.keys().copied().collect();
    names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let mut typedefs = String::new();
    let mut ctors = String::new();
    for name in names {
        if let Some(shape) = skull_mir::lower::EnumPlan::for_name(ws, name)
            .and_then(|plan| plan.cdata().cloned())
        {
            let slots: String =
                shape.slots.iter().map(|(nm, t)| format!(" {t} {nm};")).collect();
            let sym = type_symbol(name);
            typedefs.push_str(&format!("typedef struct {{ int __tag;{slots} }} {n};\n", n = sym));
            ctors.push_str(&format!("{n}_ct = ffi.typeof(\"{n}\")\n", n = sym));
        }
    }
    if typedefs.is_empty() {
        return String::new();
    }
    format!("ffi.cdef[[\n{}]]\n{}", typedefs, ctors)
}

/// Attach an `ffi.metatype` to each cdata-eligible ENUM ctype giving it value
/// `__eq` (compare `__tag` + `_0`, so two distinct `<Enum>_ct` cdata with equal
/// fields are `==`) and a `__tostring` that
/// renders `<Enum>::<Variant>` from a baked names table -- static reflection, no
/// runtime engine. Without `__eq`, LuaJIT compares struct cdata by identity, so an
/// ESCAPING enum (stored in `[Enum]`, a Map, ...) mis-compares. Name-sorted.
fn enum_cdata_metatypes(ws: &Workspace) -> String {
    let mut names: Vec<ustr::Ustr> = ws.enums_by_name.keys().copied().collect();
    names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let mut out = String::new();
    let mut any = false;
    for name in names {
        let Some(shape) = skull_mir::lower::EnumPlan::for_name(ws, name)
            .and_then(|plan| plan.cdata().cloned())
        else {
            continue;
        };
        if !any {
            out.push_str("_G.__ROCK_C_METATYPE = _G.__ROCK_C_METATYPE or {}\n");
            any = true;
        }
        // value `__eq`: all C fields equal (`__tag` + every payload slot).
        let mut eq = String::from("a.__tag == b.__tag");
        for (nm, _) in &shape.slots {
            eq.push_str(&format!(" and a.{nm} == b.{nm}"));
        }
        let sym = type_symbol(name);
        out.push_str(&format!("_G.__ROCK_C_METATYPE[\"{sym}\"] = {{\n"));
        out.push_str(&format!(
            "  __eq = function(a, b)\n\
             \x20   if type(b) ~= \"cdata\" or type(a) ~= \"cdata\" then return false end\n\
             \x20   return {eq}\n  end,\n"
        ));
        // __tostring: per-variant baked render -- compile-time reflection matching
        // rock's inspect (unit `E::V`; tuple `E::V { [1] = .., [2] = .. }`; record
        // `E::V { f = .., .. }` with record fields SORTED alphabetically). Static, no
        // runtime reflection engine -- the directive's "solve reflection at compile
        // time" for the tostring case.
        let mut ts = String::from("  __tostring = function(v)\n    local t = v.__tag\n");
        if let Some(eid) = ws.enums_by_name.get(&name) {
            use bone_semantic::hir::VariantPayload;
            let ed = &ws.modules[eid.module.index()].enums[eid.def];
            for (i, var) in ed.variants.iter().enumerate() {
                let vn = var.name;
                let render = match &var.payload {
                    VariantPayload::Tuple(tys) if !tys.is_empty() => {
                        let parts: Vec<String> = (0..tys.len())
                            .map(|j| format!("\"[{k}] = \" .. tostring(v._{j})", k = j + 1))
                            .collect();
                        format!("\"{name}::{vn} {{ \" .. {} .. \" }}\"", parts.join(" .. \", \" .. "))
                    }
                    VariantPayload::Record(fields) if !fields.is_empty() => {
                        let mut fns: Vec<ustr::Ustr> = fields.iter().map(|f| f.name).collect();
                        fns.sort_by(|a, b| a.as_str().cmp(b.as_str()));
                        let parts: Vec<String> =
                            fns.iter().map(|f| format!("\"{f} = \" .. tostring(v._{f})")).collect();
                        format!("\"{name}::{vn} {{ \" .. {} .. \" }}\"", parts.join(" .. \", \" .. "))
                    }
                    _ => format!("\"{name}::{vn}\""), // unit (or empty tuple/record)
                };
                ts.push_str(&format!("    if t == {i} then return {render} end\n"));
            }
        }
        ts.push_str("    return \"?\"\n  end,\n}\n");
        out.push_str(&ts);
        out.push_str(&format!(
            "pcall(ffi.metatype, \"{sym}\", {{\n\
             \x20 __eq = function(a, b) return _G.__ROCK_C_METATYPE[\"{sym}\"].__eq(a, b) end,\n\
             \x20 __tostring = function(v) return _G.__ROCK_C_METATYPE[\"{sym}\"].__tostring(v) end,\n\
             }})\n"
        ));
    }
    out
}

/// Attach an `ffi.metatype` to each value-struct ctype giving it value
/// `__eq` (field-wise) and `__tostring`, so `c == c2` on two distinct cdata
/// compares by value instead of by pointer identity (the LuaJIT default).
/// Mirrors rock-codegen/structs.rs: a thin metatype trampoline delegates to a
/// mutable `_G.__ROCK_C_METATYPE[name]` impl table (metatypes seal once, so the
/// trampoline indirection is what lets a reload update the impl). Gated by
/// `emitted` — exactly the structs `value_struct_defs` declared a ctype for.
fn value_struct_metatypes(ws: &Workspace, emitted: &rustc_hash::FxHashSet<ustr::Ustr>) -> String {
    let mut out = String::new();
    let mut any = false;
    {
        for s in canonical_value_structs(ws) {
            if !emitted.contains(&s.name) {
                continue;
            }
            if !any {
                out.push_str("_G.__ROCK_C_METATYPE = _G.__ROCK_C_METATYPE or {}\n");
                any = true;
            }
            let n = s.name;
            let sym = type_symbol(n);
            out.push_str(&format!("_G.__ROCK_C_METATYPE[\"{sym}\"] = {{\n"));
            out.push_str("  __eq = function(a, b)\n");
            out.push_str(
                "    if type(b) ~= \"cdata\" or type(a) ~= \"cdata\" then return false end\n",
            );
            if s.fields.is_empty() {
                out.push_str("    return true\n");
            } else {
                let eq: Vec<String> = s
                    .fields
                    .iter()
                    .map(|f| format!("a.{n} == b.{n}", n = f.name))
                    .collect();
                out.push_str(&format!("    return {}\n", eq.join(" and ")));
            }
            out.push_str("  end,\n  __tostring = function(v)\n");
            if s.fields.is_empty() {
                out.push_str(&format!("    return \"{n} {{ }}\"\n"));
            } else {
                let ts: Vec<String> = s
                    .fields
                    .iter()
                    .map(|f| format!("\"{}: \" .. tostring(v.{})", f.name, f.name))
                    .collect();
                out.push_str(&format!(
                    "    return \"{n} {{ \" .. {} .. \" }}\"\n",
                    ts.join(" .. \", \" .. ")
                ));
            }
            out.push_str("  end,\n}\n");
            out.push_str(&format!(
                "pcall(ffi.metatype, \"{sym}\", {{\n\
                 \x20 __eq = function(a, b) return _G.__ROCK_C_METATYPE[\"{sym}\"].__eq(a, b) end,\n\
                 \x20 __tostring = function(v) return _G.__ROCK_C_METATYPE[\"{sym}\"].__tostring(v) end,\n\
                 }})\n"
            ));
        }
    }
    out
}

/// Emit per-enum runtime defs. Two cases, split on representation:
///
/// - A **payload-free** enum (all unit variants) is an `int` value globally (the
///   variant's declaration-order ordinal). No singleton tables -- construction,
///   `match`, `==`, Map keys, and value-struct fields are all plain ints. We emit
///   only a `__rock_enum_<E>_names = { [0]="<E>::<V0>", ... }` table (0-indexed by
///   ordinal, pre-prefixed) for static-type-driven stringify of `{x}` interp holes
///   (see `lower::stringify`). It must match rock's `inspect` output EXACTLY.
///
/// - A **mixed** enum (has payload variants) keeps its values as self-describing
///   tables; its unit variants lower to an interned singleton
///   (`__rock_enum_<E>_<V> = <self> or { __variant=.., __ty=.. }`), so they're
///   stable Lua map keys and reference-equal across uses
///   (see `lower::enum_singleton_name`).
///
/// The `<global> = <global> or { ... }` preserve-if-exists form (matching
/// rock-codegen) is load-bearing for HOT RELOAD: a reload re-dofiles the whole
/// chunk, and a bare re-creation would mint a NEW table, breaking reference-
/// equality with enum values stored in preserved globals (e.g. a `GRID:
/// []<TileType>` whose `TileType::Wall` entries would no longer `==` the
/// freshly-created singleton -> walls silently stop rendering). A payload-free
/// enum sidesteps this entirely: its values are ints, identity-free by nature.
fn enum_runtime_defs(ws: &Workspace) -> String {
    use bone_semantic::hir::VariantPayload;
    let mut out = String::new();
    for module in ws.modules.iter() {
        for (_, e) in module.enums.iter() {
            // A cdata-eligible enum is a value cdata `{__tag, _0}` with no
            // singleton or names table; its ctype defs come from
            // `enum_cdata_defs`, while `type_table_defs` records its physical
            // slots and logical variants for typed runtime consumers.
            if skull_mir::lower::EnumPlan::for_name(ws, e.name)
                .is_some_and(|plan| plan.cdata().is_some())
            {
                continue;
            }
            let all_unit =
                !e.variants.is_empty() && e.variants.iter().all(|v| matches!(v.payload, VariantPayload::Unit));
            if all_unit {
                // Payload-free: int repr. Emit the ordinal->display-name table for
                // interp stringify (`__rock_enum_<E>_names[tag]`).
                let items: Vec<String> = e
                    .variants
                    .iter()
                    .enumerate()
                    .map(|(ord, v)| format!("[{ord}] = \"{e}::{v}\"", e = e.name, v = v.name))
                    .collect();
                // The table is a Lua GLOBAL, so its name carries the enum's
                // symbol; the values it holds are display text.
                out.push_str(&format!(
                    "__rock_enum_{}_names = {{ {} }}\n",
                    type_symbol(e.name),
                    items.join(", ")
                ));
            } else {
                // Mixed: unit variants are interned singleton tables.
                for v in e.variants.iter() {
                    if matches!(v.payload, VariantPayload::Unit) {
                        let sym = type_symbol(e.name);
                        let g = enum_singleton_name(sym.as_str(), v.name.as_str());
                        // `__ty` is looked up in `__ROCK_TYPE_TABLE`, so it is
                        // the same spelling that registry is keyed on.
                        out.push_str(&format!(
                            "{g} = {g} or {{ __variant = \"{var}\", __ty = \"{ty}\" }}\n",
                            var = v.name,
                            ty = sym,
                        ));
                    }
                }
            }
        }
    }
    out
}

/// Register an `inspect`/`copy`/`name` closure into `_G.__ROCK_C_STRUCT` for
/// every value cdata the compiler mints. That registry is what a consumer asks
/// "what is this cdata, and how do I copy it": the stdlib `inspect` renders it
/// structurally (`ffi.istype(info.name, obj)` -> `info.inspect(obj)`) instead of
/// printing an address, `deepcopy` copies it instead of SHARING it, and the sim
/// checkpoint census classifies it as `value_cdata` instead of refusing it as
/// unmodelled. Mirrors rock-codegen/src/backend/lua/structs.rs.
///
/// Every consumer asks through the stdlib's `__rock_c_struct_of`, which
/// memoizes the `ffi.istype` scan per CTypeID in `__ROCK_C_STRUCT_BY_CTYPE`.
/// The rows own that memo's lifetime: it is reset here, after the rows, so a
/// re-emission (a hot reload that re-runs this scaffolding) can never serve a
/// row that no longer exists or predates its replacement.
///
/// Two producers, one registry. `value` structs are gated by `emitted` — exactly
/// the structs `value_struct_defs` declared a ctype for, so `ffi.istype` never
/// names an undefined type. A cdata-eligible enum ([`enum_cdata_defs`]) is the
/// same layout tier: a typedef'd anonymous struct `{ int __tag; <slots> }`
/// copied through its own `<Enum>_ct`. Its `inspect` delegates to `tostring`,
/// which reaches the metatype `__tostring` [`enum_cdata_metatypes`] emitted --
/// the enum's rendering has one source, not two that can drift.
fn c_struct_inspect_defs(ws: &Workspace, emitted: &rustc_hash::FxHashSet<ustr::Ustr>) -> String {
    let mut out = String::new();
    {
        let mut enums: Vec<ustr::Ustr> = ws.enums_by_name.keys().copied().collect();
        enums.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        for name in enums {
            let Some(shape) = skull_mir::lower::EnumPlan::for_name(ws, name)
                .and_then(|plan| plan.cdata().cloned())
            else {
                continue;
            };
            let copy_args: Vec<String> =
                shape.field_names().iter().map(|f| format!("v.{f}")).collect();
            let sym = type_symbol(name);
            out.push_str(&format!(
                "_G.__ROCK_C_STRUCT[\"{sym}\"] = {{ inspect = function(v)\n  return tostring(v)\n\
                 end, copy = function(v)\n  return {sym}_ct({})\nend, name = \"{sym}\" }}\n",
                copy_args.join(", ")
            ));
        }
        for s in canonical_value_structs(ws) {
            if !emitted.contains(&s.name) {
                continue;
            }
            let n = s.name;
            let sym = type_symbol(n);
            out.push_str(&format!("_G.__ROCK_C_STRUCT[\"{sym}\"] = {{ inspect = function(v)\n"));
            if s.fields.is_empty() {
                out.push_str(&format!("  return \"{n} {{ }}\"\n"));
            } else {
                let parts: Vec<String> = s
                    .fields
                    .iter()
                    .map(|f| format!("\"{}: \" .. inspect(v.{})", f.name, f.name))
                    .collect();
                out.push_str(&format!(
                    "  return \"{n} {{ \" .. {} .. \" }}\"\n",
                    parts.join(" .. \", \" .. ")
                ));
            }
            let copy_args: Vec<String> =
                s.fields.iter().map(|f| format!("v.{}", f.name)).collect();
            out.push_str(&format!(
                "end, copy = function(v)\n  return {sym}_ct({})\nend, name = \"{sym}\" }}\n",
                copy_args.join(", ")
            ));
        }
    }
    out.push_str("_G.__ROCK_C_STRUCT_BY_CTYPE = {}\n");
    out
}

/// A RECURSIVE, structural rendering of a declared type as a Lua table literal.
///
/// [`reflect_field_type`] is flat and lossy on purpose -- it collapses a
/// nullable to `any`, an array to `[]` and drops every generic argument -- so a
/// consumer holding one of its strings cannot tell a `Vec<Image>`'s element
/// from an ordinary int. That is why a direct global, a `Vec` element, a map
/// value and a tuple member all serialize their raw process-local ids today.
/// This preserves the shape all the way down, so the walk that reaches a leaf
/// also knows what the leaf was DECLARED to be.
///
/// Purely STRUCTURAL: it names nominal types and says nothing about which of
/// them the host owns. The native-class list stays in exactly one place
/// (`stone/engine/host_projection.rock`); a copy here would be a second thing
/// to forget a type in, and the two drifting would make every handle of the
/// forgotten type serialize as a missing one.
///
/// `None` when the type contains no nominal type anywhere, so a descriptor is
/// emitted only where one could possibly matter -- `Vec<int>` gets none. Every
/// value with no descriptor keeps exactly the walk, and the bytes, it has today.
pub fn type_desc_lua(ws: &Workspace, ty: bone_semantic::prelude::TyId) -> Option<String> {
    let mut out = String::new();
    desc_into(ws, ty, &mut out, 0).then_some(out)
}

/// [`type_desc_lua`] for a position that ALREADY has the flat reflection `type`
/// string beside it -- a struct field.
///
/// A bare nominal is fully expressible there, and the struct-field consumer has
/// classified off it since B3a. Emitting a descriptor that repeats what the
/// neighbouring key already says would be a second mechanism for the one case
/// that was never broken, and would move bytes for every struct field in every
/// game while adding no coverage. A global has no such neighbour, which is why
/// it uses [`type_desc_lua`] directly.
fn field_desc_lua(ws: &Workspace, ty: bone_semantic::prelude::TyId) -> Option<String> {
    let d = type_desc_lua(ws, ty)?;
    (!d.starts_with("{k=\"nom\"")).then_some(d)
}

/// Append `ty`'s descriptor to `out`; the bool is whether any nominal type was
/// reached, which is what decides that the descriptor is worth emitting.
///
/// `depth` bounds the walk. A recursive type (`struct Node { next: Node? }`)
/// is a finite TypeExpr graph but an unbounded TREE, so an unguarded walk would
/// not terminate. The cutoff renders `{k="any"}`, which every consumer already
/// has to handle -- it is the descriptor of an untyped value, not a wrong one.
fn desc_into(ws: &Workspace, ty: bone_semantic::prelude::TyId, out: &mut String, depth: u32) -> bool {
    use bone_semantic::hir::{TypeCtor, TypeExpr};
    use bone_semantic::prelude::TyId;

    const MAX_DEPTH: u32 = 12;
    if depth >= MAX_DEPTH {
        out.push_str("{k=\"any\"}");
        return false;
    }
    let te = match ty {
        TyId::Static(m, idx) => match ws.modules.get(m.index()) {
            Some(module) => module.ty_arena[idx],
            None => {
                out.push_str("{k=\"any\"}");
                return false;
            }
        },
        // A local (body) arena type never reaches a declaration's descriptor.
        TyId::Local(_) => {
            out.push_str("{k=\"any\"}");
            return false;
        }
    };
    match te {
        TypeExpr::Builtin(_) | TypeExpr::Generic(..) | TypeExpr::Infer(..) | TypeExpr::Function { .. } => {
            out.push_str("{k=\"any\"}");
            false
        }
        TypeExpr::Nullable(inner) => {
            out.push_str("{k=\"opt\",inner=");
            let found = desc_into(ws, ty.with_idx(inner), out, depth + 1);
            out.push('}');
            found
        }
        TypeExpr::Tuple { elements } => {
            out.push_str("{k=\"tuple\",elems={");
            let mut found = false;
            for (i, e) in elements.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                found |= desc_into(ws, ty.with_idx(e), out, depth + 1);
            }
            out.push_str("}}");
            found
        }
        TypeExpr::App { ctor, params } => match ctor {
            TypeCtor::Array => {
                // An array with no argument carries no element type to render.
                let Some(elem) = params.into_iter().next() else {
                    out.push_str("{k=\"any\"}");
                    return false;
                };
                out.push_str("{k=\"vec\",elem=");
                let found = desc_into(ws, ty.with_idx(elem), out, depth + 1);
                out.push('}');
                found
            }
            TypeCtor::Map => {
                // Only the VALUE side is rendered. A map's keys are serialized
                // as JSON object keys, so a key position has no leaf slot a
                // projection marker could be written into.
                let Some(val) = params.into_iter().nth(1) else {
                    out.push_str("{k=\"any\"}");
                    return false;
                };
                out.push_str("{k=\"map\",val=");
                let found = desc_into(ws, ty.with_idx(val), out, depth + 1);
                out.push('}');
                found
            }
            TypeCtor::Ref => {
                unreachable!("frame references are rejected from reflected storage")
            }
            TypeCtor::Nominal(name) => {
                // The name is the whole answer for a nominal leaf: the consumer
                // asks the ONE classifier whether it is a native class, and a
                // struct's own fields are reached through the type table's
                // per-field descriptors rather than repeated inside this one.
                out.push_str("{k=\"nom\",name=\"");
                out.push_str(type_symbol(name).as_str());
                out.push_str("\"}");
                true
            }
        },
    }
}

/// Per-variant payload descriptors for a BOXED enum, or `None` when no variant
/// payload anywhere in the enum contains a nominal type -- in which case the
/// enum needs no reflection entry and does not get one, so an enum of plain
/// payloads keeps exactly the bytes it has today.
///
/// A boxed enum's two payload forms are distinct runtime shapes and are keyed
/// the way the serializer reaches them, not the way they were declared: a TUPLE
/// payload lands under positional keys `1..n`, a RECORD payload under its field
/// names. Keying them alike would make one of the two unreachable.
///
/// Uses [`type_desc_lua`] rather than [`field_desc_lua`] for the same reason a
/// global does: there is no flat `type` string beside a payload slot, so a bare
/// nominal has to be carried here or it is carried nowhere.
fn boxed_variant_descs(ws: &Workspace, e: &bone_semantic::hir::EnumDef) -> Option<String> {
    use bone_semantic::hir::VariantPayload;
    let mut any = false;
    let mut variants: Vec<String> = Vec::new();
    for variant in e.variants.iter() {
        let body = match &variant.payload {
            VariantPayload::Unit => continue,
            VariantPayload::Tuple(types) => {
                let entries: Vec<String> = types
                    .iter()
                    .enumerate()
                    .filter_map(|(i, ty)| {
                        let d = type_desc_lua(ws, *ty)?;
                        Some(format!("[{}]={d}", i + 1))
                    })
                    .collect();
                if entries.is_empty() {
                    continue;
                }
                format!("pos={{{}}}", entries.join(","))
            }
            VariantPayload::Record(fields) => {
                let entries: Vec<String> = fields
                    .iter()
                    .filter_map(|f| {
                        let d = type_desc_lua(ws, f.ty)?;
                        Some(format!("[{:?}]={d}", f.name.as_str()))
                    })
                    .collect();
                if entries.is_empty() {
                    continue;
                }
                format!("named={{{}}}", entries.join(","))
            }
        };
        any = true;
        variants.push(format!("[{:?}]={{{body}}}", variant.name.as_str()));
    }
    any.then(|| variants.join(","))
}

/// (reflection type-string, nullable) for a struct field's `TyId`. The string
/// matches rock's `to_simple_string` for the cases the reflection stdlib needs
/// (int/float/string/bool, a nominal name, `[..]`); anything else is "any".
/// Used only for the reflection type table, not for C layout.
fn reflect_field_type(ws: &Workspace, ty: bone_semantic::prelude::TyId) -> (String, bool) {
    use bone_semantic::hir::{BuiltinTy, TypeCtor, TypeExpr};
    let resolve = |t: bone_semantic::prelude::TyId| -> Option<TypeExpr> {
        match t {
            bone_semantic::prelude::TyId::Static(mid, idx) => ws.modules.get(mid.index()).map(|m| m.ty_arena[idx]),
            bone_semantic::prelude::TyId::Local(_) => None,
        }
    };
    match resolve(ty) {
        Some(TypeExpr::Builtin(BuiltinTy::Int)) => ("int".into(), false),
        Some(TypeExpr::Builtin(BuiltinTy::Uint)) => ("uint".into(), false),
        Some(TypeExpr::Builtin(BuiltinTy::Float)) => ("float".into(), false),
        Some(TypeExpr::Builtin(BuiltinTy::Bool)) => ("bool".into(), false),
        Some(TypeExpr::Builtin(BuiltinTy::String)) => ("string".into(), false),
        Some(TypeExpr::Nullable(_)) => ("any".into(), true),
        Some(TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        }) => (type_symbol(name).as_str().to_string(), false),
        Some(TypeExpr::App {
            ctor: TypeCtor::Array,
            ..
        }) => ("[]".into(), false),
        _ => ("any".into(), false),
    }
}

/// Register every struct + newtype into `_G.__ROCK_TYPE_TABLE`, so the stdlib
/// reflection fns (`get_type_info`, `type_info_of`, `type_name`) can recover a
/// value's metadata from its `__ty` tag. The prelude already seeds the table
/// empty, so this only adds entries. No metatables: type tagging is the direct
/// `__ty`.
///
/// `type_id` below is WRITE-ONLY, and deliberately so. It is a counter over
/// `ws.modules` in resolution order, so it renumbers whenever a declaration
/// moves between files or a dependency is added -- it is not stable across
/// builds, worktrees or manifest edits. Every consumer of this table keys by
/// the type NAME (`serialization.rock`, `snapshot.rock`, `sim_checkpoint.rock`,
/// `stdlib/unified.rock`) and reads `.kind`/`.fields`/`.variants`; nothing in
/// the tree reads `.type_id`, and `docs/incremental-testing.md` already treats
/// its variation as harmless.
///
/// It becomes load-bearing the moment anything reads it -- a `type_id<T>()`
/// intrinsic, an entry in the canonical serializer's hash, a save, a wire
/// message, an FFI tag. If that day comes, derive the number from a sorted,
/// order-independent key the way `StateSchema` already does with
/// `(kind, identity)`, rather than from module order.
/// The `host_resource` key of a type-table entry, or nothing.
///
/// `@host_resource(<class>)` on the declaration is the ONE place a type is
/// named a host resource; `__stone_host_projection_class`
/// (`stone/engine/host_projection.rock`) reads this key instead of keeping its
/// own list of type names, which is what stops the two drifting and reporting
/// every handle of a forgotten type as a missing one. A host resource the
/// native ledger does not track publishes the empty class, which reads as
/// "ordinary state" to the projection walk while still being a compile error
/// in game memory.
fn host_resource_key(hr: Option<bone_semantic::hir::HostResource>) -> String {
    match hr {
        Some(hr) => format!(", host_resource = \"{}\"", hr.ledger_class),
        None => String::new(),
    }
}

fn type_table_defs(
    ws: &Workspace,
    gc: &rustc_hash::FxHashSet<ustr::Ustr>,
) -> Result<String, CompilerError> {
    let mut out = String::from("_G.__ROCK_ENUM_FLAT_LAYOUT = {}\n_G.__ROCK_STRIDED_ROWS = {}\n");
    let mut type_id = 100i64;
    for module in ws.modules.iter() {
        for (_, s) in module.structs.iter() {
            // Row offsets of a `[S]` stored as strided rows, so reflection
            // reads a row by leaf path (`serialization.rock`).
            if s.is_nobox
                && s.generics.is_empty()
                && let Some(offsets) = skull_mir::lower::ContainerPlan::strided_struct_row_offsets(
                    ws,
                    bone_semantic::hir::FnBackend::Lua,
                    s.name,
                )
            {
                let entries = offsets
                    .iter()
                    .map(|(path, offset)| format!("[\"{path}\"] = {offset}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format!("_G.__ROCK_STRIDED_ROWS[\"{}\"] = {{ {entries} }}\n", s.name));
            }
            // Tier-distinguishing kind so reflection/inspect/serialize stay honest:
            // FFI-clean value structs are cdata (`c_struct`), GC-carrying ones are
            // immutable Lua tables (`value_table`).
            let kind = if !s.is_value {
                "struct"
            } else if gc.contains(&s.name) {
                "value_table"
            } else {
                "c_struct"
            };
            let mut fields = Vec::with_capacity(s.fields.len());
            for f in &s.fields {
                let (tystr, _) = reflect_field_type(ws, f.ty);
                let plan = skull_mir::lower::struct_field_plan(
                    ws,
                    bone_semantic::hir::FnBackend::Lua,
                    s.name,
                    f.name,
                    false,
                )
                .map_err(|e| CompilerError::new_no_location(e.message))?;
                let nullable = plan.nullable();
                let (tystr, embed) = match plan.inline_child() {
                    Some(child) => (child.as_str().to_string(), ", embed = true"),
                    None => (tystr, ""),
                };
                let desc = match field_desc_lua(ws, f.ty) {
                    Some(d) => format!(", desc = {d}"),
                    None => String::new(),
                };
                fields.push(format!(
                    "{{ name = \"{}\", type = \"{}\", nullable = {}{}{} }}",
                    f.name, tystr, nullable, embed, desc
                ));
            }
            out.push_str(&format!(
                "_G.__ROCK_TYPE_TABLE[\"{n}\"] = {{ name = \"{n}\", kind = \"{k}\", type_id = {id}{hr}, fields = {{ {f} }} }}\n",
                n = type_symbol(s.name),
                k = kind,
                id = type_id,
                hr = host_resource_key(s.host_resource),
                f = fields.join(", ")
            ));
            type_id += 1;
        }
        for (_, nt) in module.newtypes.iter() {
            out.push_str(&format!(
                "_G.__ROCK_TYPE_TABLE[\"{n}\"] = {{ name = \"{n}\", kind = \"newtype\", type_id = {id}{hr}, fields = {{ }} }}\n",
                n = type_symbol(nt.name),
                id = type_id,
                hr = host_resource_key(nt.host_resource)
            ));
            type_id += 1;
        }
    }
    for module in ws.modules.iter() {
        for (_, e) in module.enums.iter() {
            use bone_semantic::hir::VariantPayload;

            let plan = skull_mir::lower::EnumPlan::for_name(ws, e.name);
            if let Some(flat) = plan.as_ref().and_then(|plan| plan.flat()) {
                let names = flat.record_fields().iter()
                    .map(|name| format!("\"{name}\""))
                    .collect::<Vec<_>>().join(", ");
                out.push_str(&format!(
                    "_G.__ROCK_ENUM_FLAT_LAYOUT[\"{}\"] = {{ tuple_width = {}, record_fields = {{ {} }} }}\n",
                    e.name, flat.tuple_width(), names,
                ));
            }
            let Some(shape) = plan.and_then(|plan| plan.cdata().cloned())
            else {
                // A BOXED enum has no value shape, and until now no reflection
                // entry at all -- so a serializer reaching one had nothing to
                // ask about its payload and rendered every handle in it as a
                // raw id. The entry exists only to carry the payload's declared
                // shapes; a boxed enum's own storage is its `__variant` tag plus
                // the payload slots, which reflection reads directly.
                if let Some(vd) = boxed_variant_descs(ws, e) {
                    out.push_str(&format!(
                        "_G.__ROCK_TYPE_TABLE[\"{n}\"] = {{ name = \"{n}\", kind = \"enum\", type_id = {id}, fields = {{ }}, variant_desc = {{ {vd} }} }}\n",
                        n = type_symbol(e.name),
                        id = type_id,
                    ));
                    type_id += 1;
                }
                continue;
            };
            let mut fields = vec![
                "{ name = \"__tag\", type = \"int\", nullable = false }".to_string(),
            ];
            fields.extend(shape.slots.iter().map(|(name, c_type)| {
                let field_type = if *c_type == "double" { "float" } else { c_type };
                format!(
                    "{{ name = \"{name}\", type = \"{field_type}\", nullable = false }}"
                )
            }));
            let variants: Vec<String> = e
                .variants
                .iter()
                .enumerate()
                .map(|(tag, variant)| {
                    let payload: Vec<String> = match &variant.payload {
                        VariantPayload::Unit => Vec::new(),
                        VariantPayload::Tuple(types) => types
                            .iter()
                            .enumerate()
                            .map(|(index, ty)| {
                                let (field_type, _) = reflect_field_type(ws, *ty);
                                format!(
                                    "{{ name = \"{}\", slot = \"_{}\", type = \"{}\" }}",
                                    index + 1,
                                    index,
                                    field_type
                                )
                            })
                            .collect(),
                        VariantPayload::Record(fields) => fields
                            .iter()
                            .map(|field| {
                                let (field_type, _) = reflect_field_type(ws, field.ty);
                                format!(
                                    "{{ name = \"{0}\", slot = \"_{0}\", type = \"{1}\" }}",
                                    field.name, field_type
                                )
                            })
                            .collect(),
                    };
                    format!(
                        "[{tag}] = {{ name = \"{}\", fields = {{ {} }} }}",
                        variant.name,
                        payload.join(", ")
                    )
                })
                .collect();
            out.push_str(&format!(
                "_G.__ROCK_TYPE_TABLE[\"{n}\"] = {{ name = \"{n}\", kind = \"c_enum\", type_id = {id}, fields = {{ {f} }}, variants = {{ {v} }} }}\n",
                n = type_symbol(e.name),
                id = type_id,
                f = fields.join(", "),
                v = variants.join(", ")
            ));
            type_id += 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::compile_source;

    fn emit(src: &str) -> String {
        compile_source(src).unwrap_or_else(|e| panic!("skull compile failed: {}", e.message))
    }

    /// The budget itself is tuning and moves with measurement, so this pins the
    /// shape and the floor rather than a literal. LuaJIT's own defaults (512 KB
    /// total, 32 KB blocks) flush the trace arena thousands of times in a real
    /// MAG session; 8 MB still flushed it 31 times in 60 s of play. Neither
    /// architecture may regress below that. See docs/trace-pressure-work-plan.md.
    #[test]
    fn runtime_prelude_keeps_the_mag_trace_working_set() {
        let prelude = super::runtime_prelude();
        for knob in ["\"maxtrace=\" .. __mt", "maxrecord=32000", "maxsnap=4000", "\"maxirconst=\" .. __mk"] {
            assert!(prelude.contains(knob), "runtime prelude stopped setting {knob}");
        }
        // One block, not several: a trace must fit a single `sizemcode` area,
        // and on arm64 macOS dyld only leaves room for the first one near the
        // VM. Driving both from one value is what guarantees that.
        assert!(
            prelude.contains("\"maxmcode=\" .. __mc, \"sizemcode=\" .. __mc"),
            "mcode max and size must come from one value"
        );
        assert!(
            prelude.contains("ROCK_JIT_MCODE_KB"),
            "the mcode budget must stay sweepable without a rebuild"
        );
        // A doomed side trace is never blacklisted, so LuaJIT's default 10
        // re-records it ~245 times for nothing. Pin the floor and the override,
        // not the value: the value is a measured pick that may move again.
        assert!(
            prelude.contains("ROCK_JIT_HOTEXIT"),
            "the hot-exit threshold must stay sweepable without a rebuild"
        );
        let hotexit: u32 = prelude
            .split("ROCK_JIT_HOTEXIT\") or \"\") or ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .expect("prelude no longer defaults the hot-exit threshold")
            .parse()
            .expect("hot-exit default is not a literal count");
        assert!(
            hotexit > 10,
            "hot-exit default {hotexit} is back at or below LuaJIT's, which re-records doomed exits"
        );
        let picked = prelude
            .split("jit.arch == \"arm64\" and ")
            .nth(1)
            .expect("prelude no longer selects an mcode budget by architecture");
        let (arm64, rest) = picked
            .split_once(" or ")
            .expect("arch-selected mcode budget is not an `arm64 and A or B` pick");
        let x64 = rest
            .split(')')
            .next()
            .expect("arch-selected mcode budget has no x64 arm");
        for (arch, kb) in [("arm64", arm64.trim()), ("x64", x64.trim())] {
            let kb: u32 = kb
                .parse()
                .unwrap_or_else(|_| panic!("{arch} mcode budget is not a literal KB count: {kb:?}"));
            assert!(kb >= 8192, "{arch} mcode budget {kb} KB is below the 8 MB floor");
        }
    }

    #[test]
    fn runtime_prelude_rng_state_is_numeric() {
        let prelude = super::runtime_prelude();
        assert!(prelude.contains("local __rock_rng_state = 0"));
        assert!(!prelude.contains("ffi.new(\"uint64_t\", 0)"));
    }

    /// Bare-name aliases (`_G["pick"] = pick_bool_ret_int`) for the user module's
    /// fns, so the execution-oracle drivers can keep calling by SOURCE name under
    /// always-mangle (every non-exempt fn now emits a mangled symbol). A name with
    /// more than one non-exempt definition is left unaliased -- a bare-name call
    /// to an overload is ambiguous and no driver relies on one. Exempt fns
    /// (`main`/`@nomangle`/...) already emit bare, so they need no alias.
    fn user_fn_aliases(src: &str) -> String {
        let module = skull_parser::parse_module(src, ustr::Ustr::from("<skull>")).expect("parse");
        let ws = match bone_semantic::semantic_passes::semantic_pass(&mut [module]) {
            Ok(ws) => ws,
            Err(_) => return String::new(),
        };
        let Ok(mid) = super::user_module_id(&ws) else { return String::new() };
        use std::collections::HashMap;
        let mut counts: HashMap<ustr::Ustr, usize> = HashMap::new();
        for (_, f) in ws.module(mid).functions.iter() {
            if !skull_mir::mangle::is_exempt(f) {
                *counts.entry(f.name).or_insert(0) += 1;
            }
        }
        let mut out = String::new();
        for (_, f) in ws.module(mid).functions.iter() {
            if skull_mir::mangle::is_exempt(f) || counts[&f.name] > 1 {
                continue;
            }
            let mangled = skull_mir::mangle::mangle_fn(&ws, f);
            if mangled != f.name.as_str() {
                out.push_str(&format!("_G[\"{}\"] = {}\n", f.name, mangled));
            }
        }
        out
    }

    /// The schema-drift restart trigger's oracle (docs/hot-reload-schema-drift-plan.md):
    /// the hash moves iff the runtime LAYOUT moves. Body edits are invisible;
    /// each shape-edit class moves exactly its own type's entry; a removed type
    /// counts as drifted (live data under it is stale).
    #[test]
    fn type_shape_hashes_track_layout_not_bodies() {
        let hashes = |src: &str| {
            let module = skull_parser::parse_module(src, ustr::Ustr::from("<skull>"))
                .unwrap_or_else(|e| panic!("parse failed: {}", e.message));
            let ws = bone_semantic::semantic_passes::semantic_pass(&mut [module])
                .unwrap_or_else(|e| panic!("semantic_pass failed: {}", e.message));
            super::type_shape_hashes(&ws).expect("shape planning must succeed")
        };
        let base = "struct Mob { hp: int }\n\
                    enum Ev { Tick(int), Done }\n\
                    value P { x: int, y: int }\n\
                    newtype Gold(int)\n\
                    fn probe(m: Mob) -> int { m.hp }\n";
        let h0 = hashes(base);
        let none: Vec<String> = Vec::new();

        let body = base.replace("{ m.hp }", "{ m.hp + 1 }");
        assert_eq!(super::shape_diff(&h0, &hashes(&body)), none, "a body edit must not drift any shape");

        let field_add = base.replace("struct Mob { hp: int }", "struct Mob { hp: int, armor: int }");
        assert_eq!(super::shape_diff(&h0, &hashes(&field_add)), vec!["Mob"]);

        let field_retype = base.replace("struct Mob { hp: int }", "struct Mob { hp: float }");
        assert_eq!(super::shape_diff(&h0, &hashes(&field_retype)), vec!["Mob"]);

        let vs_reorder = base.replace("value P { x: int, y: int }", "value P { y: int, x: int }");
        assert_eq!(super::shape_diff(&h0, &hashes(&vs_reorder)), vec!["P"]);

        let payload_widen = base.replace("Ev { Tick(int), Done }", "Ev { Tick(int, int), Done }");
        assert_eq!(super::shape_diff(&h0, &hashes(&payload_widen)), vec!["Ev"]);

        let variant_reorder = base.replace("Ev { Tick(int), Done }", "Ev { Done, Tick(int) }");
        assert_eq!(super::shape_diff(&h0, &hashes(&variant_reorder)), vec!["Ev"]);

        let rewrap = base.replace("newtype Gold(int)", "newtype Gold(float)");
        assert_eq!(super::shape_diff(&h0, &hashes(&rewrap)), vec!["Gold"]);

        let removed = base.replace("newtype Gold(int)\n", "");
        assert_eq!(super::shape_diff(&h0, &hashes(&removed)), vec!["Gold"], "a removed type counts as drifted");
    }

    /// `embed` moves the PARENT's layout: the child's leaves become the parent's
    /// physical keys, so the parent's hash must react both to the flag itself and
    /// to the child's shape. The non-transitivity that is correct for a boxed
    /// field (a pointer either way) is wrong here, and only here.
    #[test]
    fn embed_drifts_the_parent_shape() {
        let hashes = |src: &str| {
            let module = skull_parser::parse_module(src, ustr::Ustr::from("<skull>"))
                .unwrap_or_else(|e| panic!("parse failed: {}", e.message));
            let ws = bone_semantic::semantic_passes::semantic_pass(&mut [module])
                .unwrap_or_else(|e| panic!("semantic_pass failed: {}", e.message));
            super::type_shape_hashes(&ws).expect("shape planning must succeed")
        };

        // (a) The flag alone relayouts the parent, while every rendered field
        // type stays identical -- so nothing else in the workspace drifts and
        // the parent's own entry is the only possible signal.
        let boxed = "struct Pt { x: int }\nstruct Holder { p: Pt, tag: int }\n";
        let embedded = "struct Pt { x: int }\nstruct Holder { embed p: Pt, tag: int }\n";
        assert_eq!(
            super::shape_diff(&hashes(boxed), &hashes(embedded)),
            vec!["Holder"],
            "flipping a field to `embed` relayouts the parent"
        );

        // (b) The child's fields ARE the parent's keys, so widening the child
        // widens the parent.
        let embedded_wide = "struct Pt { x: int, y: int }\nstruct Holder { embed p: Pt, tag: int }\n";
        assert_eq!(
            super::shape_diff(&hashes(embedded), &hashes(embedded_wide)),
            vec!["Holder", "Pt"],
            "a field added to an EMBEDDED child also relayouts the parent"
        );

        // (c) CONTROL, the property that must not regress: a BOXED child is a
        // pointer either way, so widening it leaves the parent's layout alone.
        // If this ever reports `Holder` too, the hash went transitive in general
        // and every nested edit now over-triggers a restart.
        let boxed_wide = "struct Pt { x: int, y: int }\nstruct Holder { p: Pt, tag: int }\n";
        assert_eq!(
            super::shape_diff(&hashes(boxed), &hashes(boxed_wide)),
            vec!["Pt"],
            "a field added to a BOXED child must NOT drift the parent"
        );

        // (d) Transitive through a chain: the deepest leaf is a key on the
        // outermost parent, so every level drifts.
        let chain = "struct Inner { c: int }\n\
                     struct Mid { embed b: Inner, m: int }\n\
                     struct Outer { embed a: Mid, o: int }\n";
        let chain_wide = "struct Inner { c: int, d: int }\n\
                          struct Mid { embed b: Inner, m: int }\n\
                          struct Outer { embed a: Mid, o: int }\n";
        assert_eq!(
            super::shape_diff(&hashes(chain), &hashes(chain_wide)),
            vec!["Inner", "Mid", "Outer"],
            "an embed chain propagates the child's shape to every enclosing parent"
        );

        // A boxed link BREAKS the chain: `Mid` holds `Inner` by pointer, so
        // widening `Inner` stops there even though `Outer` embeds `Mid`.
        let broken = "struct Inner { c: int }\n\
                      struct Mid { b: Inner, m: int }\n\
                      struct Outer { embed a: Mid, o: int }\n";
        let broken_wide = "struct Inner { c: int, d: int }\n\
                           struct Mid { b: Inner, m: int }\n\
                           struct Outer { embed a: Mid, o: int }\n";
        assert_eq!(
            super::shape_diff(&hashes(broken), &hashes(broken_wide)),
            vec!["Inner"],
            "a boxed link stops transitive propagation"
        );
    }

    #[test]
    fn int_literal_function() {
        let lua = emit("fn five() -> int { 5 }");
        assert!(lua.contains("function five_ret_int()"), "got:\n{lua}");
        // The literal is a single-use pure value -> tree-folded into the return.
        assert!(lua.contains("do return (5)"), "got:\n{lua}");
    }

    #[test]
    fn if_lowers_to_cfg_branch() {
        let lua = emit("fn f(c: bool) -> int { if c { 1 } else { 2 } }");
        assert!(lua.contains("goto bb"), "expected CFG goto, got:\n{lua}");
        assert!(
            lua.contains("then goto bb"),
            "expected a conditional branch to a block, got:\n{lua}"
        );
        assert!(lua.contains("::bb0::"), "expected block labels, got:\n{lua}");
    }

    /// Execution oracle: emit Lua, append a `driver`, run under luajit, return
    /// trimmed stdout. Returns `None` (test skips) when luajit isn't on PATH.
    /// CRLF is normalized to `\n`: on Windows luajit's stdout goes through
    /// text-mode translation (`\n` -> `\r\n`), so without this every
    /// multi-line `assert_eq!` expectation would spuriously mismatch.
    fn run(src: &str, driver: &str) -> Option<String> {
        // Stdlib-free harness: codegen emits calls to the checked Vec helpers
        // the stdlib normally provides -- prepend the canonical shims. Alias the
        // user fns back to their bare source names so the driver can call them
        // (every non-exempt fn mangles under the unified scheme).
        let lua = format!(
            "{}\n{}\n{}\n{}",
            emit(src),
            skull_codegen_lua::VEC_CHECK_SHIMS,
            user_fn_aliases(src),
            driver
        );
        let out = std::process::Command::new("luajit")
            .arg("-e")
            .arg(&lua)
            .output()
            .ok()?;
        if !out.status.success() {
            panic!(
                "luajit failed:\n{}\n--- lua ---\n{}",
                String::from_utf8_lossy(&out.stderr),
                lua
            );
        }
        Some(
            String::from_utf8_lossy(&out.stdout)
                .replace("\r\n", "\n")
                .trim()
                .to_string(),
        )
    }

    /// Run `src` + `driver` under luajit and assert stdout == `expected`.
    /// Skips (returns) when luajit is unavailable, mirroring `run`.
    fn exec_eq(src: &str, driver: &str, expected: &str) {
        let Some(out) = run(src, driver) else { return };
        assert_eq!(out, expected);
    }

    /// A global that depends on another global only THROUGH a struct
    /// default (`B = Foo {}` where Foo's default reads `A`) must still init
    /// after it. The read is inside `__skull_default_Foo_x`, invisible to
    /// direct `Op::Global` scanning — emit_workspace's transitive call-graph
    /// closure (`transitive_global_reads`) follows the `Op::Call` to the helper
    /// and picks up its read of `A`. `B` is declared FIRST so source order
    /// alone gets it wrong.
    #[test]
    fn global_init_orders_through_struct_default_helper() {
        let out = run(
            "global B: Foo = Foo {}\nstruct Foo { x: int = A }\nglobal A: int = 42\nfn bx() -> int { B.x }",
            "print(bx())",
        );
        assert_eq!(out.as_deref(), Some("42"));
    }

    /// Global-init ordering across a call boundary: `A` initializes by calling
    /// `read_b`, which reads global `B`. The dependency A -> read_b -> B is only
    /// visible transitively (the init expression `read_b()` names no global), so
    /// `B` must still init before `A`. Pre-fix, `A` (declared first) ran before
    /// `B`, read `B` as nil, and `A` latched nil — exactly the
    /// `global sprite_batch_shader = watched_shader(...)` -> `SHADERS_TO_LOAD`
    /// crash that took mag down under skull (there a nil vec push, here a nil
    /// latch). (No arithmetic: the stdlib-less test path has no `__op_add`.)
    #[test]
    fn exec_global_init_dep_through_call() {
        exec_eq("global A: int = read_b()\nglobal B: int = 42\n\
                   fn read_b() -> int { B }\nfn get_a() -> int { A }", "print(get_a())", "42");
    }


    /// Execution oracle for interpolated strings:
    /// `tostring("n=") .. tostring(7) .. tostring("!")` -> "n=7!".
    #[test]
    fn exec_string_interpolation() {
        let out = run(
            "fn greet() -> string {\n    let n = 7\n    \"n={n}!\"\n}",
            "print(greet())",
        );
        assert_eq!(out.as_deref(), Some("n=7!"));
    }

    /// Execution oracle for a format specifier: `{x:.2f}` -> `string.format`.
    #[test]
    fn exec_string_interpolation_format_spec() {
        let out = run(
            "fn f() -> string {\n    let x = 3.14159\n    \"pi={x:.2f}\"\n}",
            "print(f())",
        );
        assert_eq!(out.as_deref(), Some("pi=3.14"));
    }

    #[test]
    fn exec_if_and_identity() {
        exec_eq("fn pick(c: bool) -> int { if c { 1 } else { 2 } }\nfn id(x: int) -> int { x }", "print(pick(true)); print(pick(false)); print(id(42))", "1\n2\n42");
    }

    #[test]
    fn exec_comparison() {
        exec_eq("fn lt(a: int, b: int) -> bool { a < b }", "print(lt(1, 2)); print(lt(2, 1))", "true\nfalse");
    }

    #[test]
    fn exec_short_circuit_and_or() {
        exec_eq("fn band(a: bool, b: bool) -> bool { a && b }\nfn bor(a: bool, b: bool) -> bool { a || b }", "print(band(true,false)); print(band(true,true)); print(bor(false,false)); print(bor(false,true))", "false\ntrue\nfalse\ntrue");
    }

    #[test]
    fn exec_let_then_compare() {
        exec_eq("fn f(x: int) -> bool {\n    let lo = 0\n    lo < x\n}", "print(f(5)); print(f(-3))", "true\nfalse");
    }

    #[test]
    fn exec_nested_if() {
        exec_eq("fn sign(x: int) -> int { if x < 0 { 0 } else { if x == 0 { 1 } else { 2 } } }", "print(sign(-9)); print(sign(0)); print(sign(9))", "0\n1\n2");
    }

    #[test]
    fn exec_unary_minus() {
        exec_eq("fn neg(x: int) -> int { -x }\nfn negf(x: float) -> float { -x }", "print(neg(5)); print(neg(-3)); print(negf(2.5))", "-5\n3\n-2.5");
    }

    #[test]
    fn exec_for_range_with_assignment() {
        // `last` returns the final loop index (n-1), or 0 when the loop is empty.
        exec_eq("fn last(n: int) -> int {\n    let r = 0\n    for i in 0..n {\n        r = i\n    }\n    r\n}", "print(last(5)); print(last(1)); print(last(0))", "4\n0\n0");
    }

    #[test]
    fn exec_while_with_assignment() {
        // Loops once: sets r, then flips the flag false.
        exec_eq("fn once() -> int {\n    let flag = true\n    let r = 0\n    while flag {\n        r = 1\n        flag = false\n    }\n    r\n}", "print(once())", "1");
    }

    #[test]
    fn exec_lambda_return_and_call() {
        // bone can't call a function value yet, so call the returned closure
        // from the Lua driver.
        exec_eq("fn make() -> (int) -> int {\n    (x: int) => x\n}", "print(make()(5))", "5");
    }

    #[test]
    fn exec_lambda_captures_upvalue() {
        // The lambda captures `n` (outer slot) as a Lua upvalue and ignores x.
        exec_eq("fn constfn(n: int) -> (int) -> int {\n    (x: int) => n\n}", "print(constfn(7)(99)); print(constfn(3)(0))", "7\n3");
    }

    #[test]
    fn exec_field_assignment() {
        exec_eq("struct P { x: int, y: int }\nfn f() -> int {\n    let p = P { x: 1, y: 2 }\n    p.x = 10\n    p.x\n}", "print(f())", "10");
    }

    #[test]
    fn exec_index_assignment() {
        exec_eq("fn f() -> int {\n    let xs = [1, 2, 3]\n    xs[1] = 9\n    xs[1]\n}", "print(f())", "9");
    }

    #[test]
    fn exec_array_index() {
        // 0-indexed Rock -> 1-indexed Lua: xs[1] is the second element.
        exec_eq("fn f() -> int {\n    let xs = [10, 20, 30]\n    xs[1]\n}", "print(f())", "20");
    }

    #[test]
    fn exec_as_cast_identity() {
        exec_eq("fn w(x: int) -> float { x as float }", "print(w(5))", "5");
    }


    #[test]
    fn exec_coroutine_yield() {
        // A `co fn` emits a stackless state machine; calling it returns a
        // Frame driven by the per-co-fn `__resume` stored as a literal field
        // (the ctor returns the frame; `__`-method fields take it explicitly).
        exec_eq("co fn counter() -> Coroutine<int> {\n\
                       yield 1\n\
                       yield 2\n\
                       yield 3\n\
                   }", "local c = counter(); print(c.__resume(c)); print(c.__resume(c)); print(c.__resume(c))", "1\n2\n3");
    }

    #[test]
    fn coroutine_is_stackless_no_lua_coroutines() {
        // A `co fn` lowers to the C# state-machine model via the CFG->CFG
        // transform (`coro_transform`), never LuaJIT's native coroutines and
        // never a per-instance frame closure: one state table `s`, a TOP-LEVEL
        // `<name>__step` (shared, name-referenced via pcall), state-saving
        // returns, a terminal transition, and zero `coroutine.*`. The transform
        // emits an ORDINARY CFG (plain `goto bbN` / dispatch on `s.__state`), so
        // the emit-time `::__co_resume_K::` re-entry labels must be ABSENT.
        let lua = emit(
            "co fn counter() -> Coroutine<int> {\n\
                 yield 1\n\
                 yield 2\n\
             }",
        );
        assert!(
            !lua.contains("coroutine."),
            "co fn must not emit LuaJIT coroutines, got:\n{lua}"
        );
        // Top-level shared step fn — NOT a per-instance `local function` closure.
        assert!(
            lua.contains("function counter_ret_Coroutine_int_unit_unit__step("),
            "expected top-level step fn, got:\n{lua}"
        );
        assert!(
            !lua.contains("local function __step"),
            "step must be top-level, not a per-instance closure, got:\n{lua}"
        );
        assert!(
            lua.contains(r#"{ __ty = "Coroutine", __state = 0, __layout = "#),
            "expected the tagged+stamped frame table, got:\n{lua}"
        );
        assert!(
            lua.contains("pcall(counter_ret_Coroutine_int_unit_unit__step,"),
            "expected name-referenced pcall, got:\n{lua}"
        );
        // State machine: writes to the state field + a completed terminal -3
        // (killed is -1, errored -2), over an ordinary goto-CFG — NOT the
        // emit-time goto-into-block re-entry labels.
        assert!(lua.contains(".__state ="), "expected state-saving write, got:\n{lua}");
        assert!(lua.contains("__state = (-3)"), "expected completed-terminal transition, got:\n{lua}");
        assert!(lua.contains("goto bb"), "expected ordinary goto CFG, got:\n{lua}");
        assert!(
            !lua.contains("__co_resume_"),
            "transform must NOT use emit-time re-entry labels, got:\n{lua}"
        );
    }

    #[test]
    fn exec_coroutine_yield_delegate() {
        // `await inner()` pumps the inner coroutine's values outward.
        exec_eq("co fn inner() -> Coroutine<int> {\n\
                       yield 1\n\
                       yield 2\n\
                   }\n\
                   co fn outer() -> Coroutine<int> {\n\
                       yield 0\n\
                       await inner()\n\
                       yield 9\n\
                   }", "local c = outer(); for i=1,4 do print(c.__resume(c)) end", "0\n1\n2\n9");
    }

    #[test]
    fn exec_format_dynamic_width_precision() {
        // `{x:{w}.{p}f}` assembles the string.format conversion at runtime.
        exec_eq("fn f() -> string {\n\
                       let x = 3.14159\n\
                       let w = 8\n\
                       let p = 2\n\
                       \"[{x:{w}.{p}f}]\"\n\
                   }", "print(f())", "[    3.14]");
    }

    #[test]
    fn exec_array_spread() {
        // Tuple spread (static arity), struct spread (declared fields), and
        // array spread (runtime copy loop), all spliced into one literal.
        let src = "struct FixVec2 { x: int, y: int }\n\
                   fn tup() -> [int] { [0, ...(1, 2), 3] }\n\
                   fn strct(p: FixVec2) -> [int] { [...p, 9] }\n\
                   fn vecs(a: [int], b: [int]) -> [int] { [...a, ...b, 99] }";
        // Returns/params typed `[T]` are the Vec shape `{_data, len}`; `show`
        // reads it, `v` builds it.
        let driver = "local function show(t) local s = {} \
                      for i=1,t.len do s[i] = tostring(t._data[i]) end \
                      print(table.concat(s, \",\")) end \
                      local function v(t) return {_data = t, len = #t} end \
                      show(tup()); show(strct({x=10, y=20})); show(vecs(v{1,2}, v{3,4}))";
        let Some(out) = run(src, driver) else {
            return;
        };
        assert_eq!(out, "0,1,2,3\n10,20,9\n1,2,3,4,99");
    }

    #[test]
    fn exec_overload_mangling_dispatch() {
        // Two `pick` overloads collide unmangled (last def wins). Mangling by
        // the resolved target's signature makes `pick(5)` reach the int
        // overload (5), not the bool one (1). `use_it` is uniquely named, so it
        // stays callable by its surface name from the driver.
        exec_eq("fn pick(x: int) -> int { x }\n\
                   fn pick(x: bool) -> int { 1 }\n\
                   fn use_it() -> int { pick(5) }", "print(use_it())", "5");
    }

    #[test]
    fn exec_inline_lua_body() {
        // bone now hands skull the verbatim `inline(lua)` body (it used to drop
        // it). skull emits it as a real function with the original param names,
        // and a Rock caller resolves to it.
        exec_eq("@nomangle inline(lua) fn double(x: int) -> int = \"return x * 2\"\n\
                   fn use_it(n: int) -> int { double(n) }", "print(use_it(21))", "42");
    }

    #[test]
    fn exec_bitwise() {
        // Bitwise binops lower to LuaJIT `bit.*` calls.
        exec_eq("fn band(a: int, b: int) -> int { a & b }\n\
                   fn shl(a: int, n: int) -> int { a << n }", "print(band(6, 3)); print(shl(1, 4))", "2\n16");
    }

    #[test]
    fn exec_for_enumerate() {
        // `enumerate(coll)` yields (0-based index, element); bone special-cases
        // it in for-position, so skull detects it syntactically.
        exec_eq("fn idx() -> int {\n\
                       let r = 0\n\
                       for (i, v) in enumerate([10, 20, 30]) { r = i }\n\
                       r\n\
                   }\n\
                   fn elem() -> int {\n\
                       let r = 0\n\
                       for (i, v) in enumerate([10, 20, 30]) { r = v }\n\
                       r\n\
                   }", "print(idx()); print(elem())", "2\n30");
    }

    #[test]
    fn exec_for_over_map() {
        // `next`-based iteration; single-entry map so order is unambiguous.
        exec_eq("fn val(m: Map<string, int>) -> int {\n\
                       let n = 0\n\
                       for (k, v) in m { n = v }\n\
                       n\n\
                   }", "print(val({a = 42}))", "42");
    }

    #[test]
    fn exec_enum_match() {
        // Tuple-payload variants -> tagged tables; match discriminates on
        // `__tag` and reads payload by position.
        exec_eq("enum Shape { Circle(int), Square(int, int) }\n\
                   fn pick(s: Shape) -> int {\n\
                       match s {\n\
                           Circle(r) => r,\n\
                           Square(w, h) => w,\n\
                       }\n\
                   }\n\
                   fn mkc(r: int) -> Shape { Shape::Circle(r) }\n\
                   fn mks(w: int, h: int) -> Shape { Shape::Square(w, h) }", "print(pick(mkc(7))); print(pick(mks(3, 4)))", "7\n3");
    }

    #[test]
    fn exec_nested_pattern_bindings() {
        // Nested binding patterns: enum payload is a tuple, struct field is a
        // struct. The inner reads materialize in the matched arm (guarded), so
        // the `None` case never faults reading a missing payload.
        exec_eq("enum Opt { Some((int, int)), None }\n\
                   fn fst(o: Opt) -> int {\n\
                       match o {\n\
                           Some((x, y)) => x,\n\
                           None => -1,\n\
                       }\n\
                   }\n\
                   struct Point { x: int, y: int }\n\
                   struct Wrapper { inner: Point, tag: int }\n\
                   fn inner_x(w: Wrapper) -> int {\n\
                       match w {\n\
                           Wrapper { inner: Point { x, y }, tag } => x,\n\
                       }\n\
                   }\n\
                   fn mk(a: int, b: int) -> Opt { Opt::Some((a, b)) }\n\
                   fn none() -> Opt { Opt::None }", "print(fst(mk(3, 4))); print(fst(none())); \
             print(inner_x({inner = {x = 10, y = 20}, tag = 5}))", "3\n-1\n10");
    }

    #[test]
    fn exec_match_literal_arms() {
        exec_eq("fn name(n: int) -> string {\n\
                       match n {\n\
                           0 => \"zero\",\n\
                           1 => \"one\",\n\
                           _ => \"many\",\n\
                       }\n\
                   }", "print(name(0)); print(name(1)); print(name(9))", "zero\none\nmany");
    }

    #[test]
    fn exec_if_let_enum() {
        // `opt_unwrap` (unique name, emitted bare; the stdlib `unwrap_or` would
        // otherwise shadow it) + Rock `check_*` wrappers: the driver is RAW LUA, and
        // `Opt` is now a cdata enum that multi-returns `(tag, _0)`, so calling
        // `opt_unwrap(mk(5), 0)` directly in Lua would TRUNCATE `mk`'s second return.
        // The Rock wrappers spill the multi-return correctly (skull's own ABI).
        exec_eq("enum Opt { Some(int), None }\n\
                   fn opt_unwrap(o: Opt, d: int) -> int {\n\
                       if let Some(x) = o { x } else { d }\n\
                   }\n\
                   fn mk(x: int) -> Opt { Opt::Some(x) }\n\
                   fn empty() -> Opt { Opt::None }\n\
                   fn check_some() -> int { return opt_unwrap(mk(5), 0) }\n\
                   fn check_none() -> int { return opt_unwrap(empty(), 99) }", "print(check_some()); print(check_none())", "5\n99");
    }

    #[test]
    fn exec_for_with_conditional_assign() {
        // Conditional assignment inside the loop: r tracks i only while i < 3.
        // (Uses no `break`/`continue` — bone's inference doesn't support those
        // yet, though skull's lowering for them is in place; see skull-plan.md.)
        exec_eq("fn upto3(n: int) -> int {\n    let r = 0\n    for i in 0..n {\n        if i < 3 {\n            r = i\n        }\n    }\n    r\n}", "print(upto3(10)); print(upto3(2))", "2\n1");
    }

    /// `recv?.f(args)` on a fn-typed FIELD: nil-guarded `(recv.f)(args)`
    /// indirect call, no receiver argument. Cannot live in `tests/` while
    /// rock (legacy) rejects the shape — bone-only language rule.
    #[test]
    fn exec_safe_nav_field_call() {
        exec_eq("struct S { f: fn() -> int, g: fn(int) -> int }\n\
                   fn mk() -> S? { S { f: () => 42, g: (x) => x } }\n\
                   fn none() -> S? { null }\n\
                   fn a() -> int { mk()?.f() ?? -1 }\n\
                   fn b() -> int { mk()?.g(7) ?? -1 }\n\
                   fn c() -> int { none()?.f() ?? -1 }", "print(a()); print(b()); print(c())", "42\n7\n-1");
    }

    /// Body slice of a single emitted function — from its `function NAME()`
    /// header to the next top-level `function ` (or end). Lets a codegen
    /// assertion scope to one fn and ignore the always-emitted value-struct
    /// `copy = function(v) return V2_ct(...) end` metatype helpers.
    fn fn_body<'a>(lua: &'a str, name: &str) -> &'a str {
        // Every non-exempt fn emits under its mangled name (`mk_ret_V2`), so
        // match the `function <name>` PREFIX and scope from the header's `(` to
        // the next top-level definition. Under the bound callee shape the
        // definition reads `__rock_mk_<name> = function(<callees>) return
        // function(..)`, so the body starts after the INNER header and the next
        // definition can be a factory rather than a `function ` line.
        let factory_prefix = skull_codegen_lua::CALLEE_FACTORY_PREFIX;
        let inner = " return function(";
        let (start, header_end) = match lua.find(&format!("function {name}")) {
            Some(at) => (at, lua[at..].find('(').expect("fn header has `(`") + 1),
            None => {
                let head = format!("{factory_prefix}{name}");
                let at = lua
                    .find(&head)
                    .unwrap_or_else(|| panic!("no `function {name}` or `{head}` in:\n{lua}"));
                (at, lua[at..].find(inner).expect("a factory returns its body") + inner.len())
            }
        };
        let rest = &lua[start + header_end..];
        let factory_start = format!("\n{factory_prefix}");
        let end = [rest.find("\nfunction "), rest.find(factory_start.as_str())]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(rest.len());
        &rest[..end]
    }

    /// A defaulted flat-value param is scalarized like any other value param:
    /// an explicit value arg splats its fields with NO cdata ctor, and the
    /// default-fill stores per-field scalars (no cdata either). RED before the
    /// defaulted-value-param scalarization: the callsite packed `V2_ct(...)` to
    /// feed the single-cdata defaulted slot (mag's per-tile `wall_pivot`).
    #[test]
    fn scalar_abi_defaulted_value_param_no_cdata() {
        // No arithmetic: the stdlib-less unit path has no `__op_*` (see
        // `exec_global_init_dep_through_call`). Field reads suffice.
        let src = "value V2 { x: float, y: float }\n\
                   fn sink(base: V2, off: V2 = V2 { x: 1.0, y: 2.0 }) -> float { off.y }\n\
                   fn provided() -> float { sink(V2 { x: 3.0, y: 4.0 }, V2 { x: 5.0, y: 6.0 }) }\n\
                   fn omitted() -> float { sink(V2 { x: 3.0, y: 4.0 }) }";
        let lua = emit(src);
        let prov = fn_body(&lua, "provided");
        assert!(
            !prov.contains("_ct("),
            "provided() must splat scalars, not pack cdata; got:\n{prov}"
        );
        // sink's default-fill must not materialize a cdata for `off` either.
        let sink = fn_body(&lua, "sink");
        assert!(
            !sink.contains("_ct("),
            "sink() default-fill must store scalars, not cdata; got:\n{sink}"
        );
        let omit = fn_body(&lua, "omitted");
        assert!(
            !omit.contains("_ct("),
            "omitted() must pass scalars (Lua nils trigger default-fill); got:\n{omit}"
        );
    }

    /// Runtime correctness across all three callsite shapes: arg provided
    /// positionally, arg omitted (default fires), and arg omitted via a named
    /// LATER arg (the gap case — a single omitted scalar param must still
    /// arrive as N nils so the callee's per-field default-fill triggers).
    #[test]
    fn exec_defaulted_value_param() {
        // No arithmetic (stdlib-less path); observe via field reads. `scale` is
        // a trailing defaulted scalar to force the named-arg gap shape.
        let src = "value V2 { x: float, y: float }\n\
                   fn off_y(base: V2, off: V2 = V2 { x: 10.0, y: 20.0 }) -> float { off.y }\n\
                   fn gap_off(base: V2, off: V2 = V2 { x: 10.0, y: 20.0 }, scale: float = 3.0) -> float { off.y }\n\
                   fn gap_scale(base: V2, off: V2 = V2 { x: 10.0, y: 20.0 }, scale: float = 3.0) -> float { scale }\n\
                   fn provided() -> float { off_y(V2 { x: 1.0, y: 2.0 }, V2 { x: 3.0, y: 4.0 }) }\n\
                   fn omitted() -> float { off_y(V2 { x: 1.0, y: 2.0 }) }\n\
                   fn gap_default_off() -> float { gap_off(V2 { x: 1.0, y: 2.0 }, scale: 7.0) }\n\
                   fn gap_named_scale() -> float { gap_scale(V2 { x: 1.0, y: 2.0 }, scale: 7.0) }";
        let Some(out) = run(
            src,
            "print(provided()); print(omitted()); print(gap_default_off()); print(gap_named_scale())",
        ) else {
            return;
        };
        // provided off.y=4 ; omitted -> default off.y=20 ;
        // gap: off omitted (default 20) while scale named=7 -> off.y=20, scale=7
        assert_eq!(out, "4\n20\n20\n7");
    }

    /// A defaulted value param whose default is a GLOBAL value struct (not a
    /// literal). The global reads back as a single cdata (`Scalar`), not a
    /// `Group`, so the per-field default-fill must field-read its components —
    /// not store the whole cdata into the first slot. RED before that handling:
    /// mag crashed `Color_ct(<cdata>, nil, nil, nil)` on
    /// `journal_log("...")` omitting `hue: Color = JOURNAL_HUE_DEFAULT`.
    #[test]
    fn exec_defaulted_value_param_global_default() {
        let src = "value V2 { x: float, y: float }\n\
                   global DEF: V2 = mk(7.0, 8.0)\n\
                   fn mk(a: float, b: float) -> V2 { V2 { x: a, y: b } }\n\
                   fn pick(base: V2, off: V2 = DEF) -> float { off.x }\n\
                   fn pick_y(base: V2, off: V2 = DEF) -> float { off.y }\n\
                   fn omitted_x() -> float { pick(V2 { x: 1.0, y: 2.0 }) }\n\
                   fn omitted_y() -> float { pick_y(V2 { x: 1.0, y: 2.0 }) }\n\
                   fn provided_x() -> float { pick(V2 { x: 1.0, y: 2.0 }, V2 { x: 3.0, y: 4.0 }) }";
        let Some(out) = run(
            src,
            "print(omitted_x()); print(omitted_y()); print(provided_x())",
        ) else {
            return;
        };
        // omitted -> default DEF = (7, 8): x=7, y=8 ; provided -> off.x=3
        assert_eq!(out, "7\n8\n3");
    }

    /// A NON-flat value struct (`Rect` whose fields are themselves `V2` value
    /// structs) scalarizes through the boundary as flattened leaf scalars: the
    /// nested literal builds no cdata, the return flattens, the param rebuilds,
    /// and a nested field access (`r.br.x`) reads a leaf. RED before nested
    /// scalarization: `mk` returned `Rect_ct(V2_ct(...), V2_ct(...))`.
    #[test]
    fn scalar_abi_nested_value_no_cdata() {
        let src = "value V2 { x: float, y: float }\n\
                   value Rect { tl: V2, br: V2 }\n\
                   fn mk(a: float, b: float, c: float, d: float) -> Rect { Rect { tl: V2 { x: a, y: b }, br: V2 { x: c, y: d } } }\n\
                   fn br_x(r: Rect) -> float { r.br.x }\n\
                   fn drive() -> float { br_x(mk(1.0, 2.0, 3.0, 4.0)) }";
        let lua = emit(src);
        let mk = fn_body(&lua, "mk");
        assert!(!mk.contains("_ct("), "mk must build nested value as scalars; got:\n{mk}");
        let drive = fn_body(&lua, "drive");
        assert!(
            !drive.contains("_ct("),
            "drive must thread leaves, no cdata; got:\n{drive}"
        );
    }

    /// Runtime correctness of nested value scalarization across literal build,
    /// nested return, nested param, and nested field access (`r.tl.y`/`r.br.x`).
    #[test]
    fn exec_nested_value_struct() {
        let src = "value V2 { x: float, y: float }\n\
                   value Rect { tl: V2, br: V2 }\n\
                   fn mk(a: float, b: float, c: float, d: float) -> Rect { Rect { tl: V2 { x: a, y: b }, br: V2 { x: c, y: d } } }\n\
                   fn br_x(r: Rect) -> float { r.br.x }\n\
                   fn tl_y(r: Rect) -> float { r.tl.y }\n\
                   fn whole(r: Rect) -> Rect { r }\n\
                   fn a() -> float { br_x(mk(1.0, 2.0, 3.0, 4.0)) }\n\
                   fn b() -> float { tl_y(mk(1.0, 2.0, 3.0, 4.0)) }\n\
                   fn c() -> float { br_x(whole(mk(1.0, 2.0, 3.0, 4.0))) }";
        let Some(out) = run(src, "print(a()); print(b()); print(c())") else {
            return;
        };
        // br.x=3 ; tl.y=2 ; whole passes Rect through then br.x=3
        assert_eq!(out, "3\n2\n3");
    }

    /// `==`/`!=` on a PAYLOADLESS enum lowers to native Lua `==`/`~=` (identity
    /// == structural for unit-only variants), not the generic `__op_eq` dispatch
    /// (type()/pairs() — a static-over-dynamic smell, and trace-hostile in hot
    /// loops). RED before: `tile == E::Wall` emitted `__op_eq(tile, ...)`.
    #[test]
    fn scalar_abi_payloadless_enum_eq_native() {
        let src = "enum E { Ground, Wall }\n\
                   fn is_wall(t: E) -> bool { t == E::Wall }\n\
                   fn not_wall(t: E) -> bool { t != E::Wall }";
        let lua = emit(src);
        let w = fn_body(&lua, "is_wall");
        assert!(!w.contains("__op_eq"), "payloadless enum == must be native; got:\n{w}");
        let nw = fn_body(&lua, "not_wall");
        assert!(!nw.contains("__op_ne"), "payloadless enum != must be native; got:\n{nw}");
    }

    /// Runtime correctness: payloadless-enum equality (both arms) and nil compare
    /// give the same answers as the generic comparator would. (In the stdlib-less
    /// unit harness `__op_eq` doesn't exist, so the old codegen would also fail to
    /// run here — the native lowering is what makes this executable at all.)
    #[test]
    fn exec_payloadless_enum_eq() {
        exec_eq("enum E { Ground, Wall }\n\
                   fn check(t: E) -> int { if t == E::Wall { 1 } else { 0 } }\n\
                   fn a() -> int { check(E::Wall) }\n\
                   fn b() -> int { check(E::Ground) }", "print(a()); print(b())", "1\n0");
    }

    /// Function inlining: a tiny single-block value-struct constructor called in
    /// a hot loop is spliced into the caller. The scalar-ABI ctor arrives as a
    /// `CallMulti` (`r1, r2 = iv(...)`); after inlining the caller no longer
    /// calls it, yet computes the same sum. This is the rock-codegen-parity lever
    /// that lets the loop trace (no per-tile call frame).
    #[test]
    fn inlines_value_struct_ctor_call() {
        let src = "value IVec2 { x: int, y: int }\n\
                   @inline\n\
                   fn iv(a: int, b: int) -> IVec2 { IVec2 { x: a, y: b } }\n\
                   fn run() -> int {\n\
                       let s = 0\n\
                       for i in 0..4 {\n\
                           let c = iv(i, 100)\n\
                           s = c.y\n\
                       }\n\
                       s\n\
                   }";
        let lua = emit(src);
        // `iv()` is a small straight-line callee (a scalarized value-struct ctor),
        // so the inline pass splices it into `run()` and the hot loop traces with
        // no per-iter call frame -> no `= iv(` call survives. Splice mechanics +
        // policy: `docs/skull-inlining-plan.md`.
        assert!(
            !lua.contains("= iv("),
            "expected iv() inlined (no `= iv(` call), got:\n{lua}"
        );
        // Correctness holds regardless of inlining — last iteration c.y == 100.
        exec_eq(src, "print(run())", "100");
    }

    /// Multi-block inlining: a callee with internal control flow (`pick` has an
    /// `if`, so its entry block ends in a Branch, not a Return) is spliced via the
    /// general CFG path — caller block split, callee blocks cloned, each Return
    /// routed to a continuation block whose param is the call's destination. The
    /// call disappears and both branch outcomes stay correct.
    #[test]
    fn inlines_multi_block_callee() {
        let src = "@inline\n\
                   fn pick(c: bool) -> int { if c { 10 } else { 20 } }\n\
                   fn run(c: bool) -> int {\n\
                       let s = pick(c)\n\
                       s\n\
                   }";
        let lua = emit(src);
        // `pick()` has internal control flow (an `if`), so it is spliced via the
        // general CFG path: the caller block splits, the callee blocks clone, and
        // each Return routes to a continuation block -> no `= pick(` call survives.
        assert!(
            !lua.contains("= pick("),
            "expected pick() inlined (no `= pick(` call), got:\n{lua}"
        );
        exec_eq(src, "print(run(true)); print(run(false))", "10\n20");
    }

    #[test]
    fn inlines_straight_result_across_branch() {
        let src = "@inline\n\
                   fn identity(flag: bool) -> bool { return flag }\n\
                   fn run(select: bool) -> bool {\n\
                       let value = identity(true)\n\
                       if select { return value }\n\
                       return value\n\
                   }";
        exec_eq(src, "print(run(true)); print(run(false))", "true\ntrue");
    }

    #[test]
    fn inlines_straight_nullable_result_across_coalesce_branches() {
        let src = "@inline\n\
                   fn identity(value: int?) -> int? { return value }\n\
                   fn run(select: bool) -> int {\n\
                       let value = identity(null)\n\
                       if select { return value ?? 1 }\n\
                       return value ?? 2\n\
                   }";
        exec_eq(src, "print(run(true)); print(run(false))", "1\n2");
    }

    #[test]
    fn inlines_void_straight_result() {
        let src = "@inline\n\
                   fn touch() { return }\n\
                   fn run() -> int { touch(); return 7 }";
        exec_eq(src, "print(run())", "7");
    }

    /// Transitive inlining: `a` calls `b` calls `c`; re-scanning the caller after
    /// each splice inlines the calls the spliced body introduces, so neither
    /// `= b(` nor `= c(` survives in `a`. (Arithmetic ops aren't used — the
    /// no-stdlib `compile_source` path can't resolve `__op_*`.)
    #[test]
    fn inlines_transitively() {
        let src = "@inline\n\
                   fn c() -> int { 3 }\n\
                   @inline\n\
                   fn b() -> int { c() }\n\
                   fn a() -> int { b() }";
        let lua = emit(src);
        assert!(!lua.contains("= b("), "expected b() inlined into a(), got:\n{lua}");
        assert!(!lua.contains("= c("), "expected c() inlined transitively, got:\n{lua}");
        exec_eq(src, "print(a())", "3");
    }

    /// A self-recursive callee is NOT a candidate (cloning it would never
    /// terminate), so the call into it survives. `r` self-calls `r(true)`; the
    /// caller's `r(false)` therefore stays a real call. Output stays correct.
    #[test]
    fn recursive_callee_not_inlined() {
        let src = "@inline\n\
                   fn r(stop: bool) -> int { if stop { 7 } else { r(true) } }\n\
                   fn caller() -> int { r(false) }";
        let lua = emit(src);
        assert!(
            lua.contains("r("),
            "a self-recursive callee must stay a real call, got:\n{lua}"
        );
        exec_eq(src, "print(caller())", "7");
    }

    // --- Relooper: counted loops recovered as a real numeric `for` (FORL) ---
    //
    // The relooper (`skull-codegen-lua` `recognize_counted_loops`/`expand_for`)
    // recovers the canonical counted-loop CFG (preheader-store / `i < hi` header /
    // pure-increment latch) as a Lua numeric `for c<slot> = c<slot>, hi-1 do`, so
    // LuaJIT specializes the induction var (FORL) instead of unrolling a goto-loop
    // into an mcode blowup. Shapes that fail the soundness guards stay goto loops
    // (see `counter_reassigned_in_body_stays_correct`). Emit-shape + execution.

    #[test]
    fn counted_range_loop_emits_real_for() {
        let src = "fn sum(n: int) -> int {\n\
                   let last = 0\n\
                   for i in 0..n { last = i }\n\
                   last\n\
                   }";
        let lua = emit(src);
        assert!(
            lua.contains("for c"),
            "expected a numeric for-loop (FORL), got:\n{lua}"
        );
        let Some(out) = run(src, "print(sum(5))") else { return };
        assert_eq!(out, "4");
    }

    #[test]
    fn nested_counted_loops_both_for_and_correct() {
        let src = "fn grid(w: int, h: int) -> int {\n\
                   let last = 0\n\
                   for y in 0..h { for x in 0..w { last = x } }\n\
                   last\n\
                   }";
        let lua = emit(src);
        assert_eq!(
            lua.matches("for c").count(),
            2,
            "expected two numeric for-loops (outer + inner), got:\n{lua}"
        );
        let Some(out) = run(src, "print(grid(5, 3))") else { return };
        assert_eq!(out, "4");
    }

    /// Guard: an array spread (`[...a, ...b]`) builds via a loop whose increment
    /// is fused INTO the body block (no dedicated latch). It must NOT be treated
    /// as a counted loop — doing so would double the increment (body + the
    /// synthesized `for`). Execution proves the elements copy exactly once.
    #[test]
    fn array_spread_not_restructured_no_double_increment() {
        exec_eq("fn cat(a: [int], b: [int]) -> [int] { [...a, ...b] }", "local r = cat({_data={1,2},len=2}, {_data={3,4},len=2})\n\
             local s = {} for i=1,r.len do s[i]=tostring(r._data[i]) end\n\
             print(r.len .. ':' .. table.concat(s, ','))", "4:1,2,3,4");
    }

    /// Guard: a loop that reassigns its own counter in the body can't become a
    /// `for` (the for-variable would be loop-controlled, ignoring the write), so
    /// it stays a goto loop and the reassignment is honored. `for i in 0..6 {
    /// last = i; i = n }` with `n = 10` runs ONCE (i jumps to 10, fails `< 6`),
    /// leaving `last = 0`. A wrongly-synthesized `for` would ignore `i = n`, run
    /// 6 times, and leave `last = 5`.
    #[test]
    fn counter_reassigned_in_body_stays_correct() {
        let src = "fn f(n: int) -> int {\n\
                   let last = 99\n\
                   for i in 0..6 { last = i\n i = n }\n\
                   last\n\
                   }";
        let lua = emit(src);
        // Naive substring over the whole emitted module: any `for c` anywhere
        // (another fn's counted loop, a stray comment) would trip it, so keep
        // this program a single fn with no counted loop of its own.
        assert!(
            !lua.contains("for c"),
            "counter-reassigning loop must NOT become a for (slot stored >2x), got:\n{lua}"
        );
        let Some(out) = run(src, "print(f(10))") else { return };
        assert_eq!(out, "0");
    }
}
