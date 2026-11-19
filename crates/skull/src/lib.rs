//! The default code-generation backend over `bone`'s typed HIR.

pub mod allocation_report;
pub(crate) mod coroutine_emit;
pub mod mir_emit;
pub mod shader_artifact;

/// Whether emitted Lua carries LuaJIT line directives (`ROCK_SKULL_LINE_DIRECTIVES`).
pub use skull_codegen_lua::line_directives_mode;

pub use skull_graph::{artifact, concrete_body, instance_graph, shader, target_projection};
pub mod artifact_fingerprint;
pub mod pipeline;

use bone_core::modules::display_name;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use skull_mir::lua_nesting::{LuaDisplayPart, display_expr, display_record, display_return};
use skull_parser::CompilerError;

/// Per-nominal-type layout hash, keyed by type name (unique workspace-wide).
pub type ShapeHashes = rustc_hash::FxHashMap<ustr::Ustr, u64>;

/// Hash type layouts to detect reloads that cannot preserve live values.
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

/// The type names whose shape hash changed, was added, or was removed.
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
/// A `@test` function: its logical `name` for reports and the `symbol` the artifact defines it under.
#[derive(Clone, Debug)]
pub struct SkullTest {
    pub name: String,
    pub symbol: String,
    pub output: Option<String>,
    /// `@test(returns = <literal>)`, compared host-side.
    pub returns: Option<ValueExpect>,
    /// `@test(backends = "...")`; `None` means every backend.
    pub backends: Option<Vec<bone_core::annotations::TestBackend>>,
}

impl SkullTest {
    pub fn runs_on(&self, backend: bone_core::annotations::TestBackend) -> bool {
        self.backends
            .as_ref()
            .map_or(true, |list| list.contains(&backend))
    }
}

/// The pinned literal of a `@test(returns = ...)` value test.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ValueExpect {
    Int(i64),
    Float(f64),
    Bool(bool),
}

/// Unescape a `@test(output = "...")` string: a literal `\n` becomes a newline.
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

/// The `@test` functions in `mid`; `Err` for a malformed `@test` annotation.
pub fn module_tests(ws: &Workspace, mid: ModuleId) -> Result<Vec<SkullTest>, CompilerError> {
    Ok(module_tests_with_ids(ws, mid)?
        .into_iter()
        .map(|(_, t)| t)
        .collect())
}

/// [`module_tests`] carrying each test's `Idx<Function>` for backend emission.
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
                name: display_name(f.name.as_str()).to_string(),
                symbol: skull_mir::mangle::mangle_fn(ws, f),
                output,
                returns,
                backends,
            },
        ));
    }
    Ok(out)
}

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

/// The Lua argument list for one `@deco` application.
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

fn runtime_prelude() -> &'static str {
    // LuaJIT can fail to place a second nearby mcode block on arm64. The first block
    // holds the full budget. Higher exit and trace limits reduce repeated recording
    // and global trace flushes. The environment overrides support tuning.
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
-- Only coroutines with terminal cleanup protect their step. On failure,
-- feed their pending cleanup into the ordinary function defer unwinder.
-- Keep cleanup closures off the successful resume path: UCLO aborts traces.
local function __rock_coroutine_failed(s, val)
  local base = __rock_defer_top
  if s.__state == -1 then return __rock_defer_finish(base, false, val) end
  s.__state = -1
  if s.__on_killed then
    __rock_defer_push(function() s.__on_killed(s) end)
  end
  if s.__defers then
    for i = 1, #s.__defers do
      local d = s.__defers[i]
      __rock_defer_push(function() d(s) end)
    end
  end
  local inner = s.__inner
  s.__inner = nil
  if inner then __rock_defer_push(function() __rock_coroutine_kill(inner) end) end
  return __rock_defer_finish(base, false, val)
end
function __rock_coroutine_resume(f, s, arg)
  local ok, val = xpcall(f, __rock_defer_mark, s, arg)
  if ok then return val end
  return __rock_coroutine_failed(s, val)
end
function __rock_coroutine_fire_hot_reload(c)
  while c and c.__state >= 0 do
    if c.__on_hot_reload then c.__on_hot_reload(c) end
    if c.__on_owned_hot_reload then c.__on_owned_hot_reload(c) end
    c = c.__inner
  end
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

/// A value-struct field's C type for `ffi.cdef`, or `None` when not C-mappable.
fn c_field_type(ws: &Workspace, ty: bone_semantic::prelude::TyId) -> Option<String> {
    skull_mir::lower::value_field_c_type(ws, ty)
}

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

/// Deduped, name-sorted `value` structs.
fn canonical_value_structs(ws: &Workspace) -> Vec<&bone_semantic::hir::StructDef> {
    let mut v: Vec<_> = ws
        .struct_by_name
        .values()
        .filter_map(|sid| {
            let s = &ws.modules.get(sid.module.index())?.structs[sid.def];
            skull_mir::lower::has_value_plan(s).then_some(s)
        })
        .collect();
    v.sort_by(|a, b| display_name(a.name.as_str()).cmp(display_name(b.name.as_str())));
    v
}

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

/// Emit C declarations in dependency order and report which types use GC tables.
fn value_struct_defs(
    ws: &Workspace,
    vstructs: &[&bone_semantic::hir::StructDef],
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
    let mut emitted: rustc_hash::FxHashSet<ustr::Ustr> = Default::default();
    let mut gc: rustc_hash::FxHashSet<ustr::Ustr> = Default::default();
    if vstructs.is_empty() {
        return Ok((String::new(), emitted, gc));
    }

    let mut ffi_structs = Vec::new();
    for &s in vstructs {
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
                let ct = c_field_type(ws, f.storage_ty).expect(
                    "FFI-clean value-struct field must be C-mappable (CData storage invariant)",
                );
                let dep = ustr::Ustr::from(ct.as_str());
                if ffi_names.contains(&dep) && !emitted.contains(&dep) {
                    blocked = true;
                    continue;
                }
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
                // LuaJIT has no brace elision, so a nested field cannot be passed as
                // loose scalars to `_ct`; allocate and write leaves instead.
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
    let (checks, storage) = value_leaf_layout_checks(ws, &emitted, rep_arena)?;
    Ok((
        format!("ffi.cdef[[\n{}]]\n{}{}{}", typedefs, ctors, checks, storage),
        emitted,
        gc,
    ))
}

/// The load-time layout check: `leaves` is `path offset` pairs and `nested`
/// maps each nested field path to its C type.
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

/// Check the compiler offsets against LuaJIT and register buffer layouts.
fn value_leaf_layout_checks(
    ws: &Workspace,
    emitted: &rustc_hash::FxHashSet<ustr::Ustr>,
    rep_arena: &skull_mir::lower::RepArena,
) -> Result<(String, String), CompilerError> {
    use std::fmt::Write as _;
    let mut names: Vec<ustr::Ustr> = emitted.iter().copied().collect();
    names.sort_by(|a, b| display_name(a.as_str()).cmp(display_name(b.as_str())));
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

/// Emit constructors and metamethods for GC-backed value structs.
fn value_struct_gc_defs(
    ws: &Workspace,
    vstructs: &[&bone_semantic::hir::StructDef],
    gc: &rustc_hash::FxHashSet<ustr::Ustr>,
    rep_arena: &skull_mir::lower::RepArena,
    value_ctors: &mut ValueCtors,
) -> Result<String, CompilerError> {
    let mut out = String::new();
    for s in vstructs {
        if !gc.contains(&s.name) {
            continue;
        }
        let n = s.name;
        let sym = skull_mir::mangle::type_symbol(ws, n);
        let shown = display_name(n.as_str());
        let mt = skull_codegen_lua::value_struct_metatable_name(sym.as_str());
        out.push_str(&format!("{mt} = {mt} or {{}}\n"));
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
                    LuaDisplayPart::literal(&format!("{shown} {{ ")),
                    LuaDisplayPart::literal(""),
                    LuaDisplayPart::literal(" }"),
                ]
            } else {
                display_record(
                    &format!("{shown} {{ "),
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
            // An absent tag canonicalizes to `nil`: the scalar ABI spells absence
            // `false`, and `__eq` is a per-key compare where `false ~= nil`.
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
        let fields = skull_mir::lower::gc_table_fields(ws, bone_semantic::hir::FnBackend::Lua, n)
            .map_err(|e| CompilerError::new_no_location(e.message))?;
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
        out.push_str(&format!("{mt}.__tostring = function(v)\n"));
        if fields.is_empty() {
            out.push_str(&format!("  return \"{shown} {{ }}\"\n"));
        } else {
            let parts = display_record(
                &format!("{shown} {{ "),
                fields
                    .iter()
                    .map(|f| (format!("{}: ", f.name), f.lua_display_parts())),
                " }",
            );
            out.push_str(&display_return(&parts, "  "));
        }
        out.push_str("end\n");
        let mut params = Vec::new();
        let mut body = String::new();
        for f in &fields {
            for (key, presence) in f.keys() {
                let param = if f.is_inline() {
                    format!("__rock_l{}", params.len())
                } else {
                    key.as_str().to_string()
                };
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

/// The `ffi.cdef` typedef and `<Enum>_ct` ctor for every cdata-eligible enum.
fn enum_cdata_defs(ws: &Workspace) -> String {
    let mut names: Vec<ustr::Ustr> = ws.enums_by_name.keys().copied().collect();
    names.sort_by(|a, b| display_name(a.as_str()).cmp(display_name(b.as_str())));
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

/// Attach an `ffi.metatype` to each cdata-eligible enum ctype: without
/// `__eq`, LuaJIT compares struct cdata by identity.
fn enum_cdata_metatypes(ws: &Workspace) -> String {
    let mut names: Vec<ustr::Ustr> = ws.enums_by_name.keys().copied().collect();
    names.sort_by(|a, b| display_name(a.as_str()).cmp(display_name(b.as_str())));
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
        let mut ts = String::from("  __tostring = function(v)\n    local t = v.__tag\n");
        if let Some(eid) = ws.enums_by_name.get(&name) {
            use bone_semantic::hir::VariantPayload;
            let ed = &ws.modules[eid.module.index()].enums[eid.def];
            for (i, var) in ed.variants.iter().enumerate() {
                let vn = var.name;
                let shown = display_name(name.as_str());
                let open = format!("{shown}::{vn} {{ ");
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
                    _ => vec![LuaDisplayPart::literal(&format!("{shown}::{vn}"))], // unit (or empty tuple/record)
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
        push_cdata_metatype_trampoline(&mut out, &sym);
    }
    out
}

fn push_cdata_metatype_trampoline(out: &mut String, sym: &str) {
    out.push_str(&format!(
            "pcall(ffi.metatype, \"{sym}\", {{\n\
             \x20 __eq = function(a, b) return _G.__ROCK_C_METATYPE[\"{sym}\"].__eq(a, b) end,\n\
             \x20 __tostring = function(v) return _G.__ROCK_C_METATYPE[\"{sym}\"].__tostring(v) end,\n\
             }})\n"
        ));
}

/// Attach an `ffi.metatype` to each value-struct ctype. Metatypes seal once,
/// so a trampoline delegates to the mutable `_G.__ROCK_C_METATYPE[name]`
/// impl table, which a reload can update.
fn value_struct_metatypes(
    ws: &Workspace,
    vstructs: &[&bone_semantic::hir::StructDef],
    emitted: &rustc_hash::FxHashSet<ustr::Ustr>,
) -> String {
    let mut out = String::new();
    let mut any = false;
    {
        for s in vstructs {
            if !emitted.contains(&s.name) {
                continue;
            }
            if !any {
                out.push_str("_G.__ROCK_C_METATYPE = _G.__ROCK_C_METATYPE or {}\n");
                any = true;
            }
            let n = s.name;
            let sym = skull_mir::mangle::type_symbol(ws, n);
            let shown = display_name(n.as_str());
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
                out.push_str(&format!("    return \"{shown} {{ }}\"\n"));
            } else {
                let parts = display_record(
                    &format!("{shown} {{ "),
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
            push_cdata_metatype_trampoline(&mut out, &sym);
        }
    }
    out
}

pub(crate) type ValueCtors = Vec<(ustr::Ustr, String)>;

const VALUE_CTOR_SUFFIXES: [&str; 2] = ["_lct", "_vt"];
const VALUE_CTOR_CALLS: [&str; 2] = ["_lct(", "_vt("];

pub(crate) fn is_value_ctor_global(name: &str) -> bool {
    VALUE_CTOR_SUFFIXES.iter().any(|s| name.ends_with(s))
}

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

/// The ordinal -> display-name tables of the payload-free enums in `read`.
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
            let name = skull_mir::mangle::enum_names_global(ws, e.name);
            if !read.contains(&name) {
                continue;
            }
            let items: Vec<String> = e
                .variants
                .iter()
                .enumerate()
                .map(|(ord, v)| {
                    format!(
                        "[{ord}] = \"{e}::{v}\"",
                        e = display_name(e.name.as_str()),
                        v = v.name
                    )
                })
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
            } else {
                let mut units = Vec::new();
                for v in e.variants.iter() {
                    if matches!(v.payload, VariantPayload::Unit) {
                        let sym = skull_mir::mangle::type_symbol(ws, e.name);
                        let g = skull_mir::mangle::enum_singleton_global(ws, e.name, v.name);
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

/// Register inspection and copy functions for C-backed values.
fn c_struct_inspect_defs(
    ws: &Workspace,
    vstructs: &[&bone_semantic::hir::StructDef],
    emitted: &rustc_hash::FxHashSet<ustr::Ustr>,
) -> String {
    let mut out = String::new();
    {
        let mut enums: Vec<ustr::Ustr> = ws.enums_by_name.keys().copied().collect();
        enums.sort_by(|a, b| display_name(a.as_str()).cmp(display_name(b.as_str())));
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
        for s in vstructs {
            if !emitted.contains(&s.name) {
                continue;
            }
            let n = s.name;
            let sym = skull_mir::mangle::type_symbol(ws, n);
            let n = display_name(n.as_str());
            let mut fields: Vec<_> = s.fields.iter().collect();
            fields.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
            out.push_str(&format!(
                "_G.__ROCK_C_STRUCT[\"{sym}\"] = {{ inspect = function(v, indent, path, visited, precision)\n"
            ));
            if fields.is_empty() {
                out.push_str(&format!("  return \"{n} {{  }}\"\n"));
            } else if fields.len() <= 2 {
                let parts = display_record(
                    &format!("{n} {{ "),
                    fields.iter().map(|f| {
                        (
                            format!("{} = ", f.name),
                            vec![LuaDisplayPart::Expr(format!(
                                "__rock_inspect_value(v.{0}, indent, path .. \".{0}\", visited, precision)",
                                f.name
                            ))],
                        )
                    }),
                    " }",
                );
                out.push_str(&display_return(&parts, "  "));
            } else {
                out.push_str("  local child_indent = indent .. \"  \"\n");
                let mut parts = vec![LuaDisplayPart::literal(&format!("{n} {{\\n"))];
                for (i, f) in fields.iter().enumerate() {
                    if i > 0 {
                        parts.push(LuaDisplayPart::literal(",\\n"));
                    }
                    parts.push(LuaDisplayPart::Expr("indent".to_string()));
                    parts.push(LuaDisplayPart::literal(&format!("  {} = ", f.name)));
                    parts.push(LuaDisplayPart::Expr(format!(
                        "__rock_inspect_value(v.{0}, child_indent, path .. \".{0}\", visited, precision)",
                        f.name
                    )));
                }
                parts.push(LuaDisplayPart::literal("\\n"));
                parts.push(LuaDisplayPart::Expr("indent".to_string()));
                parts.push(LuaDisplayPart::literal("}"));
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

/// Register every struct and newtype into `_G.__ROCK_TYPE_TABLE`.
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
    /// Stdlib-free emission; programs that need the stdlib use [`crate::pipeline`].
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

    /// The hot trace working set needs one 64 MB block on either architecture.
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

    /// The shape hash moves iff the runtime layout moves.
    #[test]
    fn type_shape_hashes_track_layout_not_bodies() {
        let hashes = |src: &str| {
            let module = skull_parser::parse_module(src, ustr::Ustr::from("<skull>"))
                .unwrap_or_else(|e| panic!("parse failed: {}", e.message));
            let ws = bone_semantic::semantic_passes::semantic_pass(&mut [module])
                .unwrap_or_else(|e| panic!("semantic_pass failed: {}", e.message));
            super::type_shape_hashes(&ws)
                .expect("shape planning must succeed")
                .into_iter()
                .map(|(name, hash)| (ustr::Ustr::from(super::display_name(name.as_str())), hash))
                .collect::<super::ShapeHashes>()
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

    /// `embed` moves the parent's layout: the child's leaves are its keys.
    #[test]
    fn embed_drifts_the_parent_shape() {
        let hashes = |src: &str| {
            let module = skull_parser::parse_module(src, ustr::Ustr::from("<skull>"))
                .unwrap_or_else(|e| panic!("parse failed: {}", e.message));
            let ws = bone_semantic::semantic_passes::semantic_pass(&mut [module])
                .unwrap_or_else(|e| panic!("semantic_pass failed: {}", e.message));
            super::type_shape_hashes(&ws)
                .expect("shape planning must succeed")
                .into_iter()
                .map(|(name, hash)| (ustr::Ustr::from(super::display_name(name.as_str())), hash))
                .collect::<super::ShapeHashes>()
        };

        let boxed = "struct Pt { x: int }\nstruct Holder { p: Pt, tag: int }\n";
        let embedded = "struct Pt { x: int }\nstruct Holder { embed p: Pt, tag: int }\n";
        assert_eq!(
            super::shape_diff(&hashes(boxed), &hashes(embedded)),
            vec!["Holder"],
            "flipping a field to `embed` relayouts the parent"
        );

        let embedded_wide =
            "struct Pt { x: int, y: int }\nstruct Holder { embed p: Pt, tag: int }\n";
        assert_eq!(
            super::shape_diff(&hashes(embedded), &hashes(embedded_wide)),
            vec!["Holder", "Pt"],
            "a field added to an EMBEDDED child also relayouts the parent"
        );

        let boxed_wide = "struct Pt { x: int, y: int }\nstruct Holder { p: Pt, tag: int }\n";
        assert_eq!(
            super::shape_diff(&hashes(boxed), &hashes(boxed_wide)),
            vec!["Pt"],
            "a field added to a BOXED child must NOT drift the parent"
        );

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

    /// Emit Lua, append `driver`, run under luajit and return trimmed stdout.
    /// CRLF is normalized because luajit's stdout is text-mode on Windows.
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

    /// Flat-enum yield slots are seeded in the constructor literal so the frame shape never moves under a trace.
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
        assert!(
            lua.contains("function counter_0__step("),
            "expected top-level step fn, got:\n{lua}"
        );
        assert!(
            !lua.contains("local function __step"),
            "step must be top-level, not a per-instance closure, got:\n{lua}"
        );
        // A constant-key literal, which LuaJIT builds with one TDUP from a
        assert!(
            lua.contains(r#"{ __ty = "Coroutine", __state = 0, __layout = "#),
            "expected the tagged+stamped frame table, got:\n{lua}"
        );
        assert!(
            lua.contains("return counter_0__step(s, arg)"),
            "expected a direct step call, got:\n{lua}"
        );
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

    /// Body slice of a single emitted function.
    fn fn_body<'a>(lua: &'a str, name: &str) -> &'a str {
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

    /// `@inline` splices leave each spliced callee's symbol only at its definition.
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

    /// A self-recursive callee is never an inline candidate.
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

    /// The relooper recovers counted loops as numeric `for`, but not a loop that reassigns its counter.
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
