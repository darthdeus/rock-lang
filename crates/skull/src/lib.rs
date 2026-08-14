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

pub mod coroutine_emit;
pub mod inline_stats;
pub mod graph_snapshot;
pub mod shader_artifact;
pub mod site_join;
pub mod mir_emit;

// The shared graph layer, re-exported under its original paths. It moved BELOW
// the backends (`skull-graph`) so the JS and GLSL emitters can realize the same
// graph the luajit driver does; consumers name it the same way either side of
// that move.
pub use skull_graph::{artifact, concrete_body, expr_subtree, instance_graph, shader, target_projection};

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
pub fn type_shape_hashes(ws: &Workspace) -> ShapeHashes {
    use bone_semantic::hir::VariantPayload;
    use std::hash::{Hash, Hasher};

    let mut out = ShapeHashes::default();
    for module in ws.modules.iter() {
        for (_, s) in module.structs.iter() {
            out.insert(s.name, struct_shape_hash(ws, s, &mut Vec::new()));
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
    out
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
/// fired. `is_embed` is hashed explicitly anyway -- the flag is part of the
/// layout in its own right, and that should not depend on how the child
/// happens to contribute.
///
/// `visiting` breaks a cycle with a stable sentinel. bone rejects a cyclic
/// embed graph (`check_embed_site_legality`), so this is unreachable by
/// construction; it exists so a malformed workspace reaching codegen produces a
/// wrong-but-finite hash instead of hanging the compiler.
fn struct_shape_hash(
    ws: &Workspace,
    s: &bone_semantic::hir::StructDef,
    visiting: &mut Vec<ustr::Ustr>,
) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut h = rustc_hash::FxHasher::default();
    "struct".hash(&mut h);
    s.is_value.hash(&mut h);
    s.is_nobox.hash(&mut h);
    for f in s.fields.iter() {
        f.name.as_str().hash(&mut h);
        f.is_embed.hash(&mut h);
        ws.render(None, f.ty).hash(&mut h);
        let child = nominal_field_type(ws, f.ty)
            .and_then(|n| ws.struct_by_name.get(&ustr::Ustr::from(&n)).copied());
        let flattened = f.is_embed
            || child.is_some_and(|sid| ws.modules[sid.module.index()].structs[sid.def].is_nobox);
        if !flattened {
            continue;
        }
        match child {
            Some(sid) if !visiting.contains(&s.name) => {
                let child = &ws.modules[sid.module.index()].structs[sid.def];
                visiting.push(s.name);
                struct_shape_hash(ws, child, visiting).hash(&mut h);
                visiting.pop();
            }
            _ => "<embed-unresolved>".hash(&mut h),
        }
    }
    h.finish()
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
/// A `@test` function discovered in a module: the Lua name to call (a `@test`
/// fn is never mangled) and the expected stdout of `@test(output = "...")`.
#[derive(Clone, Debug)]
pub struct SkullTest {
    pub name: String,
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
        let Some(test_ann) = f.annotations.iter().find(|a| a.name.name.as_str() == "test") else {
            continue;
        };
        // Honor `@ignore` (same convention as rock-cli/src/test.rs): a test
        // marked ignored is not run.
        if f.annotations.iter().any(|a| a.name.name.as_str() == "ignore") {
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
    format!("__deco_{}", deco)
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
    // Mirrors rock-codegen's assembler init (`rock-codegen/src/backend/lua/
    // assembler.rs`) -- skull is an independent backend emitting for the same
    // LuaJIT runtime, so it needs the same runtime setup. Two parts:
    //   1. LuaJIT mcode/trace tuning. skull (like rock-codegen) emits more Lua
    //      than LuaJIT's small-script defaults assume (32 KB mcode blocks /
    //      512 KB total / 1000 traces). A real game's hot traces overflow the
    //      mcode arena -> "failed to allocate mcode memory" -> a GLOBAL trace
    //      flush wipes every good trace -> the hot loop drops to the interpreter
    //      and re-traces on a cliff/spiral (mag floors_walls: ~2.6 ms vs ~0.5
    //      with this). `sizemcode=512` is the load-bearing knob -- a trace's
    //      machine code must fit ONE block, and the big inlined vertex-fill
    //      traces assemble to >64 KB. Values kept IDENTICAL to assembler.rs
    //      (full diagnosis there + docs/jit-fnew-codegen-investigation.md and
    //      docs/skull-throughput-investigation.md); keep in sync -- shared-home
    //      follow-up logged in docs/unfuck-backlog.md.
    //   2. splitmix64 RNG (stdlib `random_*` inline(lua) bodies call
    //      `__rock_rng_next`/`__rock_rng_seed` by name); seeded from
    //      ROCK_TEST_SEED for deterministic test runs.
    // The `{FN_MT}` placeholder is filled once from the canonical
    // `skull_codegen_lua::FN_MT_PRELUDE` constant (one definition, shared with the
    // codegen test harness) -- the record metatable is where the emitter's
    // `setmetatable({..}, __rock_fn_mt)` output lands.
    static PRELUDE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let out = PRELUDE_TEMPLATE
            .replace("{FN_MT}", skull_codegen_lua::FN_MT_PRELUDE)
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

const PRELUDE_TEMPLATE: &str =
    r#"if jit and jit.opt then jit.opt.start("maxmcode=8192", "sizemcode=512", "maxtrace=4000", "maxrecord=32000", "maxsnap=4000") end
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
-- THE way to read/write a global found by name in `__ROCK_GLOBALS`. A `nobox`
-- global has no slot under its own name -- its storage is its leaf globals -- so
-- a bare `_G[name]` would read nil and write a slot nothing else reads. The
-- rebuild/take-apart pair lives in `__ROCK_NOBOX_GLOBALS` (emitted per nobox
-- global); every other global falls through to `_G` unchanged.
_G.__rock_global_get = function(name)
  local a = _G.__ROCK_NOBOX_GLOBALS and _G.__ROCK_NOBOX_GLOBALS[name]
  if a then return a.get() end
  return _G[name]
end
_G.__rock_global_set = function(name, v)
  local a = _G.__ROCK_NOBOX_GLOBALS and _G.__ROCK_NOBOX_GLOBALS[name]
  if a then a.set(v) else _G[name] = v end
end
-- Shared metatable of capturing-lambda env records (docs/closure-env-records-plan.md):
-- a record { __fn = <top-level fn>, <captures> } is callable as f(...) via __call.
-- Preserved across reload so pre- and post-reload records share metatable identity.
{FN_MT}
if not _G.__rock_rng_next then
  local __rock_bit = require("bit")
  local __rock_rng_state = _G.ffi.new("uint64_t", 0)
  _G.__rock_rng_seed = function(s) __rock_rng_state = _G.ffi.new("uint64_t", s) end
  -- Whole RNG state is this one counter; get/set serialize it for replay
  -- keyframe restore (docs/replay-scrub-plan.md). Returns the uint64 cdata.
  _G.__rock_rng_state_get = function() return __rock_rng_state end
  _G.__rock_rng_state_set = function(s) __rock_rng_state = _G.ffi.new("uint64_t", s) end
  _G.__rock_rng_next = function()
    __rock_rng_state = __rock_rng_state + 0x9E3779B97F4A7C15ULL
    local z = __rock_rng_state
    z = __rock_bit.bxor(z, __rock_bit.rshift(z, 30)) * 0xBF58476D1CE4E5B9ULL
    z = __rock_bit.bxor(z, __rock_bit.rshift(z, 27)) * 0x94D049BB133111EBULL
    z = __rock_bit.bxor(z, __rock_bit.rshift(z, 31))
    return z
  end
  local __seed = tonumber(os.getenv("ROCK_TEST_SEED"))
  if __seed then _G.__rock_rng_seed(__seed)
  else _G.__rock_rng_seed(os.time()) end
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
) -> Result<(String, rustc_hash::FxHashSet<ustr::Ustr>, rustc_hash::FxHashSet<ustr::Ustr>), CompilerError>
{
    let vstructs = canonical_value_structs(ws);
    let rep_arena = skull_mir::lower::RepArena::default();
    let mut emitted: rustc_hash::FxHashSet<ustr::Ustr> = Default::default();
    let mut gc: rustc_hash::FxHashSet<ustr::Ustr> = Default::default();
    if vstructs.is_empty() {
        return Ok((String::new(), emitted, gc));
    }

    // Partition by tier. GC-carrying value structs materialize as Lua tables
    // (`_vt`), never as cdef, so only FFI-clean structs enter the ordering.
    let mut ffi_structs = Vec::new();
    for s in vstructs {
        if !matches!(
            rep_arena.nominal_value_storage(ws, bone_semantic::hir::FnBackend::Lua, s.name),
            Some(skull_mir::lower::ValueStorage::CData)
        ) {
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
                body.push_str(&format!("  {} {};\n", ct, f.name));
            }
            if blocked {
                next.push(s);
            } else {
                typedefs.push_str(&format!("typedef struct {{\n{}}} {};\n", body, s.name));
                ctors.push_str(&format!("{n}_ct = ffi.typeof(\"{n}\")\n", n = s.name));
                // Leaf-taking ctor: LuaJIT has no brace elision, so a nested
                // field cannot be passed as loose scalars to `_ct`. Allocate and
                // write leaves instead, hoisting each nested reference once.
                // Used by the normalization that keeps a `nobox` field from
                // materializing when its parent boxes.
                if let Some(paths) = rep_arena.nominal_value_leaf_paths(
                    ws,
                    bone_semantic::hir::FnBackend::Lua,
                    s.name,
                ) {
                    if paths.iter().any(|p| p.contains('.')) {
                        ctors.push_str(&leaf_ctor_def(s.name, &paths));
                    }
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
    let (checks, storage) = value_leaf_layout_checks(ws, &emitted)?;
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
) -> Result<(String, String), CompilerError> {
    use std::fmt::Write as _;
    let rep_arena = skull_mir::lower::RepArena::default();
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
        let _ = writeln!(
            out,
            "if ffi.sizeof(\"{name}\") ~= {size} then error(\"skull layout drift: \
             sizeof({name}) = \" .. ffi.sizeof(\"{name}\") .. \", expected {size}\", 0) end"
        );
        for l in &leaves {
            let _ = writeln!(
                out,
                "if ({expr}) ~= {off} then error(\"skull layout drift: {name}.{path} at \" \
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
fn value_struct_gc_defs(ws: &Workspace, gc: &rustc_hash::FxHashSet<ustr::Ustr>) -> String {
    let mut out = String::new();
    let rep_arena = skull_mir::lower::RepArena::default();
    for s in canonical_value_structs(ws) {
        if !gc.contains(&s.name) {
            continue;
        }
        let n = s.name;
        let mt = format!("__rock_vt_{n}");
        out.push_str(&format!("{mt} = {mt} or {{}}\n"));
        // A leaf-table struct has no declared-field keys to compare or print, so
        // both metamethods and its ctor run off the leaf paths instead.
        if matches!(
            rep_arena.nominal_value_storage(ws, bone_semantic::hir::FnBackend::Lua, n),
            Some(skull_mir::lower::ValueStorage::LeafTable)
        ) {
            let paths = rep_arena
                .nominal_value_leaf_paths(ws, bone_semantic::hir::FnBackend::Lua, n)
                .unwrap_or_default();
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
                .nominal_value_leaf_presence_flags(ws, bone_semantic::hir::FnBackend::Lua, n)
                .unwrap_or_default();
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
                "{n}_lct = function({p})\n  return setmetatable({{ {b}__ty = \"{n}\" }}, {mt})\nend\n",
                p = params.join(", "),
                b = assigns.concat(),
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
            "{n}_vt = function({p})\n  return setmetatable({{ {b}__ty = \"{n}\" }}, {mt})\nend\n",
            p = params.join(", "),
            b = body,
        ));
    }
    out
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
            typedefs.push_str(&format!("typedef struct {{ int __tag;{slots} }} {n};\n", n = name));
            ctors.push_str(&format!("{n}_ct = ffi.typeof(\"{n}\")\n", n = name));
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
        out.push_str(&format!("_G.__ROCK_C_METATYPE[\"{name}\"] = {{\n"));
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
            "pcall(ffi.metatype, \"{name}\", {{\n\
             \x20 __eq = function(a, b) return _G.__ROCK_C_METATYPE[\"{name}\"].__eq(a, b) end,\n\
             \x20 __tostring = function(v) return _G.__ROCK_C_METATYPE[\"{name}\"].__tostring(v) end,\n\
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
            out.push_str(&format!("_G.__ROCK_C_METATYPE[\"{n}\"] = {{\n"));
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
                "pcall(ffi.metatype, \"{n}\", {{\n\
                 \x20 __eq = function(a, b) return _G.__ROCK_C_METATYPE[\"{n}\"].__eq(a, b) end,\n\
                 \x20 __tostring = function(v) return _G.__ROCK_C_METATYPE[\"{n}\"].__tostring(v) end,\n\
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
                out.push_str(&format!("__rock_enum_{}_names = {{ {} }}\n", e.name, items.join(", ")));
            } else {
                // Mixed: unit variants are interned singleton tables.
                for v in e.variants.iter() {
                    if matches!(v.payload, VariantPayload::Unit) {
                        let g = enum_singleton_name(e.name.as_str(), v.name.as_str());
                        out.push_str(&format!(
                            "{g} = {g} or {{ __variant = \"{var}\", __ty = \"{ty}\" }}\n",
                            var = v.name,
                            ty = e.name,
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
            out.push_str(&format!(
                "_G.__ROCK_C_STRUCT[\"{name}\"] = {{ inspect = function(v)\n  return tostring(v)\n\
                 end, copy = function(v)\n  return {name}_ct({})\nend, name = \"{name}\" }}\n",
                copy_args.join(", ")
            ));
        }
        for s in canonical_value_structs(ws) {
            if !emitted.contains(&s.name) {
                continue;
            }
            let n = s.name;
            out.push_str(&format!("_G.__ROCK_C_STRUCT[\"{n}\"] = {{ inspect = function(v)\n"));
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
                "end, copy = function(v)\n  return {n}_ct({})\nend, name = \"{n}\" }}\n",
                copy_args.join(", ")
            ));
        }
    }
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
            TypeCtor::Nominal(name) => {
                // The name is the whole answer for a nominal leaf: the consumer
                // asks the ONE classifier whether it is a native class, and a
                // struct's own fields are reached through the type table's
                // per-field descriptors rather than repeated inside this one.
                out.push_str("{k=\"nom\",name=\"");
                out.push_str(name.as_str());
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
        }) => (name.as_str().to_string(), false),
        Some(TypeExpr::App {
            ctor: TypeCtor::Array,
            ..
        }) => ("[]".into(), false),
        _ => ("any".into(), false),
    }
}

/// The nominal name a field's type denotes, peeling `Nullable`. An `embed`
/// field needs this even when nullable: reflection rebuilds its logical value
/// from that type's field list, and [`reflect_field_type`] renders any nullable
/// as `any`, which would lose the name.
fn nominal_field_type(ws: &Workspace, ty: bone_semantic::prelude::TyId) -> Option<String> {
    use bone_semantic::hir::{TypeCtor, TypeExpr};
    let resolve = |t: bone_semantic::prelude::TyId| -> Option<(TypeExpr, bone_semantic::prelude::TyId)> {
        match t {
            bone_semantic::prelude::TyId::Static(mid, idx) => {
                ws.modules.get(mid.index()).map(|m| (m.ty_arena[idx], t))
            }
            bone_semantic::prelude::TyId::Local(_) => None,
        }
    };
    let (mut te, mut cur) = resolve(ty)?;
    if let TypeExpr::Nullable(inner) = te {
        let (t2, c2) = resolve(cur.with_idx(inner))?;
        te = t2;
        cur = c2;
    }
    let _ = cur;
    match te {
        TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } => Some(name.as_str().to_string()),
        _ => None,
    }
}

/// Register every struct + newtype into `_G.__ROCK_TYPE_TABLE`, so the stdlib
/// reflection fns (`get_type_info`, `type_info_of`, `type_name`) can recover a
/// value's metadata from its `__ty` tag. Mirrors
/// rock-codegen/src/codegen_pass.rs. The prelude already seeds the table empty,
/// so this only adds entries. No metatables: type tagging is the direct `__ty`.
fn type_table_defs(ws: &Workspace, gc: &rustc_hash::FxHashSet<ustr::Ustr>) -> String {
    let mut out = String::new();
    let mut type_id = 100i64;
    for module in ws.modules.iter() {
        for (_, s) in module.structs.iter() {
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
            let fields: Vec<String> = s
                .fields
                .iter()
                .map(|f| {
                    let (tystr, nullable) = reflect_field_type(ws, f.ty);
                    let child = nominal_field_type(ws, f.ty)
                        .and_then(|n| ws.struct_by_name.get(&ustr::Ustr::from(&n)).copied());
                    let flattened = f.is_embed
                        || child.is_some_and(|sid| {
                            ws.modules[sid.module.index()].structs[sid.def].is_nobox
                        });
                    // `embed` marks a field whose leaves are THIS struct's
                    // physical keys (`f.x`), so reflection has to rebuild the
                    // logical child from them -- the declared name indexes
                    // nothing (a presence slot when nullable). The child's name
                    // must survive nullability for that rebuild to be possible.
                    let tystr = match flattened.then(|| nominal_field_type(ws, f.ty)).flatten() {
                        Some(n) => n,
                        None => tystr,
                    };
                    let embed = if flattened { ", embed = true" } else { "" };
                    // The recursive shape, present only where the flat `type`
                    // above cannot express it (a container, a tuple, a nullable
                    // -- anything with a nominal inside it).
                    let desc = match field_desc_lua(ws, f.ty) {
                        Some(d) => format!(", desc = {d}"),
                        None => String::new(),
                    };
                    format!(
                        "{{ name = \"{}\", type = \"{}\", nullable = {}{}{} }}",
                        f.name, tystr, nullable, embed, desc
                    )
                })
                .collect();
            out.push_str(&format!(
                "_G.__ROCK_TYPE_TABLE[\"{n}\"] = {{ name = \"{n}\", kind = \"{k}\", type_id = {id}, fields = {{ {f} }} }}\n",
                n = s.name,
                k = kind,
                id = type_id,
                f = fields.join(", ")
            ));
            type_id += 1;
        }
        for (_, nt) in module.newtypes.iter() {
            out.push_str(&format!(
                "_G.__ROCK_TYPE_TABLE[\"{n}\"] = {{ name = \"{n}\", kind = \"newtype\", type_id = {id}, fields = {{ }} }}\n",
                n = nt.name,
                id = type_id
            ));
            type_id += 1;
        }
    }
    for module in ws.modules.iter() {
        for (_, e) in module.enums.iter() {
            use bone_semantic::hir::VariantPayload;

            let Some(shape) = skull_mir::lower::EnumPlan::for_name(ws, e.name)
                .and_then(|plan| plan.cdata().cloned())
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
                        n = e.name,
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
                n = e.name,
                id = type_id,
                f = fields.join(", "),
                v = variants.join(", ")
            ));
            type_id += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::compile_source;

    fn emit(src: &str) -> String {
        compile_source(src).unwrap_or_else(|e| panic!("skull compile failed: {}", e.message))
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
            super::type_shape_hashes(&ws)
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
            super::type_shape_hashes(&ws)
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
        // Every non-exempt fn emits under its mangled name (`mk_ret_V2`), so match
        // the `function <name>` PREFIX and scope from the header's `(` to the next
        // top-level `function `.
        let head = format!("function {name}");
        let hpos = lua.find(&head).unwrap_or_else(|| panic!("no `{head}` in:\n{lua}"));
        let rest = &lua[hpos..];
        let paren = rest.find('(').expect("fn header has `(`");
        rest[paren + 1..].split("\nfunction ").next().unwrap()
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
