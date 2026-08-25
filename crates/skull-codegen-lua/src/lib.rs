//! skull-codegen-lua — the dumb Lua printer over skull-mir's typed cell-model CFG.
//!
//! luajit has real `goto`, so the CFG maps directly: each [`Block`] is a `::bbN::`
//! label, its instructions in order, then a terminator that `goto`s its
//! successors. Two luajit rules drive the shape:
//!   1. `goto` may not jump into the scope of a local — so EVERY value local and
//!      cell local is declared at the function top, before any label.
//!   2. `return` must be the last statement of its block — so every `return` is
//!      wrapped `do return v end`, which is legal as a final statement too.
//!
//! The printer NEVER dispatches on an operand's type: the IR is already typed
//! (`IntAdd` vs `FloatAdd` vs `StringConcat`), so `bone` is consulted ONLY to
//! resolve a `Call`'s [`FuncRef`] to its mangled name (D8/P3). A JS backend can't
//! use `goto`; it'll reloop the same MIR (Phase 8).
//!
//! Naming: a value-producing inst `Idx<Inst>` renders as `v<raw>`, a cell as
//! `c<raw>`, a block as `bb<raw>` — raw arena indices, so no renumbering pass and
//! references are a direct index format. (Regalloc to fit luajit's 200-local cap
//! is a later refinement; see `docs/skull-bone-mir-faithful-plan.md` Phase 5.)

mod regalloc;

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::cell::RefCell;
use std::fmt::Write as _;
use std::sync::Mutex;

use bone_semantic::hir::Function;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::{CompilerError, Span};
use comfy_la_arena::Idx;
use skull_mir::cfg::{
    PredMap, predecessors, reachable_blocks_from, reverse_postorder, successors,
};
use skull_mir::mir::*;
use skull_mir::monoty::MonoInterner;

pub use skull_mir::mangle::mangle_fn;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EmittedAllocationKind {
    StringConcat,
    StringConversion,
    StringFormat,
    TupleConstruction,
    VecConstruction,
    VecColumnConstruction,
    VecColumnHeader,
    StructConstruction,
    ValueConstruction,
    ExplicitValueMaterialization,
    EnumConstruction,
    DynamicEnumConstruction,
    ClosureEnvironment,
    FfiArray,
    FfiView,
    ContainerGrowth,
}

impl EmittedAllocationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StringConcat => "string-concat",
            Self::StringConversion => "string-conversion",
            Self::StringFormat => "string-format",
            Self::TupleConstruction => "tuple-construction",
            Self::VecConstruction => "vec-construction",
            Self::VecColumnConstruction => "vec-column-construction",
            Self::VecColumnHeader => "vec-column-header",
            Self::StructConstruction => "struct-construction",
            Self::ValueConstruction => "value-construction",
            Self::ExplicitValueMaterialization => "explicit-value-materialization",
            Self::EnumConstruction => "enum-construction",
            Self::DynamicEnumConstruction => "dynamic-enum-construction",
            Self::ClosureEnvironment => "closure-environment",
            Self::FfiArray => "ffi-array",
            Self::FfiView => "ffi-view",
            Self::ContainerGrowth => "container-growth",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmittedAllocationSite {
    pub id: u64,
    pub kind: EmittedAllocationKind,
    pub span: Option<Span>,
    pub lua_body_line: u32,
    lua_byte_offset: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LuaEmission {
    pub lua: String,
    pub allocation_sites: Vec<EmittedAllocationSite>,
}

pub const ARTIFACT_PLAN_PRELUDE: &str = r#"
function __rock_artifact_binding_plan_stage(plan)
  if not _G.__rock_state_transaction then error("Artifact binding plan stage not open", 0) end
  if not plan or not plan.census or plan.census.examined <= 0
      or plan.census.examined ~= plan.census.assigned then
    error("incomplete Artifact binding plan", 0)
  end
  _G.__rock_artifact_binding_plan_pending = plan
end
function __rock_artifact_binding_plan_publish()
  local plan = _G.__rock_artifact_binding_plan_pending
  if plan ~= nil then
    _G.__ROCK_ARTIFACT_BINDING_PLAN = plan
    _G.__rock_artifact_binding_plan_pending = nil
  end
end
function __rock_artifact_plan_begin()
  if _G.__rock_state_transaction_begin then __rock_state_transaction_begin() end
  _G.__rock_artifact_binding_plan_pending = nil
end
function __rock_artifact_plan_abort()
  if _G.__rock_state_transaction_abort then __rock_state_transaction_abort() end
  _G.__rock_artifact_binding_plan_pending = nil
end
function __rock_artifact_plan_publish()
  __rock_artifact_binding_plan_publish()
  if _G.__rock_state_transaction_commit then __rock_state_transaction_commit() end
end
"#;

/// Runtime support for callable CodeRefs and capturing environment records.
/// Slots are mutated in place only by the publish step, so existing values see
/// a compatible body edit while a failed stage leaves the previous body live.
pub const FN_MT_PRELUDE: &str = r#"
if not _G.__rock_code_slot_mt then
  _G.__rock_code_slot_mt = { __call = function(s, ...) return s.current_body(...) end }
end
if not _G.__rock_code_slots then _G.__rock_code_slots = {} end
function __rock_code_slot_ref(key)
  local slot = _G.__rock_code_slots[key]
  if (not slot or _G.__rock_definition_plan_materializing) and _G.__rock_code_slot_pending then
    local staged = _G.__rock_code_slot_pending[key]
    if staged then
      local pending = _G.__rock_code_slot_pending_slots
      local temporary = pending[key]
      if not temporary then
        temporary = setmetatable({
          __rock_code_slot = true,
          key = key, current_body = staged.body, abi = staged.abi
        }, _G.__rock_code_slot_mt)
        pending[key] = temporary
      end
      return temporary
    end
  end
  if not slot then error("missing Rock code slot " .. key, 0) end
  return slot
end
function __rock_code_slot_pending_ref(key)
  local staged = _G.__rock_code_slot_pending and _G.__rock_code_slot_pending[key]
  if staged then
    local pending = _G.__rock_code_slot_pending_slots
    local temporary = pending[key]
    if not temporary then
      temporary = setmetatable({
        __rock_code_slot = true,
        key = key, current_body = staged.body, abi = staged.abi
      }, _G.__rock_code_slot_mt)
      pending[key] = temporary
    end
    return temporary
  end
  return __rock_code_slot_ref(key)
end
function __rock_code_slot_begin()
  if _G.__rock_state_transaction_begin then __rock_state_transaction_begin() end
  _G.__rock_code_slot_pending = {}
  _G.__rock_code_slot_pending_slots = {}
  _G.__rock_definition_plan_materializing = false
  _G.__rock_lifecycle_plan_pending = nil
  _G.__rock_definition_plan_pending = nil
  _G.__rock_artifact_binding_plan_pending = nil
end
function __rock_code_slot_abort()
  if _G.__rock_state_transaction_abort then __rock_state_transaction_abort() end
  _G.__rock_code_slot_pending = nil
  _G.__rock_code_slot_pending_slots = nil
  _G.__rock_definition_plan_materializing = false
  _G.__rock_lifecycle_plan_pending = nil
  _G.__rock_definition_plan_pending = nil
  _G.__rock_artifact_binding_plan_pending = nil
  local pending_wgpu = rawget(_G, "PENDING_WGPU_SHADER_SRCS")
  if pending_wgpu then
    for key in pairs(pending_wgpu) do pending_wgpu[key] = nil end
  end
end
function __rock_code_slot_stage(key, body, abi)
  local old = _G.__rock_code_slots[key]
  if old and old.abi ~= abi then error("incompatible Rock code slot " .. key, 0) end
  if not _G.__rock_code_slot_pending then error("Rock code slot stage not open", 0) end
  _G.__rock_code_slot_pending[key] = { body = body, abi = abi }
end
function __rock_lifecycle_plan_stage(plan)
  if not _G.__rock_code_slot_pending then error("Rock code slot stage not open", 0) end
  _G.__rock_lifecycle_plan_pending = plan
end
function __rock_definition_plan_stage(plan)
  if not _G.__rock_code_slot_pending then error("Rock code slot stage not open", 0) end
  _G.__rock_definition_plan_pending = plan
end
function __rock_definition_catalog(catalog)
  local plan = rawget(_G, "__ROCK_DEFINITION_PLAN")
  if plan == nil then error("DefinitionPlan is not published", 0) end
  local view = rawget(plan, catalog)
  if view == nil then error("DefinitionPlan catalog " .. catalog .. " is not published", 0) end
  return view
end
function __rock_lifecycle_wire_due(old_plan, candidate_plan)
  local due = {}
  local old_phase = old_plan and old_plan.wire
  local old_entries = {}
  if old_phase and old_phase._data then
    for i = 1, old_phase.len do
      local entry = old_phase._data[i]
      if entry then old_entries[entry.key] = entry end
    end
  end
  local candidate_phase = candidate_plan and candidate_plan.wire
  if not candidate_phase or not candidate_phase._data then return due end
  for i = 1, candidate_phase.len do
    local candidate = candidate_phase._data[i]
    local old = candidate and old_entries[candidate.key]
    local changed = old == nil or candidate.qualification == "plain"
    if not changed and candidate.qualification ~= old.qualification then changed = true end
    if not changed and candidate.body_fingerprint ~= old.body_fingerprint then changed = true end
    if not changed then
      local candidate_deps = candidate.dependencies
      local old_deps = old.dependencies
      if not candidate_deps or not old_deps or candidate_deps.len ~= old_deps.len then
        changed = true
      else
        for j = 1, candidate_deps.len do
          local candidate_dep = candidate_deps._data[j]
          local old_dep = old_deps._data[j]
          if not candidate_dep or not old_dep
              or candidate_dep.key ~= old_dep.key
              or candidate_dep.fingerprint ~= old_dep.fingerprint then
            changed = true
            break
          end
        end
      end
    end
    if changed then due[candidate.key] = true end
  end
  return due
end
function __rock_lifecycle_schedule_all_wires()
  _G.__ROCK_WIRE_DUE = __rock_lifecycle_wire_due(nil, _G.__ROCK_LIFECYCLE_PLAN)
end
function __rock_lifecycle_take_wire(key)
  local due = rawget(_G, "__ROCK_WIRE_DUE")
  if not due or not due[key] then return false end
  due[key] = nil
  return true
end
function __rock_definition_plan_materialize(plan)
  _G.__rock_definition_plan_materializing = true
  for _, catalog in pairs(plan) do
    if type(catalog) == "table" and catalog._data and catalog.len then
      for i = 1, catalog.len do
        local entry = catalog._data[i]
        if entry and entry.factory ~= nil then
          entry.definition = entry.factory()
        end
      end
    end
  end
  _G.__rock_definition_plan_materializing = false
  return plan
end
function __rock_code_slot_publish()
  local definition_plan = _G.__rock_definition_plan_pending
  if definition_plan ~= nil then
    definition_plan = __rock_definition_plan_materialize(definition_plan)
  end
  local candidate_lifecycle_plan = _G.__rock_lifecycle_plan_pending
  local wire_due = nil
  if candidate_lifecycle_plan ~= nil then
    wire_due = __rock_lifecycle_wire_due(_G.__ROCK_LIFECYCLE_PLAN, candidate_lifecycle_plan)
  end
  local pending_wgpu = rawget(_G, "PENDING_WGPU_SHADER_SRCS")
  if pending_wgpu and next(pending_wgpu) ~= nil then
    local register_wgpu = rawget(_G, "stone_wgpu_renderer_register_pending")
    if not register_wgpu then
      register_wgpu = rawget(_G, "stone_wgpu_renderer_register_pending_ret_bool")
    end
    if not register_wgpu or not register_wgpu() then
      error("WGPU shader registration did not complete", 0)
    end
    local publish_wgpu = rawget(_G, "stone_wgpu_renderer_publish_pending")
    if not publish_wgpu then
      publish_wgpu = rawget(_G, "stone_wgpu_renderer_publish_pending_ret_bool")
    end
    if publish_wgpu then
      if not publish_wgpu() then error("WGPU shader registration did not complete", 0) end
    else
      error("WGPU shader registration did not complete", 0)
    end
  end
  local stage = _G.__rock_code_slot_pending
  if not stage then error("Rock code slot stage not open", 0) end
  for key, staged in pairs(stage) do
    local slot = _G.__rock_code_slots[key]
    if slot then
      slot.current_body = staged.body
      slot.abi = staged.abi
      local temporary = _G.__rock_code_slot_pending_slots[key]
      if temporary then
        temporary.current_body = staged.body
        temporary.abi = staged.abi
        _G.__rock_code_slots[key] = temporary
      end
    else
      local pending = _G.__rock_code_slot_pending_slots
      _G.__rock_code_slots[key] = pending[key] or setmetatable({
          __rock_code_slot = true,
          key = key, current_body = staged.body, abi = staged.abi
        }, _G.__rock_code_slot_mt)
    end
  end
  if _G.__rock_lifecycle_plan_pending ~= nil then
    _G.__ROCK_LIFECYCLE_PLAN = _G.__rock_lifecycle_plan_pending
  end
  if definition_plan ~= nil then
    _G.__ROCK_DEFINITION_PLAN = definition_plan
  end
  if __rock_artifact_binding_plan_publish then __rock_artifact_binding_plan_publish() end
  _G.__rock_code_slot_pending = nil
  _G.__rock_code_slot_pending_slots = nil
  _G.__rock_lifecycle_plan_pending = nil
  _G.__rock_definition_plan_pending = nil
  if _G.__rock_state_transaction_commit then __rock_state_transaction_commit() end
  if wire_due ~= nil then _G.__ROCK_WIRE_DUE = wire_due end
end
if not _G.__rock_fn_mt then
  _G.__rock_fn_mt = {
    __call = function(e, ...)
      if not e.__slot then error("invalid Rock CodeRef environment", 0) end
      return e.__slot.current_body(e, ...)
    end
  }
end
function __rock_code_ref_kind(v)
  if type(v) ~= "table" then return nil end
  if rawget(v, "__rock_code_slot") == true
      and getmetatable(v) == _G.__rock_code_slot_mt then
    return "slot"
  end
  if getmetatable(v) == _G.__rock_fn_mt then
    local slot = rawget(v, "__slot")
    if slot ~= nil and __rock_code_ref_kind(slot) == "slot" then return "env" end
  end
  return nil
end
"#;

pub const ALLOCATION_ACTIVATION_PRELUDE: &str = r#"
if not _G.__rock_alloc_hits then _G.__rock_alloc_hits = {} end
function __rock_alloc_hit(id, value)
  local hits = _G.__rock_alloc_hits
  hits[id] = (hits[id] or 0) + 1
  return value
end
function __rock_alloc_hit_effect(id)
  local hits = _G.__rock_alloc_hits
  hits[id] = (hits[id] or 0) + 1
end
function __rock_allocation_activation_begin()
  _G.__rock_alloc_hits = {}
end
function __rock_allocation_activation_dump(frames)
  local path = os.getenv("SKULL_ALLOCATION_ACTIVATION")
  if not path or path == "" then return false end
  local ids = {}
  for id in pairs(_G.__rock_alloc_hits) do ids[#ids + 1] = id end
  table.sort(ids)
  local file, err = io.open(path, "w")
  if not file then
    io.stderr:write("[ALLOC] cannot write activation report " .. path .. ": " .. tostring(err) .. "\n")
    return false
  end
  file:write("rock.allocation-activation.v1\n")
  file:write("frames\t", tostring(frames), "\n")
  file:write("sites\t", tostring(#ids), "\n")
  for _, id in ipairs(ids) do
    file:write(id, "\t", tostring(_G.__rock_alloc_hits[id]), "\n")
  end
  file:close()
  return true
end
"#;

/// Lua shims for the checked Vec indexing helpers codegen emits
/// (`__vec_checked_get`/`__vec_checked_set`). In real builds the stdlib
/// provides them (`stdlib/unified.rock`); stdlib-free test harnesses prepend
/// this so emitted chunks stay self-contained.
pub const VEC_CHECK_SHIMS: &str = "\
function __vec_checked_get(vec, i)\n\
  if i < 0 or i >= vec.len then error('Vec index out of bounds') end\n\
  return vec._data[i + 1]\n\
end\n\
function __vec_checked_set(vec, i, v)\n\
  if i < 0 or i >= vec.len then error('Vec index out of bounds') end\n\
  vec._data[i + 1] = v\n\
end";

/// The env-record parameter name of a hoisted capturing lambda body
/// (`function <name>__lambda<k>(<ENV_PARAM>, <params>)`). A reserved
/// double-underscore rock name so a user global/fn read inside the body
/// (`Inst::Global` emits the bare name) can never collide with it -- user
/// identifiers can't legally start with `__rock`. (The `e` local in
/// `FN_MT_PRELUDE`'s `__call` is a private Lua closure param, unaffected.)
const ENV_PARAM: &str = "__rock_env";

thread_local! {
    /// Cumulative time spent in register allocation (`regalloc::color_values`
    /// AND `regalloc::color_cells`) across every `render_scope`/`emit_*` call on
    /// this thread — including nested closures. Lets `emit_workspace_mir` split
    /// its "emit lua" stage into regalloc vs stringify for `backend-stats`. It's
    /// PER-THREAD: under parallel emit each unit resets it just before its own
    /// emit and reads it right after (a unit runs to completion on one rayon
    /// worker), so the per-unit value is exact and the summed total is correct
    /// regardless of feature.
    static REGALLOC_TIME: std::cell::Cell<std::time::Duration> =
        const { std::cell::Cell::new(std::time::Duration::ZERO) };
}

/// `regalloc::color_cells`, charged to the `REGALLOC_TIME` accumulator (so the
/// backend-stats regalloc/stringify split counts cell coloring as regalloc, not
/// stringify — it is the second half of register allocation).
fn color_cells_timed(func: &MirFunc) -> Vec<u32> {
    let t = std::time::Instant::now();
    let colors = regalloc::color_cells(func);
    REGALLOC_TIME.with(|c| c.set(c.get() + t.elapsed()));
    colors
}

/// Zero the per-thread regalloc accumulator (call before an emit run).
pub fn regalloc_time_reset() {
    REGALLOC_TIME.with(|c| c.set(std::time::Duration::ZERO));
}

/// Read the per-thread regalloc accumulator (call after an emit run).
pub fn regalloc_time_elapsed() -> std::time::Duration {
    REGALLOC_TIME.with(|c| c.get())
}

fn raw<T>(idx: Idx<T>) -> u32 {
    idx.into_raw().into()
}

fn emitted_value_allocation_kind(inst: &Inst) -> Option<EmittedAllocationKind> {
    Some(match inst {
        Inst::StringConcat(..) => EmittedAllocationKind::StringConcat,
        Inst::ToString(..) => EmittedAllocationKind::StringConversion,
        Inst::Format { .. } | Inst::FormatDyn { .. } => EmittedAllocationKind::StringFormat,
        Inst::MakeTuple(..) => EmittedAllocationKind::TupleConstruction,
        Inst::MakeVec(..) => EmittedAllocationKind::VecConstruction,
        Inst::MakeVecColumns { .. } => EmittedAllocationKind::VecColumnConstruction,
        Inst::MakeVecColumnHeader { .. } => EmittedAllocationKind::VecColumnHeader,
        Inst::MakeStruct { .. } => EmittedAllocationKind::StructConstruction,
        Inst::MakeValueStruct { .. } => EmittedAllocationKind::ValueConstruction,
        Inst::MaterializeValueStruct { .. } => {
            EmittedAllocationKind::ExplicitValueMaterialization
        }
        Inst::MakeEnum { .. } => EmittedAllocationKind::EnumConstruction,
        Inst::MakeEnumDyn { .. } => EmittedAllocationKind::DynamicEnumConstruction,
        Inst::MakeEnv { .. } => EmittedAllocationKind::ClosureEnvironment,
        Inst::FfiNewArray { .. } => EmittedAllocationKind::FfiArray,
        Inst::FfiViewCast { .. } => EmittedAllocationKind::FfiView,
        _ => return None,
    })
}

fn emitted_effect_allocation_kind(inst: &Inst) -> Option<EmittedAllocationKind> {
    match inst {
        Inst::ArrayPush { .. }
        | Inst::StoreIndex {
            kind: ContainerKind::Map,
            ..
        } => Some(EmittedAllocationKind::ContainerGrowth),
        _ => None,
    }
}

fn allocation_site_id(
    body: &str,
    id: Idx<Inst>,
    kind: EmittedAllocationKind,
    span: Option<Span>,
) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    let mut write = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    };
    write(body.as_bytes());
    write(kind.as_str().as_bytes());
    write(&raw(id).to_le_bytes());
    if let Some(span) = span {
        write(span.file.as_str().as_bytes());
        write(&span.offset_range.0.to_le_bytes());
        write(&span.offset_range.1.to_le_bytes());
    }
    hash
}

/// Append `n`'s base-10 digits to `out`. A small fast path that skips
/// `core::fmt`'s padding-aware `Display for u32` machinery — ~2x on the per-
/// operand `r<n>` / `c<n>` / `bb<n>` sites that dominate the printer.
fn push_uint(out: &mut String, mut n: u32) {
    if n == 0 {
        out.push('0');
        return;
    }
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    // buf[i..] is guaranteed ASCII digits.
    out.push_str(std::str::from_utf8(&buf[i..]).unwrap());
}

/// Whole-workspace codegen input not derivable from a single [`MirFunc`]: the
/// map of every real top-level Lua name (function names + globals) to its source
/// span (guards every synthetic `<name>__lambda<k>` / `<co>__step` / `__on_*` /
/// ... name against clobber). Computed once per emit.
pub struct CodegenCtx {
    /// Every REAL top-level Lua name (a function's mangled name or a global's
    /// name) -> the span of its source definition. A synthetic name minted
    /// through [`mint_synthetic`] that lands here would clobber the user's def
    /// last-write-wins, so it is rejected (span points at the user's def).
    reserved: HashMap<String, Span>,
    /// Rejections recorded during (parallel) emit -- name collisions and broken
    /// codegen invariants alike; drained by [`take_rejection`] at each public
    /// emit boundary. `Mutex` because `&CodegenCtx` is shared across rayon
    /// workers; contention is nil (a few per fn).
    rejections: Mutex<Vec<(String, CompilerError)>>,
    /// Compiler-assigned Session slots for mutable globals.
    state_slots: HashMap<ustr::Ustr, u32>,
}

impl Clone for CodegenCtx {
    fn clone(&self) -> Self {
        Self {
            reserved: self.reserved.clone(),
            rejections: Mutex::new(self.rejections.lock().expect("rejection sink").clone()),
            state_slots: self.state_slots.clone(),
        }
    }
}

/// True if `name` is an identifier skull emits as a bare LOCAL inside function
/// bodies -- a value temp (`r<N>`, or the uncolored `v<N>` fallback), a cell
/// local (`c<N>`), or the closure env param ([`ENV_PARAM`]). `Inst::Global`
/// reads/writes a user global by its bare name, so a top-level def with one of
/// these names is shadowed by the local inside any body that uses one (the
/// env-param-`e` miscompile `5c378c810`; `global r1` crashing at runtime with
/// "arithmetic on local 'r1' (a nil value)").
///
/// This is the reserved *local* namespace only. The `__rock_*` prefix at large
/// is a shared reserved namespace the stdlib AND stone engine legitimately
/// extend with top-level `@nomangle` fns (`__rock_create_struct`,
/// `__rock_install_serialize_value`, ...) -- those are top-level globals, never
/// body locals, so they don't collide and must not be rejected. Only the exact
/// emitted param name (`__rock_env`) is a body local.
fn is_reserved_lexical(name: &str) -> bool {
    if name == ENV_PARAM {
        return true;
    }
    let digits = match name.as_bytes().first() {
        Some(b'r' | b'c' | b'v') => &name[1..],
        _ => return false,
    };
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Record a spanned rejection if `name` (a user def's emitted Lua name) lands in
/// the reserved lexical namespace. Same sink as [`CodegenCtx::mint_synthetic`],
/// drained at the same emit boundaries.
fn reserved_namespace_collision(name: &str, span: Span, sink: &mut Vec<(String, CompilerError)>) {
    if !is_reserved_lexical(name) {
        return;
    }
    let msg = format!(
        "`{name}` collides with a reserved codegen identifier: skull emits `r<N>`/`v<N>` \
         value temps, `c<N>` cell locals, and the `{ENV_PARAM}` closure-env param as bare \
         locals inside function bodies, which would shadow this top-level definition at \
         runtime -- rename it"
    );
    sink.push((name.to_string(), CompilerError::new_with_label(span, msg)));
}

impl CodegenCtx {
    pub fn new(ws: &Workspace) -> Self {
        Self::new_with_state(ws, HashMap::default())
    }

    pub fn new_with_state(ws: &Workspace, state_slots: HashMap<ustr::Ustr, u32>) -> Self {
        let mut reserved: HashMap<String, Span> = HashMap::default();
        let mut collisions: Vec<(String, CompilerError)> = Vec::new();
        for m in ws.modules.iter() {
            for (_, f) in m.functions.iter() {
                let name = mangle_fn(ws, f);
                reserved_namespace_collision(&name, f.name_span, &mut collisions);
                reserved.insert(name, f.name_span);
            }
            for (_, g) in m.globals.iter() {
                reserved_namespace_collision(g.name.as_str(), g.name_span, &mut collisions);
                reserved.insert(g.name.as_str().to_string(), g.name_span);
            }
        }
        // Permanent mangle-collision check (reject, don't fall back): two distinct
        // signatures mangling alike, or a mangled name equal to an exempt bare
        // name. Drained through the same sink as the synthetic-name collisions.
        collisions.extend(skull_mir::mangle::collision_errors(ws));
        CodegenCtx {
            reserved,
            rejections: Mutex::new(collisions),
            state_slots,
        }
    }

    /// Mint a compiler-reserved synthetic top-level Lua name: a hoisted lambda
    /// (`<fn>__lambda<k>`) or coroutine machinery (`<co>__step`/`__resume`/
    /// `__advance`/`__defer<k>`/`__on_killed`/`__on_resume`/`__on_hot_reload`).
    /// Returns `name` unchanged. If it collides with a REAL top-level name (a
    /// user fn's Lua name or a global), records a spanned rejection pointing at
    /// that user definition -- drained after emit by [`take_rejection`]. This is
    /// reject-not-panic: a collision is illegal user code, not a compiler bug, so
    /// it surfaces as a diagnostic and keeps `rock stone` / `lsp serve` alive.
    /// THE single choke point for every synthetic top-level name across skull.
    pub fn mint_synthetic(&self, name: String) -> String {
        if let Some(&span) = self.reserved.get(&name) {
            let msg = format!(
                "`{name}` collides with a compiler-reserved synthetic name emitted by \
                 codegen; rename this definition (the `__lambda<k>`, `__step`, `__resume`, \
                 `__advance`, `__defer<k>`, and `__on_killed`/`__on_resume`/`__on_hot_reload` \
                 forms are reserved)"
            );
            self.reject(name.clone(), CompilerError::new_with_label(span, msg));
        }
        name
    }

    /// Record a rejection found mid-emit, keyed by `key` for deterministic
    /// ordering across rayon scheduling. THE sink for anything the render path
    /// cannot return a `Result` from.
    pub fn reject(&self, key: String, err: CompilerError) {
        self.rejections.lock().expect("rejection sink").push((key, err));
    }

    /// Drain the recorded rejections and return the first (by key, for
    /// determinism across rayon scheduling), or `None`. Called at each public
    /// emit boundary; draining clears the sink so a later emit on the same
    /// (cached) ctx starts fresh.
    pub fn take_rejection(&self) -> Option<CompilerError> {
        let mut sink = self.rejections.lock().expect("rejection sink");
        sink.sort_by(|a, b| a.0.cmp(&b.0));
        sink.drain(..).next().map(|(_, e)| e)
    }

    pub fn state_slot(&self, name: ustr::Ustr) -> Option<u32> {
        self.state_slots.get(&name).copied()
    }
}

/// Cells REFERENCED (Load/Store/`MapNext` or multi-return dest) anywhere the top
/// scope rooted at `entry` emits. Hoisted closure bodies are separate ordinary
/// MirFuncs, so the declaration walk only follows this function's CFG.
fn referenced_cells_in_region(func: &MirFunc, entry: Idx<Block>) -> HashSet<Idx<Cell>> {
    let mut seen: HashSet<Idx<Block>> = HashSet::default();
    let mut stack = vec![entry];
    seen.insert(entry);
    let mut cells: HashSet<Idx<Cell>> = HashSet::default();
    while let Some(b) = stack.pop() {
        for &iid in &func.blocks[b].insts {
            match &func.insts[iid] {
                Inst::Load(c) => { cells.insert(*c); }
                Inst::Store { cell, .. } => { cells.insert(*cell); }
                Inst::MapNext { key_cell, val_cell, .. } => {
                    cells.insert(*key_cell);
                    cells.insert(*val_cell);
                }
                Inst::CallMulti { dests, .. }
                | Inst::CallValueMulti { dests, .. }
                | Inst::CallBoundMulti { dests, .. } => cells.extend(dests.iter().copied()),
                _ => {}
            }
        }
        successors(&func.blocks[b].term, |s| {
            if seen.insert(s) {
                stack.push(s);
            }
        });
    }
    cells
}

/// Every closure parameter cell is owned by its nested Lua function rather
/// than by the enclosing scope's local declaration.
pub(crate) fn collect_lambda_params(func: &MirFunc) -> HashSet<Idx<Cell>> {
    let mut out = HashSet::default();
    for (_, inst) in func.insts.iter() {
        if let Inst::Lambda { params, .. } = inst {
            out.extend(params.iter().copied());
        }
    }
    out
}

/// Emit one MIR function as a top-level Lua `function <name>(...) ... end`.
/// `self_ref` identifies the function being emitted so its header name mangles
/// identically to a call site that targets it.
pub fn emit_function(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    self_ref: FuncRef,
    func: &MirFunc,
) -> String {
    let name = mangle_fn(ws, func_of(ws, self_ref));
    emit_named(ws, ctx, interner, &name, func)
}

pub fn emit_function_with_allocation_sites(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    self_ref: FuncRef,
    func: &MirFunc,
    instrument: bool,
) -> LuaEmission {
    let name = mangle_fn(ws, func_of(ws, self_ref));
    emit_named_with_allocation_sites(ws, ctx, interner, &name, func, instrument)
}

/// Emit a MIR function under the EXACT Lua name `name` (no mangling) — for
/// synthetic functions with an already-final name (a global-initializer fn).
/// Calls inside the body still mangle their `FuncRef`s via `ctx`.
pub fn emit_named(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
) -> String {
    emit_named_with_statics(ws, ctx, interner, name, func, &[])
}

pub fn emit_named_with_allocation_sites(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
    instrument: bool,
) -> LuaEmission {
    emit_named_framed_with_allocation_sites(
        ws,
        ctx,
        interner,
        name,
        func,
        None,
        &[],
        instrument,
    )
}

/// Emit a function whose static cells may be owned by an enclosing lowered
/// function. Closure conversion moves the lambda body into a sibling function,
/// while its static cell remains owned by the enclosing callable.
pub fn emit_named_with_statics(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
    inherited_statics: &[StaticCell],
) -> String {
    emit_named_framed_with_statics(ws, ctx, interner, name, func, None, inherited_statics)
}

/// The coroutine state-table context: a `co fn`'s `__step` reads/writes its
/// frame cells as fields of the state table `s` (the first param cell). A frame
/// cell renders `<s>.c<slot>` instead of a bare local and is NOT declared. The
/// slot is a DENSE per-frame ordinal (`slots`), NOT the raw cell-arena id:
/// lambda-cell allocation bumps raw ids but never reorders the frame cells, so
/// a slot -- and the layout stamp keyed on it -- is invariant under
/// lambda-internal cell-count edits (the P2 finding in
/// docs/coroutine-frame-lambda-plan.md). Lambdas are converted before emission;
/// detached hook/defer regions are ordinary `MirFunc`s by the time they reach
/// this printer.
pub struct FrameCtx {
    pub s_cell: Idx<Cell>,
    pub fields: HashSet<Idx<Cell>>,
    /// Frame cell -> dense slot (`c<slot>` field name). Total over `fields`.
    pub slots: HashMap<Idx<Cell>, u32>,
}

#[derive(Clone, Copy)]
struct AllocationEmission<'a> {
    body: &'a str,
    sites: &'a RefCell<Vec<EmittedAllocationSite>>,
    instrument: bool,
}

/// [`emit_named`] with an optional coroutine frame context (the `__step` fn).
///
/// Closures no longer convert here: `skull_transform::convert_closures` (the
/// shared MIR pass) has already rewritten every reachable `Inst::Lambda` into a
/// `MakeEnv` record / bare `Global` + a hoisted sibling `MirFunc` before emission
/// (S3, docs/closure-conversion-mir-plan.md). So this fn only renders the given
/// region.
pub fn emit_named_framed(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
    frame: Option<&FrameCtx>,
) -> String {
    emit_named_framed_with_statics(ws, ctx, interner, name, func, frame, &[])
}

fn emit_named_framed_with_statics(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
    frame: Option<&FrameCtx>,
    inherited_statics: &[StaticCell],
) -> String {
    emit_named_framed_with_statics_impl(
        ws,
        ctx,
        interner,
        name,
        func,
        frame,
        inherited_statics,
        None,
    )
}

pub fn emit_named_framed_with_allocation_sites(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
    frame: Option<&FrameCtx>,
    inherited_statics: &[StaticCell],
    instrument: bool,
) -> LuaEmission {
    let sites = RefCell::new(Vec::new());
    let lua = emit_named_framed_with_statics_impl(
        ws,
        ctx,
        interner,
        name,
        func,
        frame,
        inherited_statics,
        Some(AllocationEmission {
            body: name,
            sites: &sites,
            instrument,
        }),
    );
    let mut allocation_sites = sites.into_inner();
    let function_line_offset = lua
        .find(&format!("function {name}"))
        .map(|offset| lua.as_bytes()[..offset].iter().filter(|byte| **byte == b'\n').count())
        .unwrap_or(0) as u32;
    allocation_sites.sort_by_key(|site| site.lua_byte_offset);
    let mut byte_cursor = 0;
    let mut line = 1;
    for site in &mut allocation_sites {
        for byte in &lua.as_bytes()[byte_cursor..site.lua_byte_offset] {
            if *byte == b'\n' {
                line += 1;
            }
        }
        site.lua_body_line = line - function_line_offset;
        byte_cursor = site.lua_byte_offset;
    }
    allocation_sites.sort_by_key(|site| site.id);
    allocation_sites.dedup_by_key(|site| site.id);
    LuaEmission {
        lua,
        allocation_sites,
    }
}

fn emit_named_framed_with_statics_impl(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
    frame: Option<&FrameCtx>,
    inherited_statics: &[StaticCell],
    allocation: Option<AllocationEmission<'_>>,
) -> String {
    let entry = func.entry();
    let params = func.params.as_slice();
    // Ordinary functions color cells (collapse them onto luajit's 200-local cap);
    // a coroutine `__step` keeps raw `c<raw>` names (frame-field naming depends on
    // the raw id), so it gets an empty map.
    let cell_colors = if frame.is_some() { vec![u32::MAX; func.cells.len()] } else { color_cells_timed(func) };
    // Module-scope const hoisting: an all-constant `value` ctor is built ONCE as a
    // function-owned top-level global and referenced everywhere, instead of rebuilt
    // at each use. On a trace the per-use ctor sinks anyway, but in the interpreter
    // (mag's aborted HUD/text loops) it allocates every time -- the nested
    // `outline_color: Vec2 { 0.0, 0.0 }` per glyph vertex was the entire skull-vs
    // -rock interpreter-path text gap. rock does the same (`__rock_const_*`). The
    // def is emitted in THIS function's chunk (not shared scaffolding), so an
    // incremental body-edit overlay carries it; value structs are immutable, so
    // re-running the def on reload is a harmless no-op (no preservation guard).
    record_representation_violations(ctx, func);
    let (hoist, hoist_names, hoist_defs) = collect_const_hoists(ws, ctx, func, name);
    let static_slots = static_slots_for(
        ws,
        interner,
        func,
        inherited_statics,
        name,
        ctx,
    );
    let estimated_body_bytes = func.insts.len() * 24;
    let mut out =
        String::with_capacity(hoist_defs.len() + name.len() + 64 + estimated_body_bytes);
    out.push_str(&hoist_defs);
    let _ = write!(out, "function {}", name);
    render_scope(
        ws,
        ctx,
        interner,
        func,
        entry,
        params,
        true,
        frame,
        &cell_colors,
        &ctx.state_slots,
        &static_slots,
        &hoist,
        &hoist_names,
        allocation,
        &mut out,
    );
    out.push('\n');
    out
}

fn static_slots_for(
    ws: &Workspace,
    interner: &MonoInterner,
    func: &MirFunc,
    inherited_statics: &[StaticCell],
    name: &str,
    ctx: &CodegenCtx,
) -> HashMap<ustr::Ustr, String> {
    let mut slots = HashMap::default();
    for cell in inherited_statics.iter().chain(&func.statics) {
        let (value, done) = match skull_mir::state_schema::static_cell_state_keys(ws, interner, cell) {
            Ok(keys) => keys,
            Err(error) => {
                ctx.reject(name.to_string(), CompilerError::new_no_location(error.to_string()));
                return HashMap::default();
            }
        };
        if slots.insert(cell.cell, value).is_some() || slots.insert(cell.done_flag, done).is_some() {
            ctx.reject(
                name.to_string(),
                CompilerError::new_no_location(format!("duplicate static state slot for `{}`", cell.cell)),
            );
            return HashMap::default();
        }
    }
    slots
}

/// Whether every use of `cand` is a provably READ-ONLY consumer, so sharing one
/// module-scope instance across all of them is unobservable.
///
/// FAIL-CLOSED by construction: the allow-list is tiny and anything else --
/// including a new `Inst` variant -- disqualifies. The inverse (list the
/// escaping consumers) would silently start sharing a mutable value the day the
/// enum grows.
fn const_hoist_uses_are_read_only(_ws: &Workspace, func: &MirFunc, cand: Idx<Inst>) -> bool {
    for (_, inst) in func.insts.iter() {
        let allowed = match inst {
            // A field/tuple projection and an equality test observe the value
            // without retaining or mutating it.
            Inst::Field { base, .. } | Inst::TupleIndex { base, .. } => *base == cand,
            Inst::Eq(a, b) | Inst::NotEq(a, b) => *a == cand || *b == cand,
            // Storing INTO cdata copies by value, so the destination cannot alias
            // the shared instance: an FFI-clean `<Name>_ct(..)` ctor copies each
            // field, and a `slot.f = v` into a NativeBuffer element writes bytes.
            // A TABLE destination (plain struct, GC-tier `<Name>_vt`, Vec element,
            // global, cell) stores the REFERENCE and is what aliases.
            Inst::MakeValueStruct { representation, fields, .. }
            | Inst::MaterializeValueStruct { representation, fields, .. } => {
                !fields.iter().any(|(_, v)| *v == cand)
                    || !matches!(
                        func.rep_arena
                            .as_ref()
                            .and_then(|arena| arena.try_get(*representation))
                            .and_then(|plan| plan.value_storage()),
                        Some(
                            skull_mir::lower::ValueStorage::GcTable
                                | skull_mir::lower::ValueStorage::LeafTable
                        )
                    )
            }
            Inst::StoreField { base, value, .. } => {
                *value != cand
                    || matches!(
                        func.insts[*base],
                        Inst::Index { kind: ContainerKind::NativeBuffer | ContainerKind::NativeBufferValue(_), .. }
                    )
            }
            _ => false,
        };
        if allowed {
            continue;
        }
        let mut reads = false;
        regalloc::inst_uses(inst, &mut |u| reads |= u == cand);
        if reads {
            return false;
        }
    }
    // A terminator operand leaves the function (return value, branch
    // discriminant, yield channel), so it is never a read-only use.
    !func.blocks.iter().any(|(_, blk)| match &blk.term {
        Term::Return(vs) | Term::Yield { values: vs, .. } => vs.contains(&cand),
        Term::Branch { cond, .. } => *cond == cand,
        Term::Switch { discr, .. } => *discr == cand,
        _ => false,
    })
}


fn ctor_suffix(func: &MirFunc, representation: skull_mir::lower::RepId, form: ValueCtorForm) -> Option<&'static str> {
    let arena = func.rep_arena.as_ref()?;
    let plan = arena.try_get(representation)?;
    Some(match form {
        ValueCtorForm::Leaves => "_lct",
        ValueCtorForm::Declared => match plan.value_storage() {
            Some(skull_mir::lower::ValueStorage::GcTable) => "_vt",
            Some(skull_mir::lower::ValueStorage::LeafTable) => "_lct",
            Some(skull_mir::lower::ValueStorage::CData) | None => "_ct",
        },
    })
}

/// Check a leaf-form constructor against the representation it carries.
/// Callers supply the plan; this routine never reconstructs one from a name or
/// from the punctuation of the operand keys.
pub fn leaf_form_violation(
    plan: &skull_mir::lower::RepPlan,
    got: &[ustr::Ustr],
) -> Option<CompilerError> {
    let Some(canonical) = plan.leaf_paths() else {
        return Some(CompilerError::new_no_location(
            "leaf-form constructor carries a non-leaf representation plan",
        ));
    };
    if got == canonical {
        return None;
    }
    Some(CompilerError::new_no_location(format!(
            "leaf-form constructor does not match its canonical leaf plan: {got:?} vs {canonical:?}"
    )))
}

/// Validate every plan-tagged constructor before rendering. A missing plan is
/// an internal compiler error; it must reach the ordinary rejection sink rather
/// than making emission recover a tier from a name or operand spelling.
fn record_representation_violations(ctx: &CodegenCtx, func: &MirFunc) {
    for (_, inst) in func.insts.iter() {
        let (name, representation, form, fields) = match inst {
            Inst::MakeValueStruct { name, representation, form, fields }
            | Inst::MaterializeValueStruct { name, representation, form, fields } => {
                (*name, *representation, *form, fields)
            }
            _ => continue,
        };
        let Some(suffix) = ctor_suffix(func, representation, form) else {
            ctx.reject(
                name.to_string(),
                CompilerError::new_no_location(format!(
                    "`{name}` value constructor carries a representation plan that does not belong to this function's arena"
                )),
            );
            continue;
        };
        if matches!(form, ValueCtorForm::Leaves) {
            let Some(plan) = func
                .rep_arena
                .as_ref()
                .and_then(|arena| arena.try_get(representation))
            else {
                ctx.reject(
                    name.to_string(),
                    CompilerError::new_no_location(format!(
                        "`{name}` leaf-form constructor has no leaf paths in its supplied plan"
                    )),
                );
                continue;
            };
            let got: Vec<ustr::Ustr> = fields.iter().map(|(n, _)| *n).collect();
            if let Some(mut error) = leaf_form_violation(&plan, &got) {
                error.message = format!("`{name}` {}", error.message);
                ctx.reject(name.to_string(), error);
            }
        }
        if suffix == "_lct" && matches!(form, ValueCtorForm::Declared) {
            ctx.reject(
                name.to_string(),
                CompilerError::new_no_location(format!(
                    "`{name}` declared-form constructor selects a leaf-table plan"
                )),
            );
        }
    }
}

/// Group every all-constant `MakeValueStruct` (every field operand a `Const`) by
/// (struct name, field literals). Returns `(inst -> group, group -> global name,
/// def lines)`: each group becomes one function-owned global
/// `__skull_kc_<fn>_<g> = <Name>_ct(<lits>)`. A module-scope global is built once
/// at chunk load, so it is never worse than a per-use ctor (which it equals on a
/// trace via SINK and beats in the interpreter), unlike a per-CALL function-entry
/// local which defeats SINK.
///
/// Hoisting is NOT unconditional: it is sound only while the shared instance
/// cannot be mutated. The place model legalized leaf writes through mutable
/// storage (docs/value-semantics-axes.md), so a constant that reaches a mutable
/// place must be materialized FRESH per construction -- otherwise `a.p.x = 5`
/// through one holder rewrites every other holder of that literal, including an
/// unrelated global (`tests/value_const_hoist_aliasing.rock`). Candidates whose
/// uses are not provably read-only emit their ctor inline, exactly as before the
/// hoist existed.
fn collect_const_hoists(
    ws: &Workspace,
    ctx: &CodegenCtx,
    func: &MirFunc,
    base: &str,
) -> (HashMap<Idx<Inst>, u32>, Vec<String>, String) {
    let mut groups: Vec<(ustr::Ustr, skull_mir::lower::RepId, ValueCtorForm, Vec<Lit>)> = Vec::new();
    let mut hoist: HashMap<Idx<Inst>, u32> = HashMap::default();
    // Only insts still REACHED BY A BLOCK. The arena keeps orphans (DCE unlinks
    // from blocks, it does not free), and hoisting one emits a module-scope ctor
    // call for a value the program never builds -- which for a nobox type is a
    // materialization the verifier cannot see, since it walks `block.insts`.
    let live: HashSet<Idx<Inst>> =
        func.blocks.iter().flat_map(|(_, b)| b.insts.iter().copied()).collect();
    for (id, inst) in func.insts.iter() {
        if !live.contains(&id) {
            continue;
        }
        let Inst::MakeValueStruct { name, representation, form, fields } = inst else {
            continue;
        };
        // An int->float-subsumed constant field (`IntToFloat(Const)`) is still a
        // compile-time constant: hoist as its inner literal (identity on luajit),
        // so a `Vec2 { x: 0 }` ctor stays hoistable.
        let lits: Option<Vec<Lit>> = fields
            .iter()
            .map(|(_, op)| match &func.insts[*op] {
                Inst::Const(l) => Some(l.clone()),
                Inst::IntToFloat(x) => match &func.insts[*x] {
                    Inst::Const(l) => Some(l.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let Some(lits) = lits else { continue };
        if !const_hoist_uses_are_read_only(ws, func, id) {
            continue;
        }
        let g = match groups
            .iter()
            .position(|(n, rep, ctor_form, l)| *n == *name && *rep == *representation && *ctor_form == *form && *l == lits)
        {
            Some(p) => p as u32,
            None => {
                groups.push((*name, *representation, *form, lits));
                (groups.len() - 1) as u32
            }
        };
        hoist.insert(id, g);
    }
    if groups.is_empty() {
        return (hoist, Vec::new(), String::new());
    }
    let mut names = Vec::with_capacity(groups.len());
    let mut defs = String::new();
    for (g, (name, representation, form, lits)) in groups.iter().enumerate() {
        let gname = format!("__skull_kc_{base}_{g}");
        let Some(suffix) = ctor_suffix(func, *representation, *form) else {
            ctx.reject(
                name.to_string(),
                CompilerError::new_no_location(format!(
                    "`{name}` constant constructor has no valid supplied representation plan"
                )),
            );
            continue;
        };
        let _ = write!(defs, "{gname} = {name}{suffix}(");
        for (i, l) in lits.iter().enumerate() {
            if i > 0 {
                defs.push_str(", ");
            }
            write_lit(&mut defs, l);
        }
        defs.push_str(")\n");
        names.push(gname);
    }
    (hoist, names, defs)
}

/// The Lua name for a cell read/write: a coroutine frame cell renders as the
/// state-table field `<s>.c<raw>`; everything else is its colored local
/// (`c<color>`) or — when uncolored (a coroutine `__step`, passed an empty
/// color map) — the raw `c<raw>`.
fn cellname(func: &MirFunc, frame: Option<&FrameCtx>, colors: &[u32], c: Idx<Cell>) -> String {
    let mut s = String::new();
    write_cellname(&mut s, func, frame, colors, c);
    s
}

/// Append a cell reference (a coroutine frame field `s.c<slot>` or a plain local)
/// directly into `out` — the per-operand-allocation-free primitive `cellname`
/// wraps. A frame cell renders `<s>.c<slot>` in the `__step` body (the `s`
/// reference resolves recursively through this same fn, itself a plain local
/// there); env-record captures and box `.v` access are now materialized as MIR
/// `Field`/`StoreField` by `convert_closures`, so this fn no longer special-cases
/// them.
fn write_cellname(
    out: &mut String,
    func: &MirFunc,
    frame: Option<&FrameCtx>,
    colors: &[u32],
    c: Idx<Cell>,
) {
    if let Some(f) = frame {
        if f.fields.contains(&c) {
            write_cellname(out, func, None, colors, f.s_cell);
            out.push_str(".c");
            let slot = *f.slots.get(&c).expect("frame field cell must carry a dense slot");
            push_uint(out, slot);
            return;
        }
    }
    write_cell_local(out, func, colors, c);
}

fn write_cell_local(out: &mut String, func: &MirFunc, colors: &[u32], c: Idx<Cell>) {
    out.push('c');
    match colors[raw(c) as usize] {
        u32::MAX => push_uint(out, func.emit_cell_id(c)),
        col => push_uint(out, col),
    }
}

fn cell_is_uint(ws: &Workspace, ty: Option<bone_semantic::prelude::TyId>) -> bool {
    let Some(bone_semantic::prelude::TyId::Static(module, index)) = ty else {
        return false;
    };
    matches!(
        ws.module(module).ty_arena[index],
        bone_semantic::hir::TypeExpr::Builtin(bone_semantic::hir::BuiltinTy::Uint)
    )
}

// --- relooper: counted-`for` recovery (structured control flow) ------------
//
// A `for i in lo..hi` / `for x in xs` / `enumerate(..)` lowers to a canonical
// 4-block counted-loop CFG: a preheader storing the counter, a header testing
// `i < hi` (or `<=`), a body, and a pure-increment latch jumping back. LuaJIT
// specializes a real numeric `for` (FORL induction) but UNROLLS a goto-loop ->
// mcode blowup -> blacklist -> interpreter spiral (docs/skull-throughput-
// investigation.md). So we pattern-match that exact shape and emit a Lua `for`,
// leaving every other terminator as goto. Recognition runs at EMIT time, on the
// post-transform CFG, so it is robust to the coroutine transform / inlining
// renumbering blocks (the design's Phase-8 "reloop over the same MIR").

/// The upper bound of a counted loop — the RHS of the header `i < HI` compare.
enum HiLimit {
    /// A value computed in the preheader (e.g. `#arr`); live through the header.
    Value(Idx<Inst>),
    /// A bound cell loaded in the header (`for i in lo..hi`'s `hi`).
    Cell(Idx<Cell>),
}

/// A recognized counted loop: the `for slot = <slot>, hi[-1] do <body> end` shape.
/// The init reads `slot` (the preheader's `Store{slot, lo}` set it) so it never
/// depends on `lo`'s register still being live at the loop head.
struct CountedLoop {
    slot: Idx<Cell>,
    hi: HiLimit,
    inclusive: bool,
    body_entry: Idx<Block>,
    latch: Idx<Block>,
    exit: Idx<Block>,
    /// The natural-loop body blocks (excludes header + latch).
    region: HashSet<Idx<Block>>,
}

/// Every counted loop in `func`, keyed by header block. `frame` excludes a
/// coroutine state-field cell (it can't be a Lua `for` variable).
fn recognize_counted_loops(
    func: &MirFunc,
    rpo: &[Idx<Block>],
    frame: Option<&FrameCtx>,
) -> HashMap<Idx<Block>, CountedLoop> {
    let (all_preds, goto_preds) = predecessors(func, rpo);
    let mut loops = HashMap::default();
    for &h in rpo {
        if let Some(cl) = try_match_loop(func, h, &all_preds, &goto_preds, frame) {
            loops.insert(h, cl);
        }
    }
    loops
}

fn try_match_loop(
    func: &MirFunc,
    h: Idx<Block>,
    all_preds: &PredMap,
    goto_preds: &PredMap,
    frame: Option<&FrameCtx>,
) -> Option<CountedLoop> {
    let Term::Branch { cond, then_bb, else_bb } = func.blocks[h].term else {
        return None;
    };
    // Header is entered by exactly the preheader + the latch, both via goto.
    let gp = goto_preds.get(&h)?;
    if gp.len() != 2 || all_preds.get(&h).map_or(0, |v| v.len()) != 2 {
        return None;
    }
    // Exactly one goto-pred is a pure-increment latch; the other is the preheader.
    let mut latch = None;
    let mut slot = None;
    for &p in gp {
        if let Some(s) = latch_slot(func, p, h) {
            latch = Some(p);
            slot = Some(s);
        }
    }
    let (latch, slot) = (latch?, slot?);
    if frame.is_some_and(|f| f.fields.contains(&slot)) {
        return None;
    }
    let (hi, inclusive) = match_header(func, h, cond, slot)?;
    let region = compute_region(all_preds, then_bb, h, latch)?;
    if !slot_sound(func, slot, h, latch, &region) {
        return None;
    }
    Some(CountedLoop { slot, hi, inclusive, body_entry: then_bb, latch, exit: else_bb, region })
}

/// `Some(slot)` iff block `p` is a pure 4-inst increment latch jumping to
/// `header`: `vj = load slot; one = 1; vinc = vj + one; store slot = vinc`. The
/// strictness is load-bearing — a loop whose increment is fused into the body
/// (no dedicated latch) would otherwise match and double-increment.
fn latch_slot(func: &MirFunc, p: Idx<Block>, header: Idx<Block>) -> Option<Idx<Cell>> {
    if func.blocks[p].term != Term::Goto(header) {
        return None;
    }
    let ins = &func.blocks[p].insts;
    if ins.len() != 4 {
        return None;
    }
    let (Inst::Load(slot), Inst::Const(Lit::Int(1)), Inst::IntAdd(a, b), Inst::Store { cell, value }) =
        (&func.insts[ins[0]], &func.insts[ins[1]], &func.insts[ins[2]], &func.insts[ins[3]])
    else {
        return None;
    };
    (*a == ins[0] && *b == ins[1] && *cell == *slot && *value == ins[2]).then_some(*slot)
}

/// The header must be exactly `load slot; [load hi_cell;] (slot < HI | slot <= HI)`
/// — the counter load(s) feeding the compare, nothing else (no side effects).
fn match_header(
    func: &MirFunc,
    h: Idx<Block>,
    cond: Idx<Inst>,
    slot: Idx<Cell>,
) -> Option<(HiLimit, bool)> {
    let (lhs, rhs, inclusive) = match &func.insts[cond] {
        Inst::IntLt(a, b) => (*a, *b, false),
        Inst::IntLeq(a, b) => (*a, *b, true),
        _ => return None,
    };
    let insts = &func.blocks[h].insts;
    // LHS is a Load(slot) present in the header.
    if !matches!(&func.insts[lhs], Inst::Load(s) if *s == slot) || !insts.contains(&lhs) {
        return None;
    }
    // RHS is either a bound cell loaded in the header, or a preheader value.
    let rhs_is_cell_load =
        matches!(&func.insts[rhs], Inst::Load(_)) && insts.contains(&rhs);
    let allowed: HashSet<Idx<Inst>> = if rhs_is_cell_load {
        [lhs, rhs, cond].into_iter().collect()
    } else {
        [lhs, cond].into_iter().collect()
    };
    if insts.iter().any(|i| !allowed.contains(i)) {
        return None;
    }
    let hi = match &func.insts[rhs] {
        Inst::Load(c) if rhs_is_cell_load => HiLimit::Cell(*c),
        _ => HiLimit::Value(rhs),
    };
    Some((hi, inclusive))
}

/// The natural loop of the back-edge `latch -> header`: blocks that reach the
/// latch WITHOUT crossing the header (backward reach over preds), NOT
/// "everything reachable from body_entry" — a `break`/early-return path doesn't
/// reach the latch, so it stays OUTSIDE the loop and exits via goto. Returns
/// `None` if the body can't iterate, or the loop is multi-entry (irreducible).
fn compute_region(
    all_preds: &PredMap,
    body_entry: Idx<Block>,
    header: Idx<Block>,
    latch: Idx<Block>,
) -> Option<HashSet<Idx<Block>>> {
    let mut natural: HashSet<Idx<Block>> = HashSet::default();
    natural.insert(latch);
    let mut stack = vec![latch];
    while let Some(b) = stack.pop() {
        if let Some(ps) = all_preds.get(&b) {
            for &p in ps {
                if p != header && natural.insert(p) {
                    stack.push(p);
                }
            }
        }
    }
    if !natural.contains(&body_entry) {
        return None;
    }
    natural.remove(&latch);
    // Reducible / single-entry: a region block may be entered only from within
    // region ∪ {header, latch} — else a goto jumps into the middle of the `for`.
    for &b in &natural {
        if let Some(ps) = all_preds.get(&b) {
            for &p in ps {
                if p != header && p != latch && !natural.contains(&p) {
                    return None;
                }
            }
        }
    }
    Some(natural)
}

/// The counter slot must behave like a loop-scoped `for` variable: stored
/// EXACTLY twice (preheader init + latch increment — so it's never reassigned in
/// the body), and never read outside the loop region (incl. closures, whose body
/// blocks are outside the region — a captured counter can't be the for-variable).
fn slot_sound(
    func: &MirFunc,
    slot: Idx<Cell>,
    header: Idx<Block>,
    latch: Idx<Block>,
    region: &HashSet<Idx<Block>>,
) -> bool {
    let in_loop: HashSet<Idx<Block>> =
        region.iter().copied().chain([header, latch]).collect();
    let mut stores = 0usize;
    for (bid, b) in func.blocks.iter() {
        let inside = in_loop.contains(&bid);
        for &i in &b.insts {
            match &func.insts[i] {
                Inst::Store { cell, .. } if *cell == slot => stores += 1,
                Inst::Load(c) if *c == slot && !inside => return false,
                Inst::MapNext { key_cell, val_cell, .. }
                    if *key_cell == slot || *val_cell == slot =>
                {
                    return false
                }
                _ => {}
            }
        }
    }
    stores == 2
}

/// Whether a value-producing inst is ALWAYS safe to tree-fold into its single use
/// site, regardless of what executes between def and use: a pure computation over
/// value operands only (operands are SSA — immutable once computed) with no read
/// of mutable state (cell/global/heap) and no side effect. Excludes `Load`/
/// `Global`/`Field`/`Index`/`Len`/`Call` (order-sensitive memory reads / effects)
/// and closure records — those keep their register.
/// The splice-able shape of an `inline(lua)` body (for `@inline` splicing).
#[derive(Clone, Copy)]
enum InlineLuaShape {
    /// A single bare expression (no `return`, no statement keyword) -> the body
    /// IS the rvalue (`<result> = <body>`).
    Expr,
    /// Statements ending in a single TAIL `return X` -> emit the leading
    /// statements verbatim, rewrite the tail to `<result> = X`.
    TailReturn,
    /// Return-free statements (a unit body, e.g. `buf.ptr[i] = v`) -> verbatim,
    /// no result bound.
    UnitStmts,
}

/// `Some(rest)` if `line` (trimmed) is a Lua `return` statement, where `rest` is
/// the returned expression (`""` for a bare `return`). Word-boundary aware so
/// `returned`/`return_x` don't match. `None` otherwise.
fn inline_lua_return_rest(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("return")?;
    match rest.chars().next() {
        None => Some(""),
        Some(c) if !c.is_alphanumeric() && c != '_' => Some(rest.trim()),
        _ => None,
    }
}

/// Classify an `inline(lua)` body for splicing, or `None` if it is not
/// splice-safe (early or multiple `return`s, whose line-by-line rewrite to
/// `<result> = X` would not preserve the control flow -- fall back to a call).
fn classify_inline_lua_body(body: &str) -> Option<InlineLuaShape> {
    let t = body.trim();
    if t.is_empty() {
        return Some(InlineLuaShape::UnitStmts);
    }
    let nonempty: Vec<&str> = t.lines().filter(|l| !l.trim().is_empty()).collect();
    let returns = nonempty.iter().filter(|l| inline_lua_return_rest(l).is_some()).count();
    if returns == 0 {
        return Some(if is_expr_inline_lua_body(t) {
            InlineLuaShape::Expr
        } else {
            InlineLuaShape::UnitStmts
        });
    }
    // Exactly one return, and it is the LAST non-empty line -> tail return.
    let last_is_return = nonempty.last().is_some_and(|l| inline_lua_return_rest(l).is_some());
    if returns == 1 && last_is_return {
        Some(InlineLuaShape::TailReturn)
    } else {
        None
    }
}

/// Emit the spliced (return-free / tail-return) body, rewriting the tail
/// `return X` to `<lv> = X`. Param locals are emitted by the caller; this writes
/// only the body. Each line keeps the body's own text (Lua ignores indentation).
fn render_spliced_inline_lua(out: &mut String, body: &str, shape: InlineLuaShape, lv: &str) {
    match shape {
        InlineLuaShape::Expr => {
            let _ = writeln!(out, "    {lv} = {}", body.trim());
        }
        InlineLuaShape::UnitStmts => {
            for line in body.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let _ = writeln!(out, "    {}", line.trim_end());
            }
        }
        InlineLuaShape::TailReturn => {
            for line in body.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                match inline_lua_return_rest(line) {
                    Some("") => {} // bare tail return: nothing to bind (unit)
                    Some(rest) => {
                        let _ = writeln!(out, "    {lv} = {rest}");
                    }
                    None => {
                        let _ = writeln!(out, "    {}", line.trim_end());
                    }
                }
            }
        }
    }
}

/// Is this (return-free) `inline(lua)` body a single bare expression (so it can
/// be the RHS of `lv = …`), vs a statement / multi-statement unit body? Any
/// statement-keyword start, a top-level `=` assignment, `;`, or a second
/// non-blank line => stmts.
fn is_expr_inline_lua_body(s: &str) -> bool {
    let t = s.trim();
    let mut lines = t.lines().filter(|l| !l.trim().is_empty());
    let Some(first) = lines.next() else {
        return false;
    };
    if lines.next().is_some() {
        return false; // multi-line => statements
    }
    let f = first.trim_start();
    for kw in ["local ", "if ", "for ", "while ", "do ", "return", "::", "repeat", "break", "goto "] {
        if f.starts_with(kw) {
            return false;
        }
    }
    if f.contains(';') {
        return false;
    }
    // A top-level (paren-depth 0) `=` that is not ==/~=/<=/>= is an assignment.
    let b = f.as_bytes();
    let mut depth: i32 = 0;
    for i in 0..b.len() {
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'=' if depth == 0 => {
                let prev = if i > 0 { b[i - 1] } else { 0 };
                let next = if i + 1 < b.len() { b[i + 1] } else { 0 };
                let is_cmp = next == b'=' || matches!(prev, b'=' | b'~' | b'<' | b'>' | b'!');
                if !is_cmp {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

fn always_inlinable(inst: &Inst) -> bool {
    matches!(
        inst,
        Inst::Const(_)
            | Inst::IntAdd(..) | Inst::IntSub(..) | Inst::IntMul(..) | Inst::IntDiv(..) | Inst::IntMod(..)
            | Inst::UintAdd(..) | Inst::UintSub(..) | Inst::UintMul(..) | Inst::UintDiv(..) | Inst::UintMod(..)
            | Inst::FloatAdd(..) | Inst::FloatSub(..) | Inst::FloatMul(..) | Inst::FloatDiv(..) | Inst::FloatMod(..)
            | Inst::StringConcat(..)
            | Inst::IntLt(..) | Inst::IntLeq(..) | Inst::IntGt(..) | Inst::IntGeq(..)
            | Inst::UintLt(..) | Inst::UintLeq(..) | Inst::UintGt(..) | Inst::UintGeq(..)
            | Inst::FloatLt(..) | Inst::FloatLeq(..) | Inst::FloatGt(..) | Inst::FloatGeq(..)
            | Inst::Eq(..) | Inst::NotEq(..)
            | Inst::BitAnd(..) | Inst::BitOr(..) | Inst::BitXor(..) | Inst::Shl(..) | Inst::Shr(..)
            | Inst::UintBitAnd(..) | Inst::UintBitOr(..) | Inst::UintBitXor(..) | Inst::UintShl(..) | Inst::UintShr(..)
            | Inst::Neg(_) | Inst::Not(_) | Inst::BitNot(_) | Inst::UintBitNot(_) | Inst::ToString(_)
            | Inst::FloatToInt(_) | Inst::IntToFloat(_) | Inst::UintToFloat(_) | Inst::IntToUint(_)
            | Inst::UintToInt(_) | Inst::FloatToUint(_) | Inst::UintBitcastInt(_) | Inst::IntBitcastUint(_)
            | Inst::Format { .. } | Inst::FormatDyn { .. }
            | Inst::MakeTuple(_) | Inst::MakeVec(_)
            | Inst::MakeStruct { .. } | Inst::MakeValueStruct { .. } | Inst::MakeEnum { .. }
            | Inst::MakeEnumDyn { .. }
    )
}

/// The set of values to TREE-FOLD into their use site (no `local rN`): an
/// `always_inlinable` value used EXACTLY ONCE within this scope, def and use in
/// the same block. Single-use keeps it size-neutral (no duplicated expression);
/// "always-safe" + same-block means the fold is correct no matter what runs in
/// between. (Values are single-block by construction, so same-block nearly always
/// holds; the guard keeps it honest.) The colorer skips these; the emitter emits
/// each value's expression at its one use via [`Emitter::opnd`].
fn compute_inlinable(func: &MirFunc, scope: &[Idx<Block>]) -> Vec<bool> {
    // Dense side-tables keyed by raw inst id (no hashing): use count, and the
    // block of the (single) use and of the def. `NONE` = unset. Insts outside
    // `scope` (other closures share the arena) stay at 0/NONE and never qualify.
    const NONE: u32 = u32::MAX;
    let ni = func.insts.len();
    let mut use_count = vec![0u32; ni];
    let mut use_block = vec![NONE; ni];
    let mut def_block = vec![NONE; ni];
    for &b in scope {
        let br = raw(b);
        for &id in &func.blocks[b].insts {
            if func.insts[id].produces_value() {
                def_block[raw(id) as usize] = br;
            }
            regalloc::inst_uses(&func.insts[id], &mut |u| {
                let k = raw(u) as usize;
                use_count[k] += 1;
                use_block[k] = br;
            });
        }
        regalloc::term_uses(&func.blocks[b].term, &mut |u| {
            let k = raw(u) as usize;
            use_count[k] += 1;
            use_block[k] = br;
        });
    }
    // Dense `raw(inst) -> bool` (no hashing); probed per operand in `write_opnd`
    // and per use in `color_values`'s liveness/interference loops.
    let mut inl = vec![false; ni];
    for &b in scope {
        for &id in &func.blocks[b].insts {
            let k = raw(id) as usize;
            if use_count[k] == 1
                && def_block[k] != NONE
                && def_block[k] == use_block[k]
                && always_inlinable(&func.insts[id])
            {
                inl[k] = true;
            }
        }
    }
    inl
}

/// Render `function(<params>) <locals> <blocks> end` for the ordinary MIR
/// function rooted at `entry`, appending to `out` after its name.
#[allow(clippy::too_many_arguments)]
fn render_scope(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    func: &MirFunc,
    entry: Idx<Block>,
    params: &[Idx<Cell>],
    is_top: bool,
    frame: Option<&FrameCtx>,
    cell_colors: &[u32],
    state_slots: &HashMap<ustr::Ustr, u32>,
    static_slots: &HashMap<ustr::Ustr, String>,
    hoist: &HashMap<Idx<Inst>, u32>,
    hoist_names: &[String],
    allocation: Option<AllocationEmission<'_>>,
    out: &mut String,
) {
    // RPO drives both block layout (preheader-before-header, for the relooper)
    // and the reachable set the regalloc colors over.
    let rpo = reverse_postorder(func, entry);
    let loops = recognize_counted_loops(func, &rpo, frame);
    // Tree-folded values get no register; the colorer sees through them.
    let inlinable = compute_inlinable(func, &rpo);
    let t_ra = std::time::Instant::now();
    // Counted-loop regions partition the coloring so loop-body temps get
    // body-scoped `local`s (see `ValueAlloc::color_scope`). Partitioning trades
    // some color sharing for scoping; if a monster function's palette total
    // nears LuaJIT's 200-active-locals parse cap, drop the partitions.
    let mut loop_scopes: Vec<regalloc::LoopScope> = loops
        .iter()
        .map(|(&h, cl)| regalloc::LoopScope { header: h, region: cl.region.iter().copied().collect() })
        .collect();
    loop_scopes.sort_by_key(|ls| func.emit_block_id(ls.header));
    let mut va = regalloc::color_values(func, &rpo, &inlinable, &loop_scopes);
    if va.color_scope.len() > 150 && !loop_scopes.is_empty() {
        va = regalloc::color_values(func, &rpo, &inlinable, &[]);
    }
    let regalloc::ValueAlloc { colors, color_scope } = va;
    REGALLOC_TIME.with(|c| c.set(c.get() + t_ra.elapsed()));

    // Param list `(a, b, c)` — written straight into the buffer (no Vec<String>).
    // A hoisted capturing lambda's env-record param is a materialized cell in
    // `params` (the pass), so it renders through the normal loop.
    out.push('(');
    let mut wrote_param = false;
    for &c in params.iter() {
        if wrote_param {
            out.push_str(", ");
        }
        wrote_param = true;
        write_cell_local(out, func, cell_colors, c);
    }
    out.push_str(")\n");

    for &c in params {
        if cell_is_uint(ws, func.cells[c].ty) {
            out.push_str("  ");
            write_cell_local(out, func, cell_colors, c);
            out.push_str(" = ((");
            write_cell_local(out, func, cell_colors, c);
            out.push_str(" % 4294967296) + 4294967296) % 4294967296\n");
        }
    }

    // Declare registers in sorted color order (the dense table holds `u32::MAX`
    // for uncolored slots — inlined / non-value insts — which we skip), then the
    // cell locals. All written directly into a single `  local ...` line; we
    // stamp the keyword up front and `truncate` it back if nothing is declared.
    // Loop-owned colors are declared inside their `for` block (expand_for), not
    // here.
    let mut distinct: Vec<u32> = colors
        .iter()
        .copied()
        .filter(|&c| c != u32::MAX && color_scope[c as usize].is_none())
        .collect();
    distinct.sort_unstable();
    distinct.dedup();
    let decl_start = out.len();
    out.push_str("  local ");
    let mut any = false;
    for &c in &distinct {
        if any {
            out.push_str(", ");
        }
        any = true;
        out.push('r');
        push_uint(out, c);
    }
    // Only cells this region actually references get a local (a hoisted lambda
    // body is a full-arena clone; declaring the dead enclosing/sibling cells would
    // blow LuaJIT's 200-local cap on a large co-fn step). Computed once per top
    // scope; inline (non-`is_top`) renders declare nothing, so they skip it.
    let referenced = if is_top { referenced_cells_in_region(func, entry) } else { HashSet::default() };
    if is_top {
        let param_cells: HashSet<Idx<Cell>> = params.iter().copied().collect();
        let is_frame = |c: &Idx<Cell>| frame.is_some_and(|f| f.fields.contains(c));
        // Declare each cell LOCAL exactly once. Colored cells collapse onto their
        // shared `c<color>` (deduped, and excluding any color already named by a
        // param); uncolored cells (coroutine `__step`) declare per raw id.
        let param_colors: HashSet<u32> = params
            .iter()
            .map(|c| cell_colors[raw(*c) as usize])
            .filter(|&col| col != u32::MAX)
            .collect();
        let mut seen_color: HashSet<u32> = HashSet::default();
        for (cid, _) in func.cells.iter() {
            if param_cells.contains(&cid) || is_frame(&cid) || !referenced.contains(&cid) {
                continue;
            }
            let (decl, col) = match cell_colors[raw(cid) as usize] {
                u32::MAX => (true, None),                        // uncolored -> c<raw>
                col if !param_colors.contains(&col) => (seen_color.insert(col), Some(col)),
                _ => (false, None),                             // color already a param's
            };
            if !decl {
                continue;
            }
            if any {
                out.push_str(", ");
            }
            any = true;
            out.push('c');
            match col {
                Some(col) => push_uint(out, col),
                None => push_uint(out, func.emit_cell_id(cid)),
            }
        }
    }
    if any {
        out.push('\n');
    } else {
        out.truncate(decl_start); // no locals -> drop the stamped `  local `
    }

    // (Box `{ v = .. }` init lines are gone -- `convert_closures` materializes a
    // shared mutable capture as an explicit `MakeEnv { fn_ref: None }` value in the
    // MIR, so codegen never sees a "boxed cell".)

    let mut em = Emitter {
        ws,
        ctx,
        interner,
        func,
        colors,
        color_scope,
        cell_colors,
        frame,
        loops: &loops,
        rpo: &rpo,
        inlinable,
        emitted: vec![false; func.blocks.len()],
        hoist,
        hoist_names,
        state_slots,
        static_slots,
        allocation,
    };
    for &bb in &rpo {
        em.emit_block(out, bb); // self-skips an already-emitted block (loop body)
    }
    out.push_str("end");
}

fn func_of(ws: &Workspace, r: FuncRef) -> &Function {
    &ws.module(r.module).functions[r.func]
}

/// The mangled Lua name for a `FuncRef` (the same name a call site emits).
pub fn mangled(ws: &Workspace, r: FuncRef) -> String {
    mangle_fn(ws, func_of(ws, r))
}

/// The Lua local name for a value: its register `r<color>` (colored values) or
/// the raw `v<raw>` fallback (an uncolored value — should not occur once every
/// value is colored, but keeps the printer total).
fn write_vname(out: &mut String, func: &MirFunc, colors: &[u32], id: Idx<Inst>) {
    match colors[raw(id) as usize] {
        u32::MAX => {
            out.push('v');
            push_uint(out, func.emit_inst_id(id));
        }
        c => {
            out.push('r');
            push_uint(out, c);
        }
    }
}

struct Emitter<'a> {
    ws: &'a Workspace,
    ctx: &'a CodegenCtx,
    /// The build's one arena, so the symbol a call spells comes from the same
    /// [`Instance`] object the definition was emitted under rather than from a
    /// second derivation of the same encoder.
    ///
    /// [`Instance`]: skull_mir::instance::Instance
    interner: &'a MonoInterner,
    func: &'a MirFunc,
    /// Dense `raw(inst) -> color` (`u32::MAX` = uncolored); read per operand by
    /// [`write_vname`] as `colors[raw(id)]` instead of a hash probe.
    colors: Vec<u32>,
    /// `color -> owning loop header` (None = function top): loop-owned colors
    /// get their `local` line inside the `for` block ([`Self::expand_for`]).
    color_scope: Vec<Option<Idx<Block>>>,
    cell_colors: &'a [u32],
    frame: Option<&'a FrameCtx>,
    /// Recognized counted loops, keyed by header — a goto into an un-emitted
    /// header expands a structured `for`.
    loops: &'a HashMap<Idx<Block>, CountedLoop>,
    /// Reverse-postorder block layout (loop bodies emit their region in RPO).
    rpo: &'a [Idx<Block>],
    /// Values tree-folded into their single use site (no register); referenced
    /// through [`Self::opnd`], which renders their expression in place.
    /// Dense `raw(inst) -> bool`: values tree-folded into their single use site
    /// (no register), rendered in place by [`Self::write_opnd`].
    inlinable: Vec<bool>,
    /// Dense `raw(block) -> bool`: blocks already emitted (loop bodies self-skip).
    emitted: Vec<bool>,
    /// All-constant `MakeValueStruct` insts hoisted to a module-scope global:
    /// `inst -> group`. A reference emits `hoist_names[group]`; the statement is
    /// skipped (the def is emitted once, before the function).
    hoist: &'a HashMap<Idx<Inst>, u32>,
    /// `group -> global name` for [`Self::hoist`].
    hoist_names: &'a [String],
    state_slots: &'a HashMap<ustr::Ustr, u32>,
    static_slots: &'a HashMap<ustr::Ustr, String>,
    allocation: Option<AllocationEmission<'a>>,
}

impl Emitter<'_> {
    fn has_state_slot(&self, name: ustr::Ustr) -> bool {
        self.state_slots.contains_key(&name) || self.static_slots.contains_key(&name)
    }

    fn write_state_slot(&self, out: &mut String, name: ustr::Ustr) {
        if let Some(&slot) = self.state_slots.get(&name) {
            push_uint(out, slot);
        } else if let Some(slot) = self.static_slots.get(&name) {
            out.push_str(&lua_string(slot));
        } else {
            unreachable!("state slot checked before emission")
        }
    }

    fn record_value_allocation_site(
        &self,
        id: Idx<Inst>,
        inst: &Inst,
        lua_byte_offset: usize,
    ) -> Option<EmittedAllocationSite> {
        let allocation = self.allocation?;
        let kind = emitted_value_allocation_kind(inst)?;
        let site = self.record_allocation_site(allocation, id, kind, lua_byte_offset);
        allocation.instrument.then_some(site)
    }

    fn record_effect_allocation_site(
        &self,
        id: Idx<Inst>,
        inst: &Inst,
        lua_byte_offset: usize,
    ) -> Option<EmittedAllocationSite> {
        let allocation = self.allocation?;
        let kind = emitted_effect_allocation_kind(inst)?;
        let site = self.record_allocation_site(allocation, id, kind, lua_byte_offset);
        allocation.instrument.then_some(site)
    }

    fn record_allocation_site(
        &self,
        allocation: AllocationEmission<'_>,
        id: Idx<Inst>,
        kind: EmittedAllocationKind,
        lua_byte_offset: usize,
    ) -> EmittedAllocationSite {
        let span = self.func.inst_spans.get(id).copied();
        let site = EmittedAllocationSite {
            id: allocation_site_id(allocation.body, id, kind, span),
            kind,
            span,
            lua_body_line: 0,
            lua_byte_offset,
        };
        allocation.sites.borrow_mut().push(site.clone());
        site
    }

    fn emit_block(&mut self, out: &mut String, bb: Idx<Block>) {
        if self.emitted[raw(bb) as usize] {
            return; // already emitted (e.g. as a loop body block)
        }
        self.emitted[raw(bb) as usize] = true;
        out.push_str("  ::bb");
        push_uint(out, self.func.emit_block_id(bb));
        out.push_str("::\n");
        let insts: &[Idx<Inst>] = &self.func.blocks[bb].insts;
        for &id in insts {
            self.emit_inst(out, id);
        }
        // A goto into an un-emitted counted-loop header becomes a structured
        // `for` (the preheader falls through into it); everything else is a goto.
        let term = &self.func.blocks[bb].term;
        if let Term::Goto(t) = term {
            let t = *t;
            if self.loops.contains_key(&t) && !self.emitted[raw(t) as usize] {
                self.expand_for(out, t);
                return;
            }
        }
        self.emit_term(out, &self.func.blocks[bb].term);
    }

    /// Emit a recognized counted loop as `for <slot> = <slot>, hi[-1] do .. end`.
    /// The init reads the slot cell (the preheader's store set it to `lo`), so it
    /// never depends on `lo`'s register being live at the loop head. The header's
    /// compare/branch and the latch's increment are subsumed by the `for`; the
    /// latch keeps only its label so a body `goto bb<latch>` (continue / normal
    /// fall-through) lands at the loop tail and re-iterates.
    fn expand_for(&mut self, out: &mut String, header: Idx<Block>) {
        let cl = &self.loops[&header];
        let (slot, inclusive, body_entry, latch, exit) =
            (cl.slot, cl.inclusive, cl.body_entry, cl.latch, cl.exit);
        let region: HashSet<Idx<Block>> = cl.region.iter().copied().collect();
        let slot_name = cellname(self.func, self.frame, self.cell_colors, slot);
        let hi = match &cl.hi {
            HiLimit::Value(v) => self.opnd_str(*v),
            HiLimit::Cell(c) => cellname(self.func, self.frame, self.cell_colors, *c),
        };
        let limit = if inclusive { hi } else { format!("{hi} - 1") };

        self.emitted[raw(header) as usize] = true;
        let _ = writeln!(out, "  for {slot_name} = {slot_name}, {limit} do");
        // Body-scoped temp registers: declared INSIDE the `for` block, above the
        // loop-control slots, so LuaJIT's snapshot liveness can prune them at
        // loop exits (else they pin their values into every side trace and
        // `asm_head_side` aborts past ~16 inherited FP values — the
        // "NYI: register coalescing too complex" blacklist class).
        let mut own: Vec<u32> = self
            .color_scope
            .iter()
            .enumerate()
            .filter(|(_, s)| **s == Some(header))
            .map(|(c, _)| c as u32)
            .collect();
        if !own.is_empty() {
            own.sort_unstable();
            out.push_str("  local ");
            for (i, &c) in own.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push('r');
                push_uint(out, c);
            }
            out.push('\n');
        }
        self.emit_block(out, body_entry);
        for &bb in self.rpo {
            if region.contains(&bb) {
                self.emit_block(out, bb); // self-skips body_entry / already-emitted
            }
        }
        // Latch label only (the `for` does the increment); then close + the
        // loop-done edge (header's false branch).
        self.emitted[raw(latch) as usize] = true;
        out.push_str("  ::bb");
        push_uint(out, self.func.emit_block_id(latch));
        out.push_str("::\n");
        out.push_str("  end\n");
        out.push_str("  goto bb");
        push_uint(out, self.func.emit_block_id(exit));
        out.push('\n');
    }

    fn emit_inst(&mut self, out: &mut String, id: Idx<Inst>) {
        // A tree-folded value emits NO statement; its expression is rendered at
        // its single use site by `opnd`.
        if self.inlinable[raw(id) as usize] {
            return;
        }
        // A hoisted const ctor emits NO statement -- its value is the module-scope
        // global, referenced via `write_opnd`. (Skips the dead `rN = ctor` a
        // multi-use hoisted const would otherwise materialize.)
        if self.hoist.contains_key(&id) {
            return;
        }
        // An `@inline inline(lua)` call is spliced inline (body in the caller's
        // scope) instead of emitting a `callee(args)` call -- see the method.
        if matches!(&self.func.insts[id], Inst::Call { .. })
            && self.try_emit_inline_lua_splice(out, id)
        {
            return;
        }
        let inst = &self.func.insts[id];
        if inst.produces_value() {
            out.push_str("  ");
            write_vname(out, self.func, &self.colors, id);
            out.push_str(" = ");
            self.write_value(out, id, inst);
            out.push('\n');
        } else {
            self.write_effect(out, id, inst);
        }
    }

    /// Splice an `@inline inline(lua)` `Call` inline, if `id` is one. Returns
    /// true iff it spliced; else the caller emits the normal `callee(args)`.
    /// The emitted symbol this call targets. A call the lowerer gave a concrete
    /// instance (its value args splatted to leaves) targets that instance's
    /// definition. The same instance object drives definition and call spelling,
    /// so the two cannot drift.
    fn callee_name(&self, id: Idx<Inst>, func: FuncRef) -> String {
        match self.func.spec_bindings.get(&id) {
            Some(inst) => inst.symbol(self.ws, self.interner),
            None => mangle_fn(self.ws, func_of(self.ws, func)),
        }
    }

    fn try_emit_inline_lua_splice(&self, out: &mut String, id: Idx<Inst>) -> bool {
        let Inst::Call { func, args } = &self.func.insts[id] else {
            return false;
        };
        let callee = func_of(self.ws, *func);
        self.splice_inline_lua(out, callee, args, id)
    }

    /// Emit `callee`'s `inline(lua)` body inline at this invocation -- a `do … end`
    /// block: each param bound to a `local` = its arg, a tail `return X` rewritten
    /// to `<result> = X`. Returns true iff spliced (false leaves the call to the
    /// caller). Splicing keeps any cdata the body produces in the CALLER's scope
    /// so LuaJIT can sink it; a call boundary forces it to materialize.
    ///
    /// Conservative: only `@inline`, plain-param inline(lua) (no `@scalar_abi`
    /// leaf expansion) whose body is a single bare expression, a statement-shaped
    /// unit body, or a single TAIL `return`. Early / multiple returns fall back to
    /// a call -- rewriting those line-by-line would drop the control flow.
    fn splice_inline_lua(
        &self,
        out: &mut String,
        callee: &Function,
        args: &[Idx<Inst>],
        result: Idx<Inst>,
    ) -> bool {
        // Only a lua-targeted inline body can splice into lua output (a js body
        // would be nonsense here).
        let Some((bone_semantic::hir::FnBackend::Lua, body)) = callee.inline_body else {
            return false;
        };
        let ann = |n: &str| callee.annotations.iter().any(|a| a.name.name.as_str() == n);
        if !ann("inline") || ann("scalar_abi") {
            return false;
        }
        if callee.param_decls.len() != args.len() {
            return false;
        }
        let Some(shape) = classify_inline_lua_body(body.as_str()) else {
            return false;
        };
        // A `Call`/`Index` value is never tree-folded here (`always_inlinable`
        // excludes them), so `result` has a register.
        let mut lv = String::new();
        write_vname(&mut lv, self.func, &self.colors, result);

        out.push_str("  do\n");
        for (p, a) in callee.param_decls.iter().zip(args.iter()) {
            let _ = write!(out, "    local {} = ", p.name);
            self.write_opnd(out, *a);
            out.push('\n');
        }
        render_spliced_inline_lua(out, body.as_str(), shape, &lv);
        out.push_str("  end\n");
        true
    }

    /// Render a reference to value `id` as an operand directly into `out`: its
    /// register `r<color>`, or — if it was tree-folded — its expression in
    /// parentheses, recursively. Every value-operand reference goes through here.
    fn write_opnd(&self, out: &mut String, id: Idx<Inst>) {
        // int->float subsumption is identity on luajit (all doubles): a FOLDED
        // (inlinable) coercion renders exactly as the inner operand in this
        // position -- no register, no extra precedence wrap -- so emitted Lua
        // stays byte-identical. A MATERIALIZED one (its operand is reused, e.g.
        // an @inline-spliced arg) keeps its own register (fall through) and the
        // `rN = <inner>` def carries the identity.
        if self.inlinable[raw(id) as usize] {
            if let Inst::IntToFloat(x) = self.func.insts[id] {
                return self.write_opnd(out, x);
            }
        }
        // A hoisted all-constant value ctor renders as its module-scope global,
        // whether it would otherwise inline or materialize a register.
        if let Some(&g) = self.hoist.get(&id) {
            out.push_str(&self.hoist_names[g as usize]);
            return;
        }
        if self.inlinable[raw(id) as usize] {
            // Always parenthesize the folded expression so it composes safely in
            // the parent context regardless of precedence/adjacency. (Notably a
            // bare negative literal under unary `-` would form `--` — a Lua
            // comment — without the wrap.)
            out.push('(');
            self.write_value(out, id, &self.func.insts[id]);
            out.push(')');
        } else {
            write_vname(out, self.func, &self.colors, id);
        }
    }

    /// Collect the flat operand list of a `StringConcat` tree, descending into
    /// child `StringConcat`s that are inlined here (single-use, folded into this
    /// expression). A non-inlinable child stays a leaf -- it is a separate `..`
    /// already materialized in its own cell, so flattening past it would be wrong.
    fn collect_concat(&self, parts: &mut Vec<Idx<Inst>>, id: Idx<Inst>) {
        if self.inlinable[raw(id) as usize] {
            if let Inst::StringConcat(a, b) = &self.func.insts[id] {
                self.collect_concat(parts, *a);
                self.collect_concat(parts, *b);
                return;
            }
        }
        parts.push(id);
    }

    /// `write_opnd` into a fresh `String` — for the few cold sites that need an
    /// owned operand string (loop-limit arithmetic in `expand_for`).
    fn opnd_str(&self, id: Idx<Inst>) -> String {
        let mut s = String::new();
        self.write_opnd(&mut s, id);
        s
    }

    fn write_value(&self, out: &mut String, id: Idx<Inst>, inst: &Inst) {
        let instrument = self.record_value_allocation_site(id, inst, out.len());
        if let Some(ref site) = instrument {
            out.push_str("__rock_alloc_hit(\"");
            write!(out, "{:016x}", site.id).unwrap();
            out.push_str("\", ");
        }
        self.write_value_inner(out, id, inst);
        if instrument.is_some() {
            out.push(')');
        }
    }

    /// The RHS expression for a value-producing instruction.
    fn write_value_inner(&self, out: &mut String, id: Idx<Inst>, inst: &Inst) {
        match inst {
            Inst::Const(l) => write_lit(out, l),
            Inst::Global(g) => {
                if let Some(identity) = self.func.code_value_identities.get(&id) {
                    out.push_str("__rock_code_slot_ref(");
                    out.push_str(&lua_string(&identity.logical_key(self.ws, self.interner)));
                    out.push(')');
                } else if self.has_state_slot(*g) {
                    out.push_str("__rock_active_session.__rock_state[");
                    self.write_state_slot(out, *g);
                    out.push(']');
                } else {
                    out.push_str(g.as_str());
                }
            }
            Inst::Load(c) => write_cellname(out, self.func, self.frame, self.cell_colors, *c),

            Inst::IntAdd(a, b) | Inst::FloatAdd(a, b) => self.write_bin(out, "+", a, b),
            Inst::IntSub(a, b) | Inst::FloatSub(a, b) => self.write_bin(out, "-", a, b),
            Inst::IntMul(a, b) | Inst::FloatMul(a, b) => self.write_bin(out, "*", a, b),
            Inst::UintAdd(a, b) => self.write_u32_bin(out, "+", a, b),
            Inst::UintSub(a, b) => self.write_u32_bin(out, "-", a, b),
            Inst::UintMul(a, b) => self.write_u32_mul(out, a, b),
            // Integer `/` is floor-division (rock semantics); float `/` is real.
            Inst::IntDiv(a, b) | Inst::UintDiv(a, b) => {
                out.push_str("math.floor(");
                self.write_opnd(out, *a);
                out.push_str(" / ");
                self.write_opnd(out, *b);
                out.push(')');
            }
            Inst::FloatDiv(a, b) => self.write_bin(out, "/", a, b),
            Inst::IntMod(a, b) | Inst::FloatMod(a, b) => self.write_bin(out, "%", a, b),
            Inst::UintMod(a, b) => self.write_u32_bin(out, "%", a, b),
            Inst::StringConcat(a, b) => {
                // Emit the whole inlinable concat tree as ONE flat `..` run
                // (`x .. y .. z`) rather than left-nested binary concats
                // (`((x .. y) .. z)`). LuaJIT's BC_CAT folds a flat run into a
                // SINGLE string allocation; nested binary `..` allocates an
                // intermediate per step (interpolation is ~2x the allocations
                // otherwise -- the dominant skull-vs-rock HUD/text alloc gap).
                let mut parts = Vec::new();
                self.collect_concat(&mut parts, *a);
                self.collect_concat(&mut parts, *b);
                for (i, p) in parts.iter().enumerate() {
                    if i > 0 {
                        out.push_str(" .. ");
                    }
                    self.write_opnd(out, *p);
                }
            }
            Inst::ToString(x) => {
                out.push_str("tostring(");
                self.write_opnd(out, *x);
                out.push(')');
            }
            Inst::Format { value, fmt } => {
                out.push_str("string.format(");
                write_lua_string(out, fmt.as_str());
                out.push_str(", ");
                self.write_opnd(out, *value);
                out.push(')');
            }
            Inst::FormatDyn { value, fmt } => {
                out.push_str("string.format(");
                self.write_opnd(out, *fmt);
                out.push_str(", ");
                self.write_opnd(out, *value);
                out.push(')');
            }
            Inst::FloatToInt(x) => {
                out.push_str("math.floor(");
                self.write_opnd(out, *x);
                out.push(')');
            }
            // int->float subsumption is invisible on luajit (all doubles): render
            // the inner value's expression verbatim (identity). Reached only if a
            // pass ever materializes one; the operand path (`write_opnd`) folds it
            // away. `write_value` (not `write_opnd`) to avoid a double wrap.
            Inst::IntToFloat(x) | Inst::UintToFloat(x) | Inst::UintToInt(x) => {
                self.write_value(out, *x, &self.func.insts[*x])
            }
            Inst::IntToUint(x) | Inst::FloatToUint(x) => {
                self.write_checked_u32(out, *x, "uint conversion out of range")
            }
            Inst::UintBitcastInt(x) => {
                out.push_str("(((");
                self.write_opnd(out, *x);
                out.push_str(" + 2147483648) % 4294967296) - 2147483648)");
            }
            Inst::IntBitcastUint(x) => {
                out.push_str("((");
                self.write_opnd(out, *x);
                out.push_str(") % 4294967296)");
            }

            Inst::IntLt(a, b) | Inst::UintLt(a, b) | Inst::FloatLt(a, b) => {
                self.write_bin(out, "<", a, b)
            }
            Inst::IntLeq(a, b) | Inst::UintLeq(a, b) | Inst::FloatLeq(a, b) => {
                self.write_bin(out, "<=", a, b)
            }
            Inst::IntGt(a, b) | Inst::UintGt(a, b) | Inst::FloatGt(a, b) => {
                self.write_bin(out, ">", a, b)
            }
            Inst::IntGeq(a, b) | Inst::UintGeq(a, b) | Inst::FloatGeq(a, b) => {
                self.write_bin(out, ">=", a, b)
            }
            Inst::Eq(a, b) => self.write_bin(out, "==", a, b),
            Inst::NotEq(a, b) => self.write_bin(out, "~=", a, b),

            Inst::Neg(x) => {
                out.push('-');
                self.write_opnd(out, *x);
            }
            Inst::Not(x) => {
                out.push_str("not ");
                self.write_opnd(out, *x);
            }
            Inst::BitNot(x) => {
                out.push_str("bit.bnot(");
                self.write_opnd(out, *x);
                out.push(')');
            }
            Inst::UintBitNot(x) => {
                out.push_str("(bit.bnot(");
                self.write_opnd(out, *x);
                out.push_str(")) % 4294967296");
            }

            Inst::BitAnd(a, b) => self.write_bitfn(out, "bit.band", a, b),
            Inst::BitOr(a, b) => self.write_bitfn(out, "bit.bor", a, b),
            Inst::BitXor(a, b) => self.write_bitfn(out, "bit.bxor", a, b),
            Inst::Shl(a, b) => self.write_bitfn(out, "bit.lshift", a, b),
            Inst::Shr(a, b) => self.write_bitfn(out, "bit.rshift", a, b),
            Inst::UintBitAnd(a, b) => self.write_u32_bitfn(out, "bit.band", a, b),
            Inst::UintBitOr(a, b) => self.write_u32_bitfn(out, "bit.bor", a, b),
            Inst::UintBitXor(a, b) => self.write_u32_bitfn(out, "bit.bxor", a, b),
            Inst::UintShl(a, b) => self.write_u32_shift(out, "bit.lshift", a, b),
            Inst::UintShr(a, b) => {
                out.push_str("(function(value, count) return math.floor(value / (2 ^ (count <= 31 and count or error('uint shift count out of range')))) end)(");
                self.write_opnd(out, *a);
                out.push_str(", ");
                self.write_opnd(out, *b);
                out.push(')');
            }

            Inst::Call { func, args } => {
                let callee = self.callee_name(id, *func);
                out.push_str(&callee);
                out.push('(');
                self.write_csv(out, args);
                out.push(')');
            }
            Inst::CallValue { callee, args } => {
                self.write_opnd(out, *callee);
                out.push('(');
                self.write_csv(out, args);
                out.push(')');
            }
            Inst::Lambda { .. } => {
                unreachable!("residual Inst::Lambda reached Lua emission")
            }

            Inst::MakeTuple(vs) => {
                out.push('{');
                self.write_csv(out, vs);
                out.push('}');
            }
            Inst::MakeVec(vs) => {
                out.push_str("{_data = {");
                self.write_csv(out, vs);
                let _ = write!(out, "}}, len = {}, __ty = \"Vec\"}}", vs.len());
            }
            // Column storage: one array per leaf, sharing the single `len`.
            // Keys are written in brackets because a nested leaf path is dotted
            // (`min.x`), which is not a bare Lua identifier.
            Inst::MakeVecColumns {
                elem,
                columns,
                len,
                capacity,
                ..
            } => {
                out.push_str("{[");
                write_lua_string(out, skull_mir::mir::VEC_COLUMN_HEADER_FIELD);
                out.push_str("] = {repr = ");
                write_lua_string(out, skull_mir::mir::VEC_COLUMN_REPR);
                out.push_str(", elem = ");
                write_lua_string(out, elem.as_str());
                out.push_str(", cap = ");
                self.write_opnd(out, *capacity);
                out.push_str("}, ");
                for (name, vs) in columns {
                    out.push('[');
                    write_lua_string(out, name.as_str());
                    out.push_str("] = {");
                    self.write_csv(out, vs);
                    out.push_str("}, ");
                }
                let _ = write!(out, "len = {len}, __ty = \"Vec\"}}");
            }
            Inst::MakeVecColumnHeader { elem, capacity, .. } => {
                out.push_str("{repr = ");
                write_lua_string(out, skull_mir::mir::VEC_COLUMN_REPR);
                out.push_str(", elem = ");
                write_lua_string(out, elem.as_str());
                out.push_str(", cap = ");
                self.write_opnd(out, *capacity);
                out.push('}');
            }
            Inst::MakeStruct { ty, fields } => {
                out.push('{');
                let mut first = true;
                if let Some(name) = ty {
                    out.push_str("__ty = ");
                    write_lua_string(out, name.as_str());
                    first = false;
                }
                for (n, id) in fields {
                    if !first {
                        out.push_str(", ");
                    }
                    first = false;
                    write_table_key(out, n.as_str());
                    self.write_opnd(out, *id);
                }
                out.push('}');
            }
            Inst::MakeValueStruct { name, representation, form, fields }
            | Inst::MaterializeValueStruct { name, representation, form, fields } => {
                let Some(suffix) = ctor_suffix(self.func, *representation, *form) else {
                    self.ctx.reject(
                        name.to_string(),
                        CompilerError::new_no_location(format!(
                            "`{name}` value constructor has no valid supplied representation plan"
                        )),
                    );
                    return;
                };
                let _ = write!(out, "{name}{suffix}(");
                for (i, (_, id)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    self.write_opnd(out, *id);
                }
                out.push(')');
            }
            Inst::MakeEnum {
                tag,
                ty,
                positional,
                named,
            } => {
                out.push_str("{__variant = ");
                write_lua_string(out, tag.as_str());
                out.push_str(", __ty = ");
                write_lua_string(out, ty.as_str());
                for id in positional {
                    out.push_str(", ");
                    self.write_opnd(out, *id);
                }
                for (n, id) in named {
                    let _ = write!(out, ", {n} = ");
                    self.write_opnd(out, *id);
                }
                out.push('}');
            }
            Inst::MakeEnumDyn { tag, ty, positional, named } => {
                out.push_str("{__variant = ");
                self.write_opnd(out, *tag);
                out.push_str(", __ty = ");
                write_lua_string(out, ty.as_str());
                for id in positional {
                    out.push_str(", ");
                    self.write_opnd(out, *id);
                }
                for (n, id) in named {
                    let _ = write!(out, ", {n} = ");
                    self.write_opnd(out, *id);
                }
                out.push('}');
            }

            Inst::FfiNewArray { ctype, count, .. } => {
                let _ = write!(out, "ffi.new(\"{ctype}[?]\", ");
                self.write_opnd(out, *count);
                out.push(')');
            }
            Inst::FfiViewCast { cty, ptr } => {
                let _ = write!(out, "ffi.cast(\"{cty}*\", ");
                self.write_opnd(out, *ptr);
                out.push(')');
            }
            Inst::Field { base, field } => {
                self.write_opnd(out, *base);
                write_field_key(out, field.as_str());
            }
            Inst::TupleIndex { base, index } => {
                self.write_opnd(out, *base);
                out.push('[');
                push_uint(out, (index + 1) as u32);
                out.push(']');
            }

            Inst::Index { base, index, kind } => self.write_index(out, *base, *index, *kind),
            Inst::VecCheckedIndex { vec, index } => {
                out.push_str("__vec_checked_index(");
                self.write_opnd(out, *vec);
                out.push_str(", ");
                self.write_opnd(out, *index);
                out.push(')');
            }
            Inst::Len { base, kind } => match kind {
                ContainerKind::Vec
                | ContainerKind::VecValueColumns(_)
                | ContainerKind::NativeBuffer
                | ContainerKind::NativeBufferValue(_) => {
                    self.write_opnd(out, *base);
                    out.push_str(".len");
                }
                // Neither a bare leaf column nor a leaf VIEW has a `len` of its
                // own -- the owning container holds the single length, so
                // lowering asks it there.
                ContainerKind::NativeBufferLeaf => {
                    unreachable!("`len` of a leaf view: lowering must ask the owning buffer")
                }
                ContainerKind::VecColumn => {
                    out.push('#');
                    self.write_opnd(out, *base);
                }
                ContainerKind::Map => {
                    // No O(1) length on a Map; lowering shouldn't emit this kind.
                    out.push('#');
                    self.write_opnd(out, *base);
                }
            },

            // Effects don't produce values.
            Inst::Store { .. }
            | Inst::StoreField { .. }
            | Inst::StoreIndex { .. }
            | Inst::StoreGlobal { .. }
            | Inst::MapNext { .. }
            | Inst::CallMulti { .. }
            | Inst::CallValueMulti { .. }
            | Inst::CoroSetInner { .. }
            | Inst::CoroHook { .. }
            | Inst::ArrayPush { .. } => unreachable!("effect has no value"),

            // Substituted by the inliner (or rejected by mir_emit's residual
            // check) long before codegen.
            Inst::CallBoundMulti { .. } => {
                unreachable!("placeholder bound-call must be substituted before emit")
            }

            // The shared closure-conversion pass (`skull_transform::convert_closures`)
            // materializes a capturing lambda as an env record and a shared mutable
            // capture as a box, both as `MakeEnv`. `fn_ref = Some(name)` is a
            // callable record `setmetatable({ __slot = ..., c<raw> = .., .. },
            // __rock_fn_mt)` (invoked via `__call`); `fn_ref = None` is a box `{}`
            // / `{ v = <seed> }`. Field access (`e.c<raw>`, `.v`) already flows
            // through `Inst::Field`/`Inst::StoreField`. EVERY servable path (ordinary
            // fns AND the coroutine step, S2+S3) now routes through the pass, so
            // `MakeEnv` is the only capturing closure shape codegen sees.
            Inst::MakeEnv { fn_ref, fields } => match fn_ref {
                Some(_name) => {
                    out.push_str("setmetatable({ ");
                    let identity = self
                        .func
                        .code_value_identities
                        .get(&id)
                        .expect("Rock closure MakeEnv missing compiler-owned CodeRef identity");
                    out.push_str("__slot = __rock_code_slot_ref(");
                    out.push_str(&lua_string(&identity.logical_key(self.ws, self.interner)));
                    out.push(')');
                    for (f, v) in fields {
                        out.push_str(", ");
                        out.push_str(f.as_str());
                        out.push_str(" = ");
                        self.write_opnd(out, *v);
                    }
                    out.push_str(" }, __rock_fn_mt)");
                }
                None => {
                    out.push('{');
                    for (i, (f, v)) in fields.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        out.push_str(f.as_str());
                        out.push_str(" = ");
                        self.write_opnd(out, *v);
                    }
                    out.push('}');
                }
            },
        }
    }

    /// `a <op> b` written directly into `out` (no per-operand allocation).
    fn write_bin(&self, out: &mut String, op: &str, a: &Idx<Inst>, b: &Idx<Inst>) {
        self.write_opnd(out, *a);
        out.push(' ');
        out.push_str(op);
        out.push(' ');
        self.write_opnd(out, *b);
    }

    /// `f(a, b)` (bit-library call shape) written directly into `out`.
    fn write_bitfn(&self, out: &mut String, f: &str, a: &Idx<Inst>, b: &Idx<Inst>) {
        out.push_str(f);
        out.push('(');
        self.write_opnd(out, *a);
        out.push_str(", ");
        self.write_opnd(out, *b);
        out.push(')');
    }

    fn write_u32_bin(&self, out: &mut String, op: &str, a: &Idx<Inst>, b: &Idx<Inst>) {
        out.push_str("(");
        self.write_bin(out, op, a, b);
        out.push_str(") % 4294967296");
    }

    fn write_u32_bitfn(&self, out: &mut String, f: &str, a: &Idx<Inst>, b: &Idx<Inst>) {
        out.push('(');
        self.write_bitfn(out, f, a, b);
        out.push_str(" % 4294967296)");
    }

    fn write_u32_shift(&self, out: &mut String, f: &str, a: &Idx<Inst>, b: &Idx<Inst>) {
        out.push_str("(function(value, count) return ");
        out.push_str(f);
        out.push_str("(value, (count <= 31 and count or error('uint shift count out of range'))) % 4294967296 end)(");
        self.write_opnd(out, *a);
        out.push_str(", ");
        self.write_opnd(out, *b);
        out.push(')');
    }

    fn write_u32_mul(&self, out: &mut String, a: &Idx<Inst>, b: &Idx<Inst>) {
        // Every product here is below 2^53. Splitting at 16 bits avoids the
        // double-rounding that a direct Lua multiplication would incur.
        out.push_str("((");
        self.write_opnd(out, *a);
        out.push_str(" % 65536) * (");
        self.write_opnd(out, *b);
        out.push_str(" % 65536) + ((math.floor(");
        self.write_opnd(out, *a);
        out.push_str(" / 65536) * (");
        self.write_opnd(out, *b);
        out.push_str(" % 65536) + (");
        self.write_opnd(out, *a);
        out.push_str(" % 65536) * math.floor(");
        self.write_opnd(out, *b);
        out.push_str(" / 65536)) % 65536) * 65536) % 4294967296");
    }

    fn write_checked_u32(&self, out: &mut String, x: Idx<Inst>, message: &str) {
        out.push_str("(function(value) if value >= 0 and value <= 4294967295 then return math.floor(value) else error(");
        write_lua_string(out, message);
        out.push_str(") end end)(");
        self.write_opnd(out, x);
        out.push(')');
    }

    /// READ index `base[index]`. NativeBuffer and Vec go through the
    /// debug-mode bounds-checked stdlib helpers (`__nb_checked_get` /
    /// `__vec_checked_get`) — skull runs debug; release stripping is future
    /// release-mode work. Map indexes raw (miss returns nil; `.get` is the
    /// checked form).
    fn write_index(&self, out: &mut String, base: Idx<Inst>, index: Idx<Inst>, kind: ContainerKind) {
        match kind {
            ContainerKind::Vec => {
                out.push_str("__vec_checked_get(");
                self.write_opnd(out, base);
                out.push_str(", ");
                self.write_opnd(out, index);
                out.push(')');
            }
            // A leaf column: `index` is already 1-based and already checked
            // (`VecCheckedIndex`), so this is a raw array read.
            ContainerKind::VecColumn => {
                self.write_opnd(out, base);
                out.push('[');
                self.write_opnd(out, index);
                out.push(']');
            }
            // A whole-element read has no single slot to name -- column storage
            // has no element table. Lowering decomposes every such read into
            // per-column reads (`column_element_repr`), so reaching emit means
            // that decomposition was bypassed. Fail loudly: emitting ANY Lua
            // here would be silently wrong code.
            ContainerKind::VecValueColumns(elem) => {
                unreachable!(
                    "whole-element index of column-stored [{elem}] reached emit, in `{}`",
                    self.func.name
                )
            }
            ContainerKind::NativeBuffer | ContainerKind::NativeBufferValue(_) => {
                out.push_str("__nb_checked_get(");
                self.write_opnd(out, base);
                out.push_str(", ");
                self.write_opnd(out, index);
                out.push(')');
            }
            // A leaf view is a raw C pointer: index it directly. The index is
            // absolute and the ELEMENT access was bounds-checked once already.
            ContainerKind::NativeBufferLeaf | ContainerKind::Map => {
                self.write_opnd(out, base);
                out.push('[');
                self.write_opnd(out, index);
                out.push(']');
            }
        }
    }

    fn write_effect(&self, out: &mut String, id: Idx<Inst>, inst: &Inst) {
        if let Some(site) = self.record_effect_allocation_site(id, inst, out.len()) {
            out.push_str("  __rock_alloc_hit_effect(\"");
            write!(out, "{:016x}", site.id).unwrap();
            out.push_str("\")\n");
        }
        match inst {
            Inst::Store { cell, value } => {
                out.push_str("  ");
                write_cellname(out, self.func, self.frame, self.cell_colors, *cell);
                out.push_str(" = ");
                self.write_opnd(out, *value);
                out.push('\n');
            }
            Inst::StoreField { base, field, value } => {
                out.push_str("  ");
                self.write_opnd(out, *base);
                write_field_key(out, field.as_str());
                out.push_str(" = ");
                self.write_opnd(out, *value);
                out.push('\n');
            }
            Inst::StoreIndex {
                base,
                index,
                value,
                kind,
            } => {
                match kind {
                    // NativeBuffer write -> debug-mode bounds-checked helper.
                    ContainerKind::NativeBuffer | ContainerKind::NativeBufferValue(_) => {
                        out.push_str("  __nb_checked_set(");
                        self.write_opnd(out, *base);
                        out.push_str(", ");
                        self.write_opnd(out, *index);
                        out.push_str(", ");
                        self.write_opnd(out, *value);
                        out.push_str(")\n");
                    }
                    ContainerKind::Vec => {
                        out.push_str("  __vec_checked_set(");
                        self.write_opnd(out, *base);
                        out.push_str(", ");
                        self.write_opnd(out, *index);
                        out.push_str(", ");
                        self.write_opnd(out, *value);
                        out.push_str(")\n");
                    }
                    // Raw 1-based write into one leaf column; the owning Vec's
                    // `len` was checked once by `VecCheckedIndex`.
                    ContainerKind::Map
                    | ContainerKind::VecColumn
                    | ContainerKind::NativeBufferLeaf => {
                        out.push_str("  ");
                        self.write_opnd(out, *base);
                        out.push('[');
                        self.write_opnd(out, *index);
                        out.push_str("] = ");
                        self.write_opnd(out, *value);
                        out.push('\n');
                    }
                    // One `value` operand cannot carry K leaves; lowering splits
                    // an element write into per-column stores.
                    ContainerKind::VecValueColumns(elem) => {
                        unreachable!("whole-element store into column-stored [{elem}] reached emit")
                    }
                }
            }
            Inst::StoreGlobal { name, value } => {
                if self.has_state_slot(*name) {
                    out.push_str("  __rock_active_session.__rock_state[");
                    self.write_state_slot(out, *name);
                    out.push_str("] = ");
                    self.write_opnd(out, *value);
                    out.push_str("\n  __rock_active_session.__rock_initialized[");
                    self.write_state_slot(out, *name);
                    out.push_str("] = true\n");
                } else {
                    let _ = write!(out, "  {name} = ");
                    self.write_opnd(out, *value);
                    out.push('\n');
                }
            }
            Inst::ArrayPush { arr, value, kind } => {
                // `arr` is referenced up to 4x; render it once.
                let a = self.opnd_str(*arr);
                match kind {
                    ContainerKind::Vec => {
                        // increment len BEFORE assign (1-indexed data table).
                        let _ = writeln!(out, "  {a}.len = {a}.len + 1");
                        let _ = write!(out, "  {a}._data[{a}.len] = ");
                        self.write_opnd(out, *value);
                        out.push('\n');
                    }
                    // Column push bumps the shared `len` once, then writes K
                    // columns -- more than one `value`, so lowering splits it.
                    ContainerKind::NativeBuffer
                    | ContainerKind::NativeBufferValue(_)
                    | ContainerKind::NativeBufferLeaf
                    | ContainerKind::VecValueColumns(_)
                    | ContainerKind::VecColumn
                    | ContainerKind::Map => {
                        unreachable!("ArrayPush only lowers for Vec array literals, got {kind:?}")
                    }
                }
            }
            Inst::MapNext {
                key_cell,
                val_cell,
                table,
            } => {
                // `key_cell` is referenced twice; render it once.
                let k = cellname(self.func, self.frame, self.cell_colors, *key_cell);
                let _ = write!(out, "  {k}, ");
                write_cellname(out, self.func, self.frame, self.cell_colors, *val_cell);
                let _ = write!(out, " = next(");
                self.write_opnd(out, *table);
                let _ = writeln!(out, ", {k})");
            }
            // Scalar-ABI multi-return: `d0, d1, ... = callee(args)`. The callee is
            // a value-returning fn whose return is scalarized to N leaf values.
            Inst::CallMulti { dests, func, args } => {
                out.push_str("  ");
                for (i, c) in dests.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write_cellname(out, self.func, self.frame, self.cell_colors, *c);
                }
                let callee = self.callee_name(id, *func);
                let _ = write!(out, " = {callee}(");
                self.write_csv(out, args);
                out.push_str(")\n");
            }
            // Indirect multi-return: `d0, d1, ... = callee(args)` -- a tuple
            // returned through a fn-value. Multi-assign so Lua doesn't truncate.
            Inst::CallValueMulti { dests, callee, args } => {
                out.push_str("  ");
                for (i, c) in dests.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write_cellname(out, self.func, self.frame, self.cell_colors, *c);
                }
                out.push_str(" = ");
                self.write_opnd(out, *callee);
                out.push('(');
                self.write_csv(out, args);
                out.push_str(")\n");
            }
            // Rewritten to `s.__inner = ..` by the coroutine transform; if it
            // reaches codegen the transform didn't run (a bug).
            Inst::CoroSetInner(_) => unreachable!("CoroSetInner must be removed by the coroutine transform"),
            // Relocated to the constructor by the coroutine transform.
            Inst::CoroHook { .. } => unreachable!("CoroHook must be relocated by the coroutine transform"),
            _ => unreachable!("not an effect"),
        }
    }

    fn emit_term(&self, out: &mut String, term: &Term) {
        match term {
            // luajit rule 2: `return` wrapped `do ... end` so it is never the
            // block's mid-sequence statement (and is legal as the last one too).
            Term::Return(vals) if !vals.is_empty() => {
                out.push_str("  do return ");
                self.write_csv(out, vals);
                out.push_str(" end\n");
            }
            Term::Return(_) | Term::Unreachable => {
                out.push_str("  do return end\n");
            }
            Term::Goto(b) => {
                out.push_str("  goto bb");
                push_uint(out, self.func.emit_block_id(*b));
                out.push('\n');
            }
            Term::Branch {
                cond,
                then_bb,
                else_bb,
            } => {
                out.push_str("  if ");
                self.write_opnd(out, *cond);
                out.push_str(" then goto bb");
                push_uint(out, self.func.emit_block_id(*then_bb));
                out.push_str(" else goto bb");
                push_uint(out, self.func.emit_block_id(*else_bb));
                out.push_str(" end\n");
            }
            Term::Switch {
                discr,
                cases,
                default,
            } => {
                // `discr` is compared in every case arm; render it once.
                let d = self.opnd_str(*discr);
                let mut first = true;
                for (l, b) in cases {
                    let kw = if first { "if" } else { "elseif" };
                    first = false;
                    let _ = write!(out, "  {kw} {d} == ");
                    write_lit(out, l);
                    out.push_str(" then goto bb");
                    push_uint(out, self.func.emit_block_id(*b));
                    out.push('\n');
                }
                if first {
                    out.push_str("  goto bb");
                    push_uint(out, self.func.emit_block_id(*default));
                    out.push('\n');
                } else {
                    out.push_str("  else goto bb");
                    push_uint(out, self.func.emit_block_id(*default));
                    out.push_str(" end\n");
                }
            }
            Term::Yield { .. } => {
                unreachable!("Yield must be removed by the coroutine transform before codegen")
            }
        }
    }

    fn write_csv(&self, out: &mut String, ids: &[Idx<Inst>]) {
        for (i, id) in ids.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            self.write_opnd(out, *id);
        }
    }

}

fn write_lit(out: &mut String, l: &Lit) {
    match l {
        Lit::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Lit::Uint(i) => {
            let _ = write!(out, "{i}");
        }
        Lit::Float(x) => {
            let _ = write!(out, "{x:?}");
        }
        Lit::Bool(b) => {
            let _ = write!(out, "{b}");
        }
        Lit::Str(s) => write_lua_string(out, s.as_str()),
        Lit::Nil => out.push_str("nil"),
    }
}

/// Escape `s` as a double-quoted Lua string literal (also used by the workspace
/// emitter for inline(lua) default-value rendering).
pub fn lua_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    write_lua_string(&mut out, s);
    out
}

/// A Rock `[string]` value as a Lua literal.
///
/// The Vec representation is owned by `Inst::MakeVec` above; this is the same
/// shape, exposed so a compiler-emitted call site (the `rock_shader`
/// registrations in `skull::mir_emit`) cannot drift from it.
pub fn lua_string_vec(items: &[&str]) -> String {
    let mut out = String::from("{_data = {");
    for (i, s) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_lua_string(&mut out, s);
    }
    let _ = write!(out, "}}, len = {}, __ty = \"Vec\"}}", items.len());
    out
}

/// The `[int]` sibling of [`lua_string_vec`], for driver text handing the engine
/// a list of integers (a uniform block's member offsets).
pub fn lua_int_vec(items: &[i64]) -> String {
    let mut out = String::from("{_data = {");
    for (i, n) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "{n}");
    }
    let _ = write!(out, "}}, len = {}, __ty = \"Vec\"}}", items.len());
    out
}

/// A field key is a Lua identifier unless it is an `embed` leaf's physical key,
/// which is a dotted path (`"p.x"`). Lua keywords never reach here -- the
/// tokenizer already escapes them to `__rock_<kw>` -- so anything non-identifier
/// is a compiler-minted physical key. Constant-string bracket indexing compiles
/// to the same `TGETS` as dot sugar, so the bracket form costs nothing.
fn is_lua_ident(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Suffix position: `base.f` or `base["a.b"]`.
fn write_field_key(out: &mut String, field: &str) {
    if is_lua_ident(field) {
        let _ = write!(out, ".{field}");
    } else {
        out.push('[');
        write_lua_string(out, field);
        out.push(']');
    }
}

/// Table-constructor key position: `f = ` or `["a.b"] = `.
fn write_table_key(out: &mut String, field: &str) {
    if is_lua_ident(field) {
        let _ = write!(out, "{field} = ");
    } else {
        out.push('[');
        write_lua_string(out, field);
        out.push_str("] = ");
    }
}

/// Append the double-quoted, escaped Lua string literal for `s` into `out`.
fn write_lua_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
}
