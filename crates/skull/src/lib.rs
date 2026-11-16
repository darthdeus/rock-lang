//! skull: the default code-generation backend over `bone`'s typed HIR.
//!
//! skull consumes `bone` as a library (`parse_rock` -> `semantic_pass` ->
//! `Workspace`), lowers the typed HIR to the `skull-mir` cell-model CFG, runs
//! the `skull-transform` passes over it, and assembles a LuaJIT program from
//! the `skull-codegen-lua` output ([`mir_emit`]). The JS and GLSL backends
//! read the same MIR.

pub mod allocation_report;
pub(crate) mod coroutine_emit;
pub mod mir_emit;
pub mod shader_artifact;

/// Whether emitted Lua carries LuaJIT line directives (`ROCK_SKULL_LINE_DIRECTIVES`).
pub use skull_codegen_lua::line_directives_mode;

// The shared graph layer, re-exported under its original paths. It moved BELOW
// the backends (`skull-graph`) so the JS and GLSL emitters can realize the same
// graph the luajit driver does; consumers name it the same way either side of
// that move.
pub use skull_graph::{
    artifact, concrete_body, expr_subtree, instance_graph, shader, target_projection,
};
pub mod artifact_fingerprint;
pub mod pipeline;

use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use skull_mir::lua_nesting::{LuaDisplayPart, display_expr, display_record, display_return};
use skull_parser::CompilerError;

/// Per-nominal-type layout hash, keyed by type name (unique workspace-wide).
pub type ShapeHashes = rustc_hash::FxHashMap<ustr::Ustr, u64>;

/// Hash every nominal type's RUNTIME LAYOUT for the hot-reload schema-drift
/// restart trigger: live data outlives
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
    changed.extend(
        old.keys()
            .filter(|n| !new.contains_key(*n))
            .map(|n| n.to_string()),
    );
    changed.sort();
    changed
}

/// The single user module id (everything in the workspace but the builtin one).
pub fn user_module_id(ws: &Workspace) -> Result<ModuleId, CompilerError> {
    ws.source_modules()
        .map(|m| m.id)
        .next()
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
    /// `@test(backends = "...")`: the only backends that compile and run the
    /// test. `None` = every backend.
    pub backends: Option<Vec<bone_core::annotations::TestBackend>>,
}

impl SkullTest {
    pub fn runs_on(&self, backend: bone_core::annotations::TestBackend) -> bool {
        self.backends
            .as_ref()
            .map_or(true, |list| list.contains(&backend))
    }
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
    Ok(module_tests_with_ids(ws, mid)?
        .into_iter()
        .map(|(_, t)| t)
        .collect())
}

/// [`module_tests`] carrying each test's `Idx<Function>` too -- the JS test
/// executor needs the `FuncRef` to lower/emit the fn, not just its Lua name.
pub fn module_tests_with_ids(
    ws: &Workspace,
    mid: ModuleId,
) -> Result<
    Vec<(
        bone_semantic::prelude::Idx<bone_semantic::hir::Function>,
        SkullTest,
    )>,
    CompilerError,
> {
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
        let returns = match test_ann.args.iter().find(|arg| {
            arg.name
                .as_ref()
                .map_or(false, |n| n.name.as_str() == "returns")
        }) {
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
            arg.name
                .as_ref()
                .map_or(false, |n| n.name.as_str() == "expect_failure")
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
        let output = match test_ann.args.iter().find(|arg| {
            arg.name
                .as_ref()
                .map_or(false, |n| n.name.as_str() == "output")
        }) {
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
        let backends = match test_ann.args.iter().find(|arg| {
            arg.name
                .as_ref()
                .map_or(false, |n| n.name.as_str() == "backends")
        }) {
            Some(arg) => {
                let parsed = match &arg.value {
                    bone_semantic::hir::AnnoValue::String(s) => {
                        bone_core::annotations::parse_test_backends(s.as_str())
                    }
                    _ => Err("backends expects a string such as \"lua\" or \"lua, js\"".into()),
                };
                match parsed {
                    Ok(list) => Some(list),
                    Err(message) => {
                        return Err(CompilerError::new_with_label(
                            test_ann.span,
                            format!("@test on `{}`: {message}", f.name.as_str()),
                        ));
                    }
                }
            }
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
                backends,
            },
        ));
    }
    Ok(out)
}

// --- emit-name helpers (pure; shared by the new-path `mir_emit` + scaffolding) ---

/// Lua name for a global's init fn (`<global> = __skull_ginit_<global>()`).
pub(crate) fn global_init_fn_name(global: &str) -> String {
    format!("__skull_ginit_{}", global)
}

/// A `@deco`-annotation argument (or `deco` param default) as a Lua literal.
pub(crate) fn annotation_value_lua(v: &bone_semantic::hir::AnnoValue) -> Option<String> {
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
/// named annotation arg if present, else its default (`nil`, or the omitted
/// marker where an explicit `null` overrides the default -- filled callee-side
/// in `__deco_<name>`'s prologue). Required params without an arg, and
/// non-literal args/defaults, error. The selection (named/default/required) is
/// the backend-agnostic `skull_mir::decorator::select_decorator_args`, shared
/// with the JS backend's `decorator_call_args_js`; only the literal rendering
/// (`annotation_value_lua`) and the default tokens are Lua-specific.
pub(crate) fn decorator_call_args(
    ws: &Workspace,
    module: ModuleId,
    decorator: bone_semantic::prelude::Idx<bone_semantic::hir::DecoratorDef>,
    ann: &bone_semantic::hir::Annotation,
) -> Result<Vec<String>, CompilerError> {
    use skull_mir::decorator::DecoArg;
    let d = &ws.module(module).decorators[decorator];
    let mut args = Vec::new();
    for choice in skull_mir::decorator::select_decorator_args(ws, module, decorator, ann)? {
        args.push(match choice {
            DecoArg::Value(v) => annotation_value_lua(v).ok_or_else(|| {
                CompilerError::new_no_location(format!(
                    "skull: decorator `{}` arg is a non-literal expression (unsupported)",
                    d.name
                ))
            })?,
            DecoArg::Default => "nil".to_string(),
            DecoArg::Omitted => skull_codegen_lua::OMITTED_ARG.to_string(),
        });
    }
    Ok(args)
}

/// Runtime globals the stdlib expects pre-seeded (rock's codegen emits an init
/// that does this; skull emits this small prelude instead). Empty tables are
/// enough — the stdlib's accessors (`__rock_create_struct`, `vec_new`)
/// nil-guard per-key, so they self-heal off a present-but-empty registry.
fn runtime_prelude() -> &'static str {
    // Runtime setup has two parts:
    //   1. LuaJIT mcode/trace tuning. skull emits more Lua
    //      than LuaJIT's small-script defaults assume (32 KB mcode blocks /
    //      512 KB total / 1000 traces). Exhausting the mcode arena triggers a
    //      global trace flush, dropping hot loops back to the interpreter.
    //      A trace must fit one `sizemcode` block. On arm64 macOS, later
    //      mappings can prevent a second block near the VM, so the first one
    //      must hold the hot set: size == max on both arches, one 64 MB block.
    //      x64 has a +/-2 GB jump range; arm64's +/-128 MB range still permits
    //      a 64 MB block, with placement checked by LuaJIT's allocator. A 32 MB
    //      budget still causes global flushes on larger hot sets; 64 MB avoids
    //      them and has been validated on arm64 macOS.
    //      ROCK_JIT_MCODE_KB overrides the shared default, so the budget can
    //      be swept without a rebuild; this prelude runs at chunk load and
    //      beats any -O flag passed on the command line.
    //      `hotexit` is raised from LuaJIT's default 10 to 100 because a side
    //      trace that FAILS is never penalised or blacklisted
    //      (lj_trace.c:596-606) and snap->count only saturates on success, so a
    //      doomed exit is re-recorded ~245 times for nothing. application's hottest
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
    //      An application combat session compiles ~24,600 traces; at 4000 the trace
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
    //      are exposed as ROCK_JIT_HOTLOOP / ROCK_JIT_LOOPUNROLL: application's inner
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
            .replace("{UINT_CHECK}", skull_codegen_lua::UINT_CHECK_PRELUDE)
            .replace("{NB_ALLOC}", skull_codegen_lua::NB_ALLOC_PRELUDE)
            .replace("{LEAF_VIEWS}", &skull_mir::lower::leaf_views_lua_table())
            .replace(
                "{NB_PRIM_STORAGE}",
                &skull_mir::lower::nb_prim_storage_lua(),
            );
        // `str::replace` on a placeholder that drifted out of the template is a
        // NO-OP, and the survivor is valid Lua: `= {LEAF_VIEWS}` reads an unset
        // global, so the table is EMPTY rather than a syntax error. `pairs({})`
        // then makes `__nb_refresh_views` do nothing and every view dangles
        // after a grow -- silently, which is the class that killed application's
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

const PRELUDE_TEMPLATE: &str = r#"if jit and jit.opt then local __mc = tonumber(os.getenv("ROCK_JIT_MCODE_KB") or "") or 65536 local __he = tonumber(os.getenv("ROCK_JIT_HOTEXIT") or "") or 100 local __hl = tonumber(os.getenv("ROCK_JIT_HOTLOOP") or "") or 56 local __lu = tonumber(os.getenv("ROCK_JIT_LOOPUNROLL") or "") or 15 local __mt = tonumber(os.getenv("ROCK_JIT_MAXTRACE") or "") or 32000 local __mk = tonumber(os.getenv("ROCK_JIT_MAXIRCONST") or "") or 500 jit.opt.start("maxmcode=" .. __mc, "sizemcode=" .. __mc, "maxtrace=" .. __mt, "maxrecord=32000", "maxsnap=4000", "maxirconst=" .. __mk, "hotexit=" .. __he, "hotloop=" .. __hl, "loopunroll=" .. __lu) end
-- Everything up to the entry call runs inside one block, so the block's locals
-- -- the storage cell first of all -- are CLOSED upvalues (UREFC: one guard,
-- no constant) by the time an emitted function runs under the JIT. Left open,
-- every inlined frame reading a cell would intern its own 2-slot i64 guard
-- constant. The matching
-- `end` is the last line the full emitter writes; the entry call the CLI
-- appends runs after it.
do
-- The chunk-level cell every bound function reads a mutable global through;
-- the program assigns it from __rock_globals_open before running any code.
local __rock_state_cell
{UINT_CHECK}
if not _G.__rock_vec_headers then _G.__rock_vec_headers = {} end
-- Shared headers of unreserved flat Vecs. The stride is part of the key, so a
-- hot reload that changes an element's layout gets a fresh header.
function __rock_vec_header(repr, elem, stride)
  local by_elem = __rock_vec_headers[elem]
  if not by_elem then by_elem = {}; __rock_vec_headers[elem] = by_elem end
  local key = repr == "strided" and stride or repr
  local h = by_elem[key]
  if not h then
    h = repr == "strided" and { repr = repr, elem = elem, stride = stride, cap = 0 }
      or { repr = repr, elem = elem, cap = 0 }
    by_elem[key] = h
  end
  return h
end
local __rock_table_new = require("table.new")
if not _G.ffi then _G.ffi = require("ffi") end
if not _G.__ROCK_C_STRUCT then _G.__ROCK_C_STRUCT = {} end
if not _G.__ROCK_TYPE_TABLE then _G.__ROCK_TYPE_TABLE = {} end
-- skull_codegen_lua::OMITTED_ARG: what a call passes for an omitted argument
-- whose nullable parameter has a non-null default (EXPLICIT_NULL_DEFAULT).
if not _G.__rock_omitted then
  _G.__rock_omitted = setmetatable({}, { __tostring = function() return "<omitted argument>" end })
end
_G.__ROCK_ENUM_UNITS = {}
-- Ordinary-fn `defer` unwinding (skull_mir::mir::DEFER_PUSH). Registering a
-- defer pushes its cleanup closure and every scope-exit splice pops it, so the
-- stack holds exactly the cleanups of scopes still open. A body that pushes
-- re-enters itself through __rock_defer_call; when an error escapes, the
-- cleanups above that call's base run LIFO and the error is re-raised.
if not _G.__rock_defer_stack then
  _G.__rock_defer_stack = {}
  _G.__rock_defer_top = 0
end
_G.__rock_defer_entering = false
function __rock_defer_push(d)
  local n = __rock_defer_top + 1
  __rock_defer_stack[n] = d
  __rock_defer_top = n
end
function __rock_defer_pop()
  local n = __rock_defer_top
  __rock_defer_stack[n] = nil
  __rock_defer_top = n - 1
end
-- Never raises: a cleanup error's __tostring may itself throw.
local function __rock_defer_describe(e)
  local ok, s = pcall(tostring, e)
  if ok and type(s) == "string" then return s end
  return "<unprintable " .. type(e) .. " error>"
end
local __rock_defer_finish
-- Whether the error being handled is __rock_defer_finish re-raising one it
-- unwound: `error` called directly by it, `level` frames above the caller.
local function __rock_defer_reraised(level)
  local raiser = debug.getinfo(level + 1, "f")
  local site = debug.getinfo(level + 2, "f")
  return raiser ~= nil and raiser.func == error and site ~= nil and site.func == __rock_defer_finish
end
-- The trampoline's handler records the raise-site traceback of a fresh error.
-- The record is only read for a re-raise, and __rock_defer_finish sets it
-- immediately before re-raising, so a record left by a caught error is never
-- picked up.
local function __rock_defer_mark(e)
  if not __rock_defer_reraised(2) then
    _G.__rock_unwind_traceback = debug.traceback("", 2)
  end
  return e
end
local function __rock_defer_report(msg)
  local sink = rawget(_G, "__rock_log_sink")
  if not (sink and pcall(sink, 1, "DEFER", msg, false)) then
    print("[DEFER] " .. msg)
  end
end
__rock_defer_finish = function(base, ok, ...)
  if ok then return ... end
  -- An error raised before the re-entry consumed the flag must not leave it set.
  __rock_defer_entering = false
  local err = ...
  -- A cleanup may raise through other trampolines, which record their own.
  local traceback = _G.__rock_unwind_traceback
  local stack = __rock_defer_stack
  while __rock_defer_top > base do
    local n = __rock_defer_top
    local d = stack[n]
    stack[n] = nil
    __rock_defer_top = n - 1
    local dok, derr = pcall(d)
    -- The original error wins. A string error carries the cleanup's message;
    -- any other value must stay identical, so the message goes to the log.
    if not dok then
      if type(err) == "string" then
        err = err .. "\n  (defer raised while unwinding: " .. __rock_defer_describe(derr) .. ")"
      else
        __rock_defer_report("defer raised while unwinding a " .. type(err)
          .. " error: " .. __rock_defer_describe(derr))
      end
    end
  end
  _G.__rock_unwind_traceback = traceback
  -- Not a tail call: __rock_defer_reraised finds this frame above `error`.
  error(err, 0)
end
function __rock_defer_call(f, ...)
  local base = __rock_defer_top
  __rock_defer_entering = true
  return __rock_defer_finish(base, xpcall(f, __rock_defer_mark, ...))
end
-- An xpcall message handler: the raise-site traceback of an error that was
-- unwound through a defer, else the current one.
function __rock_traceback(e)
  if __rock_defer_reraised(2) then
    return __rock_defer_describe(e) .. _G.__rock_unwind_traceback
  end
  return debug.traceback(__rock_defer_describe(e), 2)
end
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
-- The write into an `f32` field: round through one reused float cell, so the
-- field holds the same number in a table, a scalarized local or cdata.
local __rock_f32_cell = ffi.new("float[1]")
local function __rock_f32(x)
  __rock_f32_cell[0] = x
  return __rock_f32_cell[0]
end
_G.__rock_f32 = __rock_f32
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
{NB_ALLOC}
{NB_PRIM_STORAGE}
if not _G.__rock_on_hot_reload then _G.__rock_on_hot_reload = {} end
-- Mutable globals and statics live in one table per process, created by the
-- first full program and never replaced, so an incremental overlay runs
-- against the values the program wrote. Globals take compiler-assigned
-- integer slots; statics take string keys from their logical identity. The
-- table is sized at creation to the compiler's rehash fixpoint (narr, nhash):
-- LuaJIT specialises a constant-key read to the table's CURRENT array/hash
-- split, and a table grown lazily from `{}` rehashes on every hash-part
-- overflow, which left each trace recorded before a move with a permanently
-- failing guard. A program whose global layout differs from the one that
-- created the table cannot reuse it: its slots would name other globals.
_G.__rock_globals_open = function(layout, narr, nhash)
  local globals = rawget(_G, "__rock_globals")
  if globals == nil then
    globals = __rock_table_new(narr, nhash)
    rawset(_G, "__rock_globals", globals)
    rawset(_G, "__rock_globals_layout", layout)
    rawset(_G, "__rock_globals_fresh", true)
  elseif rawget(_G, "__rock_globals_layout") ~= layout then
    error("Rock global layout changed; restart required", 0)
  else
    rawset(_G, "__rock_globals_fresh", false)
  end
  return globals
end
-- A static whose initializer changed since the last load is reset, so its
-- next reach re-runs the edited initializer; an unchanged one keeps its value.
if not _G.__rock_static_init_keys then _G.__rock_static_init_keys = {} end
_G.__rock_static_prepare = function(value_key, done_key, initializer_key)
  local keys = _G.__rock_static_init_keys
  local old = keys[value_key]
  if old ~= nil and old ~= initializer_key then
    local globals = _G.__rock_globals
    globals[value_key] = nil
    globals[done_key] = false
  end
  keys[value_key] = initializer_key
end
-- A static an edit removed: its cells stop holding the last value.
_G.__rock_static_drop = function(value_key, done_key)
  local globals = _G.__rock_globals
  globals[value_key] = nil
  globals[done_key] = nil
  _G.__rock_static_init_keys[value_key] = nil
end
-- The global directory, published by every full program: one entry per
-- mutable global, { name, identity, physical = {leaf names}, slots = {...} }.
-- A host adds `get`/`set` by identity (__rock_global_decorate) for a
-- global stored as several leaves.
-- Reflection and the `_G` bridge read globals only through it.
_G.__rock_globals_publish = function(directory)
  rawset(_G, "__ROCK_GLOBALS", directory)
  rawset(_G, "__rock_global_index", nil)
end
_G.__rock_global_index_build = function()
  local index = {}
  local function add(key, entry)
    if not key then return end
    local old = index[key]
    if old == nil or old == entry then index[key] = entry else index[key] = false end
  end
  for _, entry in ipairs(rawget(_G, "__ROCK_GLOBALS") or {}) do
    add(entry.name, entry)
    add(entry.identity, entry)
    for _, physical in ipairs(entry.physical) do add(physical, entry) end
  end
  rawset(_G, "__rock_global_index", index)
  return index
end
-- By name, identity or physical leaf name; nil when absent or ambiguous.
_G.__rock_global_entry = function(name)
  local index = rawget(_G, "__rock_global_index")
  if not index then index = _G.__rock_global_index_build() end
  local entry = index[name]
  return entry ~= false and entry or nil
end
_G.__rock_global_decorate = function(rows)
  for identity, fields in pairs(rows) do
    local entry = _G.__rock_global_entry(identity)
    if not entry then error("no Rock global " .. identity .. " to decorate", 0) end
    for key, value in pairs(fields) do entry[key] = value end
  end
end
_G.__rock_global_get = function(entry)
  if entry.get then return entry.get() end
  if #entry.slots == 1 then return _G.__rock_globals[entry.slots[1]] end
  error("Rock global " .. entry.identity .. " has no single-value accessor", 0)
end
_G.__rock_global_set = function(entry, value)
  if entry.set then return entry.set(value) end
  if #entry.slots == 1 then _G.__rock_globals[entry.slots[1]] = value; return end
  error("Rock global " .. entry.identity .. " has no single-value accessor", 0)
end
-- Inline Lua names a Rock global by its emitted symbol, which lives in the
-- storage table rather than in `_G`; the bridge routes those reads and writes.
_G.__rock_globals_install_bridge = function()
  local directory = rawget(_G, "__ROCK_GLOBALS") or {}
  local mt = getmetatable(_G) or {}
  local old_index, old_newindex = mt.__rock_globals_original_index, mt.__rock_globals_original_newindex
  if not mt.__rock_globals_bridge then
    old_index, old_newindex = mt.__index, mt.__newindex
    mt.__rock_globals_original_index = old_index
    mt.__rock_globals_original_newindex = old_newindex
  end
  -- Which of the three pre-bridge fallback shapes applies is decided HERE, once,
  -- rather than re-decided inside the metamethod on every miss. `_G` normally
  -- carries no metatable at all, so both resolve to nil and the miss path is a
  -- plain `return nil` / `rawset` with no Lua-level TAIL CALL -- and a miss is
  -- the common case: a read of any global that is not a Rock global and
  -- holds nil (an unset feature flag, say) reaches `__index` every time, and
  -- cannot be cached raw, because rawsetting nil leaves the key absent.
  local index_fn = type(old_index) == "function" and old_index or nil
  local index_tab = type(old_index) == "table" and old_index or nil
  local newindex_fn = type(old_newindex) == "function" and old_newindex or nil
  local newindex_tab = type(old_newindex) == "table" and old_newindex or nil
  local previous_physical = rawget(_G, "__rock_globals_bridge_physical") or {}
  for _, physical in ipairs(previous_physical) do rawset(_G, physical, nil) end
  local current_physical = {}
  for _, entry in ipairs(directory) do
    for _, physical in ipairs(entry.physical) do
      rawset(_G, physical, nil)
      current_physical[#current_physical + 1] = physical
    end
  end
  rawset(_G, "__rock_globals_bridge_physical", current_physical)
  mt.__index = function(t, key)
    local entry = _G.__rock_global_entry(key)
    if entry then return _G.__rock_global_get(entry) end
    if index_fn then return index_fn(t, key) end
    if index_tab then return index_tab[key] end
    return nil
  end
  mt.__newindex = function(t, key, value)
    local entry = _G.__rock_global_entry(key)
    if entry then return _G.__rock_global_set(entry, value) end
    if newindex_fn then return newindex_fn(t, key, value) end
    if newindex_tab then newindex_tab[key] = value; return end
    rawset(t, key, value)
  end
  mt.__rock_globals_bridge = true
  setmetatable(_G, mt)
end
-- Shared metatable of capturing-lambda env records:
-- a record { __slot = <CodeRef>, <captures> } is callable as f(...) via __call.
-- Preserved across reload so pre- and post-reload records share metatable identity.
{FN_MT}
if not _G.__rock_rng_next then
  local __rock_bit = require("bit")
  local __rock_rng_state = 0
  local function active_rng_state() return __rock_rng_state end
  local function store_rng_state(state) __rock_rng_state = state end
  _G.__rock_rng_seed = function(s)
    local state = __rock_bit.tobit(tonumber(s))
    store_rng_state(state)
  end
  -- Whole RNG state is this one counter, a plain 32-bit number.
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
  local function active_presentation_rng_state() return __rock_presentation_rng_state end
  local function store_presentation_rng_state(state) __rock_presentation_rng_state = state end
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

/// Whether a struct named `name` is declared `value` (C-layout cdata).
fn is_value_struct(ws: &Workspace, name: ustr::Ustr) -> bool {
    ws.struct_by_name
        .get(&name)
        .and_then(|sid| {
            ws.modules
                .get(sid.module.index())
                .map(|m| skull_mir::lower::has_value_plan(&m.structs[sid.def]))
        })
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
            skull_mir::lower::has_value_plan(s).then(|| s.clone())
        })
        .collect();
    v.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
    v
}

/// Allocate a native value from scalar leaves without nested FFI descriptors.
fn leaf_ctor_def(ws: &Workspace, name: ustr::Ustr, paths: &[ustr::Ustr]) -> String {
    let sym = skull_mir::mangle::type_symbol(ws, name);
    let params: Vec<String> = (0..paths.len()).map(|i| format!("l{i}")).collect();
    let mut body = format!("  local t = {sym}_ct()\n");
    for (i, path) in paths.iter().enumerate() {
        let key = if path.contains('.') {
            skull_mir::lower::cdata_leaf_alias(ws, name, path.as_str())
        } else {
            *path
        };
        body.push_str(&format!("  t.{key} = {}\n", params[i]));
    }
    format!(
        "{sym}_lct = function({})\n{body}  return t\nend\n",
        params.join(", ")
    )
}

fn cdata_alias_fields(
    ws: &Workspace,
    ffi_names: &rustc_hash::FxHashSet<ustr::Ustr>,
    root: ustr::Ustr,
    child: ustr::Ustr,
    prefix: &str,
) -> Result<String, CompilerError> {
    if let Some(shape) =
        skull_mir::lower::EnumPlan::for_name(ws, child).and_then(|plan| plan.cdata().cloned())
    {
        let tag = skull_mir::lower::cdata_leaf_alias(ws, root, &format!("{prefix}.__tag"));
        let mut out = format!("int {tag}; ");
        for (name, ct) in shape.slots {
            let alias = skull_mir::lower::cdata_leaf_alias(ws, root, &format!("{prefix}.{name}"));
            out.push_str(&format!("{ct} {alias}; "));
        }
        return Ok(out);
    }
    let sid = ws.struct_by_name[&child];
    let mut out = String::new();
    for field in &ws.modules[sid.module.index()].structs[sid.def].fields {
        let ct = c_field_type(ws, field.storage_ty).ok_or_else(|| {
            CompilerError::new_no_location(format!(
                "native leaf alias has no C type: {child}.{}",
                field.name
            ))
        })?;
        let dep = ustr::Ustr::from(&ct);
        let path = format!("{prefix}.{}", field.name);
        if ffi_names.contains(&dep)
            || skull_mir::lower::EnumPlan::for_name(ws, dep)
                .is_some_and(|plan| plan.cdata().is_some())
        {
            let nested = cdata_alias_fields(ws, ffi_names, root, dep, &path)?;
            out.push_str(&format!("struct {{ {nested} }}; "));
        } else {
            let alias = skull_mir::lower::cdata_leaf_alias(ws, root, &path);
            out.push_str(&format!(
                "{} {alias}; ",
                skull_mir::mangle::type_symbol(ws, dep)
            ));
        }
    }
    Ok(out)
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
/// emission is dependency-ordered: each round emits the structs whose
/// value-struct fields are all emitted. A round with no progress is a
/// definition cycle: rejected, not emitted broken. A GC-carrying struct never
/// enters the cdef ordering (a table has no C layout to sequence).
fn value_struct_defs(
    ws: &Workspace,
    rep_arena: &skull_mir::lower::RepArena,
    value_ctors: &mut ValueCtors,
) -> Result<
    (
        String,
        rustc_hash::FxHashSet<ustr::Ustr>,
        rustc_hash::FxHashSet<ustr::Ustr>,
    ),
    CompilerError,
> {
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
                let ct = c_field_type(ws, f.storage_ty).expect(
                    "FFI-clean value-struct field must be C-mappable (CData storage invariant)",
                );
                let dep = ustr::Ustr::from(ct.as_str());
                if ffi_names.contains(&dep) && !emitted.contains(&dep) {
                    blocked = true;
                    continue;
                }
                // The dependency key above is the LOGICAL name (what the
                // workspace sets hold); the cdef text is the C one.
                let field_type = skull_mir::mangle::type_symbol(ws, dep);
                if ffi_names.contains(&dep)
                    || skull_mir::lower::EnumPlan::for_name(ws, dep)
                        .is_some_and(|plan| plan.cdata().is_some())
                {
                    // Mirror nested aggregates rather than spelling padding as
                    // char arrays: padding fields would change the native ABI's
                    // integer/SSE classification for by-value FFI calls.
                    let aliases = cdata_alias_fields(ws, &ffi_names, s.name, dep, f.name.as_str())?;
                    body.push_str(&format!(
                        "  union {{ {field_type} {}; struct {{ {aliases} }}; }};\n",
                        f.name
                    ));
                } else {
                    body.push_str(&format!("  {field_type} {};\n", f.name));
                }
            }
            if blocked {
                next.push(s);
            } else {
                let sym = skull_mir::mangle::type_symbol(ws, s.name);
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
                    value_ctors.push((
                        ustr::Ustr::from(&format!("{sym}_lct")),
                        leaf_ctor_def(ws, s.name, &paths),
                    ));
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
    Ok((
        format!("ffi.cdef[[\n{}]]\n{}{}{}", typedefs, ctors, checks, storage),
        emitted,
        gc,
    ))
}

/// The load-time check behind [`value_leaf_layout_checks`]. `leaves` is
/// `path offset` pairs and `nested` maps each nested field path to its C type,
/// so the helper composes `ffi.offsetof` along a leaf's path. A nested leaf is
/// also checked through its flattened alias (`cdata_leaf_alias`), whose name
/// is `prefix` plus the hex of the path.
const LAYOUT_CHECK_LUA: &str = r#"do
local hex = {}
for i = 0, 255 do hex[string.char(i)] = string.format("%02x", i) end
function __rock_layout_check(sym, size, leaves, nested, prefix)
  local ct = ffi.typeof(sym)
  if ffi.sizeof(ct) ~= size then
    error("skull layout drift: sizeof(" .. sym .. ") = " .. ffi.sizeof(ct) .. ", expected " .. size, 0)
  end
  local owner = {}
  for path, ty in string.gmatch(nested, "(%S+) (%S+)") do owner[path] = ffi.typeof(ty) end
  for path, want in string.gmatch(leaves, "(%S+) (%d+)") do
    want = tonumber(want)
    local at, ty, start = 0, ct, 1
    while true do
      local dot = string.find(path, ".", start, true)
      local hop = ffi.offsetof(ty, string.sub(path, start, (dot or 0) - 1))
      if hop == nil then at = nil break end
      at = at + hop
      if not dot then break end
      ty = owner[string.sub(path, 1, dot - 1)]
      start = dot + 1
    end
    if at ~= want then
      error("skull layout drift: " .. sym .. "." .. path .. " at " .. tostring(at) .. ", expected " .. want, 0)
    end
    if start > 1 then
      local alias = (prefix or "__rock_leaf_") .. string.gsub(path, ".", hex)
      if ffi.offsetof(ct, alias) ~= want then error("skull leaf alias drift: " .. sym .. "." .. alias, 0) end
    end
  end
end
end
-- Runs once per type at load: tracing its loops costs 3x what it saves.
if jit and jit.off then jit.off(__rock_layout_check) end
"#;

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
/// Leaf addressing indexes typed
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
        let sym = skull_mir::mangle::type_symbol(ws, name);
        if out.is_empty() {
            out.push_str(LAYOUT_CHECK_LUA);
        }
        let _ = write!(out, "__rock_layout_check(\"{sym}\", {size}, \"");
        let mut nested: Vec<(&str, ustr::Ustr)> = Vec::new();
        for (i, leaf) in leaves.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            let path = leaf.path.as_str();
            let _ = write!(out, "{path} {}", leaf.byte_off);
            let mut ends = path.match_indices('.').map(|(at, _)| at);
            for (owner, end) in leaf.owners.iter().zip(&mut ends) {
                if !nested.iter().any(|(p, _)| *p == &path[..end]) {
                    nested.push((&path[..end], *owner));
                }
            }
        }
        out.push_str("\", \"");
        for (i, (path, owner)) in nested.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            let _ = write!(out, "{path} {owner}");
        }
        out.push('"');
        let prefix = skull_mir::lower::cdata_leaf_alias(ws, name, "");
        if !nested.is_empty() && prefix != "__rock_leaf_" {
            let _ = write!(out, ", \"{prefix}\"");
        }
        out.push_str(")\n");
        storage.push_str(&skull_mir::lower::nb_value_storage_lua(
            ws, name, &leaves, size,
        ));
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
    value_ctors: &mut ValueCtors,
) -> Result<String, CompilerError> {
    let mut out = String::new();
    for s in canonical_value_structs(ws) {
        if !gc.contains(&s.name) {
            continue;
        }
        let n = s.name;
        // `n` stays the readable path for DISPLAY text; every name the chunk
        // defines or tags with is the type's symbol.
        let sym = skull_mir::mangle::type_symbol(ws, n);
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
            let cmps: Vec<String> = paths
                .iter()
                .map(|p| format!("a[\"{p}\"] == b[\"{p}\"]"))
                .collect();
            out.push_str(&format!(
                "  return {}\n",
                if cmps.is_empty() {
                    "true".to_string()
                } else {
                    cmps.join(" and ")
                }
            ));
            out.push_str("end\n");
            out.push_str(&format!("{mt}.__tostring = function(v)\n"));
            let parts = if paths.is_empty() {
                vec![
                    LuaDisplayPart::literal(&format!("{n} {{ ")),
                    LuaDisplayPart::literal(""),
                    LuaDisplayPart::literal(" }"),
                ]
            } else {
                display_record(
                    &format!("{n} {{ "),
                    paths.iter().map(|p| {
                        (
                            format!("{p}: "),
                            vec![LuaDisplayPart::Expr(format!("tostring(v[\"{p}\"])"))],
                        )
                    }),
                    " }",
                )
            };
            out.push_str(&display_return(&parts, "  "));
            out.push_str("end\n");
            let params: Vec<String> = (0..paths.len()).map(|i| format!("l{i}")).collect();
            // An absent tag canonicalizes to `nil` -- the form a `= null` write
            // and an absent literal both store. The scalar ABI spells absence
            // `false` on the wire, and two logically-equal absent values then
            // compared unequal (`__eq` is a per-key compare, and `false ~= nil`).
            let presence = rep_arena
                .nominal_value_leaf_presence_flags_required(
                    ws,
                    bone_semantic::hir::FnBackend::Lua,
                    n,
                )
                .map_err(|e| CompilerError::new_no_location(e.message))?;
            let assigns: Vec<String> = paths
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let tag = presence.get(i).copied().unwrap_or(false);
                    let v = if tag {
                        format!("l{i} or nil")
                    } else {
                        format!("l{i}")
                    };
                    format!("[\"{p}\"] = {v}, ")
                })
                .collect();
            value_ctors.push((
                ustr::Ustr::from(&format!("{sym}_lct")),
                format!(
                    "{sym}_lct = function({p})\n{r}\nend\n",
                    p = params.join(", "),
                    r = skull_codegen_lua::wrapper_return(&format!(
                        "setmetatable({{ {b}__ty = \"{sym}\" }}, {mt})",
                        b = assigns.concat(),
                    )),
                ),
            ));
            continue;
        }
        // The box's keys: a declared field's name, or an inline child's dotted
        // leaf paths (`skull_mir::lower::gc_table_fields`, shared with every
        // MIR reader and writer of the box).
        let fields = skull_mir::lower::gc_table_fields(ws, bone_semantic::hir::FnBackend::Lua, n)
            .map_err(|e| CompilerError::new_no_location(e.message))?;
        // value __eq
        out.push_str(&format!("{mt}.__eq = function(a, b)\n"));
        out.push_str("  if type(a) ~= \"table\" or type(b) ~= \"table\" then return false end\n");
        let cmps: Vec<String> = fields
            .iter()
            .flat_map(|f| f.keys().into_iter().zip(f.keys_eq(ws)))
            .map(|((key, _), equality)| {
                let (a, b) = (
                    lua_key_read("a", key.as_str()),
                    lua_key_read("b", key.as_str()),
                );
                match equality {
                    skull_mir::lower::GcFieldEq::Reference => format!("rawequal({a}, {b})"),
                    skull_mir::lower::GcFieldEq::Native => format!("{a} == {b}"),
                    skull_mir::lower::GcFieldEq::Structural => format!("__op_eq({a}, {b})"),
                }
            })
            .collect();
        if cmps.is_empty() {
            out.push_str("  return true\n");
        } else {
            out.push_str(&format!("  return {}\n", cmps.join(" and ")));
        }
        out.push_str("end\n");
        // __tostring: field-wise, mirroring the cdata tier's `Name { f: v, .. }`.
        out.push_str(&format!("{mt}.__tostring = function(v)\n"));
        if fields.is_empty() {
            out.push_str(&format!("  return \"{n} {{ }}\"\n"));
        } else {
            let parts = display_record(
                &format!("{n} {{ "),
                fields
                    .iter()
                    .map(|f| (format!("{}: ", f.name), f.lua_display_parts())),
                " }",
            );
            out.push_str(&display_return(&parts, "  "));
        }
        out.push_str("end\n");
        // `<Name>_vt(...)` positional ctor over the box keys. A declared field's
        // parameter keeps its name; an inline leaf's is positional, since a
        // dotted path has no injective identifier spelling.
        let mut params = Vec::new();
        let mut body = String::new();
        for f in &fields {
            for (key, presence) in f.keys() {
                let param = if f.is_inline() {
                    format!("__rock_l{}", params.len())
                } else {
                    key.as_str().to_string()
                };
                // An absent tag canonicalizes to `nil`, as the leaf-table ctor's.
                let value = if presence {
                    format!("{param} or nil")
                } else {
                    param.clone()
                };
                if f.is_inline() {
                    body.push_str(&format!("[\"{key}\"] = {value}, "));
                } else {
                    body.push_str(&format!("{key} = {value}, "));
                }
                params.push(param);
            }
        }
        value_ctors.push((
            ustr::Ustr::from(&format!("{sym}_vt")),
            format!(
                "{sym}_vt = function({p})\n{r}\nend\n",
                p = params.join(", "),
                r = skull_codegen_lua::wrapper_return(&format!(
                    "setmetatable({{ {body}__ty = \"{sym}\" }}, {mt})",
                )),
            ),
        ));
    }
    Ok(out)
}

/// `t.key`, or `t["a.b"]` for a dotted leaf key.
fn lua_key_read(table: &str, key: &str) -> String {
    if key.contains('.') {
        format!("{table}[\"{key}\"]")
    } else {
        format!("{table}.{key}")
    }
}

/// Emit the `ffi.cdef` typedef + `<Enum>_ct` ctor for every cdata-eligible enum
/// (`skull_mir::lower::enum_value_shape`): a `value`-struct-style cdata
/// `{ int __tag; <T> _0; }` carrying the variant tag + its single scalar payload.
/// Scalar payloads only (cut 1), so there is no inter-enum dependency; emitted
/// BEFORE value structs so a future enum-typed value-struct field resolves. Name-
/// sorted for deterministic output. Reflection is sacrificed: there is no
/// `{__variant}` table.
fn enum_cdata_defs(ws: &Workspace) -> String {
    let mut names: Vec<ustr::Ustr> = ws.enums_by_name.keys().copied().collect();
    names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let mut typedefs = String::new();
    let mut ctors = String::new();
    for name in names {
        if let Some(shape) =
            skull_mir::lower::EnumPlan::for_name(ws, name).and_then(|plan| plan.cdata().cloned())
        {
            let slots: String = shape
                .slots
                .iter()
                .map(|(nm, t)| format!(" {t} {nm};"))
                .collect();
            let sym = skull_mir::mangle::type_symbol(ws, name);
            typedefs.push_str(&format!(
                "typedef struct {{ int __tag;{slots} }} {n};\n",
                n = sym
            ));
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
        let Some(shape) =
            skull_mir::lower::EnumPlan::for_name(ws, name).and_then(|plan| plan.cdata().cloned())
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
        let sym = skull_mir::mangle::type_symbol(ws, name);
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
                let open = format!("{name}::{vn} {{ ");
                let parts = match &var.payload {
                    VariantPayload::Tuple(tys) if !tys.is_empty() => display_record(
                        &open,
                        (0..tys.len()).map(|j| {
                            (
                                format!("[{}] = ", j + 1),
                                vec![LuaDisplayPart::Expr(format!("tostring(v._{j})"))],
                            )
                        }),
                        " }",
                    ),
                    VariantPayload::Record(fields) if !fields.is_empty() => {
                        let mut fns: Vec<ustr::Ustr> = fields.iter().map(|f| f.name).collect();
                        fns.sort_by(|a, b| a.as_str().cmp(b.as_str()));
                        display_record(
                            &open,
                            fns.iter().map(|f| {
                                (
                                    format!("{f} = "),
                                    vec![LuaDisplayPart::Expr(format!("tostring(v._{f})"))],
                                )
                            }),
                            " }",
                        )
                    }
                    _ => vec![LuaDisplayPart::literal(&format!("{name}::{vn}"))], // unit (or empty tuple/record)
                };
                match display_expr(&parts) {
                    Some(render) => {
                        ts.push_str(&format!("    if t == {i} then return {render} end\n"))
                    }
                    None => {
                        ts.push_str(&format!("    if t == {i} then\n"));
                        ts.push_str(&display_return(&parts, "      "));
                        ts.push_str("    end\n");
                    }
                }
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
/// A thin metatype trampoline delegates to a mutable
/// `_G.__ROCK_C_METATYPE[name]` impl table (metatypes seal once, so the
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
            let sym = skull_mir::mangle::type_symbol(ws, n);
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
                let parts = display_record(
                    &format!("{n} {{ "),
                    s.fields.iter().map(|f| {
                        (
                            format!("{}: ", f.name),
                            vec![LuaDisplayPart::Expr(format!("tostring(v.{})", f.name))],
                        )
                    }),
                    " }",
                );
                out.push_str(&display_return(&parts, "    "));
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
///   (see `skull_mir::mangle::enum_singleton_global`).
///
/// The `<global> = <global> or { ... }` preserve-if-exists form is
/// load-bearing for HOT RELOAD: a reload re-dofiles the whole chunk, and a
/// bare re-creation would mint a NEW table, breaking reference-
/// equality with enum values stored in preserved globals (e.g. a `GRID:
/// []<TileType>` whose `TileType::Wall` entries would no longer `==` the
/// freshly-created singleton -> walls silently stop rendering). A payload-free
/// enum sidesteps this entirely: its values are ints, identity-free by nature.
/// A value struct's table-building constructor (`<Sym>_lct` from leaves,
/// `<Sym>_vt` for a GC table) and its definition. Generated code is its only
/// caller, so assembly defines just the ones the emitted text calls; each
/// depends only on its struct's ctype or metatable.
pub(crate) type ValueCtors = Vec<(ustr::Ustr, String)>;

const VALUE_CTOR_SUFFIXES: [&str; 2] = ["_lct", "_vt"];
/// Each suffix followed by the call's opening parenthesis.
const VALUE_CTOR_CALLS: [&str; 2] = ["_lct(", "_vt("];

pub(crate) fn is_value_ctor_global(name: &str) -> bool {
    VALUE_CTOR_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// Adds every value ctor that `lua` calls to `out`.
pub(crate) fn push_value_ctor_calls(lua: &str, out: &mut Vec<ustr::Ustr>) {
    for call in VALUE_CTOR_CALLS {
        for at in memchr::memmem::find_iter(lua.as_bytes(), call.as_bytes()) {
            let start = lua[..at]
                .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .map_or(0, |i| i + 1);
            let name = ustr::Ustr::from(&lua[start..at + call.len() - 1]);
            if !out.contains(&name) {
                out.push(name);
            }
        }
    }
}

/// The definitions of the value ctors in `called`, in scaffold order.
pub(crate) fn value_ctor_defs(
    defs: &ValueCtors,
    called: &rustc_hash::FxHashSet<ustr::Ustr>,
) -> String {
    defs.iter()
        .filter(|(name, _)| called.contains(name))
        .map(|(_, def)| def.as_str())
        .collect()
}

/// Whether `name` is the `__rock_enum_<E>_names` table of a payload-free enum.
pub(crate) fn is_enum_names_global(name: &str) -> bool {
    name.starts_with("__rock_enum_") && name.ends_with("_names")
}

/// The ordinal -> display-name tables (`__rock_enum_<E>_names[tag]`) of the
/// payload-free enums whose table is in `read`. Interp stringify is their only
/// reader, so a table nothing reads is not emitted. They hold no identity, so
/// an overlay may define one again.
pub(crate) fn enum_names_defs(ws: &Workspace, read: &rustc_hash::FxHashSet<ustr::Ustr>) -> String {
    use bone_semantic::hir::VariantPayload;
    let mut out = String::new();
    if read.is_empty() {
        return out;
    }
    for module in ws.modules.iter() {
        for (_, e) in module.enums.iter() {
            if e.variants.is_empty()
                || !e
                    .variants
                    .iter()
                    .all(|v| matches!(v.payload, VariantPayload::Unit))
            {
                continue;
            }
            // The table is a Lua GLOBAL, so its name carries the enum's
            // symbol; the values it holds are display text.
            let name = skull_mir::mangle::enum_names_global(ws, e.name);
            if !read.contains(&name) {
                continue;
            }
            let items: Vec<String> = e
                .variants
                .iter()
                .enumerate()
                .map(|(ord, v)| format!("[{ord}] = \"{e}::{v}\"", e = e.name, v = v.name))
                .collect();
            out.push_str(&format!("{name} = {{ {} }}\n", items.join(", ")));
        }
    }
    out
}

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
            let all_unit = !e.variants.is_empty()
                && e.variants
                    .iter()
                    .all(|v| matches!(v.payload, VariantPayload::Unit));
            if all_unit {
                // Payload-free: int repr. Its names table comes from
                // `enum_names_defs`, and only when emitted code reads it.
            } else {
                // Mixed: unit variants are interned singleton tables.
                let mut units = Vec::new();
                for v in e.variants.iter() {
                    if matches!(v.payload, VariantPayload::Unit) {
                        let sym = skull_mir::mangle::type_symbol(ws, e.name);
                        let g = skull_mir::mangle::enum_singleton_global(ws, e.name, v.name);
                        // `__ty` is looked up in `__ROCK_TYPE_TABLE`, so it is
                        // the same spelling that registry is keyed on.
                        out.push_str(&format!(
                            "{g} = {g} or {{ __variant = \"{var}\", __ty = \"{ty}\" }}\n",
                            var = v.name,
                            ty = sym,
                        ));
                        units.push(format!("[{:?}] = {g}", v.name.as_str()));
                    }
                }
                if !units.is_empty() {
                    out.push_str(&format!(
                        "_G.__ROCK_ENUM_UNITS[{:?}] = {{ {} }}\n",
                        skull_mir::mangle::type_symbol(ws, e.name).as_str(),
                        units.join(", "),
                    ));
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
/// unmodelled.
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
            let copy_args: Vec<String> = shape
                .field_names()
                .iter()
                .map(|f| format!("v.{f}"))
                .collect();
            let sym = skull_mir::mangle::type_symbol(ws, name);
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
            let sym = skull_mir::mangle::type_symbol(ws, n);
            out.push_str(&format!(
                "_G.__ROCK_C_STRUCT[\"{sym}\"] = {{ inspect = function(v)\n"
            ));
            if s.fields.is_empty() {
                out.push_str(&format!("  return \"{n} {{ }}\"\n"));
            } else {
                let parts = display_record(
                    &format!("{n} {{ "),
                    s.fields.iter().map(|f| {
                        (
                            format!("{}: ", f.name),
                            vec![LuaDisplayPart::Expr(format!("inspect(v.{})", f.name))],
                        )
                    }),
                    " }",
                );
                out.push_str(&display_return(&parts, "  "));
            }
            let copy_args: Vec<String> = s.fields.iter().map(|f| format!("v.{}", f.name)).collect();
            out.push_str(&format!(
                "end, copy = function(v)\n  return {sym}_ct({})\nend, name = \"{sym}\" }}\n",
                copy_args.join(", ")
            ));
        }
    }
    out.push_str("_G.__ROCK_C_STRUCT_BY_CTYPE = {}\n");
    out
}

fn shape_lua(shape: &skull_mir::type_table::Shape, out: &mut String) {
    use skull_mir::type_table::Shape;
    match shape {
        Shape::Prim(name) => out.push_str(&format!("{{k=\"{name}\"}}")),
        Shape::Opt(inner) => {
            out.push_str("{k=\"opt\",inner=");
            shape_lua(inner, out);
            out.push('}');
        }
        Shape::Tuple(elems) => {
            out.push_str("{k=\"tuple\",elems={");
            for (i, e) in elems.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                shape_lua(e, out);
            }
            out.push_str("}}");
        }
        Shape::Vec(elem) => {
            out.push_str("{k=\"vec\",elem=");
            shape_lua(elem, out);
            out.push('}');
        }
        Shape::Map(key, val) => {
            out.push_str("{k=\"map\",key=");
            shape_lua(key, out);
            out.push_str(",val=");
            shape_lua(val, out);
            out.push('}');
        }
        Shape::Field(inner) => {
            out.push_str("{k=\"field\",inner=");
            shape_lua(inner, out);
            out.push('}');
        }
        Shape::Nom(name) => {
            out.push_str("{k=\"nom\",name=\"");
            out.push_str(name.as_str());
            out.push_str("\"}");
        }
    }
}

fn shape_lua_string(shape: &skull_mir::type_table::Shape) -> String {
    let mut out = String::new();
    shape_lua(shape, &mut out);
    out
}

fn variant_lua(entry: &skull_mir::type_table::VariantEntry) -> String {
    use skull_mir::type_table::VariantEntry;
    match entry {
        VariantEntry::Positional(positions) if positions.is_empty() => "pos={}".to_string(),
        VariantEntry::Positional(positions) => {
            let entries: Vec<String> = positions
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let mut d = p
                        .shape
                        .as_ref()
                        .map_or_else(|| "{k=\"any\"}".into(), shape_lua_string);
                    if let Some(slot) = p.slot {
                        d.pop();
                        d.push_str(&format!(",slot={}", slot + 1));
                        if let Some(paths) = &p.leaves {
                            let paths = paths
                                .iter()
                                .map(|path| format!("{:?}", path.as_str()))
                                .collect::<Vec<_>>()
                                .join(",");
                            d.push_str(&format!(",leaves={{{paths}}}"));
                        }
                        d.push('}');
                    }
                    format!("[{}]={d}", i + 1)
                })
                .collect();
            format!("pos={{{}}}", entries.join(","))
        }
        VariantEntry::Named(fields) => {
            let entries: Vec<String> = fields
                .iter()
                .map(|(name, shape)| format!("[{:?}]={}", name.as_str(), shape_lua_string(shape)))
                .collect();
            format!("named={{{}}}", entries.join(","))
        }
    }
}

fn type_entry_lua(entry: &skull_mir::type_table::Entry, out: &mut String) {
    use skull_codegen_lua::lua_string;
    use skull_mir::type_table::Entry;
    match entry {
        Entry::StridedRows { name, paths } => {
            let entries = paths
                .iter()
                .enumerate()
                .map(|(i, path)| format!("[{}] = {}", lua_string(path.as_str()), i + 1))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!(
                "_G.__ROCK_STRIDED_ROWS[{}] = {{ {entries} }}\n",
                lua_string(name.as_str())
            ));
        }
        Entry::Struct {
            name,
            kind,
            fields,
            vec_columns,
        } => {
            let fields: Vec<String> = fields
                .iter()
                .map(|f| {
                    let embed = if f.embed { ", embed = true" } else { "" };
                    let flat = f
                        .flat
                        .as_ref()
                        .map(|keys| {
                            let keys = keys
                                .iter()
                                .map(|key| format!("{:?}", key.as_str()))
                                .collect::<Vec<_>>()
                                .join(",");
                            format!(", flat = {{{keys}}}")
                        })
                        .unwrap_or_default();
                    let desc = f
                        .shape
                        .as_ref()
                        .map(|d| format!(", desc = {}", shape_lua_string(d)))
                        .unwrap_or_default();
                    let mut json = f
                        .json_name
                        .map(|name| format!(", json_name = {}", lua_string(name.as_str())))
                        .unwrap_or_default();
                    if let Some(factory) = &f.json_default {
                        json.push_str(&format!(", json_default = {}", lua_string(factory)));
                    }
                    if let Some(predicate) = &f.json_omit {
                        json.push_str(&format!(", json_omit = {}", lua_string(predicate)));
                    }
                    if f.json_skip {
                        json.push_str(", json_skip = true");
                    }
                    format!(
                        "{{ name = \"{}\", type = \"{}\", nullable = {}{}{}{}{} }}",
                        f.name, f.ty, f.nullable, embed, flat, desc, json
                    )
                })
                .collect();
            let vec_columns = vec_columns
                .as_ref()
                .map(|paths| {
                    let keys = paths
                        .iter()
                        .map(|p| lua_string(p.as_str()))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(", vec_columns = {{ {keys} }}")
                })
                .unwrap_or_default();
            out.push_str(&format!(
                "_G.__ROCK_TYPE_TABLE[\"{name}\"] = {{ name = \"{name}\", kind = \"{kind}\", fields = {{ {f} }}{vec_columns} }}\n",
                f = fields.join(", ")
            ));
        }
        Entry::Newtype { name } => out.push_str(&format!(
            "_G.__ROCK_TYPE_TABLE[\"{name}\"] = {{ name = \"{name}\", kind = \"newtype\", fields = {{ }} }}\n",
        )),
        Entry::FlatLayout {
            name,
            tuple_width,
            record_fields,
        } => {
            let names = record_fields
                .iter()
                .map(|name| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!(
                "_G.__ROCK_ENUM_FLAT_LAYOUT[\"{name}\"] = {{ tuple_width = {tuple_width}, record_fields = {{ {names} }} }}\n",
            ));
        }
        Entry::EnumTag { name, variants } => {
            let variants = variants
                .iter()
                .enumerate()
                .map(|(i, v)| format!("[{i}]={:?}", v.as_str()))
                .collect::<Vec<_>>()
                .join(",");
            out.push_str(&format!(
                "_G.__ROCK_TYPE_TABLE[\"{name}\"] = {{ name = \"{name}\", kind = \"enum_tag\", variants = {{ {variants} }} }}\n"
            ));
        }
        Entry::Enum { name, variants } => {
            let vd = variants
                .iter()
                .map(|(variant, entry)| format!("[{:?}]={{{}}}", variant.as_str(), variant_lua(entry)))
                .collect::<Vec<_>>()
                .join(",");
            out.push_str(&format!(
                "_G.__ROCK_TYPE_TABLE[\"{name}\"] = {{ name = \"{name}\", kind = \"enum\", fields = {{ }}, variant_desc = {{ {vd} }} }}\n",
            ));
        }
        Entry::CEnum {
            name,
            slots,
            variants,
        } => {
            let mut fields =
                vec!["{ name = \"__tag\", type = \"int\", nullable = false }".to_string()];
            fields.extend(slots.iter().map(|(slot, c_type)| {
                let field_type = if *c_type == "double" { "float" } else { c_type };
                format!("{{ name = \"{slot}\", type = \"{field_type}\", nullable = false }}")
            }));
            let variants: Vec<String> = variants
                .iter()
                .enumerate()
                .map(|(tag, (variant, payload))| {
                    let payload: Vec<String> = payload
                        .iter()
                        .map(|p| {
                            format!(
                                "{{ name = \"{}\", slot = \"{}\", type = \"{}\" }}",
                                p.name, p.slot, p.ty
                            )
                        })
                        .collect();
                    format!(
                        "[{tag}] = {{ name = \"{variant}\", fields = {{ {} }} }}",
                        payload.join(", ")
                    )
                })
                .collect();
            out.push_str(&format!(
                "_G.__ROCK_TYPE_TABLE[\"{name}\"] = {{ name = \"{name}\", kind = \"c_enum\", fields = {{ {f} }}, variants = {{ {v} }} }}\n",
                f = fields.join(", "),
                v = variants.join(", ")
            ));
        }
    }
}

/// Register every struct + newtype into `_G.__ROCK_TYPE_TABLE`, so the stdlib
/// reflection fns (`get_type_info`, `type_info_of`, `type_name`) can recover a
/// value's metadata from its `__ty` tag. The prelude already seeds the table
/// empty, so this only adds entries. No metatables: type tagging is the direct
/// `__ty`.
fn type_table_defs(
    ws: &Workspace,
    gc: &rustc_hash::FxHashSet<ustr::Ustr>,
    arena: &skull_mir::lower::RepArena,
) -> Result<String, CompilerError> {
    use skull_mir::type_table;
    let factories = type_table::json_field_factories(ws);
    let is_gc = |name| gc.contains(&name);
    let mut out = String::from("_G.__ROCK_ENUM_FLAT_LAYOUT = {}\n_G.__ROCK_STRIDED_ROWS = {}\n");
    for module in ws.modules.iter() {
        for (_, s) in module.structs.iter() {
            let entries = type_table::struct_entries(
                ws,
                bone_semantic::hir::FnBackend::Lua,
                arena,
                &is_gc,
                &factories,
                s,
            )?;
            entries.iter().for_each(|e| type_entry_lua(e, &mut out));
        }
        for (_, nt) in module.newtypes.iter() {
            type_entry_lua(
                &type_table::Entry::Newtype {
                    name: skull_mir::mangle::type_symbol(ws, nt.name),
                },
                &mut out,
            );
        }
    }
    for module in ws.modules.iter() {
        for (_, e) in module.enums.iter() {
            for entry in type_table::enum_entries(ws, e) {
                type_entry_lua(&entry, &mut out);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    /// Stdlib-free emission: these tests pin codegen shapes on programs that
    /// need no stdlib, and the harness below supplies the few shims they call.
    /// Programs that need the stdlib compile through [`crate::pipeline`].
    fn emit(src: &str) -> String {
        let module = skull_parser::parse_module(src, ustr::Ustr::from("<skull>"))
            .unwrap_or_else(|e| panic!("parse failed: {}", e.message));
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut [module])
            .unwrap_or_else(|e| panic!("semantic pass failed: {}", e.message));
        if let Some(e) = skull_mir::verdict::body_errors(&ws).first() {
            panic!("skull compile failed: {}", e.message);
        }
        let mid = super::user_module_id(&ws).expect("user module");
        crate::mir_emit::emit_workspace_mir(&ws, mid)
            .unwrap_or_else(|e| panic!("skull compile failed: {}", e.message))
            .lua
    }

    /// The hot trace working set needs one 64 MB block on either architecture;
    /// smaller arenas cause global trace flushes. Keep the environment override
    /// so the budget can still be swept without rebuilding.
    #[test]
    fn runtime_prelude_keeps_the_application_trace_working_set() {
        let prelude = super::runtime_prelude();
        for knob in [
            "\"maxtrace=\" .. __mt",
            "maxrecord=32000",
            "maxsnap=4000",
            "\"maxirconst=\" .. __mk",
        ] {
            assert!(
                prelude.contains(knob),
                "runtime prelude stopped setting {knob}"
            );
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
        let kb: u32 = prelude
            .split("ROCK_JIT_MCODE_KB\") or \"\") or ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .expect("prelude no longer defaults the sweepable mcode budget")
            .parse()
            .expect("mcode default is not one architecture-independent literal KB count");
        assert_eq!(kb, 65536, "both architectures need a 64 MB mcode default");
    }

    #[test]
    fn runtime_prelude_rng_state_is_numeric() {
        let prelude = super::runtime_prelude();
        assert!(prelude.contains("local __rock_rng_state = 0"));
        assert!(!prelude.contains("ffi.new(\"uint64_t\", 0)"));
    }

    /// The schema-drift restart trigger's oracle:
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
        assert_eq!(
            super::shape_diff(&h0, &hashes(&body)),
            none,
            "a body edit must not drift any shape"
        );

        let field_add = base.replace(
            "struct Mob { hp: int }",
            "struct Mob { hp: int, armor: int }",
        );
        assert_eq!(super::shape_diff(&h0, &hashes(&field_add)), vec!["Mob"]);

        let field_retype = base.replace("struct Mob { hp: int }", "struct Mob { hp: float }");
        assert_eq!(super::shape_diff(&h0, &hashes(&field_retype)), vec!["Mob"]);

        let vs_reorder = base.replace("value P { x: int, y: int }", "value P { y: int, x: int }");
        assert_eq!(super::shape_diff(&h0, &hashes(&vs_reorder)), vec!["P"]);

        let payload_widen = base.replace("Ev { Tick(int), Done }", "Ev { Tick(int, int), Done }");
        assert_eq!(super::shape_diff(&h0, &hashes(&payload_widen)), vec!["Ev"]);

        let variant_reorder = base.replace("Ev { Tick(int), Done }", "Ev { Done, Tick(int) }");
        assert_eq!(
            super::shape_diff(&h0, &hashes(&variant_reorder)),
            vec!["Ev"]
        );

        let rewrap = base.replace("newtype Gold(int)", "newtype Gold(float)");
        assert_eq!(super::shape_diff(&h0, &hashes(&rewrap)), vec!["Gold"]);

        let removed = base.replace("newtype Gold(int)\n", "");
        assert_eq!(
            super::shape_diff(&h0, &hashes(&removed)),
            vec!["Gold"],
            "a removed type counts as drifted"
        );
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
        let embedded_wide =
            "struct Pt { x: int, y: int }\nstruct Holder { embed p: Pt, tag: int }\n";
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
        assert!(lua.contains("function five_0()"), "got:\n{lua}");
        // The literal is a single-use pure value -> tree-folded into the return.
        assert!(lua.contains("do return (5)"), "got:\n{lua}");
    }

    #[test]
    fn if_prints_as_a_structured_if() {
        let lua = emit("fn f(c: bool) -> int { if c { 1 } else { 2 } }");
        let body = &lua[lua.find("function f_bool(").expect("f emitted")..];
        let body = &body[..body.find("\nend\n").expect("f closed")];
        assert!(
            body.contains(" then\n") && body.contains("\n else\n"),
            "expected an if/else, got:\n{body}"
        );
        assert!(
            !body.contains("goto") && !body.contains("::"),
            "expected no labels or jumps, got:\n{body}"
        );
    }

    /// Execution oracle: emit Lua, append a `driver`, run under luajit, return
    /// trimmed stdout. Drivers call the emitted (mangled) names. Panics when
    /// luajit isn't on PATH: a host without it must report red for tests that
    /// never ran, never green. CRLF is normalized to `\n` because luajit's
    /// stdout is text-mode on Windows.
    fn run(src: &str, driver: &str) -> String {
        let lua = format!("{}\n{}", emit(src), driver);
        let out = std::process::Command::new("luajit")
            .arg("-e")
            .arg(&lua)
            .output()
            .expect("luajit is required for this execution test but could not be spawned");
        if !out.status.success() {
            panic!(
                "luajit failed:\n{}\n--- lua ---\n{}",
                String::from_utf8_lossy(&out.stderr),
                lua
            );
        }
        String::from_utf8_lossy(&out.stdout)
            .replace("\r\n", "\n")
            .trim()
            .to_string()
    }

    /// A flat-enum channel writes its yield leaves into reused `s.__y*`
    /// slots. The constructor's literal seeds those keys with `false` and
    /// clears them to nil, so every slot has a hash node from birth: a store
    /// to a present key never grows the table, the frame's shape never moves
    /// under a trace, and a frame that stays inside one trace can be sunk.
    /// Short-lived coroutine creation measured 222 -> 108 ns escaping and
    /// 196 -> 2 ns sinkable for it (docs/luajit-performance.md). The nil
    /// clears keep the seeds invisible: pairs and snapshots see the same
    /// frame as before.
    #[test]
    fn channel_constructor_seeds_and_clears_yield_slots() {
        let lua = emit(
            "enum Ev { Quiet, Hit(float, float), Text(string, int) }
             co fn pulse() -> Coroutine<Ev> {
                 yield Ev::Hit(1.0, 2.0)
                 yield Ev::Text(\"t\", 3)
                 yield Ev::Quiet
             }",
        );
        assert!(
            lua.contains("__y0 = false, __y1 = false, __y2 = false"),
            "expected seeded yield slots in the frame literal, got:\n{lua}"
        );
        assert!(
            lua.contains(".__y0 = nil") && lua.contains(".__y2 = nil"),
            "expected the seeds cleared to nil, got:\n{lua}"
        );
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
        // Top-level shared step fn -- NOT a per-instance `local function` closure.
        assert!(
            lua.contains("function counter_0__step("),
            "expected top-level step fn, got:\n{lua}"
        );
        assert!(
            !lua.contains("local function __step"),
            "step must be top-level, not a per-instance closure, got:\n{lua}"
        );
        // A constant-key literal, which LuaJIT builds with one TDUP from a
        // template table; see the pre-sizing entry in the rejected list of
        // docs/luajit-performance.md before changing this to assignments.
        assert!(
            lua.contains(r#"{ __ty = "Coroutine", __state = 0, __layout = "#),
            "expected the tagged+stamped frame table, got:\n{lua}"
        );
        assert!(
            lua.contains("pcall(counter_0__step,"),
            "expected name-referenced pcall, got:\n{lua}"
        );
        // State machine: writes to the state field + a completed terminal -3
        // (killed is -1, errored -2), over an ordinary CFG -- NOT the
        // emit-time goto-into-block re-entry labels.
        assert!(
            lua.contains(".__state ="),
            "expected state-saving write, got:\n{lua}"
        );
        assert_eq!(
            run(
                "co fn counter() -> Coroutine<int> {\n yield 1\n yield 2\n }",
                "local c = counter_0(); c.__resume(c); c.__resume(c); c.__resume(c); print(c.__state)",
            ),
            "-3"
        );
        assert!(
            !lua.contains("__co_resume_"),
            "transform must NOT use emit-time re-entry labels, got:\n{lua}"
        );
    }

    /// Body slice of a single emitted function -- from its `function NAME()`
    /// header to the next top-level `function ` (or end). Lets a codegen
    /// assertion scope to one fn and ignore the always-emitted value-struct
    /// `copy = function(v) return V2_ct(...) end` metatype helpers.
    fn fn_body<'a>(lua: &'a str, name: &str) -> &'a str {
        // Every non-exempt fn emits under its mangled name (`mk`), so
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
                (
                    at,
                    lua[at..].find(inner).expect("a factory returns its body") + inner.len(),
                )
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

    /// Scalar-ABI boundaries that must not build cdata or dispatch generically.
    /// Each row lists functions whose bodies must not contain a token:
    /// - a defaulted flat-value param splats its fields at the call site and
    ///   default-fills per-field scalars (it once packed `V2_ct(...)` for the
    ///   single defaulted slot);
    /// - a NON-flat value (`Rect` of `V2`s) builds, returns and threads
    ///   flattened leaves (it once returned `Rect_ct(V2_ct(...), V2_ct(...))`);
    /// - `==`/`!=` on a payloadless enum is native Lua equality, not the
    ///   trace-hostile `__op_eq`/`__op_ne` dispatch.
    #[test]
    fn scalar_abi_boundaries_build_no_cdata() {
        let cases: &[(&str, &str, &[(&str, &str)])] = &[
            (
                "defaulted value param",
                "value V2 { x: float, y: float }\n\
                 fn sink(base: V2, off: V2 = V2 { x: 1.0, y: 2.0 }) -> float { off.y }\n\
                 fn provided() -> float { sink(V2 { x: 3.0, y: 4.0 }, V2 { x: 5.0, y: 6.0 }) }\n\
                 fn omitted() -> float { sink(V2 { x: 3.0, y: 4.0 }) }",
                &[("provided", "_ct("), ("sink", "_ct("), ("omitted", "_ct(")],
            ),
            (
                "nested value",
                "value V2 { x: float, y: float }\n\
                 value Rect { tl: V2, br: V2 }\n\
                 fn mk(a: float, b: float, c: float, d: float) -> Rect { Rect { tl: V2 { x: a, y: b }, br: V2 { x: c, y: d } } }\n\
                 fn br_x(r: Rect) -> float { r.br.x }\n\
                 fn drive() -> float { br_x(mk(1.0, 2.0, 3.0, 4.0)) }",
                &[("mk", "_ct("), ("drive", "_ct(")],
            ),
            (
                "payloadless enum equality",
                "enum E { Ground, Wall }\n\
                 fn is_wall(t: E) -> bool { t == E::Wall }\n\
                 fn not_wall(t: E) -> bool { t != E::Wall }",
                &[("is_wall", "__op_eq"), ("not_wall", "__op_ne")],
            ),
        ];
        for (label, src, checks) in cases {
            let lua = emit(src);
            for (name, token) in *checks {
                let body = fn_body(&lua, name);
                assert!(
                    !body.contains(token),
                    "{label}: {name}() must not contain `{token}`; got:\n{body}"
                );
            }
        }
    }

    /// `@inline` splices: a scalarized value-struct ctor in a hot loop (so the
    /// loop traces with no per-iteration call frame), a multi-block callee
    /// spliced through the general CFG path, and a transitive chain re-scanned
    /// after each splice. A surviving call names the callee symbol at the call
    /// site or in the caller's bound-callee factory, so each spliced callee's
    /// symbol must occur once: at its own definition. The driver proves the
    /// spliced code computes the same result.
    #[test]
    fn inline_splices_callees_and_keeps_results() {
        let cases: &[(&str, &str, &[&str], &str, &str)] = &[
            (
                "value-struct ctor in a loop",
                "value IVec2 { x: int, y: int }\n\
                 @inline\n\
                 fn iv(a: int, b: int) -> IVec2 { IVec2 { x: a, y: b } }\n\
                 fn run() -> int {\n\
                     let s = 0\n\
                     for i in 0..4 {\n\
                         let c = iv(i, 100)\n\
                         s = c.y\n\
                     }\n\
                     s\n\
                 }",
                &["iv_int_int"],
                "print(run_0())",
                "100",
            ),
            (
                "multi-block callee",
                "@inline\n\
                 fn pick(c: bool) -> int { if c { 10 } else { 20 } }\n\
                 fn run(c: bool) -> int {\n\
                     let s = pick(c)\n\
                     s\n\
                 }",
                &["pick_bool"],
                "print(run_bool(true)); print(run_bool(false))",
                "10\n20",
            ),
            (
                "transitive chain",
                "@inline\n\
                 fn c() -> int { 3 }\n\
                 @inline\n\
                 fn b() -> int { c() }\n\
                 fn a() -> int { b() }",
                &["b_0", "c_0"],
                "print(a_0())",
                "3",
            ),
        ];
        for (label, src, callees, driver, expected) in cases {
            let lua = emit(src);
            for callee in *callees {
                assert_eq!(
                    lua.matches(callee).count(),
                    1,
                    "{label}: `{callee}` must be referenced only by its definition; got:\n{lua}"
                );
            }
            assert_eq!(run(src, driver), *expected, "{label}");
        }
    }

    /// A self-recursive callee is never an inline candidate (cloning it would
    /// not terminate), so its body (the literal 7) must not appear in the caller.
    #[test]
    fn recursive_callee_not_inlined() {
        let src = "@inline\n\
                   fn r(stop: bool) -> int { if stop { 7 } else { r(true) } }\n\
                   fn caller() -> int { r(false) }";
        let lua = emit(src);
        let body = fn_body(&lua, "caller_0");
        assert!(
            !body.contains("(7)"),
            "a self-recursive callee must stay a real call, got:\n{body}"
        );
        assert_eq!(run(src, "print(caller_0())"), "7");
    }

    /// The relooper recovers the canonical counted-loop CFG (preheader store,
    /// `i < hi` header, pure-increment latch) as a numeric `for c<slot> = ...`
    /// so LuaJIT specializes the induction variable (FORL) instead of unrolling
    /// a goto loop. A loop that reassigns its own counter must stay a goto loop:
    /// a synthesized `for` would ignore `i = n`, run 6 times and return 5
    /// instead of 0. Each program is a single fn, so the module-wide `for c`
    /// count is that fn's.
    #[test]
    fn counted_loops_become_numeric_for_only_when_sound() {
        let cases: &[(&str, &str, usize, &str, &str)] = &[
            (
                "range loop",
                "fn sum(n: int) -> int {\n\
                 let last = 0\n\
                 for i in 0..n { last = i }\n\
                 last\n\
                 }",
                1,
                "print(sum_int(5))",
                "4",
            ),
            (
                "nested range loops",
                "fn grid(w: int, h: int) -> int {\n\
                 let last = 0\n\
                 for y in 0..h { for x in 0..w { last = x } }\n\
                 last\n\
                 }",
                2,
                "print(grid_int_int(5, 3))",
                "4",
            ),
            (
                "counter reassigned in the body",
                "fn f(n: int) -> int {\n\
                 let last = 99\n\
                 for i in 0..6 { last = i\n i = n }\n\
                 last\n\
                 }",
                0,
                "print(f_int(10))",
                "0",
            ),
        ];
        for (label, src, fors, driver, expected) in cases {
            let lua = emit(src);
            assert_eq!(
                lua.matches("for c").count(),
                *fors,
                "{label}: numeric for-loops; got:\n{lua}"
            );
            assert_eq!(run(src, driver), *expected, "{label}");
        }
    }
}
