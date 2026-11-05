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
//! is a later refinement.)

mod frame_limits;
mod regalloc;
mod trace_frames;

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::cell::RefCell;
use std::fmt::Write as _;
use std::sync::Mutex;

use bone_semantic::hir::Function;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::{CompilerError, Span};
use comfy_la_arena::Idx;
use skull_mir::cfg::{PredMap, predecessors, reachable_blocks_from, reverse_postorder, successors};
use skull_mir::mir::*;
use skull_mir::monoty::MonoInterner;

pub use regalloc::color_cells;
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
    pub mir_inst: u32,
    lua_byte_offset: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LuaEmission {
    pub lua: String,
    pub allocation_sites: Vec<EmittedAllocationSite>,
}

/// Shared validation for numeric uint conversions and shift counts.
pub const UINT_CHECK_PRELUDE: &str = r#"
function __rock_checked_u32(value)
  if value >= 0 and value <= 4294967295 then
    return math.floor(value)
  else
    error("uint conversion out of range")
  end
end
function __rock_u32_shift_count(count)
  if count <= 31 then return count else error('uint shift count out of range') end
end
"#;

fn checked_u32_constant(func: &MirFunc, x: Idx<Inst>) -> Option<f64> {
    let value = match func.insts[x] {
        Inst::Const(Lit::Int(value)) => value as f64,
        Inst::Const(Lit::Uint(value)) => value as f64,
        Inst::Const(Lit::Float(value)) => value,
        _ => return None,
    };
    (0.0..=4294967295.0).contains(&value).then(|| value.floor())
}

#[cfg(test)]
mod uint_constant_tests {
    use super::*;

    #[test]
    fn uint_constant_folding_keeps_invalid_values_checked() {
        let mut func = MirFunc::new(ustr::Ustr::from("checked"));
        for value in [
            Lit::Int(-1),
            Lit::Int(4294967296),
            Lit::Float(-0.1),
            Lit::Float(4294967295.5),
            Lit::Float(f64::NAN),
            Lit::Float(f64::INFINITY),
            Lit::Float(f64::NEG_INFINITY),
        ] {
            let x = func.alloc_inst(Inst::Const(value));
            assert_eq!(checked_u32_constant(&func, x), None);
        }
        let x = func.alloc_inst(Inst::Const(Lit::Float(-0.0)));
        assert_eq!(
            checked_u32_constant(&func, x).unwrap().to_bits(),
            (-0.0f64).to_bits()
        );
    }
}

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
  _G.__rock_code_slot_pending = {}
  _G.__rock_code_slot_pending_slots = {}
  _G.__rock_definition_plan_materializing = false
  _G.__rock_lifecycle_plan_pending = nil
  _G.__rock_definition_plan_pending = nil
  _G.__rock_definition_materializers_pending = nil
end
function __rock_code_slot_abort()
  _G.__rock_code_slot_pending = nil
  _G.__rock_code_slot_pending_slots = nil
  _G.__rock_definition_plan_materializing = false
  _G.__rock_lifecycle_plan_pending = nil
  _G.__rock_definition_plan_pending = nil
  _G.__rock_definition_materializers_pending = nil
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
function __rock_definition_plan_stage(plan, materializers)
  if not _G.__rock_code_slot_pending then error("Rock code slot stage not open", 0) end
  _G.__rock_definition_plan_pending = plan
  _G.__rock_definition_materializers_pending = materializers
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
function __rock_definition_plan_materialize(plan, materializers)
  _G.__rock_definition_plan_materializing = true
  for _, catalog in pairs(plan) do
    if type(catalog) == "table" and catalog._data and catalog.len then
      for i = 1, catalog.len do
        local entry = catalog._data[i]
        if entry and entry.factory ~= nil then
          if type(entry.factory) == "function" then
            -- Explicit foreign factories already return one boxed result.
            entry.definition = entry.factory()
          else
            local materialize = materializers and materializers[entry.factory.key]
            if materialize == nil then error("DefinitionPlan factory has no ABI-aware materializer", 0) end
            entry.definition = materialize(entry.factory)
          end
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
    definition_plan = __rock_definition_plan_materialize(definition_plan, _G.__rock_definition_materializers_pending)
  end
  local candidate_lifecycle_plan = _G.__rock_lifecycle_plan_pending
  local wire_due = nil
  if candidate_lifecycle_plan ~= nil then
    wire_due = __rock_lifecycle_wire_due(_G.__ROCK_LIFECYCLE_PLAN, candidate_lifecycle_plan)
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
  _G.__rock_code_slot_pending = nil
  _G.__rock_code_slot_pending_slots = nil
  _G.__rock_lifecycle_plan_pending = nil
  _G.__rock_definition_plan_pending = nil
  _G.__rock_definition_materializers_pending = nil
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
if not _G.__rock_alloc_frame_hits then _G.__rock_alloc_frame_hits = {} end
if not _G.__rock_alloc_active_frames then _G.__rock_alloc_active_frames = {} end
if not _G.__rock_alloc_max_frame_hits then _G.__rock_alloc_max_frame_hits = {} end
if not _G.__rock_alloc_outside_hits then _G.__rock_alloc_outside_hits = {} end
if not _G.__rock_alloc_last_frame then _G.__rock_alloc_last_frame = {} end
if not _G.__rock_alloc_current_frame_hits then _G.__rock_alloc_current_frame_hits = {} end
function __rock_alloc_hit(id, value)
  local hits = _G.__rock_alloc_hits
  hits[id] = (hits[id] or 0) + 1
  local frame = _G.__rock_alloc_frame
  if frame ~= nil then
    local frame_hits = _G.__rock_alloc_frame_hits
    frame_hits[id] = (frame_hits[id] or 0) + 1
    local last = _G.__rock_alloc_last_frame
    local counts = _G.__rock_alloc_current_frame_hits
    if last[id] ~= frame then
      last[id] = frame
      _G.__rock_alloc_active_frames[id] = (_G.__rock_alloc_active_frames[id] or 0) + 1
      counts[id] = 0
    end
    counts[id] = counts[id] + 1
    if counts[id] > (_G.__rock_alloc_max_frame_hits[id] or 0) then
      _G.__rock_alloc_max_frame_hits[id] = counts[id]
    end
  else
    _G.__rock_alloc_outside_hits[id] = (_G.__rock_alloc_outside_hits[id] or 0) + 1
  end
  return value
end
function __rock_alloc_hit_effect(id)
  __rock_alloc_hit(id, false)
end
-- A value site reads `(__rock_alloc_site[id] and value)`: the lookup takes the
-- register `value` lands in, where a wrapping call would reserve three more
-- and push a function at LJ_MAX_SLOTS over it. The table stays empty so every
-- read reaches __index.
__rock_alloc_site = setmetatable({}, { __index = function(_, id)
  __rock_alloc_hit(id, false)
  return true
end })
function __rock_allocation_activation_begin()
  _G.__rock_alloc_hits = {}
  _G.__rock_alloc_frame_hits = {}
  _G.__rock_alloc_active_frames = {}
  _G.__rock_alloc_max_frame_hits = {}
  _G.__rock_alloc_outside_hits = {}
  _G.__rock_alloc_last_frame = {}
  _G.__rock_alloc_current_frame_hits = {}
  _G.__rock_alloc_frame_serial = 0
  _G.__rock_alloc_frame = nil
end
function __rock_allocation_activation_frame_begin()
  _G.__rock_alloc_frame_serial = (_G.__rock_alloc_frame_serial or 0) + 1
  _G.__rock_alloc_frame = _G.__rock_alloc_frame_serial
end
function __rock_allocation_activation_frame_end()
  _G.__rock_alloc_frame = nil
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
  file:write("rock.allocation-activation.v2\n")
  file:write("frames\t", tostring(frames), "\n")
  file:write("sites\t", tostring(#ids), "\n")
  for _, id in ipairs(ids) do
    file:write(id,
      "\t", tostring(_G.__rock_alloc_hits[id]),
      "\t", tostring(_G.__rock_alloc_frame_hits[id] or 0),
      "\t", tostring(_G.__rock_alloc_active_frames[id] or 0),
      "\t", tostring(_G.__rock_alloc_max_frame_hits[id] or 0),
      "\t", tostring(_G.__rock_alloc_outside_hits[id] or 0), "\n")
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
function __rock_vec_oob(vec, i)\n\
  error('Vec index out of bounds')\n\
end\n\
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
fn color_cells_timed(
    analysis: &regalloc::CellAnalysis<'_>,
    inlinable: &[bool],
    extra_pinned: &[Idx<Cell>],
) -> Vec<u32> {
    let t = bone_core::prelude::Instant::now();
    let colors = regalloc::color_cells_with_analysis(analysis, Some(inlinable), extra_pinned);
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
        Inst::MakeVecStrided { .. } => EmittedAllocationKind::VecConstruction,
        Inst::MakeVecStridedHeader { .. } => EmittedAllocationKind::StructConstruction,
        Inst::MakeStruct { .. } => EmittedAllocationKind::StructConstruction,
        Inst::MakeValueStruct { .. } => EmittedAllocationKind::ValueConstruction,
        Inst::MaterializeValueStruct { .. } => EmittedAllocationKind::ExplicitValueMaterialization,
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
    /// Compiler-assigned storage slots for mutable globals.
    state_slots: HashMap<ustr::Ustr, u32>,
    scalar_constants: HashMap<ustr::Ustr, Lit>,
    /// Every function that lies on a call cycle, from the whole-program instance
    /// graph projected onto declarations. Read ONLY by
    /// [`TailCallShape::Cycles`]; empty on any path that never populated it,
    /// which makes that shape degrade to [`TailCallShape::All`] rather than to
    /// the unsafe direction.
    recursive_callees: HashSet<FuncRef>,
    /// The emitted shape for a call in tail position. Read from
    /// `ROCK_SKULL_TAIL_CALLS` once per context rather than once per emitted
    /// block, so a test can drive all three shapes in one process.
    tail_calls: TailCallShape,
    /// The emitted shape for a call to another emitted function. Read from
    /// `ROCK_SKULL_CALLEE_BINDING` once per context, same as `tail_calls`.
    callee_binding: CalleeBindingMode,
    /// Whether a mutable-global access under the bound shape reads the global
    /// storage through a chunk-level upvalue cell instead of reading
    /// `_G.__rock_globals`. Read from
    /// `ROCK_SKULL_STATE_CELL` once per context; only effective under
    /// [`CalleeBindingMode::Upvalue`], see [`CodegenCtx::state_cell`].
    state_cell: bool,
    /// The emitted shape for a natural loop that is not a counted `for`. Read
    /// from `ROCK_SKULL_LOOP_SHAPE` once per context, same as `tail_calls`.
    loops: LoopShape,
    /// The emitted shape for a GC-tier `value` struct construction. Read from
    /// `ROCK_SKULL_VALUE_CTOR` once per context, same as `tail_calls`.
    value_ctor: ValueCtorShape,
    /// The emitted shape for a bounds-checked Vec access. Read from
    /// `ROCK_SKULL_INDEX_SHAPE` once per context, same as `tail_calls`.
    index_shape: IndexShape,
    /// Every function's strongly connected component number over the
    /// whole-program call graph, from [`call_sccs`]. Read ONLY by
    /// [`callee_binding_plan`]; empty on any path that never populated it,
    /// which binds nothing rather than binding unsoundly.
    call_sccs: HashMap<FuncRef, u32>,
    /// Whether a body over LuaJIT's parse-time frame limits is rejected
    /// ([`frame_limits`]). Always true in production; the boundary-agreement
    /// test turns it off so it can obtain the Lua whose `loadfile` verdict the
    /// check is compared against. There is deliberately NO environment switch:
    /// a program the check rejects cannot load, so shipping an escape hatch
    /// would only trade a Rock diagnostic for LuaJIT's.
    frame_limits: bool,
    /// Whether emitted functions carry LuaJIT `# <line> "<file>"` directives
    /// mapping their statements to Rock source ([`line_directives_mode`]).
    line_directives: bool,
    trace_frames: Option<Mutex<trace_frames::Contributors>>,
}

impl Clone for CodegenCtx {
    fn clone(&self) -> Self {
        Self {
            reserved: self.reserved.clone(),
            rejections: Mutex::new(self.rejections.lock().expect("rejection sink").clone()),
            state_slots: self.state_slots.clone(),
            scalar_constants: self.scalar_constants.clone(),
            recursive_callees: self.recursive_callees.clone(),
            tail_calls: self.tail_calls,
            callee_binding: self.callee_binding,
            state_cell: self.state_cell,
            loops: self.loops,
            value_ctor: self.value_ctor,
            index_shape: self.index_shape,
            call_sccs: self.call_sccs.clone(),
            frame_limits: self.frame_limits,
            line_directives: self.line_directives,
            trace_frames: self
                .trace_frames
                .as_ref()
                .map(|rows| Mutex::new(rows.lock().expect("trace frame sink").clone())),
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
/// is a shared reserved namespace the stdlib and host runtimes legitimately
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

fn scalar_constants(ws: &Workspace) -> HashMap<ustr::Ustr, Lit> {
    use bone_semantic::hir::{Body, BodyOwner, Callee, Expr};
    use bone_semantic::inference::BodyInference;
    use skull_mir::monoty::{MonoSubst, MonoTy, convert};

    struct Folder<'a> {
        ws: &'a Workspace,
        body: &'a Body,
        inf: Option<&'a BodyInference>,
        known: &'a HashMap<ustr::Ustr, Lit>,
        subst: &'a MonoSubst,
    }
    impl Folder<'_> {
        fn numeric_type(&self, id: Idx<Expr>) -> Option<MonoTy> {
            let inf = self.inf?;
            let ty = convert(self.ws, inf, *inf.expr_types.get(id)?, self.subst).ok()?;
            matches!(ty, MonoTy::Int | MonoTy::Float).then_some(ty)
        }

        fn fold(&self, id: Idx<Expr>, depth: usize) -> Option<Lit> {
            // Initializers can be arbitrarily nested; leave large expressions at
            // runtime instead of growing the compiler stack without a bound.
            if depth >= 64 {
                return None;
            }
            match self.body.expr_arena[id] {
                Expr::Int(v) => Some(Lit::Int(v)),
                Expr::Float(v) => Some(Lit::Float(v.into_inner())),
                Expr::Bool(v) => Some(Lit::Bool(v)),
                Expr::String(v) => Some(Lit::Str(v)),
                Expr::Global(name) => self.known.get(&name).cloned(),
                Expr::UnaryMinus(inner, _) => match self.fold(inner, depth + 1)? {
                    Lit::Int(v) => v.checked_neg().map(Lit::Int),
                    Lit::Float(v) => Some(Lit::Float(-v)),
                    _ => None,
                },
                Expr::Call {
                    callee: Callee::Unresolved { name, .. },
                    args,
                    ..
                } if matches!(
                    name.as_str(),
                    "__op_add" | "__op_sub" | "__op_mul" | "__op_div"
                ) =>
                {
                    let mut args = args.into_iter();
                    let left = args.next()?;
                    let right = args.next()?;
                    if args.next().is_some() {
                        return None;
                    }
                    // Only the primitive numeric route is admitted. In particular,
                    // uint, newtype operators and string coercions keep their
                    // runtime checks and overload behavior.
                    self.numeric_type(left)?;
                    self.numeric_type(right)?;
                    let result_ty = self.numeric_type(id)?;
                    let number = |lit| match lit {
                        Lit::Int(v) if v.unsigned_abs() <= 9_007_199_254_740_991 => Some(v as f64),
                        Lit::Float(v) if v.is_finite() => Some(v),
                        _ => None,
                    };
                    let left = number(self.fold(left, depth + 1)?)?;
                    let right = number(self.fold(right, depth + 1)?)?;
                    let result = match name.as_str() {
                        "__op_add" => left + right,
                        "__op_sub" => left - right,
                        "__op_mul" => left * right,
                        "__op_div" if right != 0.0 => left / right,
                        _ => return None,
                    };
                    if !result.is_finite() {
                        return None;
                    }
                    match result_ty {
                        MonoTy::Float => Some(Lit::Float(result)),
                        MonoTy::Int
                            if result.abs() <= 9_007_199_254_740_991.0
                                && !(result == 0.0 && result.is_sign_negative()) =>
                        {
                            Some(Lit::Int(result as i64))
                        }
                        _ => None,
                    }
                }
                _ => None,
            }
        }
    }
    let mut pending: Vec<_> = ws
        .modules
        .iter()
        .flat_map(|module| {
            module.globals.iter().filter_map(|(id, global)| {
                if !global.is_const {
                    return None;
                }
                let owner = BodyOwner::Global(id);
                let inf = ws
                    .inference(module.id)
                    .and_then(|inf| inf.per_fn.get(&owner));
                module
                    .bodies
                    .get(&owner)
                    .map(|body| (global.name, body, inf))
            })
        })
        .collect();
    let mut known = HashMap::default();
    let subst = MonoSubst::default();
    loop {
        let before = pending.len();
        pending.retain(|(name, body, inf)| {
            let folder = Folder {
                ws,
                body,
                inf: *inf,
                known: &known,
                subst: &subst,
            };
            if let Some(lit) = body.root.and_then(|root| folder.fold(root, 0)) {
                known.insert(*name, lit);
                false
            } else {
                true
            }
        });
        if pending.len() == before {
            return known;
        }
    }
}

impl CodegenCtx {
    /// Refresh baked scalar values after a constant initializer edit. Readers of
    /// every returned name need new emitted text, including readers of aliases.
    pub fn refresh_scalar_constants(&mut self, ws: &Workspace) -> HashSet<ustr::Ustr> {
        let next = scalar_constants(ws);
        let changed = self
            .scalar_constants
            .keys()
            .chain(next.keys())
            .filter(
                |name| match (self.scalar_constants.get(name), next.get(name)) {
                    (Some(Lit::Float(x)), Some(Lit::Float(y))) => x.to_bits() != y.to_bits(),
                    (a, b) => a != b,
                },
            )
            .copied()
            .collect();
        self.scalar_constants = next;
        changed
    }

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
                let symbol = skull_mir::mangle::symbol_of_key(g.name);
                reserved_namespace_collision(symbol.as_str(), g.name_span, &mut collisions);
                reserved.insert(symbol.as_str().to_string(), g.name_span);
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
            scalar_constants: scalar_constants(ws),
            recursive_callees: HashSet::default(),
            tail_calls: tail_call_shape(),
            callee_binding: callee_binding_mode(),
            state_cell: state_cell_mode(),
            loops: loop_shape(),
            value_ctor: value_ctor_shape(),
            index_shape: index_shape(),
            call_sccs: HashMap::default(),
            frame_limits: true,
            line_directives: line_directives_mode(),
            trace_frames: trace_frames::enabled().then(|| Mutex::new(Default::default())),
        }
    }

    /// Override the emitted tail-call shape this context was built with. The
    /// production path takes it from `ROCK_SKULL_TAIL_CALLS`; a test uses this
    /// so one process can emit all three shapes.
    pub fn set_tail_call_shape(&mut self, shape: TailCallShape) {
        self.tail_calls = shape;
    }

    pub fn tail_call_shape(&self) -> TailCallShape {
        self.tail_calls
    }

    /// Override the emitted callee-binding shape this context was built with.
    /// Production reads `ROCK_SKULL_CALLEE_BINDING`; tests use this override
    /// to compare both shapes in one process.
    pub fn set_callee_binding(&mut self, mode: CalleeBindingMode) {
        self.callee_binding = mode;
    }

    pub fn callee_binding(&self) -> CalleeBindingMode {
        self.callee_binding
    }

    /// Override the storage-cell switch this context was built with. The
    /// production path takes it from `ROCK_SKULL_STATE_CELL`; a test uses this
    /// so one process can emit both shapes.
    pub fn set_state_cell(&mut self, on: bool) {
        self.state_cell = on;
    }

    /// Whether mutable-global accesses are emitted through the storage cell
    /// (`__rock_state_cell[k]`) rather than a `_G.__rock_globals` read. This optimization is
    /// enabled together with bound callees; overlays retain both optimizations.
    pub fn state_cell(&self) -> bool {
        self.state_cell && self.callee_binding == CalleeBindingMode::Upvalue
    }

    /// The table a mutable-global access indexes: the chunk-level cell under the
    /// bound shape, else the `_G` lookup an overlay chunk can also perform.
    pub fn state_table(&self) -> &'static str {
        if self.state_cell() {
            STATE_CELL
        } else {
            "__rock_globals"
        }
    }

    /// Override the emitted loop shape this context was built with. The
    /// production path takes it from `ROCK_SKULL_LOOP_SHAPE`; a test uses this
    /// so one process can emit both shapes.
    pub fn set_loop_shape(&mut self, shape: LoopShape) {
        self.loops = shape;
    }

    pub fn loop_shape(&self) -> LoopShape {
        self.loops
    }

    /// Override the emitted value-ctor shape this context was built with. The
    /// production path takes it from `ROCK_SKULL_VALUE_CTOR`; a test uses this
    /// so one process can emit both shapes.
    pub fn set_value_ctor_shape(&mut self, shape: ValueCtorShape) {
        self.value_ctor = shape;
    }

    pub fn value_ctor_shape(&self) -> ValueCtorShape {
        self.value_ctor
    }

    /// Override the emitted index shape this context was built with. The
    /// production path takes it from `ROCK_SKULL_INDEX_SHAPE`; a test uses this
    /// so one process can emit both shapes.
    pub fn set_index_shape(&mut self, shape: IndexShape) {
        self.index_shape = shape;
    }

    pub fn index_shape(&self) -> IndexShape {
        self.index_shape
    }

    /// Turn the LuaJIT frame-limit rejection off for this context. ONLY the
    /// boundary-agreement test does this: it needs the emitted Lua of a body
    /// the check rejects so it can ask the bundled LuaJIT whether that Lua
    /// really fails to load. Production always leaves it on.
    pub fn set_frame_limits(&mut self, on: bool) {
        self.frame_limits = on;
    }

    pub fn frame_limits(&self) -> bool {
        self.frame_limits
    }

    /// Override the line-directive switch this context was built with. The
    /// production path takes it from `ROCK_SKULL_LINE_DIRECTIVES`.
    pub fn set_line_directives(&mut self, on: bool) {
        self.line_directives = on;
    }

    pub fn line_directives(&self) -> bool {
        self.line_directives
    }

    /// Opt-in trace-slot diagnostic over the final assembled Lua. The selected
    /// LuaJIT parses the chunk without running its initializers or user code.
    /// Diagnostic failures remain warnings and never change parse-limit checks.
    pub fn report_trace_frames(&self, lua: &str) {
        let Some(rows) = &self.trace_frames else {
            return;
        };
        let result = trace_frames::report(lua, &rows.lock().expect("trace frame sink"));
        match result {
            Ok(report) => {
                if let Some(path) = std::env::var_os("ROCK_LUA_TRACE_FRAME_REPORT") {
                    if let Err(error) = std::fs::write(path, report) {
                        eprintln!(
                            "warning: cannot write LuaJIT emitted-call frame report: {error}"
                        );
                    }
                } else {
                    eprint!("{report}");
                }
            }
            Err(error) => {
                eprintln!("warning: LuaJIT emitted-call frame report unavailable: {error}")
            }
        }
    }

    /// Record every function's call-graph strongly connected component. The
    /// caller owns the graph this is derived from (`skull::mir_emit` projects
    /// the whole-program instance graph onto declarations, which can only MERGE
    /// components and so only ever binds less); codegen only reads the answer,
    /// and only under [`CalleeBindingMode::Upvalue`].
    pub fn set_call_sccs(&mut self, sccs: HashMap<FuncRef, u32>) {
        self.call_sccs = sccs;
    }

    /// `func`'s strongly connected component number, or `None` when the graph
    /// this context was given does not mention it.
    pub fn scc_of(&self, func: FuncRef) -> Option<u32> {
        self.call_sccs.get(&func).copied()
    }

    /// Record the functions that lie on a call cycle. The caller owns the graph
    /// this is derived from (`skull::mir_emit` projects the whole-program
    /// instance graph onto declarations); codegen only reads the answer, and
    /// only under `ROCK_SKULL_TAIL_CALLS=cycles`.
    pub fn set_recursive_callees(&mut self, callees: HashSet<FuncRef>) {
        self.recursive_callees = callees;
    }

    /// True if `func` lies on a call cycle, so a call to it can grow the Lua
    /// stack without bound unless it stays a real tail call.
    pub fn is_recursive_callee(&self, func: FuncRef) -> bool {
        self.recursive_callees.contains(&func)
    }

    /// Mint a compiler-reserved synthetic top-level Lua name: a hoisted lambda
    /// (`<fn>__lambda<k>`) or coroutine machinery (`<co>__step`/`__resume`/
    /// `__advance`/`__defer<k>`/`__on_killed`/`__on_resume`/`__on_hot_reload`).
    /// Returns `name` unchanged. If it collides with a REAL top-level name (a
    /// user fn's Lua name or a global), records a spanned rejection pointing at
    /// that user definition -- drained after emit by [`take_rejection`]. This is
    /// reject-not-panic: a collision is illegal user code, not a compiler bug, so
    /// it surfaces as a diagnostic and keeps interactive compiler hosts alive.
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
        self.rejections
            .lock()
            .expect("rejection sink")
            .push((key, err));
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

    /// The storage slot of `name`. The map is keyed on the emitted SYMBOL, so a
    /// caller may hand over either spelling of a v1 global: its DefKey (what MIR
    /// carries) or the symbol (what the schema publishes).
    pub fn state_slot(&self, name: ustr::Ustr) -> Option<u32> {
        self.state_slots
            .get(&skull_mir::mangle::symbol_of_key(name))
            .copied()
    }

    /// The highest compiler-assigned storage slot, 0 when there is none. The
    /// emitter sizes the storage table's array part to this slot's rehash fixpoint.
    pub fn state_slot_max(&self) -> u32 {
        self.state_slots.values().copied().max().unwrap_or(0)
    }
}

/// Cells REFERENCED (Load/Store/`MapNext` or multi-return dest) anywhere the top
/// scope rooted at `entry` emits. Hoisted closure bodies are separate ordinary
/// MirFuncs, so the declaration walk only follows this function's CFG.
fn referenced_cells_in_region(
    func: &MirFunc,
    entry: Idx<Block>,
    tail_forwards: &[Option<TailForward>],
    suppressed: &[bool],
) -> HashSet<Idx<Cell>> {
    let mut seen: HashSet<Idx<Block>> = HashSet::default();
    let mut stack = vec![entry];
    seen.insert(entry);
    let mut cells: HashSet<Idx<Cell>> = HashSet::default();
    while let Some(b) = stack.pop() {
        let tail = tail_forwards[raw(b) as usize].as_ref();
        for (position, &iid) in func.blocks[b].insts.iter().enumerate() {
            if tail.is_some_and(|tail| position >= tail.first_suppressed) {
                continue;
            }
            if suppressed[raw(iid) as usize] {
                continue;
            }
            match &func.insts[iid] {
                Inst::Load(c) => {
                    cells.insert(*c);
                }
                Inst::Store { cell, .. } => {
                    cells.insert(*cell);
                }
                Inst::MapNext {
                    key_cell, val_cell, ..
                } => {
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

#[derive(Clone)]
enum TailForwardCall {
    Direct {
        call: Idx<Inst>,
        func: FuncRef,
        args: Vec<Idx<Inst>>,
    },
    Value {
        callee: Idx<Inst>,
        args: Vec<Idx<Inst>>,
    },
}

#[derive(Clone)]
struct TailForward {
    first_suppressed: usize,
    suppressed: Vec<Idx<Inst>>,
    call: TailForwardCall,
}

#[derive(Clone)]
struct CallArgForward {
    producer: TailForwardCall,
    prefix: Vec<Idx<Inst>>,
    consumer: Idx<Inst>,
    func: FuncRef,
}

fn cell_reference_counts(func: &MirFunc) -> Vec<u32> {
    let mut counts = vec![0; func.cells.len()];
    for (_, block) in func.blocks.iter() {
        for &id in &block.insts {
            match &func.insts[id] {
                Inst::Load(cell) | Inst::Store { cell, .. } => counts[raw(*cell) as usize] += 1,
                Inst::CallMulti { dests, .. }
                | Inst::CallValueMulti { dests, .. }
                | Inst::CallBoundMulti { dests, .. } => {
                    for cell in dests {
                        counts[raw(*cell) as usize] += 1;
                    }
                }
                Inst::MapNext {
                    key_cell, val_cell, ..
                } => {
                    counts[raw(*key_cell) as usize] += 1;
                    counts[raw(*val_cell) as usize] += 1;
                }
                Inst::Lambda { params, .. } => {
                    for cell in params {
                        counts[raw(*cell) as usize] += 1;
                    }
                }
                _ => {}
            }
        }
    }
    counts
}

fn instruction_reference_counts(func: &MirFunc) -> Vec<u32> {
    let mut counts = vec![0; func.insts.len()];
    for (_, block) in func.blocks.iter() {
        for &id in &block.insts {
            regalloc::inst_uses(&func.insts[id], &mut |used| {
                counts[raw(used) as usize] += 1;
            });
        }
        regalloc::term_uses(&block.term, &mut |used| {
            counts[raw(used) as usize] += 1;
        });
    }
    counts
}

/// The strongly connected components of a `(caller, callee)` call-edge stream,
/// the one Tarjan run behind both [`callees_on_cycles`] and [`call_sccs`].
///
/// Components are numbered in Tarjan COMPLETION order, which is a reverse
/// topological order of the condensation: a component is closed only after
/// every component it can reach, so `component[callee] < component[caller]`
/// whenever the two differ. [`call_sccs`] hands that number out as the
/// callee-binding rank, and the binding pass emits in ascending rank so every
/// captured callee is already its final closure.
struct CallComponents {
    nodes: Vec<FuncRef>,
    /// Node index -> component number.
    component: Vec<u32>,
    /// Component number -> whether it is a genuine cycle (more than one member,
    /// or a self-edge).
    is_cycle: Vec<bool>,
}

fn tarjan_components(edges: impl IntoIterator<Item = (FuncRef, FuncRef)>) -> CallComponents {
    fn intern(
        f: FuncRef,
        ids: &mut HashMap<FuncRef, usize>,
        nodes: &mut Vec<FuncRef>,
        succ: &mut Vec<Vec<usize>>,
    ) -> usize {
        if let Some(&i) = ids.get(&f) {
            return i;
        }
        let i = nodes.len();
        ids.insert(f, i);
        nodes.push(f);
        succ.push(Vec::new());
        i
    }

    let mut ids: HashMap<FuncRef, usize> = HashMap::default();
    let mut nodes: Vec<FuncRef> = Vec::new();
    let mut succ: Vec<Vec<usize>> = Vec::new();
    for (from, to) in edges {
        let from = intern(from, &mut ids, &mut nodes, &mut succ);
        let to = intern(to, &mut ids, &mut nodes, &mut succ);
        succ[from].push(to);
    }

    // Iterative Tarjan: a program-sized call graph has chains deep enough to
    // overflow a recursive DFS.
    const UNVISITED: usize = usize::MAX;
    let n = nodes.len();
    let mut index = vec![UNVISITED; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut component = vec![u32::MAX; n];
    let mut is_cycle: Vec<bool> = Vec::new();
    let mut members: Vec<usize> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut work: Vec<(usize, usize)> = Vec::new();
    let mut next_index = 0usize;
    for root in 0..n {
        if index[root] != UNVISITED {
            continue;
        }
        work.push((root, 0));
        while let Some((v, resume)) = work.pop() {
            if resume == 0 {
                index[v] = next_index;
                low[v] = next_index;
                next_index += 1;
                stack.push(v);
                on_stack[v] = true;
            }
            let mut descended = false;
            let mut i = resume;
            while i < succ[v].len() {
                let w = succ[v][i];
                i += 1;
                if index[w] == UNVISITED {
                    work.push((v, i));
                    work.push((w, 0));
                    descended = true;
                    break;
                }
                if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            }
            if descended {
                continue;
            }
            if low[v] == index[v] {
                members.clear();
                loop {
                    let w = stack.pop().expect("tarjan component stack");
                    on_stack[w] = false;
                    members.push(w);
                    if w == v {
                        break;
                    }
                }
                let id = is_cycle.len() as u32;
                // A one-node component is on a cycle only through a self-edge.
                is_cycle.push(members.len() > 1 || succ[v].contains(&v));
                for &w in &members {
                    component[w] = id;
                }
            }
            if let Some(&(parent, _)) = work.last() {
                low[parent] = low[parent].min(low[v]);
            }
        }
    }
    CallComponents {
        nodes,
        component,
        is_cycle,
    }
}

/// Every function that lies on a cycle of `edges`, the input
/// `ROCK_SKULL_TAIL_CALLS=cycles` reads through
/// [`CodegenCtx::set_recursive_callees`].
///
/// `edges` is a `(caller, callee)` stream; a function is on a cycle when its
/// strongly connected component has more than one member or it calls itself.
/// The caller supplies the graph, so the emitter never has to know how the
/// program's call edges were discovered.
pub fn callees_on_cycles(edges: impl IntoIterator<Item = (FuncRef, FuncRef)>) -> HashSet<FuncRef> {
    let c = tarjan_components(edges);
    c.nodes
        .iter()
        .zip(c.component.iter())
        .filter(|(_, comp)| c.is_cycle[**comp as usize])
        .map(|(f, _)| *f)
        .collect()
}

/// Each function's strongly connected component number over `edges`, the input
/// `ROCK_SKULL_CALLEE_BINDING=upvalue` reads through
/// [`CodegenCtx::set_call_sccs`]. Two functions share a number exactly when
/// they are mutually reachable, and a callee's number is strictly lower than
/// its caller's whenever they differ -- see [`CallComponents`].
pub fn call_sccs(edges: impl IntoIterator<Item = (FuncRef, FuncRef)>) -> HashMap<FuncRef, u32> {
    let c = tarjan_components(edges);
    c.nodes
        .iter()
        .copied()
        .zip(c.component.iter().copied())
        .collect()
}

/// Which calls in tail position keep Lua's `do return f(...) end` shape.
///
/// `do return f(...) end` compiles to `CALLT`, and LuaJIT's recorder counts
/// every recorded `CALLT` against `loopunroll` (default 15) in
/// `lj_record_tailcall`, so a trace that walks through more than 15 of them
/// aborts with "loop unroll limit reached". A plain call is inlined by the
/// recorder too, so dropping the shape does not stop inlining -- it moves the
/// limit the trace hits (`lj_record_call` grows `baseslot` instead and trips
/// "trace too deep" at 250 slots) and costs a real interpreter frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TailCallShape {
    /// Every call in tail position is `do return f(...) end`.
    All,
    /// No call keeps the shape. MEASUREMENT ONLY: this drops tail-call
    /// elimination for recursive calls too, so an unbounded tail recursion
    /// overflows the Lua stack (measured between 10,000 and 60,000 frames).
    None,
    /// Only a callee that lies on a call cycle keeps the shape, so genuine
    /// recursion keeps tail-call elimination and a straight-line accessor or
    /// draw chain does not.
    Cycles,
}

/// `ROCK_SKULL_TAIL_CALLS` selects the emitted shape for a call in tail
/// position: `all` (the default), `none`, or `cycles`. An unset or unrecognized
/// value is `all`, so the switch can only be entered deliberately.
pub fn tail_call_shape() -> TailCallShape {
    static SHAPE: std::sync::OnceLock<TailCallShape> = std::sync::OnceLock::new();
    *SHAPE.get_or_init(|| match std::env::var("ROCK_SKULL_TAIL_CALLS").as_deref() {
        Ok("none") => TailCallShape::None,
        Ok("cycles") => TailCallShape::Cycles,
        _ => TailCallShape::All,
    })
}

/// How a call to another EMITTED function is spelled inside a function body.
///
/// A global lookup (`GGET`) costs a trace 5 constant-pool slots per distinct
/// callee on GC64 -- the name string (a GC constant, 2 slots), its `KSLOT`
/// hash slot, and the resolved function object (2 more) -- plus the guard that
/// the global still holds that function. LuaJIT's `maxirconst` limit is 500
/// SLOTS, and "trace too long" (the limit being hit) can dominate trace aborts. An IMMUTABLE upvalue costs 2: only the function object, folded into
/// the trace with no lookup and no guard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CalleeBindingMode {
    /// Every call is a global lookup by the callee's emitted name.
    Global,
    /// A function that calls other emitted functions is defined as a FACTORY
    /// (`__rock_mk_F = function(a, b) return function(..) .. end end`) whose
    /// parameters are named exactly like the callees, so the body text is
    /// unchanged and each call reads an immutable upvalue instead of a global.
    /// A second pass binds `F = __rock_mk_F(a, b)` once every callee it
    /// captures is final.
    Upvalue,
}

/// `ROCK_SKULL_CALLEE_BINDING` selects the emitted shape for a call to another
/// emitted function: `global` or `upvalue`. An unset or unrecognized value is
/// the shipped default, so the switch can only be entered deliberately.
pub fn callee_binding_mode() -> CalleeBindingMode {
    static MODE: std::sync::OnceLock<CalleeBindingMode> = std::sync::OnceLock::new();
    *MODE.get_or_init(
        || match std::env::var("ROCK_SKULL_CALLEE_BINDING").as_deref() {
            Ok("upvalue") => CalleeBindingMode::Upvalue,
            Ok("global") => CalleeBindingMode::Global,
            _ => DEFAULT_CALLEE_BINDING,
        },
    )
}

/// `ROCK_SKULL_LINE_DIRECTIVES=1` writes LuaJIT line directives into every
/// emitted function (see [`write_line_directives`]). Off otherwise: a LuaJIT
/// without directive support rejects them as a syntax error.
pub fn line_directives_mode() -> bool {
    static MODE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        matches!(
            std::env::var("ROCK_SKULL_LINE_DIRECTIVES").as_deref(),
            Ok("1")
        )
    })
}

/// The global holding the `Lit::Omitted` marker: a private table, so no Rock
/// value compares equal to it. The runtime prelude creates it once per Lua
/// state, so reloaded code compares against the same table.
pub const OMITTED_ARG: &str = "__rock_omitted";

/// Chunk-level local that every emitted function reads a mutable global through
/// under the bound shape: the process's one global storage table,
/// `_G.__rock_globals`. Each chunk (a full program or an overlay) declares its
/// own and assigns the same table, so no upvalue ever needs rejoining.
pub const STATE_CELL: &str = "__rock_state_cell";

/// `ROCK_SKULL_STATE_RESERVE=0` leaves the global storage table to grow lazily
/// from `{}` instead of sizing it to its rehash fixpoint at creation, for A/B measurement.
/// Default: on.
pub fn state_reserve_mode() -> bool {
    static MODE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        !matches!(
            std::env::var("ROCK_SKULL_STATE_RESERVE").as_deref(),
            Ok("0")
        )
    })
}

/// `ROCK_SKULL_STATE_CELL=0` keeps the `_G.__rock_globals` read under the
/// bound shape, for A/B measurement. Anything else is the
/// shipped default: on.
pub fn state_cell_mode() -> bool {
    static MODE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| !matches!(std::env::var("ROCK_SKULL_STATE_CELL").as_deref(), Ok("0")))
}

/// How a natural loop that is NOT a counted `for` is spelled.
///
/// LuaJIT's parser turns a backward `goto` into `BC_LOOP` with `bc_j == -1`
/// (`lj_parse.c`, `parse_goto`: "No BC range check"). `rec_setup_root` then
/// leaves `bc_min`/`bc_extent` at their no-limit defaults, so a root trace
/// started at such a loop keeps recording past the loop's exit through the rest
/// of the function until it hits a compiled loop, the unroll limit or the
/// constant limit; and `rec_loop_interp`'s interpreted-inner-loop check is
/// skipped, because it is guarded on `bc_j(*pc) != -1`. A `while true do .. end`
/// gets `BC_LOOP` with a real range (`parse_while` patches it with
/// `jmp_patchins`), so both paths see a closed loop.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoopShape {
    /// Every non-counted loop stays labelled blocks plus a backward `goto`.
    Goto,
    /// Every natural loop the CFG can prove single-entry is wrapped in
    /// `while true do .. end`, its back edges rewritten to a forward `goto` to a
    /// continue label at the end of the loop body. Exits stay ordinary `goto`s
    /// OUT of the block, which Lua allows; only jumping IN is forbidden, and the
    /// single-entry proof is exactly what rules that out.
    Structured,
}

/// `ROCK_SKULL_LOOP_SHAPE` selects the emitted shape for a natural loop that is
/// not a counted `for`: `goto` or `structured`. An unset or unrecognized value
/// is the shipped default, so the switch can only be entered deliberately.
pub fn loop_shape() -> LoopShape {
    static SHAPE: std::sync::OnceLock<LoopShape> = std::sync::OnceLock::new();
    *SHAPE.get_or_init(|| match std::env::var("ROCK_SKULL_LOOP_SHAPE").as_deref() {
        Ok("structured") => LoopShape::Structured,
        Ok("goto") => LoopShape::Goto,
        _ => DEFAULT_LOOP_SHAPE,
    })
}

/// The shipped emitted loop shape.
pub const DEFAULT_LOOP_SHAPE: LoopShape = LoopShape::Goto;

/// How a GC-tier `value` struct construction is spelled at its use site.
///
/// [`ValueCtorShape::Call`] calls the chunk-level `<Name>_vt(..)` ctor, whose
/// whole body is `return setmetatable({ .. }, <mt>)`. That call frame is what
/// makes a four-field constructor hot enough to become a `rec_ret` TRACE ROOT --
/// and its `return setmetatable(..)` is one
/// `CALLT` against LuaJIT's 15-slot `loopunroll` budget in every trace that
/// records it. [`ValueCtorShape::Splice`] writes the table constructor and the
/// `setmetatable` at the use site instead, so there is no frame to root.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ValueCtorShape {
    /// Every construction is a call to the chunk-level `<Name>_vt` ctor.
    Call,
    /// A GC-table (`_vt`) construction with declared fields is spliced in place.
    /// The cdata (`_ct`) and leaf-table (`_lct`) tiers are untouched: `_ct` is an
    /// `ffi.typeof` ctor with no Lua body to splice, and `_lct` carries the leaf
    /// plan's presence tagging, which belongs in one place.
    Splice,
}

/// `ROCK_SKULL_VALUE_CTOR` selects the emitted shape for a GC-tier `value`
/// struct construction: `call` or `splice`. An unset or unrecognized value is
/// the shipped default, so the switch can only be entered deliberately.
pub fn value_ctor_shape() -> ValueCtorShape {
    static SHAPE: std::sync::OnceLock<ValueCtorShape> = std::sync::OnceLock::new();
    *SHAPE.get_or_init(|| match std::env::var("ROCK_SKULL_VALUE_CTOR").as_deref() {
        Ok("splice") => ValueCtorShape::Splice,
        Ok("call") => ValueCtorShape::Call,
        _ => DEFAULT_VALUE_CTOR_SHAPE,
    })
}

/// The shipped emitted value-ctor shape.
pub const DEFAULT_VALUE_CTOR_SHAPE: ValueCtorShape = ValueCtorShape::Call;

/// How a bounds-checked Vec element read, store or column index is spelled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IndexShape {
    /// Every access calls the stdlib helper (`__vec_checked_get` and kin). In a
    /// trace that is one callee constant per trace plus one frame-link constant
    /// per site, and the helper's own entry is a trace root.
    Call,
    /// An access whose base and index render as bare names spells the check
    /// itself: `if i < 0 or i >= v.len then __rock_vec_oob(v, i) end` and then
    /// the raw `_data` access. An operand that is a folded expression keeps the
    /// call, because the guard names each operand twice.
    Inline,
}

/// `ROCK_SKULL_INDEX_SHAPE` selects the emitted shape for a bounds-checked Vec
/// access: `call` or `inline`. An unset or unrecognized value is the shipped
/// default, so the switch can only be entered deliberately.
pub fn index_shape() -> IndexShape {
    static SHAPE: std::sync::OnceLock<IndexShape> = std::sync::OnceLock::new();
    *SHAPE.get_or_init(
        || match std::env::var("ROCK_SKULL_INDEX_SHAPE").as_deref() {
            Ok("call") => IndexShape::Call,
            Ok("inline") => IndexShape::Inline,
            _ => DEFAULT_INDEX_SHAPE,
        },
    )
}

/// How a one-expression wrapper -- an `extern(ffi)` binding, a value-struct
/// `_vt`/`_lct` ctor -- hands its result back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WrapperRetShape {
    /// `return lib.f(..)`: a `CALLT` into C. A root trace started at the
    /// wrapper's entry cannot leave through it (`rec_ret` needs a real `RET`)
    /// and aborts `leaving loop in root trace` every time it is tried.
    Tail,
    /// `local __rock_ret = lib.f(..)` then `return __rock_ret`: one bytecode
    /// more, a real `RET`, and the root trace links out cleanly.
    Local,
}

/// `ROCK_SKULL_WRAPPER_RET` selects the wrapper return shape: `tail` or
/// `local`. An unset or unrecognized value is the shipped default, so the
/// switch can only be entered deliberately.
pub fn wrapper_ret_shape() -> WrapperRetShape {
    static SHAPE: std::sync::OnceLock<WrapperRetShape> = std::sync::OnceLock::new();
    *SHAPE.get_or_init(
        || match std::env::var("ROCK_SKULL_WRAPPER_RET").as_deref() {
            Ok("tail") => WrapperRetShape::Tail,
            Ok("local") => WrapperRetShape::Local,
            _ => DEFAULT_WRAPPER_RET_SHAPE,
        },
    )
}

/// The shipped wrapper return shape.
pub const DEFAULT_WRAPPER_RET_SHAPE: WrapperRetShape = WrapperRetShape::Local;

/// The return statement(s) of a wrapper whose whole result is `expr`, under
/// [`wrapper_ret_shape`]. Two-space indented, no trailing newline. The local
/// is a reserved `__rock` name so it can never shadow a wrapper parameter
/// that `expr` reads.
pub fn wrapper_return(expr: &str) -> String {
    match wrapper_ret_shape() {
        WrapperRetShape::Tail => format!("  return {expr}"),
        WrapperRetShape::Local => format!("  local __rock_ret = {expr}\n  return __rock_ret"),
    }
}

/// The shipped emitted index shape. `Inline` measured as a redistribution on
/// representative workloads, not a reduction: the aborts that started inside the helper reappeared
/// as leaving-loop roots at the callers' own `for` headers, and the first
/// frames got no faster.
pub const DEFAULT_INDEX_SHAPE: IndexShape = IndexShape::Call;

/// The shared per-type metatable global of a GC-tier `value` struct -- the table
/// every instance of `<name>` carries, so LuaJIT's `__eq` fires between any two
/// of them. ONE spelling, read by the definition emitter
/// (`skull::value_struct_gc_defs`) and by a spliced construction alike.
pub fn value_struct_metatable_name(name: &str) -> String {
    format!("__rock_vt_{name}")
}

/// Whether this process emits callees as bound upvalues.
pub fn callee_binding_is_bound() -> bool {
    callee_binding_mode() == CalleeBindingMode::Upvalue
}

/// The shipped emitted callee binding shape:
/// the measurement (mean frame 5.73 ms -> 4.99 ms, "trace too long" aborts
/// 1339 -> 386) settled `AUTHOR DECISION -- CALLEE BINDING DEFAULT` in favour
/// of [`CalleeBindingMode::Upvalue`] on 2026-09-11.
pub const DEFAULT_CALLEE_BINDING: CalleeBindingMode = CalleeBindingMode::Upvalue;

/// LuaJIT's hard limit on upvalues per function prototype (`LJ_MAX_UPVAL`).
/// A body with more bindable callees than this binds the first
/// `MAX_BOUND_CALLEES` in name order and leaves the rest global lookups.
pub const MAX_BOUND_CALLEES: usize = 60;

/// The factory holding the only copy of a bound function's body.
pub const CALLEE_FACTORY_PREFIX: &str = "__rock_mk_";

/// Recover the body symbol from a top-level emitted definition header.
/// Factories return the body on the same line, preserving body-local offsets.
pub fn emitted_body_name(line: &str) -> Option<&str> {
    let name = if let Some(rest) = line.strip_prefix("function ") {
        rest.split_once('(')?.0
    } else {
        let rest = line.strip_prefix(CALLEE_FACTORY_PREFIX)?;
        let (name, signature) = rest.split_once(" = function(")?;
        let (_, body) = signature.split_once(')')?;
        body.strip_prefix(" return function(")?;
        name
    };
    is_lua_ident(name).then_some(name)
}

#[cfg(test)]
mod emitted_body_header_tests {
    use super::emitted_body_name;

    #[test]
    fn recognizes_both_emitted_definition_shapes() {
        assert_eq!(emitted_body_name("function leaf_int(c0)"), Some("leaf_int"));
        assert_eq!(
            emitted_body_name("__rock_mk_leaf_int = function(callee) return function(c0)"),
            Some("leaf_int")
        );
    }

    #[test]
    fn rejects_non_headers_and_malformed_factory_shapes() {
        for line in [
            "leaf = __rock_mk_leaf(callee)",
            "__rock_mk_leaf = function(callee)",
            "__rock_mk_leaf = function(callee) return callee()",
            "__rock_mk_ = function() return function()",
            "__rock_mk_bad-name = function() return function()",
            "function ()",
            "function bad.name()",
            "function 1bad()",
            "  function nested()",
            "-- function commented()",
            "local function nested()",
        ] {
            assert_eq!(emitted_body_name(line), None, "{line}");
        }
    }
}

/// The callees one body binds as immutable upvalues, and the rank its binding
/// line must sort at. Built by [`callee_binding_plan`]; consumed by the emitter
/// (which names the factory parameters) and by the assembler (which emits the
/// second pass).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalleeBindingPlan {
    /// The bound function's emitted Lua name.
    pub name: String,
    /// Its factory's name, minted through [`CodegenCtx::mint_synthetic`].
    pub factory: String,
    /// The callee names to capture, sorted, deduplicated, and truncated to
    /// [`MAX_BOUND_CALLEES`]. Each is BOTH the factory parameter name and the
    /// global read at bind time, so the body text never changes.
    pub captures: Vec<String>,
    /// Ascending emission order for the binding pass: the caller's strongly
    /// connected component number, which is strictly greater than every
    /// component it captures from (see [`call_sccs`]).
    pub rank: u32,
    /// True when this body had more bindable callees than [`MAX_BOUND_CALLEES`],
    /// so the tail stayed global lookups.
    pub capped: bool,
}

impl CalleeBindingPlan {
    /// The second-pass line: `F = __rock_mk_F(a, b)`.
    pub fn bind_line(&self) -> String {
        let mut out = String::with_capacity(self.name.len() + self.factory.len() + 8);
        out.push_str(&self.name);
        out.push_str(" = ");
        out.push_str(&self.factory);
        out.push('(');
        for (i, c) in self.captures.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(c);
        }
        out.push_str(")\n");
        out
    }
}

/// The emitted symbol a `Call`/`CallMulti` at `id` targets. THE derivation the
/// body rendering uses ([`Emitter::callee_name`]), exported so the binding plan
/// cannot drift from the call spelling: a call the lowerer gave a concrete
/// instance targets that instance's definition, everything else its
/// declaration's mangled name.
pub fn call_target_symbol(
    ws: &Workspace,
    interner: &MonoInterner,
    func: &MirFunc,
    id: Idx<Inst>,
    callee: FuncRef,
) -> String {
    match func.spec_bindings.get(&id) {
        Some(inst) => inst.symbol(ws, interner),
        None => mangle_fn(ws, func_of(ws, callee)),
    }
}

/// The binding plan for `func`, emitted under `name` for declaration
/// `self_ref`, or `None` when nothing is bound (the body then emits exactly as
/// it does under [`CalleeBindingMode::Global`]).
///
/// A callee is bound only when the emitter can PROVE it is an ordinary emitted
/// Lua function whose global is final by the time this body's binding line
/// runs. Everything else stays a global lookup:
///
/// - a callee in the caller's own strongly connected component (including
///   direct self-recursion): binding it would need its final closure before it
///   exists, and the condensation rank cannot separate them;
/// - a callee, or a caller, absent from the call graph the context was given:
///   with no component number there is no proof of either fact;
/// - an EXEMPT declaration (`extern`, `@nomangle`, `@test`, `main`, `test_*`):
///   its bare global is a published name anything may rebind, and hot reload
///   and the engine both look those up by name;
/// - an `inline(lua)` body: the definition is hand-written text, and a splice
///   may remove the call entirely;
/// - an unspecialized generic or an `Iterator`-bounded declaration: neither has
///   a standalone definition of its own.
///
/// A call THROUGH a function value (`CallValue`) has no statically known callee
/// and is never bound.
/// Runtime uint validators are initialized by the prelude and can be bound
/// even when the caller has no edges in the call graph.
pub fn callee_binding_plan(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    self_ref: FuncRef,
    func: &MirFunc,
) -> Option<CalleeBindingPlan> {
    if ctx.callee_binding != CalleeBindingMode::Upvalue {
        return None;
    }
    let rank = ctx.scc_of(self_ref);
    let mut captures: Vec<String> = Vec::new();
    let mut touches_state = false;
    // Cleanup unlinks instructions without freeing arena slots. Match the
    // emitted body's CFG so discarded calls and separate scopes use no slots.
    let blocks = reverse_postorder(func, func.entry());
    for id in blocks
        .iter()
        .flat_map(|bid| func.blocks[*bid].insts.iter().copied())
    {
        let inst = &func.insts[id];
        touches_state |= matches!(inst, Inst::Global(_) | Inst::StoreGlobal { .. });
        match inst {
            Inst::IntToUint(x) | Inst::FloatToUint(x)
                if checked_u32_constant(func, *x).is_none() =>
            {
                captures.push("__rock_checked_u32".to_string());
            }
            Inst::UintShl(..) | Inst::UintShr(..) => {
                captures.push("__rock_u32_shift_count".to_string());
            }
            _ => {}
        }
        let callee = match inst {
            Inst::Call { func, .. } | Inst::CallMulti { func, .. } => *func,
            _ => continue,
        };
        // Ranks remain fixed through body edits. New backedges must stay
        // global so the retained factory order remains acyclic.
        match (rank, ctx.scc_of(callee)) {
            (Some(ours), Some(theirs)) if theirs < ours => {}
            _ => continue,
        }
        let decl = func_of(ws, callee);
        if skull_mir::mangle::is_exempt(decl) || decl.inline_body.is_some() {
            continue;
        }
        if decl.generic_bounds.iter().any(Option::is_some) {
            continue;
        }
        if !decl.generics.is_empty() && !func.spec_bindings.contains_key(&id) {
            continue;
        }
        let symbol = call_target_symbol(ws, interner, func, id, callee);
        // A body that calls its own emitted name (a specialized instance
        // recursing through the shared declaration) would capture itself.
        if symbol == name {
            continue;
        }
        captures.push(symbol);
    }
    captures.sort_unstable();
    captures.dedup();
    if captures.is_empty() {
        return None;
    }
    // A body that touches a mutable global captures the storage cell as an
    // upvalue too, against the same 60-upvalue limit the callees count toward.
    let reserved = if ctx.state_cell() && touches_state {
        1
    } else {
        0
    };
    let limit = MAX_BOUND_CALLEES - reserved;
    let capped = captures.len() > limit;
    captures.truncate(limit);
    Some(CalleeBindingPlan {
        factory: ctx.mint_synthetic(format!("{CALLEE_FACTORY_PREFIX}{name}")),
        name: name.to_string(),
        captures,
        // Runtime validators have no body dependencies, including for a leaf
        // function absent from the call-edge inventory.
        rank: rank.unwrap_or(0),
        capped,
    })
}

/// THE gate for the emitted tail-call shape. Returning false drops the forward,
/// which leaves the ordinary emission path to write `r<N> = f(args)` followed by
/// `do return r<N> end` -- the same statements a non-tail-position call already
/// emits, so no new emitted form is introduced.
fn keeps_tail_call(ctx: &CodegenCtx, call: &TailForwardCall) -> bool {
    match ctx.tail_calls {
        TailCallShape::All => true,
        TailCallShape::None => false,
        // A call through a function value has no statically known callee, so the
        // call graph cannot prove it off every cycle.
        TailCallShape::Cycles => match call {
            TailForwardCall::Value { .. } => true,
            TailForwardCall::Direct { func, .. } => ctx.is_recursive_callee(*func),
        },
    }
}

fn tail_forwards(
    ws: &Workspace,
    ctx: &CodegenCtx,
    func: &MirFunc,
    direct: bool,
) -> Vec<Option<TailForward>> {
    let mut forwards = vec![None; func.blocks.len()];
    let cell_references = cell_reference_counts(func);
    for (block_id, block) in func.blocks.iter() {
        let Term::Return(values) = &block.term else {
            continue;
        };
        if direct
            && let Some(&call_id) = block.insts.last()
            && let Inst::Call {
                func: callee_ref,
                args,
            } = &func.insts[call_id]
        {
            let callee = func_of(ws, *callee_ref);
            let exact_wire =
                func.call_abi_plans
                    .get(&call_id)
                    .is_some_and(|abi| match abi.result_lanes() {
                        0 => values.is_empty(),
                        1 => values.as_slice() == [call_id],
                        _ => false,
                    });
            // Foreign bodies can return extra Lua values beyond their declared wire.
            if exact_wire
                && callee.inline_body.is_none()
                && !callee.is_extern
                && !callee.is_coroutine
            {
                forwards[raw(block_id) as usize] = Some(TailForward {
                    first_suppressed: block.insts.len() - 1,
                    suppressed: vec![call_id],
                    call: TailForwardCall::Direct {
                        call: call_id,
                        func: *callee_ref,
                        args: args.clone(),
                    },
                });
                continue;
            }
        }
        if values.is_empty() || block.insts.len() < values.len() + 1 {
            continue;
        }
        let first = block.insts.len() - values.len() - 1;
        let call_id = block.insts[first];
        let (dests, call) = match &func.insts[call_id] {
            Inst::CallMulti { dests, func, args } => (
                dests,
                TailForwardCall::Direct {
                    call: call_id,
                    func: *func,
                    args: args.clone(),
                },
            ),
            Inst::CallValueMulti {
                dests,
                callee,
                args,
            } => (
                dests,
                TailForwardCall::Value {
                    callee: *callee,
                    args: args.clone(),
                },
            ),
            _ => continue,
        };
        if dests.len() != values.len() {
            continue;
        }
        let exact_wire = values
            .iter()
            .zip(dests)
            .enumerate()
            .all(|(i, (&value, &dest))| {
                block.insts[first + i + 1] == value
                    && matches!(func.insts[value], Inst::Load(cell) if cell == dest)
                    && cell_references[raw(dest) as usize] == 2
            });
        if !exact_wire {
            continue;
        }
        forwards[raw(block_id) as usize] = Some(TailForward {
            first_suppressed: first,
            suppressed: block.insts[first..].to_vec(),
            call,
        });
    }
    if ctx.tail_calls != TailCallShape::All {
        for forward in forwards.iter_mut() {
            if forward
                .as_ref()
                .is_some_and(|tail| !keeps_tail_call(ctx, &tail.call))
            {
                *forward = None;
            }
        }
    }
    forwards
}

fn call_arg_forwards(ws: &Workspace, func: &MirFunc) -> (Vec<Option<CallArgForward>>, Vec<bool>) {
    let mut forwards = vec![None; func.insts.len()];
    let mut suppressed = vec![false; func.insts.len()];
    let cell_references = cell_reference_counts(func);
    let instruction_references = instruction_reference_counts(func);
    for (_, block) in func.blocks.iter() {
        for first in 0..block.insts.len() {
            let producer_id = block.insts[first];
            let (dests, producer) = match &func.insts[producer_id] {
                Inst::CallMulti { dests, func, args } => (
                    dests,
                    TailForwardCall::Direct {
                        call: producer_id,
                        func: *func,
                        args: args.clone(),
                    },
                ),
                Inst::CallValueMulti {
                    dests,
                    callee,
                    args,
                } => (
                    dests,
                    TailForwardCall::Value {
                        callee: *callee,
                        args: args.clone(),
                    },
                ),
                _ => continue,
            };
            let consumer_position = first + dests.len() + 1;
            if dests.is_empty() || consumer_position >= block.insts.len() {
                continue;
            }
            let consumer = block.insts[consumer_position];
            let Inst::Call {
                func: consumer_func,
                args: consumer_args,
            } = &func.insts[consumer]
            else {
                continue;
            };
            if func_of(ws, *consumer_func).inline_body.is_some()
                || consumer_args.len() < dests.len()
            {
                continue;
            }
            let prefix_len = consumer_args.len() - dests.len();
            let loads = &block.insts[first + 1..consumer_position];
            let exact_wire = loads
                .iter()
                .zip(dests)
                .enumerate()
                .all(|(i, (&load, &dest))| {
                    consumer_args[prefix_len + i] == load
                        && matches!(func.insts[load], Inst::Load(cell) if cell == dest)
                        && cell_references[raw(dest) as usize] == 2
                        && instruction_references[raw(load) as usize] == 1
                });
            if !exact_wire {
                continue;
            }
            suppressed[raw(producer_id) as usize] = true;
            for &load in loads {
                suppressed[raw(load) as usize] = true;
            }
            forwards[raw(consumer) as usize] = Some(CallArgForward {
                producer,
                prefix: consumer_args[..prefix_len].to_vec(),
                consumer,
                func: *consumer_func,
            });
        }
    }
    (forwards, suppressed)
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
    bound: Option<&CalleeBindingPlan>,
) -> String {
    let name = mangle_fn(ws, func_of(ws, self_ref));
    emit_named(ws, ctx, interner, &name, func, bound)
}

pub fn emit_function_with_allocation_sites(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    self_ref: FuncRef,
    func: &MirFunc,
    bound: Option<&CalleeBindingPlan>,
    instrument: bool,
) -> LuaEmission {
    let name = mangle_fn(ws, func_of(ws, self_ref));
    emit_named_with_allocation_sites(ws, ctx, interner, &name, func, bound, instrument)
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
    bound: Option<&CalleeBindingPlan>,
) -> String {
    emit_named_with_statics(ws, ctx, interner, name, func, &[], bound)
}

pub fn emit_named_with_allocation_sites(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
    bound: Option<&CalleeBindingPlan>,
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
        bound,
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
    bound: Option<&CalleeBindingPlan>,
) -> String {
    emit_named_framed_with_statics(
        ws,
        ctx,
        interner,
        name,
        func,
        None,
        inherited_statics,
        bound,
    )
}

/// The coroutine state-table context: a `co fn`'s `__step` reads/writes its
/// frame cells as fields of the state table `s` (the first param cell). A frame
/// cell renders `<s>.c<slot>` instead of a bare local and is NOT declared. The
/// slot is a DENSE per-frame ordinal (`slots`), NOT the raw cell-arena id:
/// lambda-cell allocation bumps raw ids but never reorders the frame cells, so
/// a slot -- and the layout stamp keyed on it -- is invariant under
/// lambda-internal cell-count edits (the P2 finding).
/// Lambdas are converted before emission;
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
/// `MakeEnv` record / bare `Global` + a hoisted sibling `MirFunc` before emission.
/// So this fn only renders the given
/// region.
pub fn emit_named_framed(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
    frame: Option<&FrameCtx>,
    bound: Option<&CalleeBindingPlan>,
) -> String {
    emit_named_framed_with_statics(ws, ctx, interner, name, func, frame, &[], bound)
}

fn emit_named_framed_with_statics(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
    frame: Option<&FrameCtx>,
    inherited_statics: &[StaticCell],
    bound: Option<&CalleeBindingPlan>,
) -> String {
    emit_named_framed_with_statics_impl(
        ws,
        ctx,
        interner,
        name,
        func,
        frame,
        inherited_statics,
        bound,
        None,
    )
    .0
}

pub fn emit_named_framed_with_allocation_sites(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
    frame: Option<&FrameCtx>,
    inherited_statics: &[StaticCell],
    bound: Option<&CalleeBindingPlan>,
    instrument: bool,
) -> LuaEmission {
    let sites = RefCell::new(Vec::new());
    // The header offset comes back from the emission rather than from a search
    // for `function <name>`: under `CalleeBindingMode::Upvalue` the body's only
    // copy sits inside a factory and that spelling does not appear at all.
    let (lua, header_offset) = emit_named_framed_with_statics_impl(
        ws,
        ctx,
        interner,
        name,
        func,
        frame,
        inherited_statics,
        bound,
        Some(AllocationEmission {
            body: name,
            sites: &sites,
            instrument,
        }),
    );
    let mut allocation_sites = sites.into_inner();
    let function_line_offset = lua.as_bytes()[..header_offset]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count() as u32;
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

/// Returns the emitted definition and the byte offset of its header (the
/// `function` keyword that opens the body), which the allocation-site reporter
/// turns into a body-relative line number.
fn emit_named_framed_with_statics_impl(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    func: &MirFunc,
    frame: Option<&FrameCtx>,
    inherited_statics: &[StaticCell],
    bound: Option<&CalleeBindingPlan>,
    allocation: Option<AllocationEmission<'_>>,
) -> (String, usize) {
    let mut literals = None;
    for (id, inst) in func.insts.iter() {
        if let Inst::Global(name) = inst {
            if let Some(lit) = ctx.scalar_constants.get(name) {
                let rewritten = literals.get_or_insert_with(|| func.clone());
                rewritten.insts[id] = Inst::Const(lit.clone());
            }
        }
    }
    let func = literals.as_ref().unwrap_or(func);
    let entry = func.entry();
    let params = func.params.as_slice();
    let rpo = reverse_postorder(func, entry);
    if let Some(rows) = &ctx.trace_frames {
        let mut contributors = Vec::new();
        if let Some(abi) = &func.abi_plan {
            let decl = func_of(ws, abi.decl());
            let subst = abi.subject.subst(ws, interner);
            let type_name = |ty| {
                skull_mir::monoty::convert_decl(ws, ty, &subst)
                    .map(|ty| ty.render())
                    .unwrap_or_else(|_| ws.render(None, ty))
            };
            if let Some(widths) = abi.param_leaf_widths() {
                for (i, width) in widths
                    .into_iter()
                    .enumerate()
                    .filter(|(_, width)| *width > 1)
                {
                    let rep = abi.representation().params()[i];
                    let nobox = abi.rep_plan(rep).nobox();
                    let param = &decl.param_decls[i];
                    contributors.push(format!(
                        "input `{}` of `{}` has type `{}` and is {width} scalar lanes{}",
                        param.name,
                        func.name,
                        type_name(param.ty),
                        if nobox {
                            " (nobox: cannot be boxed)"
                        } else {
                            ""
                        }
                    ));
                }
            }
            if abi.result_lanes() > 1 {
                let nobox = abi
                    .representation()
                    .result()
                    .is_some_and(|rep| abi.rep_plan(rep).nobox());
                let ty = decl
                    .return_ty
                    .map(type_name)
                    .unwrap_or_else(|| "unit".into());
                contributors.push(format!(
                    "return of `{}` has type `{ty}` and is {} scalar lanes{}",
                    func.name,
                    abi.result_lanes(),
                    if nobox {
                        " (nobox: cannot be boxed)"
                    } else {
                        ""
                    }
                ));
            }
        }
        for &block in &rpo {
            for &id in &func.blocks[block].insts {
                for line in wide_call_contributors(ws, func, id) {
                    if !contributors.contains(&line) {
                        contributors.push(line);
                    }
                }
            }
        }
        rows.lock()
            .expect("trace frame sink")
            .insert(name.to_string(), contributors);
    }
    let tail_forwards = tail_forwards(ws, ctx, func, frame.is_none());
    let (call_arg_forwards, suppressed) = if frame.is_some() {
        // Coroutine lifting moves later uses into frame fields, outside the
        // ordinary cell reference count that proves a producer unobserved.
        (vec![None; func.insts.len()], vec![false; func.insts.len()])
    } else {
        call_arg_forwards(ws, func)
    };
    let t_cells = bone_core::prelude::Instant::now();
    let cell_analysis = regalloc::CellAnalysis::new(func);
    REGALLOC_TIME.with(|c| c.set(c.get() + t_cells.elapsed()));
    let (mut inlinable, repeatable) = compute_inlinable(func, &rpo, params, &cell_analysis.pinned);
    for tail in tail_forwards.iter().flatten() {
        for &id in &tail.suppressed[1..] {
            inlinable[raw(id) as usize] = true;
        }
    }
    for (id, inst) in func.insts.iter() {
        let raw_id = raw(id) as usize;
        if suppressed[raw_id] && matches!(inst, Inst::Load(_)) {
            inlinable[raw_id] = true;
        }
    }
    alias_int_to_float(func, &rpo, &mut inlinable, &repeatable);
    skull_mir::sinking::preserve_effect_order(func, &rpo, &mut inlinable);
    bound_fold_levels(func, &rpo, &mut inlinable);
    // Frame fields map to their dense slots before this table is consulted.
    // Frame accesses name `s` implicitly in the printer rather than through a
    // MIR cell use; reserve its parameter's color so a later local can never
    // overwrite the state table itself.
    let cell_colors = if let Some(frame) = frame {
        color_cells_timed(&cell_analysis, &inlinable, &[frame.s_cell])
    } else {
        color_cells_timed(&cell_analysis, &inlinable, &[])
    };
    // Module-scope const hoisting: an all-constant `value` ctor is built ONCE as a
    // function-owned top-level global and referenced everywhere, instead of rebuilt
    // at each use. On a trace the per-use ctor sinks anyway, but in the interpreter
    // (uncompiled text-rendering loops) it allocates every time -- the nested
    // `outline_color: Vec2 { 0.0, 0.0 }` per glyph vertex was the entire skull-vs
    // -rock interpreter-path text gap. rock does the same (`__rock_const_*`). The
    // def is emitted in THIS function's chunk (not shared scaffolding), so an
    // incremental body-edit overlay carries it; value structs are immutable, so
    // re-running the def on reload is a harmless no-op (no preservation guard).
    record_representation_violations(ctx, func);
    let (hoist, hoist_names, hoist_defs) = collect_const_hoists(ws, ctx, func, name);
    let static_slots = static_slots_for(ws, interner, func, inherited_statics, name, ctx);
    // An unnamed definition is an expression its caller embeds mid-line, where
    // a directive (which must start a line) cannot go.
    let line_marks = (ctx.line_directives() && !name.is_empty()).then(|| RefCell::new(Vec::new()));
    let estimated_body_bytes = func.insts.len() * 24;
    let mut out = String::with_capacity(hoist_defs.len() + name.len() + 64 + estimated_body_bytes);
    out.push_str(&hoist_defs);
    // Under `CalleeBindingMode::Upvalue` the definition becomes a factory whose
    // parameters are named exactly like the callees they replace, so the body
    // text below is byte-identical to the global-lookup shape and each call
    // reads an immutable upvalue. `assemble` emits the matching
    // `<name> = <factory>(<captures>)` line once every capture is final.
    let header_offset = match bound {
        Some(plan) => {
            debug_assert_eq!(plan.name, name);
            let _ = write!(out, "{} = function(", plan.factory);
            for (i, capture) in plan.captures.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(capture);
            }
            out.push_str(") return ");
            let offset = out.len();
            out.push_str("function");
            offset
        }
        None => {
            let offset = out.len();
            let _ = write!(out, "function {}", name);
            offset
        }
    };
    render_scope(
        ws,
        ctx,
        interner,
        name,
        func,
        entry,
        params,
        &rpo,
        inlinable,
        true,
        frame,
        &cell_colors,
        &cell_analysis.pinned,
        &ctx.state_slots,
        &static_slots,
        &hoist,
        &hoist_names,
        allocation,
        &tail_forwards,
        &call_arg_forwards,
        &suppressed,
        line_marks.as_ref(),
        &mut out,
    );
    if bound.is_some() {
        out.push_str(" end");
    }
    out.push('\n');
    let Some(marks) = line_marks else {
        return (out, header_offset);
    };
    let Some((lua, inserted)) = write_line_directives(&out, header_offset, &marks.into_inner())
    else {
        return (out, header_offset);
    };
    // A directive goes in front of a line, so an offset at that line's start
    // moves past it.
    let shift = |offset: usize| {
        offset
            + inserted
                .iter()
                .take_while(|(at, _)| *at <= offset)
                .map(|(_, len)| len)
                .sum::<usize>()
    };
    if let Some(allocation) = allocation {
        for site in allocation.sites.borrow_mut().iter_mut() {
            site.lua_byte_offset = shift(site.lua_byte_offset);
        }
    }
    (lua, shift(header_offset))
}

/// A statement's start offset in a function's emitted text, and the span of
/// the instruction it renders.
type LineMark = (usize, Span);

/// The line LuaJIT reads after [`LINE_DIRECTIVE_RESET`] until the assembler
/// resolves it ([`resolve_line_directive_resets`]).
pub const LINE_DIRECTIVE_RESET: &str = "# 0 \"=[generated]\"";

/// Number the lines of one emitted definition with Rock source lines.
///
/// LuaJIT 2.1 (since commit c41d727d) reads `# <line> ["<file>"]` at column 0
/// as "the next line is `<line>` of `<file>`". A prototype keeps only the last
/// file it saw, so the definition is numbered against ONE file, the one most of
/// its marks come from; instructions inlined from other files keep the line of
/// the statement before them. Every line from the header on carries the line
/// of the latest mark at or before it, so the nil kills, jumps and loop heads
/// that follow a statement report that statement. Label lines produce no
/// bytecode and get no directive. Text before the header (hoisted constants)
/// belongs to the enclosing chunk and is left alone, and the definition ends
/// with [`LINE_DIRECTIVE_RESET`] so code after it is not numbered as Rock.
///
/// Returns the new text and each insertion as (offset in `lua`, bytes), in
/// ascending order, or `None` when no mark names a source file.
fn write_line_directives(
    lua: &str,
    header_offset: usize,
    marks: &[LineMark],
) -> Option<(String, Vec<(usize, usize)>)> {
    let unknown = Span::unknown().file;
    let known = |span: &Span| span.file != unknown;
    let mut counts: Vec<(ustr::Ustr, usize)> = Vec::new();
    for (_, span) in marks.iter().filter(|(_, span)| known(span)) {
        match counts.iter_mut().find(|(file, _)| *file == span.file) {
            Some((_, count)) => *count += 1,
            None => counts.push((span.file, 1)),
        }
    }
    // `max_by_key` keeps the last maximum; reversing makes a tie go to the
    // file seen first.
    let (file, _) = counts
        .iter()
        .rev()
        .max_by_key(|(_, count)| *count)
        .copied()?;
    let own: Vec<(usize, u32)> = marks
        .iter()
        .filter(|(_, span)| span.file == file)
        .map(|(offset, span)| (*offset, span.line_range.0 as u32 + 1))
        .collect();

    let header_line = lua[..header_offset].rfind('\n').map_or(0, |at| at + 1);
    let mut out = String::with_capacity(lua.len() + lua.len() / 4);
    out.push_str(&lua[..header_line]);
    let mut inserted = Vec::new();
    let mut next_mark = 0;
    let mut target = own[0].1;
    // The line LuaJIT gives the next line, once a directive has set it.
    let mut current: Option<u32> = None;
    let mut start = header_line;
    while start < lua.len() {
        let end = lua[start..]
            .find('\n')
            .map_or(lua.len(), |at| start + at + 1);
        while next_mark < own.len() && own[next_mark].0 < end {
            target = own[next_mark].1;
            next_mark += 1;
        }
        let line = &lua[start..end];
        if current != Some(target) && !line.trim_start().starts_with("::") {
            let before = out.len();
            let _ = write!(out, "# {target}");
            if current.is_none() {
                out.push(' ');
                if file.as_str().starts_with(['=', '@', ' ']) {
                    write_lua_string(&mut out, &format!("@{file}"));
                } else {
                    write_lua_string(&mut out, file.as_str());
                }
            }
            out.push('\n');
            inserted.push((start, out.len() - before));
            current = Some(target);
        }
        out.push_str(line);
        current = current.map(|line| line + 1);
        start = end;
    }
    let before = out.len();
    out.push_str(LINE_DIRECTIVE_RESET);
    out.push('\n');
    inserted.push((lua.len(), out.len() - before));
    Some((out, inserted))
}

/// Replace every [`LINE_DIRECTIVE_RESET`] in an assembled chunk with a
/// directive that numbers the following lines by their position in the chunk,
/// under the file name `chunk`. Code outside Rock definitions then reports
/// where it sits in the generated file.
pub fn resolve_line_directive_resets(lua: &str, chunk: &str) -> String {
    let mut out = String::with_capacity(lua.len());
    for (index, line) in lua.split_inclusive('\n').enumerate() {
        if line.trim_end_matches('\n') == LINE_DIRECTIVE_RESET {
            // `index` is zero-based, so the next line is `index + 2`.
            let _ = write!(out, "# {} ", index + 2);
            write_lua_string(&mut out, chunk);
            out.push('\n');
        } else {
            out.push_str(line);
        }
    }
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
    match skull_mir::global_storage::static_storage_slots(
        ws,
        interner,
        inherited_statics.iter().chain(&func.statics),
    ) {
        Ok(slots) => slots.into_iter().collect(),
        Err(error) => {
            ctx.reject(name.to_string(), CompilerError::new_no_location(error));
            HashMap::default()
        }
    }
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
            Inst::MakeValueStruct {
                representation,
                fields,
                ..
            }
            | Inst::MaterializeValueStruct {
                representation,
                fields,
                ..
            } => {
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
                        Inst::Index {
                            kind: ContainerKind::NativeBuffer | ContainerKind::NativeBufferValue(_),
                            ..
                        }
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

/// Whether a construction may be written in place instead of called.
///
/// Only the GC-table tier (`_vt`), whose ctor body is exactly the table this
/// splices; the cdata ctor is an `ffi.typeof` with no Lua body and the
/// leaf-table ctor carries the plan's presence tagging, which stays in one
/// place. A field literally named `__ty` would collide with the type tag the
/// table carries, so that construction keeps its call rather than emit a table
/// with a duplicate key.
fn splice_is_admissible(suffix: &str, fields: &[(ustr::Ustr, Idx<Inst>)]) -> bool {
    suffix == "_vt" && fields.iter().all(|(name, _)| name.as_str() != "__ty")
}

/// THE answer to "does this value construction emit a table literal rather
/// than a `<Name>_vt(..)` call" -- read by the emitter that writes it and by
/// the frame-limit model that counts the call's arity. Two seams for one
/// question drift, and a model that guessed CALL for a spliced construction
/// would over-count the frame and reject a program LuaJIT loads.
fn value_ctor_is_spliced(ctx: &CodegenCtx, func: &MirFunc, inst: &Inst) -> bool {
    let (Inst::MakeValueStruct {
        representation,
        form,
        fields,
        ..
    }
    | Inst::MaterializeValueStruct {
        representation,
        form,
        fields,
        ..
    }) = inst
    else {
        return false;
    };
    ctx.value_ctor_shape() == ValueCtorShape::Splice
        && ctor_suffix(func, *representation, *form)
            .is_some_and(|suffix| splice_is_admissible(suffix, fields))
}

fn ctor_suffix(
    func: &MirFunc,
    representation: skull_mir::lower::RepId,
    form: ValueCtorForm,
) -> Option<&'static str> {
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
            Inst::MakeValueStruct {
                name,
                representation,
                form,
                fields,
            }
            | Inst::MaterializeValueStruct {
                name,
                representation,
                form,
                fields,
            } => (*name, *representation, *form, fields),
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
/// storage, so a constant that reaches a mutable
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
    let mut groups: Vec<(ustr::Ustr, skull_mir::lower::RepId, ValueCtorForm, Vec<Lit>)> =
        Vec::new();
    let mut hoist: HashMap<Idx<Inst>, u32> = HashMap::default();
    // Only insts still REACHED BY A BLOCK. The arena keeps orphans (DCE unlinks
    // from blocks, it does not free), and hoisting one emits a module-scope ctor
    // call for a value the program never builds -- which for a nobox type is a
    // materialization the verifier cannot see, since it walks `block.insts`.
    let live: HashSet<Idx<Inst>> = func
        .blocks
        .iter()
        .flat_map(|(_, b)| b.insts.iter().copied())
        .collect();
    for (id, inst) in func.insts.iter() {
        if !live.contains(&id) {
            continue;
        }
        let Inst::MakeValueStruct {
            name,
            representation,
            form,
            fields,
        } = inst
        else {
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
        let g = match groups.iter().position(|(n, rep, ctor_form, l)| {
            *n == *name && *rep == *representation && *ctor_form == *form && *l == lits
        }) {
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
        let _ = write!(
            defs,
            "{gname} = {}{suffix}(",
            skull_mir::mangle::type_symbol(ws, *name)
        );
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
/// state-table field `<s>.c<slot>`; everything else is its colored local
/// (`c<color>`) or, when uncolored, the raw `c<raw>` fallback.
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
            let slot = *f
                .slots
                .get(&c)
                .expect("frame field cell must carry a dense slot");
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
    let Term::Branch {
        cond,
        then_bb,
        else_bb,
    } = func.blocks[h].term
    else {
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
    // The post-for edge must leave the loop. A short-circuit condition can
    // route the false arm through a join that is still inside the body.
    if else_bb == h || else_bb == latch || region.contains(&else_bb) {
        return None;
    }
    if !slot_sound(func, slot, h, latch, &region) {
        return None;
    }
    Some(CountedLoop {
        slot,
        hi,
        inclusive,
        body_entry: then_bb,
        latch,
        exit: else_bb,
        region,
    })
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
    let (
        Inst::Load(slot),
        Inst::Const(Lit::Int(1)),
        Inst::IntAdd(a, b),
        Inst::Store { cell, value },
    ) = (
        &func.insts[ins[0]],
        &func.insts[ins[1]],
        &func.insts[ins[2]],
        &func.insts[ins[3]],
    )
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
    let rhs_is_cell_load = matches!(&func.insts[rhs], Inst::Load(_)) && insts.contains(&rhs);
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
    let in_loop: HashSet<Idx<Block>> = region.iter().copied().chain([header, latch]).collect();
    let mut stores = 0usize;
    for (bid, b) in func.blocks.iter() {
        let inside = in_loop.contains(&bid);
        for &i in &b.insts {
            match &func.insts[i] {
                Inst::Store { cell, .. } if *cell == slot => stores += 1,
                Inst::Load(c) if *c == slot && !inside => return false,
                Inst::MapNext {
                    key_cell, val_cell, ..
                } if *key_cell == slot || *val_cell == slot => return false,
                _ => {}
            }
        }
    }
    stores == 2
}

// A loop that is not one of the counted shapes above stays labelled blocks plus
// a backward `goto`, which LuaJIT's parser turns into `BC_LOOP` with
// `bc_j == -1` and NO bytecode range (`lj_parse.c`, `parse_goto`). Under
// [`LoopShape::Structured`] every natural loop whose body the CFG proves is
// single-entry is wrapped in `while true do .. end` instead, so the `BC_LOOP`
// carries the range `parse_while` patches into it.

/// A natural loop, before the emitability checks.
struct NaturalLoop {
    header: Idx<Block>,
    /// Blocks of the loop EXCLUDING the header: everything that reaches a latch
    /// without crossing the header. The header is emitted as the first
    /// statements inside the `while`, so it is never in this set.
    region: HashSet<Idx<Block>>,
}

/// A natural loop the emitter spells as `while true do .. end`, keyed by header.
struct StructuredLoop {
    region: HashSet<Idx<Block>>,
}

/// Every natural loop of `func`: one per header of a back edge (an edge whose
/// target dominates its source), with the union of that header's back edges as
/// the body. A header with several latches (a `continue`) is ONE loop.
fn natural_loops(
    func: &MirFunc,
    entry: Idx<Block>,
    rpo: &[Idx<Block>],
    all_preds: &PredMap,
) -> Vec<NaturalLoop> {
    let idom = skull_mir::cfg::dominators(func, entry);
    let reach: HashSet<Idx<Block>> = rpo.iter().copied().collect();
    let mut latches: HashMap<Idx<Block>, Vec<Idx<Block>>> = HashMap::default();
    for &b in rpo {
        successors(&func.blocks[b].term, |s| {
            if reach.contains(&s) && skull_mir::cfg::dominates(&idom, s, b) {
                latches.entry(s).or_default().push(b);
            }
        });
    }
    let mut out = Vec::new();
    for &h in rpo {
        let Some(ls) = latches.get(&h) else { continue };
        let mut region: HashSet<Idx<Block>> = HashSet::default();
        let mut stack: Vec<Idx<Block>> = Vec::new();
        for &l in ls {
            if l != h && region.insert(l) {
                stack.push(l);
            }
        }
        while let Some(b) = stack.pop() {
            if let Some(ps) = all_preds.get(&b) {
                for &p in ps {
                    if p != h && region.insert(p) {
                        stack.push(p);
                    }
                }
            }
        }
        out.push(NaturalLoop { header: h, region });
    }
    out
}

/// Every natural loop of `func` that may be emitted as `while true do .. end`.
///
/// The proof obligations are exactly Lua's two block rules. A body block may be
/// entered only from inside the loop, because its label moves inside the `while`
/// and a `goto` may not jump into a block; the HEADER may be entered from
/// anywhere, because its label stays outside. And two loops must nest, because
/// a Lua block cannot interleave with another.
fn recognize_structured_loops(
    func: &MirFunc,
    entry: Idx<Block>,
    rpo: &[Idx<Block>],
    counted: &HashMap<Idx<Block>, CountedLoop>,
) -> HashMap<Idx<Block>, StructuredLoop> {
    let (all_preds, _) = predecessors(func, rpo);
    let mut kept: Vec<NaturalLoop> = Vec::new();
    for nl in natural_loops(func, entry, rpo, &all_preds) {
        if counted.contains_key(&nl.header) {
            continue;
        }
        let single_entry = nl.region.iter().all(|b| {
            all_preds
                .get(b)
                .is_none_or(|ps| ps.iter().all(|p| *p == nl.header || nl.region.contains(p)))
        });
        if !single_entry {
            continue;
        }
        kept.push(nl);
    }
    // Proper nesting, pairwise: the block sets (header included) are disjoint or
    // one contains the other. A reducible CFG always satisfies this; an
    // interleaving pair would put one body's blocks on both sides of the other's
    // `end`, so BOTH are dropped rather than guessing which to keep.
    let blocks = |nl: &NaturalLoop| -> HashSet<Idx<Block>> {
        nl.region.iter().copied().chain([nl.header]).collect()
    };
    let sets: Vec<HashSet<Idx<Block>>> = kept.iter().map(blocks).collect();
    let mut bad = vec![false; kept.len()];
    for i in 0..kept.len() {
        for j in (i + 1)..kept.len() {
            let (a, b) = (&sets[i], &sets[j]);
            let overlaps = a.iter().any(|x| b.contains(x));
            if overlaps && !a.is_subset(b) && !b.is_subset(a) {
                bad[i] = true;
                bad[j] = true;
            }
        }
    }
    let mut loops: HashMap<Idx<Block>, StructuredLoop> = HashMap::default();
    for (i, nl) in kept.into_iter().enumerate() {
        if !bad[i] {
            loops.insert(nl.header, StructuredLoop { region: nl.region });
        }
    }
    loops
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
    let returns = nonempty
        .iter()
        .filter(|l| inline_lua_return_rest(l).is_some())
        .count();
    if returns == 0 {
        return Some(if is_expr_inline_lua_body(t) {
            InlineLuaShape::Expr
        } else {
            InlineLuaShape::UnitStmts
        });
    }
    // Exactly one return, and it is the LAST non-empty line -> tail return.
    let last_is_return = nonempty
        .last()
        .is_some_and(|l| inline_lua_return_rest(l).is_some());
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
    for kw in [
        "local ", "if ", "for ", "while ", "do ", "return", "::", "repeat", "break", "goto ",
    ] {
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
            | Inst::IntAdd(..)
            | Inst::IntSub(..)
            | Inst::IntMul(..)
            | Inst::IntDiv(..)
            | Inst::IntMod(..)
            | Inst::UintAdd(..)
            | Inst::UintSub(..)
            | Inst::UintMul(..)
            | Inst::UintDiv(..)
            | Inst::UintMod(..)
            | Inst::FloatAdd(..)
            | Inst::FloatSub(..)
            | Inst::FloatMul(..)
            | Inst::FloatDiv(..)
            | Inst::FloatMod(..)
            | Inst::StringConcat(..)
            | Inst::IntLt(..)
            | Inst::IntLeq(..)
            | Inst::IntGt(..)
            | Inst::IntGeq(..)
            | Inst::UintLt(..)
            | Inst::UintLeq(..)
            | Inst::UintGt(..)
            | Inst::UintGeq(..)
            | Inst::FloatLt(..)
            | Inst::FloatLeq(..)
            | Inst::FloatGt(..)
            | Inst::FloatGeq(..)
            | Inst::Eq(..)
            | Inst::NotEq(..)
            | Inst::BitAnd(..)
            | Inst::BitOr(..)
            | Inst::BitXor(..)
            | Inst::Shl(..)
            | Inst::Shr(..)
            | Inst::UintBitAnd(..)
            | Inst::UintBitOr(..)
            | Inst::UintBitXor(..)
            | Inst::UintShl(..)
            | Inst::UintShr(..)
            | Inst::Neg(_)
            | Inst::Not(_)
            | Inst::BitNot(_)
            | Inst::UintBitNot(_)
            | Inst::ToString(_)
            | Inst::FloatToInt(_)
            | Inst::IntToFloat(_)
            | Inst::UintToFloat(_)
            | Inst::FloatToF32(_)
            | Inst::IntToUint(_)
            | Inst::UintToInt(_)
            | Inst::FloatToUint(_)
            | Inst::UintBitcastInt(_)
            | Inst::IntBitcastUint(_)
            | Inst::Format { .. }
            | Inst::FormatDyn { .. }
            | Inst::MakeTuple(_)
            | Inst::MakeVec(_)
            | Inst::MakeStruct { .. }
            | Inst::MakeValueStruct { .. }
            | Inst::MakeNullableValue { .. }
            | Inst::MakeEnum { .. }
            | Inst::MakeEnumDyn { .. }
    )
}

/// Cells written exactly once in `scope`, as a multi-return destination, and
/// read only after that call in the same block. A load of one is the call's
/// result local itself, like a parameter load: without this every leaf of a
/// wide value returned by a call occupied two locals (the result and a copy),
/// which is what pushed functions holding a few wide values past LuaJIT's
/// 250-slot frame limit. No later write can intervene, because the one write
/// dominates every read. Narrow receives qualify too.
fn single_receive_cells(
    func: &MirFunc,
    scope: &[Idx<Block>],
    pinned: &[bool],
) -> HashSet<Idx<Cell>> {
    // cell -> (defs, the receive's block and position). Only a receive records
    // a position, so a cell qualifies iff its one def is one.
    let mut defs: rustc_hash::FxHashMap<Idx<Cell>, (u32, Option<(Idx<Block>, usize)>)> =
        rustc_hash::FxHashMap::default();
    for &b in scope {
        for (pos, &id) in func.blocks[b].insts.iter().enumerate() {
            match &func.insts[id] {
                Inst::CallMulti { dests, .. } | Inst::CallValueMulti { dests, .. } => {
                    for cell in dests {
                        let entry = defs.entry(*cell).or_insert((0, None));
                        entry.0 += 1;
                        entry.1 = Some((b, pos));
                    }
                }
                // A placeholder the bound substitution replaces; never a receive.
                Inst::CallBoundMulti { dests, .. } => {
                    for cell in dests {
                        defs.entry(*cell).or_insert((0, None)).0 += 1;
                    }
                }
                other => regalloc::cell_defs(other, &mut |cell| {
                    defs.entry(cell).or_insert((0, None)).0 += 1;
                }),
            }
        }
    }
    let captured = |cell: Idx<Cell>| {
        func.closure_env_plans
            .values()
            .any(|plan| plan.captures_cell(cell) || plan.env_cell() == Some(cell))
    };
    let mut out: HashSet<Idx<Cell>> = defs
        .iter()
        .filter(|(cell, (count, at))| {
            *count == 1 && at.is_some() && !pinned[raw(**cell) as usize] && !captured(**cell)
        })
        .map(|(cell, _)| *cell)
        .collect();
    for &b in scope {
        for (pos, &id) in func.blocks[b].insts.iter().enumerate() {
            regalloc::cell_uses(&func.insts[id], &mut |cell| {
                if !out.contains(&cell) {
                    return;
                }
                let Some((_, Some((def_block, def_pos)))) = defs.get(&cell) else {
                    return;
                };
                if *def_block != b || *def_pos >= pos || !matches!(func.insts[id], Inst::Load(_)) {
                    out.remove(&cell);
                }
            });
        }
    }
    out
}

/// The set of values to TREE-FOLD into their use site (no `local rN`). Expression
/// candidates qualify at one same-block use, subject to the shared order check.
/// Loads of parameters never written in this scope qualify at every use: the
/// Lua parameter is already their value.
/// Returns the folded values, and the folded loads that may be rendered at any
/// number of use sites: a parameter never written here, or a cell whose one
/// write dominates every read.
fn compute_inlinable(
    func: &MirFunc,
    scope: &[Idx<Block>],
    params: &[Idx<Cell>],
    pinned: &[bool],
) -> (Vec<bool>, Vec<bool>) {
    // Dense side-tables keyed by raw inst id (no hashing): use count, and the
    // block of the (single) use and of the def. `NONE` = unset. Insts outside
    // `scope` (other closures share the arena) stay at 0/NONE and never qualify.
    const NONE: u32 = u32::MAX;
    let ni = func.insts.len();
    let mut use_count = vec![0u32; ni];
    let mut use_block = vec![NONE; ni];
    let mut def_block = vec![NONE; ni];
    let mut immutable_params: HashSet<Idx<Cell>> = params.iter().copied().collect();
    for &b in scope {
        let br = raw(b);
        for &id in &func.blocks[b].insts {
            match &func.insts[id] {
                Inst::Store { cell, .. } => {
                    immutable_params.remove(cell);
                }
                Inst::CallMulti { dests, .. }
                | Inst::CallValueMulti { dests, .. }
                | Inst::CallBoundMulti { dests, .. } => {
                    for cell in dests {
                        immutable_params.remove(cell);
                    }
                }
                Inst::MapNext {
                    key_cell, val_cell, ..
                } => {
                    immutable_params.remove(key_cell);
                    immutable_params.remove(val_cell);
                }
                _ => {}
            }
            if func.insts[id].produces_value() {
                def_block[raw(id) as usize] = br;
            }
            regalloc::inst_uses(&func.insts[id], &mut |u| {
                let k = raw(u) as usize;
                use_count[k] += 1;
                use_block[k] = br;
            });
            // Selected-layout emission may read the dynamic tag more than once.
            if let Inst::MakeEnumDyn { tag, .. } = func.insts[id] {
                if !matches!(func.insts[tag], Inst::Const(_)) {
                    use_count[raw(tag) as usize] += 1;
                }
            }
        }
        regalloc::term_uses(&func.blocks[b].term, &mut |u| {
            let k = raw(u) as usize;
            use_count[k] += 1;
            use_block[k] = br;
        });
    }
    let received = single_receive_cells(func, scope, pinned);
    // Dense `raw(inst) -> bool` (no hashing); probed per operand in `write_opnd`
    // and per use in `color_values`'s liveness/interference loops.
    let mut inl = vec![false; ni];
    let mut repeatable = vec![false; ni];
    for &b in scope {
        for &id in &func.blocks[b].insts {
            let k = raw(id) as usize;
            let parameter_load = matches!(
                &func.insts[id],
                Inst::Load(cell) if immutable_params.contains(cell) || received.contains(cell)
            );
            repeatable[k] = parameter_load;
            if parameter_load
                || (use_count[k] == 1
                    && def_block[k] != NONE
                    && def_block[k] == use_block[k]
                    && always_inlinable(&func.insts[id]))
            {
                inl[k] = true;
            }
        }
    }
    // A folded operand renders parenthesized, and a statement opening with `(`
    // is ambiguous Lua after a line that ends in an expression. A store base
    // other than a plain local therefore keeps its own register.
    for &b in scope {
        for &id in &func.blocks[b].insts {
            if let Inst::StoreField { base, .. } | Inst::StoreIndex { base, .. } = func.insts[id]
                && !matches!(func.insts[base], Inst::Load(_))
            {
                inl[raw(base) as usize] = false;
            }
        }
    }
    (inl, repeatable)
}

/// Fold every `IntToFloat` whose operand renders as a bare local (a register
/// or a `repeatable` load) into its uses, however many: the conversion is the
/// identity on LuaJIT numbers, and its own register would only copy that
/// local. Folded reads see through to the operand, so its register stays live
/// to the last use of the conversion. A value live across a yield is already
/// spilled to a frame field by the coroutine transform, so a resumed step
/// reads the field, never the register. An operand folded into the
/// conversion (`(a + b) as float`) keeps the conversion's register, so it is
/// still evaluated once.
fn alias_int_to_float(
    func: &MirFunc,
    scope: &[Idx<Block>],
    inlinable: &mut [bool],
    repeatable: &[bool],
) {
    for &b in scope {
        for &id in &func.blocks[b].insts {
            if let Inst::IntToFloat(x) = func.insts[id] {
                let x = raw(x) as usize;
                if !inlinable[x] || repeatable[x] {
                    inlinable[raw(id) as usize] = true;
                }
            }
        }
    }
}

/// Append the operands of the flat `..` run `id` renders as: a folded child
/// `StringConcat` contributes its own operands, anything else is one operand.
fn concat_run(func: &MirFunc, inlinable: &[bool], parts: &mut Vec<Idx<Inst>>, id: Idx<Inst>) {
    if inlinable[raw(id) as usize] {
        if let Inst::StringConcat(a, b) = func.insts[id] {
            concat_run(func, inlinable, parts, a);
            concat_run(func, inlinable, parts, b);
            return;
        }
    }
    parts.push(id);
}

/// Each operand of the folded value `id`, with the LuaJIT syntax levels its
/// rendering nests that operand under, not counting the parenthesis
/// `write_opnd` wraps `id` in. A binary operator's left operand is parsed in
/// its own loop and costs nothing; the right operand, a call argument list, a
/// table constructor and a parenthesis cost one each, so the default allows
/// two; a `..` run's operand costs its position. Allocation instrumentation
/// wraps an allocating value in `(<site> and ..)`, counted always so the
/// instrumented and plain builds fold alike.
fn fold_operand_levels(func: &MirFunc, inlinable: &[bool], id: Idx<Inst>) -> Vec<(Idx<Inst>, u32)> {
    let inst = &func.insts[id];
    let instrument = if emitted_value_allocation_kind(inst).is_some() {
        2
    } else {
        0
    };
    let uniform = |levels: u32| {
        let mut out = Vec::new();
        regalloc::inst_uses(inst, &mut |u| out.push((u, levels)));
        out
    };
    match *inst {
        Inst::StringConcat(a, b) => {
            let mut run = Vec::new();
            concat_run(func, inlinable, &mut run, a);
            concat_run(func, inlinable, &mut run, b);
            run.into_iter().zip(instrument..).collect()
        }
        Inst::IntAdd(a, b)
        | Inst::FloatAdd(a, b)
        | Inst::IntSub(a, b)
        | Inst::FloatSub(a, b)
        | Inst::IntMul(a, b)
        | Inst::FloatMul(a, b)
        | Inst::FloatDiv(a, b)
        | Inst::IntMod(a, b)
        | Inst::FloatMod(a, b)
        | Inst::IntLt(a, b)
        | Inst::UintLt(a, b)
        | Inst::FloatLt(a, b)
        | Inst::IntLeq(a, b)
        | Inst::UintLeq(a, b)
        | Inst::FloatLeq(a, b)
        | Inst::IntGt(a, b)
        | Inst::UintGt(a, b)
        | Inst::FloatGt(a, b)
        | Inst::IntGeq(a, b)
        | Inst::UintGeq(a, b)
        | Inst::FloatGeq(a, b)
        | Inst::Eq(a, b)
        | Inst::NotEq(a, b) => vec![(a, 0), (b, 1)],
        Inst::Neg(x) | Inst::Not(x) => vec![(x, 1)],
        Inst::UintMul(..) => uniform(8),
        Inst::UintShr(..) => uniform(5),
        Inst::UintBitcastInt(_) => uniform(3),
        _ => uniform(2 + instrument),
    }
}

/// Give a register to every folded operand that would nest its user's
/// expression past [`skull_mir::lua_nesting::EXPRESSION_LEVEL_BUDGET`] LuaJIT
/// syntax levels. Folding is otherwise unbounded: a wide value interpolation
/// folds into one `..` run (a level per operand) and a run of `total += x`
/// statements into one expression with a parenthesis per statement, and the
/// chunk then fails to load. Values are visited in definition order, so every
/// folded operand is already within the budget; a value over it unfolds the
/// operands that put it there, which restores the bound without touching any
/// expression that already fits. An unfolded value is evaluated at its own
/// definition, which is the order the MIR states.
fn bound_fold_levels(func: &MirFunc, scope: &[Idx<Block>], inlinable: &mut [bool]) {
    let budget = skull_mir::lua_nesting::EXPRESSION_LEVEL_BUDGET;
    // Levels a value costs where it is rendered as an operand; 0 for a register.
    let mut levels = vec![0u32; func.insts.len()];
    let measure = |inlinable: &[bool], levels: &[u32], id: Idx<Inst>| -> u32 {
        match func.insts[id] {
            Inst::Load(_) => 0,
            // Rendered as its operand alone.
            Inst::IntToFloat(x) => levels[raw(x) as usize],
            _ => {
                let deepest = fold_operand_levels(func, inlinable, id)
                    .iter()
                    .map(|&(u, at)| at + levels[raw(u) as usize])
                    .max()
                    .unwrap_or(0);
                1 + deepest
            }
        }
    };
    for &b in scope {
        for &id in &func.blocks[b].insts {
            let k = raw(id) as usize;
            if !inlinable[k] {
                continue;
            }
            levels[k] = measure(inlinable, &levels, id);
            if levels[k] <= budget {
                continue;
            }
            // A run too long by itself: its folded halves become registers.
            if let Inst::StringConcat(a, b) = func.insts[id] {
                for half in [a, b] {
                    let h = raw(half) as usize;
                    if inlinable[h] && matches!(func.insts[half], Inst::StringConcat(..)) {
                        inlinable[h] = false;
                        levels[h] = 0;
                    }
                }
            }
            for (u, at) in fold_operand_levels(func, inlinable, id) {
                let x = raw(u) as usize;
                if inlinable[x] && 1 + at + levels[x] > budget && always_inlinable(&func.insts[u]) {
                    inlinable[x] = false;
                    levels[x] = 0;
                }
            }
            levels[k] = measure(inlinable, &levels, id);
        }
    }
}

/// Render `function(<params>) <locals> <blocks> end` for the ordinary MIR
/// function rooted at `entry`, appending to `out` after its name.
#[allow(clippy::too_many_arguments)]
fn render_scope(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    // The emitted Lua name, for the rejection key of a frame-limit overflow.
    lua_name: &str,
    func: &MirFunc,
    entry: Idx<Block>,
    params: &[Idx<Cell>],
    rpo: &[Idx<Block>],
    inlinable: Vec<bool>,
    is_top: bool,
    frame: Option<&FrameCtx>,
    cell_colors: &[u32],
    pinned: &[bool],
    state_slots: &HashMap<ustr::Ustr, u32>,
    static_slots: &HashMap<ustr::Ustr, String>,
    hoist: &HashMap<Idx<Inst>, u32>,
    hoist_names: &[String],
    allocation: Option<AllocationEmission<'_>>,
    tail_forwards: &[Option<TailForward>],
    call_arg_forwards: &[Option<CallArgForward>],
    suppressed: &[bool],
    line_marks: Option<&RefCell<Vec<LineMark>>>,
    out: &mut String,
) {
    // RPO drives both block layout (preheader-before-header, for the relooper)
    // and the reachable set the regalloc colors over.
    let loops = recognize_counted_loops(func, rpo, frame);
    // Only under the non-default shape: `goto` must cost nothing and emit the
    // same bytes, so the analysis does not run at all.
    let structured = match ctx.loop_shape() {
        LoopShape::Goto => HashMap::default(),
        LoopShape::Structured => recognize_structured_loops(func, entry, rpo, &loops),
    };
    let t_ra = bone_core::prelude::Instant::now();
    // Counted-loop regions partition the coloring so loop-body temps get
    // body-scoped `local`s (see `ValueAlloc::color_scope`). Partitioning trades
    // some color sharing for scoping; if a monster function's palette total
    // nears LuaJIT's 200-active-locals parse cap, drop the partitions.
    let mut loop_scopes: Vec<regalloc::LoopScope> = loops
        .iter()
        .map(|(&h, cl)| regalloc::LoopScope {
            header: h,
            region: cl.region.iter().copied().collect(),
        })
        .collect();
    loop_scopes.sort_by_key(|ls| func.emit_block_id(ls.header));
    let mut va = regalloc::color_values(func, rpo, &inlinable, &loop_scopes);
    if va.color_scope.len() > 150 && !loop_scopes.is_empty() {
        va = regalloc::color_values(func, rpo, &inlinable, &[]);
    }
    let regalloc::ValueAlloc {
        colors,
        color_scope,
    } = va;
    // Only cells this region actually references get a local (a hoisted lambda
    // body is a full-arena clone; declaring the dead enclosing/sibling cells would
    // blow LuaJIT's 200-local cap on a large co-fn step). Computed once per top
    // scope; inline (non-`is_top`) renders declare nothing, so they skip it.
    let referenced = if is_top {
        referenced_cells_in_region(func, entry, tail_forwards, suppressed)
    } else {
        HashSet::default()
    };
    let param_cells: HashSet<Idx<Cell>> = params.iter().copied().collect();
    let is_frame = |c: &Idx<Cell>| frame.is_some_and(|f| f.fields.contains(c));
    // THE one answer to "does this cell render as a bare `c<..>` local here" --
    // read by the nil-kill exclusion just below and by the declaration loop that
    // emits those locals. A kill of a cell with no local assigns a GLOBAL of that
    // name instead: a store the pass never intended, which under the `_G` state
    // bridge costs a `__newindex` metamethod dispatch on every execution.
    let cell_has_local = |cid: Idx<Cell>| -> bool {
        param_cells.contains(&cid) || (is_top && !is_frame(&cid) && referenced.contains(&cid))
    };
    // Where each declared local dies, for the `<local> = nil` kills that keep
    // dead slots out of LuaJIT exit snapshots (see `regalloc::nil_kills`). A
    // coroutine `__step` keeps raw cell names and frame fields; it gets none.
    let kills = if frame.is_some() || !nil_kill_enabled() {
        regalloc::NilKills::none(func)
    } else {
        let mut emits_nothing = inlinable.clone();
        for id in hoist.keys() {
            emits_nothing[raw(*id) as usize] = true;
        }
        // A counted loop's header (compare) and latch (increment) are subsumed
        // by the `for` line; their instructions never run as statements.
        for (h, cl) in loops.iter() {
            for &id in func.blocks[*h]
                .insts
                .iter()
                .chain(func.blocks[cl.latch].insts.iter())
            {
                emits_nothing[raw(id) as usize] = true;
            }
        }
        // A loop-scoped register may be killed anywhere inside its `for` block:
        // the body region and the latch label, not the header (rendered as the
        // `for` line itself, outside the block).
        let kill_scopes: Vec<regalloc::LoopScope> = loops
            .iter()
            .map(|(&h, cl)| regalloc::LoopScope {
                header: h,
                region: cl
                    .region
                    .iter()
                    .copied()
                    .chain(std::iter::once(cl.latch))
                    .collect(),
            })
            .collect();
        // A cell with no local of its own is excluded exactly like a pinned one:
        // both mean "this cell is not a killable `c<color>` in this body".
        let mut excluded = pinned.to_vec();
        for (cid, _) in func.cells.iter() {
            if !cell_has_local(cid) {
                excluded[raw(cid) as usize] = true;
            }
        }
        regalloc::nil_kills(
            func,
            rpo,
            &emits_nothing,
            &colors,
            &color_scope,
            &kill_scopes,
            cell_colors,
            &excluded,
            params,
        )
    };
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

    // A body that registers a `defer` cleanup re-enters itself under the
    // unwind trampoline, which runs the cleanups still registered when an
    // error escapes and re-raises it. The flag is consumed by that re-entry,
    // so a recursive or nested call is protected again.
    if is_top && func.registers_defer_cleanup(rpo) {
        assert!(
            frame.is_none(),
            "coroutine step `{lua_name}` registers an unwind defer"
        );
        out.push_str("  if not __rock_defer_entering then return ");
        out.push_str(skull_mir::mir::DEFER_CALL);
        out.push('(');
        out.push_str(lua_name);
        for &c in params.iter() {
            out.push_str(", ");
            write_cell_local(out, func, cell_colors, c);
        }
        out.push_str(") end\n  __rock_defer_entering = false\n");
    }

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
    // Counted as the names are written, so the frame-limit model reads THIS
    // declaration rather than a second derivation of it.
    let mut top_locals = 0usize;
    for &c in &distinct {
        if any {
            out.push_str(", ");
        }
        any = true;
        top_locals += 1;
        out.push('r');
        push_uint(out, c);
    }
    if is_top {
        // Declare each cell LOCAL exactly once. Colored cells collapse onto their
        // shared `c<color>` (deduped, and excluding any color already named by a
        // param); any remaining uncolored cells declare per raw id.
        let param_colors: HashSet<u32> = params
            .iter()
            .map(|c| cell_colors[raw(*c) as usize])
            .filter(|&col| col != u32::MAX)
            .collect();
        let mut seen_color: HashSet<u32> = HashSet::default();
        for (cid, _) in func.cells.iter() {
            if param_cells.contains(&cid) || !cell_has_local(cid) {
                continue;
            }
            let (decl, col) = match cell_colors[raw(cid) as usize] {
                u32::MAX => (true, None), // uncolored -> c<raw>
                col if !param_colors.contains(&col) => (seen_color.insert(col), Some(col)),
                _ => (false, None), // color already a param's
            };
            if !decl {
                continue;
            }
            if any {
                out.push_str(", ");
            }
            any = true;
            top_locals += 1;
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

    // The declarations above are final, so this is the earliest point at which
    // the emitted frame is fully known. A body over one of LuaJIT's parse-time
    // caps is rejected here with a Rock span instead of failing at `loadfile`
    // with a Lua chunk position (`frame_limits`).
    if ctx.frame_limits() {
        check_frame_limits(
            ws,
            ctx,
            lua_name,
            func,
            rpo,
            params.len() + top_locals,
            &loops,
            &color_scope,
            hoist,
            tail_forwards,
            call_arg_forwards,
        );
    }

    // (Box `{ v = .. }` init lines are gone -- `convert_closures` materializes a
    // shared mutable capture as an explicit `MakeEnv { fn_ref: None }` value in the
    // MIR, so codegen never sees a "boxed cell".)

    let mut em = Emitter {
        enum_box_groups: RefCell::new(HashMap::default()),
        selection_preds: std::cell::OnceCell::new(),
        ws,
        ctx,
        interner,
        func,
        colors,
        color_scope,
        cell_colors,
        frame,
        loops: &loops,
        structured: &structured,
        open_loops: HashSet::default(),
        rpo,
        inlinable,
        emitted: vec![false; func.blocks.len()],
        hoist,
        hoist_names,
        state_slots,
        static_slots,
        allocation,
        tail_forwards,
        call_arg_forwards,
        suppressed,
        kills,
        line_marks,
    };
    for &bb in rpo {
        em.emit_block(out, bb); // self-skips an already-emitted block (loop body)
    }
    out.push_str("end");
}

/// Reject a body whose emitted Lua function would break one of LuaJIT's
/// parse-time frame caps, with a Rock span in place of LuaJIT's load-time
/// error. See [`frame_limits`] for the model and the direction it errs in.
///
/// Everything it reads is the emitter's own final answer: `top_locals` is the
/// `local` line just written, `color_scope` and `loops` are what the body-scoped
/// declarations and `for` lines come from, and the forward tables are what
/// decides which calls keep their results and which return statements survive.
#[allow(clippy::too_many_arguments)]
fn check_frame_limits(
    ws: &Workspace,
    ctx: &CodegenCtx,
    lua_name: &str,
    func: &MirFunc,
    rpo: &[Idx<Block>],
    top_locals: usize,
    loops: &HashMap<Idx<Block>, CountedLoop>,
    color_scope: &[Option<Idx<Block>>],
    hoist: &HashMap<Idx<Inst>, u32>,
    tail_forwards: &[Option<TailForward>],
    call_arg_forwards: &[Option<CallArgForward>],
) {
    // Almost every body is far under both caps, and the exact walk below needs
    // several instruction-sized side tables. Take an allocation-free upper
    // bound first, so a large project pays one linear scan per body rather than
    // five allocations.
    let loop_scoped_locals = color_scope.iter().flatten().count();
    if frame_limits::fits_without_measuring(func, top_locals, loops.len(), loop_scoped_locals) {
        return;
    }

    // Locals in scope inside each block: the function-top declarations, plus,
    // for every enclosing counted loop, its four control locals and its own
    // body-scoped `local r<N>` line. Regions nest, so adding per loop over its
    // region accumulates the whole enclosing nest.
    let mut actives = vec![top_locals; func.blocks.len()];
    let mut loop_locals: HashMap<Idx<Block>, usize> = HashMap::default();
    for header in color_scope.iter().flatten() {
        *loop_locals.entry(*header).or_default() += 1;
    }
    for (header, cl) in loops.iter() {
        let add = frame_limits::FOR_CONTROL_LOCALS + loop_locals.get(header).copied().unwrap_or(0);
        for &b in cl.region.iter().chain(std::iter::once(&cl.latch)) {
            actives[raw(b) as usize] += add;
        }
    }

    let mut emits_nothing = vec![false; func.insts.len()];
    for id in hoist.keys() {
        emits_nothing[raw(*id) as usize] = true;
    }
    for (header, cl) in loops.iter() {
        for &id in func.blocks[*header]
            .insts
            .iter()
            .chain(func.blocks[cl.latch].insts.iter())
        {
            emits_nothing[raw(id) as usize] = true;
        }
    }
    let mut results_unassigned = vec![false; func.insts.len()];
    let mut return_replaced = vec![false; func.blocks.len()];
    for (b, forward) in tail_forwards.iter().enumerate() {
        let Some(tail) = forward else { continue };
        return_replaced[b] = true;
        if let Some(&call) = tail.suppressed.first() {
            results_unassigned[raw(call) as usize] = true;
        }
    }
    let mut fixed_arity_override = vec![None; func.insts.len()];
    for forward in call_arg_forwards.iter().flatten() {
        // `f(prefix.., g(..))`: only the prefix are fixed arguments of `f`, and
        // `g`'s own results never land in locals.
        fixed_arity_override[raw(forward.consumer) as usize] = Some(forward.prefix.len());
        let producer = match &forward.producer {
            TailForwardCall::Direct { call, .. } => Some(*call),
            TailForwardCall::Value { .. } => None,
        };
        if let Some(call) = producer {
            results_unassigned[raw(call) as usize] = true;
        }
    }
    let mut spliced_value_ctor = vec![false; func.insts.len()];
    for (id, inst) in func.insts.iter() {
        spliced_value_ctor[raw(id) as usize] = value_ctor_is_spliced(ctx, func, inst);
    }

    let shape = frame_limits::EmittedShape {
        emits_nothing: &emits_nothing,
        results_unassigned: &results_unassigned,
        fixed_arity_override: &fixed_arity_override,
        return_replaced: &return_replaced,
        spliced_value_ctor: &spliced_value_ctor,
    };
    let Some(overflow) = frame_limits::frame_overflow(func, rpo, &actives, &shape) else {
        return;
    };
    ctx.reject(lua_name.to_string(), frame_limit_error(ws, func, &overflow));
}

/// The spanned diagnostic for a [`frame_limits::FrameOverflow`]: which Rock
/// function, which LuaJIT limit, the emitter's count, and the wide values that
/// are live across the statement that reaches it.
fn frame_limit_error(
    ws: &Workspace,
    func: &MirFunc,
    overflow: &frame_limits::FrameOverflow,
) -> CompilerError {
    let limit = overflow.limit;
    let rock_name = func.name;
    let mut message = format!(
        "`{rock_name}` is too wide for LuaJIT: the emitted Lua function needs {} {}, \
         and LuaJIT refuses to load a function over {} ({} is {}, \"{}\")",
        overflow.count,
        limit.subject(),
        limit.max_loadable(),
        limit.constant(),
        match limit {
            frame_limits::FrameLimit::Slots => frame_limits::LJ_MAX_SLOTS,
            frame_limits::FrameLimit::Locals => frame_limits::LJ_MAX_LOCVAR,
        },
        limit.luajit_message(),
    );
    if overflow.at.is_some() && (overflow.cost.call_arity > 0 || overflow.cost.wire_values > 0) {
        message.push_str(&format!(
            "\n  {} locals are live at that statement, which needs {} more slots \
             ({} call arguments, {} values received or returned)",
            overflow.actives,
            overflow.cost.slots,
            overflow.cost.call_arity,
            overflow.cost.wire_values,
        ));
    }
    let mut contributors: Vec<String> = Vec::new();
    for &call in &overflow.wide_calls {
        for line in wide_call_contributors(ws, func, call) {
            if !contributors.contains(&line) {
                contributors.push(line);
            }
        }
    }
    contributors.truncate(3);
    let nobox = contributors.iter().any(|c| c.contains("nobox"));
    if !contributors.is_empty() {
        message.push_str("\n  widest values live across a call in it:");
    }
    for line in &contributors {
        message.push_str("\n    ");
        message.push_str(line);
    }
    if nobox {
        message.push_str(
            "\n  a `nobox value` cannot be boxed automatically -- keep fewer wide values \
             live across one call, or split this function",
        );
    } else {
        message.push_str("\n  keep fewer values live across one call, or split this function");
    }
    let span = overflow
        .at
        .into_iter()
        .chain(overflow.wide_calls.iter().copied())
        .find_map(|at| func.inst_spans.get(at).copied())
        .or_else(|| any_inst_span(func))
        .unwrap_or_else(Span::unknown);
    let mut err = CompilerError::new_with_label(
        span,
        format!("`{rock_name}` exceeds LuaJIT's {} limit", limit.constant()),
    );
    err.message = message;
    err
}

/// The widest values on the wire of the call at `id`, as message lines. Read
/// from the ABI plan the call lowering itself used, so the lane counts are the
/// ones the emitted argument list actually has.
fn wide_call_contributors(ws: &Workspace, func: &MirFunc, id: Idx<Inst>) -> Vec<String> {
    let Some(abi) = func.call_abi_plans.get(&id) else {
        return Vec::new();
    };
    let decl = abi.decl();
    let callee = func_of(ws, decl);
    let callee_name = callee.name;
    let mut lines: Vec<(usize, String)> = Vec::new();
    if let Some(widths) = abi.param_leaf_widths() {
        let reps = abi.representation().params();
        for (i, width) in widths.iter().copied().enumerate() {
            if width < 2 {
                continue;
            }
            let nobox = reps.get(i).is_some_and(|r| abi.rep_plan(*r).nobox());
            let param = callee
                .param_decls
                .get(i)
                .map(|p| p.name.to_string())
                .unwrap_or_else(|| format!("#{i}"));
            lines.push((
                width,
                format!(
                    "parameter `{param}` of `{callee_name}` is {width} scalar lanes{}",
                    if nobox {
                        " (nobox: cannot be boxed)"
                    } else {
                        ""
                    }
                ),
            ));
        }
    }
    let result_lanes = abi.result_lanes();
    if result_lanes > 1 {
        let nobox = abi
            .representation()
            .result()
            .is_some_and(|r| abi.rep_plan(r).nobox());
        lines.push((
            result_lanes,
            format!(
                "the return of `{callee_name}` is {result_lanes} scalar lanes{}",
                if nobox {
                    " (nobox: cannot be boxed)"
                } else {
                    ""
                }
            ),
        ));
    }
    lines.sort_by_key(|x| std::cmp::Reverse(x.0));
    lines.truncate(3);
    lines.into_iter().map(|(_, line)| line).collect()
}

/// Any source span inside `func`, for a rejection with no single offending
/// instruction. The declarations that overflow have no instruction of their
/// own; the body's first spanned instruction still points inside the Rock
/// function the user has to change.
fn any_inst_span(func: &MirFunc) -> Option<Span> {
    for (_, block) in func.blocks.iter() {
        for &id in &block.insts {
            if let Some(span) = func.inst_spans.get(id) {
                return Some(*span);
            }
        }
    }
    None
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

type EnumBoxGroups = Vec<(Vec<ustr::Ustr>, (usize, Vec<ustr::Ustr>))>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectionTarget {
    Cell { cell: Idx<Cell>, join: Idx<Block> },
    Return,
}

#[derive(Clone, Copy)]
enum SelectionArm {
    Join {
        store: Option<(Idx<Cell>, Idx<Inst>)>,
        join: Idx<Block>,
    },
    Return(Idx<Inst>),
}

impl SelectionArm {
    fn target_value(self) -> Option<(SelectionTarget, Idx<Inst>)> {
        match self {
            Self::Join {
                store: Some((cell, value)),
                join,
            } => Some((SelectionTarget::Cell { cell, join }, value)),
            Self::Return(value) => Some((SelectionTarget::Return, value)),
            _ => None,
        }
    }
}

struct Emitter<'a> {
    enum_box_groups: RefCell<HashMap<ustr::Ustr, std::rc::Rc<EnumBoxGroups>>>,
    selection_preds: std::cell::OnceCell<PredMap>,
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
    /// Natural loops emitted as `while true do .. end`, keyed by header. Empty
    /// under [`LoopShape::Goto`], which is what makes that shape byte-identical.
    structured: &'a HashMap<Idx<Block>, StructuredLoop>,
    /// The headers of the `while` blocks currently open around the emission
    /// point, innermost last. An edge to one of these is a back edge and must be
    /// spelled as a FORWARD `goto` to that loop's continue label.
    open_loops: HashSet<Idx<Block>>,
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
    tail_forwards: &'a [Option<TailForward>],
    call_arg_forwards: &'a [Option<CallArgForward>],
    suppressed: &'a [bool],
    /// Death point of every declared local, written as `<local> = nil`.
    kills: regalloc::NilKills,
    /// Where each statement starts and which instruction it renders, when the
    /// definition is numbered with line directives.
    line_marks: Option<&'a RefCell<Vec<LineMark>>>,
}

/// `ROCK_SKULL_NIL_KILL=0` turns the nil-kill emission off, so an abort census
/// or a frame-time run can be compared against the same source without it.
fn nil_kill_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("ROCK_SKULL_NIL_KILL").map_or(true, |v| v != "0"))
}

impl Emitter<'_> {
    /// Record that the line starting at the end of `out` renders `id`.
    fn mark_line(&self, out: &str, id: Idx<Inst>) {
        if let (Some(marks), Some(span)) = (self.line_marks, self.func.inst_spans.get(id)) {
            marks.borrow_mut().push((out.len(), *span));
        }
    }

    /// [`Self::mark_line`] for an operand a terminator renders in place. A
    /// register operand was computed by an earlier statement and is not this
    /// line's source.
    fn mark_inline_operand(&self, out: &str, id: Idx<Inst>) {
        if self.inlinable[raw(id) as usize] {
            self.mark_line(out, id);
        }
    }

    fn write_kills(&self, out: &mut String, locals: &[regalloc::KillLocal]) {
        for local in locals {
            out.push_str("  ");
            match *local {
                regalloc::KillLocal::Value(c) => {
                    out.push('r');
                    push_uint(out, c);
                }
                regalloc::KillLocal::Cell(c) => {
                    out.push('c');
                    push_uint(out, c);
                }
            }
            out.push_str(" = nil\n");
        }
    }

    /// The kills due before the next statement of `bb`: the block's entry kills
    /// when no statement has been written yet, else the ones after `after`.
    fn write_pending_kills(
        &self,
        out: &mut String,
        bb: Idx<Block>,
        at_entry: bool,
        after: Option<Idx<Inst>>,
    ) {
        let locals: &[regalloc::KillLocal] = if at_entry {
            &self.kills.at_entry[raw(bb) as usize]
        } else {
            match after {
                Some(id) => &self.kills.after_inst[raw(id) as usize],
                None => &[],
            }
        };
        self.write_kills(out, locals);
    }

    /// Whether `emit_inst` writes a statement for `id` (kills attach after it).
    fn emits_statement(&self, id: Idx<Inst>) -> bool {
        !self.suppressed[raw(id) as usize]
            && !self.inlinable[raw(id) as usize]
            && !self.hoist.contains_key(&id)
    }
    fn has_state_slot(&self, name: ustr::Ustr) -> bool {
        let symbol = skull_mir::mangle::symbol_of_key(name);
        self.state_slots.contains_key(&symbol) || self.static_slots.contains_key(&symbol)
    }

    fn write_state_slot(&self, out: &mut String, name: ustr::Ustr) {
        let name = skull_mir::mangle::symbol_of_key(name);
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
            mir_inst: raw(id),
            lua_byte_offset,
        };
        allocation.sites.borrow_mut().push(site.clone());
        site
    }

    fn emit_block(&mut self, out: &mut String, bb: Idx<Block>) {
        if self.emitted[raw(bb) as usize] {
            return; // already emitted (e.g. as a loop body block)
        }
        if self.structured.contains_key(&bb) {
            self.expand_while(out, bb);
            return;
        }
        self.emitted[raw(bb) as usize] = true;
        self.write_block_label(out, bb);
        self.emit_block_contents(out, bb);
    }

    fn write_block_label(&self, out: &mut String, bb: Idx<Block>) {
        out.push_str("  ::bb");
        push_uint(out, self.func.emit_block_id(bb));
        out.push_str("::\n");
    }

    /// The label an edge to `t` names: `t`'s own block label, or the continue
    /// label of an enclosing `while` when `t` is that loop's header. The second
    /// case is a back edge, and it MUST be a forward jump to the end of the loop
    /// body: a backward `goto` would make LuaJIT emit its own rangeless
    /// `BC_LOOP` inside the loop the `while` is there to close.
    fn write_label_ref(&self, out: &mut String, t: Idx<Block>) {
        out.push_str(if self.open_loops.contains(&t) {
            "lc"
        } else {
            "bb"
        });
        push_uint(out, self.func.emit_block_id(t));
    }

    fn write_goto(&self, out: &mut String, t: Idx<Block>) {
        out.push_str("  goto ");
        self.write_label_ref(out, t);
        out.push('\n');
    }

    /// The statements of `bb` and its terminator, without its label.
    fn emit_block_contents(&mut self, out: &mut String, bb: Idx<Block>) {
        let insts: &[Idx<Inst>] = &self.func.blocks[bb].insts;
        let tail = self.tail_forwards[raw(bb) as usize].as_ref();
        let emitted_len = tail.map_or(insts.len(), |tail| tail.first_suppressed);
        // Kills are written before the next statement, so the ones after the
        // last statement of a returning block are dropped: `return` (and a
        // tail call, which replaces the frame) drops every slot itself.
        let returns = tail.is_some()
            || matches!(
                self.func.blocks[bb].term,
                Term::Return(_) | Term::Unreachable
            );
        // In a returning block, only a statement that may call can expose a
        // stale slot to an exit; kills past the last such statement are dropped.
        let last_impure = if returns {
            insts[..emitted_len]
                .iter()
                .rposition(|&id| {
                    self.emits_statement(id)
                        && !regalloc::statement_is_pure(self.func, &self.inlinable, id)
                })
                .map(|i| insts[i])
        } else {
            None
        };
        let mut live_region = !returns || last_impure.is_some();
        // Indexed rather than borrowed: `emit_inst` needs `&mut self`.
        let mut pending: Option<Idx<Inst>> = None;
        let mut first = true;
        for &id in &insts[..emitted_len] {
            if self.suppressed[raw(id) as usize] {
                continue;
            }
            if self.emits_statement(id) {
                if live_region {
                    self.write_pending_kills(out, bb, first, pending);
                }
                first = false;
                pending = Some(id);
            }
            self.emit_inst(out, id);
            if Some(id) == last_impure {
                live_region = !returns;
            }
        }
        if live_region {
            self.write_pending_kills(out, bb, first, pending);
        }
        if let Some(tail) = tail {
            self.emit_tail_forward(out, tail);
            return;
        }
        // A goto into an un-emitted counted-loop header becomes a structured
        // `for` (the preheader falls through into it); everything else is a goto.
        let term = &self.func.blocks[bb].term;
        if let Term::Goto(t) = term {
            let t = *t;
            if !self.emitted[raw(t) as usize] {
                if self.loops.contains_key(&t) {
                    self.expand_for(out, t);
                    return;
                }
                // The preheader falls straight into the `while`, so the entering
                // edge costs no jump. An edge that is NOT the fall-through (a
                // second entry, or a back edge from a block emitted after the
                // loop) still reaches the header label, which sits outside it.
                if self.structured.contains_key(&t) {
                    self.expand_while(out, t);
                    return;
                }
            }
        }
        self.emit_term(out, &self.func.blocks[bb].term, bb);
    }

    fn emit_tail_forward(&self, out: &mut String, tail: &TailForward) {
        if let TailForwardCall::Direct { call, .. } = &tail.call {
            self.mark_line(out, *call);
        }
        out.push_str("  do return ");
        self.write_forward_call(out, &tail.call);
        out.push_str(" end\n");
    }

    fn write_forward_call(&self, out: &mut String, call: &TailForwardCall) {
        match call {
            TailForwardCall::Direct { call, func, args } => {
                if let Some(forward) = &self.call_arg_forwards[raw(*call) as usize] {
                    self.write_call_arg_forward(out, forward);
                    return;
                }
                out.push_str(&self.callee_name(*call, *func));
                out.push('(');
                self.write_csv(out, args);
            }
            TailForwardCall::Value { callee, args } => {
                self.write_opnd(out, *callee);
                out.push('(');
                self.write_csv(out, args);
            }
        }
        out.push(')');
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
        // Dead on entry to the header is dead for the whole loop unless
        // redefined, so the header's kills go once, above the `for`.
        self.write_kills(out, &self.kills.at_entry[raw(header) as usize]);
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
        self.write_kills(out, &self.kills.at_entry[raw(latch) as usize]);
        out.push_str("  end\n");
        self.write_goto(out, exit);
    }

    /// Emit a proven natural loop as `while true do <header> <body> ::lcN:: end`.
    ///
    /// The header's LABEL goes before the `while` and its STATEMENTS inside, so
    /// an entering `goto` never jumps into the block (Lua forbids that) while the
    /// loop-back edge still re-runs the condition. Every back edge is rewritten
    /// to `goto lcN`, a forward jump to the continue label that closes the body;
    /// exits stay ordinary `goto`s to labels outside, which Lua allows and the
    /// single-entry proof in [`recognize_structured_loops`] is what makes safe.
    fn expand_while(&mut self, out: &mut String, header: Idx<Block>) {
        let region: HashSet<Idx<Block>> = self.structured[&header].region.iter().copied().collect();
        self.emitted[raw(header) as usize] = true;
        self.write_block_label(out, header);
        out.push_str("  while true do\n");
        self.open_loops.insert(header);
        self.emit_block_contents(out, header);
        for &bb in self.rpo {
            if region.contains(&bb) {
                self.emit_block(out, bb); // self-skips already-emitted blocks
            }
        }
        self.open_loops.remove(&header);
        let mut label = String::from("  ::lc");
        push_uint(&mut label, self.func.emit_block_id(header));
        // The last block emitted usually IS a latch, and its back edge is then a
        // jump to the very next instruction. Dropping it is a pure deletion:
        // falling off the end of the body is the same back edge. Only a
        // whole-statement suffix at the loop body's own nesting level can match,
        // because `expand_for`/`expand_while` both close with `end` first.
        let jump = format!("{}\n", label.replacen("  ::", "  goto ", 1));
        if out.ends_with(&jump) {
            let keep = out.len() - jump.len();
            out.truncate(keep);
        }
        label.push_str("::\n");
        out.push_str(&label);
        out.push_str("  end\n");
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
        self.mark_line(out, id);
        if let Some(forward) = &self.call_arg_forwards[raw(id) as usize] {
            self.emit_call_arg_forward(out, forward);
            return;
        }
        // An `@inline inline(lua)` call is spliced inline (body in the caller's
        // scope) instead of emitting a `callee(args)` call -- see the method.
        if matches!(&self.func.insts[id], Inst::Call { .. })
            && self.try_emit_inline_lua_splice(out, id)
        {
            return;
        }
        if self.try_emit_inline_index(out, id) {
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

    fn emit_call_arg_forward(&self, out: &mut String, forward: &CallArgForward) {
        out.push_str("  ");
        write_vname(out, self.func, &self.colors, forward.consumer);
        out.push_str(" = ");
        self.write_call_arg_forward(out, forward);
        out.push('\n');
    }

    fn write_call_arg_forward(&self, out: &mut String, forward: &CallArgForward) {
        out.push_str(&self.callee_name(forward.consumer, forward.func));
        out.push('(');
        self.write_csv(out, &forward.prefix);
        if !forward.prefix.is_empty() {
            out.push_str(", ");
        }
        self.write_forward_call(out, &forward.producer);
        out.push(')');
    }

    /// Splice an `@inline inline(lua)` `Call` inline, if `id` is one. Returns
    /// true iff it spliced; else the caller emits the normal `callee(args)`.
    /// The emitted symbol this call targets. A call the lowerer gave a concrete
    /// instance (its value args splatted to leaves) targets that instance's
    /// definition. The same instance object drives definition and call spelling,
    /// so the two cannot drift.
    fn callee_name(&self, id: Idx<Inst>, func: FuncRef) -> String {
        call_target_symbol(self.ws, self.interner, self.func, id, func)
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
    /// True if `id` renders in operand position as a bare name or a literal --
    /// a register, a cell, a hoisted module global or a constant -- so an inline
    /// shape may spell it twice without re-evaluating an expression or
    /// repeating an allocation (`MakeVec` and `StringConcat` fold too).
    fn opnd_is_plain(&self, id: Idx<Inst>) -> bool {
        if self.hoist.contains_key(&id) || !self.inlinable[raw(id) as usize] {
            return true;
        }
        matches!(self.func.insts[id], Inst::Load(_) | Inst::Const(_))
    }

    /// A folded `IntToFloat` of a plain operand renders as that operand alone,
    /// so it reads no more and evaluates no more than a plain operand does.
    fn plain_int_to_float(&self, id: Idx<Inst>) -> bool {
        matches!(self.func.insts[id], Inst::IntToFloat(x) if self.opnd_is_plain(x))
    }

    /// The inline index shape is selected and both operands may be named twice.
    fn inline_index_operands(&self, base: Idx<Inst>, index: Idx<Inst>) -> bool {
        self.ctx.index_shape() == IndexShape::Inline
            && self.opnd_is_plain(base)
            && self.opnd_is_plain(index)
    }

    /// `if i < 0 or i >= v.len then __rock_vec_oob(v, i) end`. The failing
    /// branch is never recorded, so the message text lives in the one cold
    /// stdlib function rather than in every indexing function's constants.
    fn write_vec_bounds_guard(&self, out: &mut String, base: Idx<Inst>, index: Idx<Inst>) {
        out.push_str("  if ");
        self.write_opnd(out, index);
        out.push_str(" < 0 or ");
        self.write_opnd(out, index);
        out.push_str(" >= ");
        self.write_opnd(out, base);
        out.push_str(".len then __rock_vec_oob(");
        self.write_opnd(out, base);
        out.push_str(", ");
        self.write_opnd(out, index);
        out.push_str(") end\n");
    }

    /// Emit a bounds-checked Vec read (`Inst::Index { kind: Vec }`) or column
    /// index (`Inst::VecCheckedIndex`) as the inline guard followed by ONE
    /// assignment, when the shape is selected and the operands are plain.
    /// Returns false to let the caller emit the helper call. The destination
    /// register usually aliases an operand, so the guard must read both
    /// operands before anything is assigned.
    fn try_emit_inline_index(&self, out: &mut String, id: Idx<Inst>) -> bool {
        match &self.func.insts[id] {
            Inst::Index {
                base,
                index,
                kind: ContainerKind::Vec,
            } if self.inline_index_operands(*base, *index) => {
                self.write_vec_bounds_guard(out, *base, *index);
                out.push_str("  ");
                write_vname(out, self.func, &self.colors, id);
                out.push_str(" = ");
                self.write_opnd(out, *base);
                out.push_str("._data[");
                self.write_opnd(out, *index);
                out.push_str(" + 1]\n");
                true
            }
            Inst::VecCheckedIndex { vec, index } if self.inline_index_operands(*vec, *index) => {
                self.write_vec_bounds_guard(out, *vec, *index);
                out.push_str("  ");
                write_vname(out, self.func, &self.colors, id);
                out.push_str(" = ");
                self.write_opnd(out, *index);
                out.push_str(" + 1\n");
                true
            }
            _ => false,
        }
    }

    fn write_opnd(&self, out: &mut String, id: Idx<Inst>) {
        if self.inlinable[raw(id) as usize] {
            if let Inst::Load(cell) = self.func.insts[id] {
                write_cellname(out, self.func, self.frame, self.cell_colors, cell);
                return;
            }
        }
        // int->float subsumption is identity on luajit (all doubles): a FOLDED
        // (inlinable) coercion renders exactly as the inner operand in this
        // position -- no register, no extra precedence wrap -- so emitted Lua
        // stays byte-identical. A reused one is folded only when its operand is
        // a bare local (`alias_int_to_float`); otherwise it keeps its own
        // register (fall through) and the `rN = <inner>` def carries the identity.
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
        concat_run(self.func, &self.inlinable, parts, id);
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
            out.push_str("(__rock_alloc_site[\"");
            write!(out, "{:016x}", site.id).unwrap();
            out.push_str("\"] and ");
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
                    out.push_str(self.ctx.state_table());
                    out.push('[');
                    self.write_state_slot(out, *g);
                    out.push(']');
                } else {
                    out.push_str(skull_mir::mangle::symbol_of_key(*g).as_str());
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
            Inst::VecStrideNilToFalse(value) => {
                out.push('(');
                self.write_opnd(out, *value);
                out.push_str(" or false)");
            }
            Inst::VecStrideFalseToNil(value) => {
                out.push('(');
                self.write_opnd(out, *value);
                out.push_str(" or nil)");
            }
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
            // These LuaJIT identity conversions must reference their operand:
            // rendering it would evaluate a producer twice.
            Inst::IntToFloat(x) | Inst::UintToFloat(x) | Inst::UintToInt(x) => {
                self.write_opnd(out, *x)
            }
            Inst::FloatToF32(x) => {
                out.push_str("__rock_f32(");
                self.write_opnd(out, *x);
                out.push(')');
            }
            Inst::IntToUint(x) | Inst::FloatToUint(x) => self.write_checked_u32(out, *x),
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
                out.push_str("math.floor(");
                self.write_opnd(out, *a);
                out.push_str(" / (2 ^ __rock_u32_shift_count(");
                self.write_opnd(out, *b);
                out.push_str(")))");
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
                write_lua_string(out, skull_mir::mangle::type_symbol(self.ws, *elem).as_str());
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
                write_lua_string(out, skull_mir::mangle::type_symbol(self.ws, *elem).as_str());
                out.push_str(", cap = ");
                self.write_opnd(out, *capacity);
                out.push('}');
            }
            Inst::MakeVecStrided {
                elem,
                stride,
                values,
                len,
                capacity,
                ..
            } => {
                out.push_str("{_data = {");
                self.write_csv(out, values);
                out.push_str("}, len = ");
                let _ = write!(out, "{len}");
                out.push_str(", __ty = \"Vec\", [");
                write_lua_string(out, skull_mir::mir::VEC_COLUMN_HEADER_FIELD);
                out.push_str("] = {repr = ");
                write_lua_string(out, skull_mir::mir::VEC_STRIDED_REPR);
                out.push_str(", elem = ");
                write_lua_string(out, skull_mir::mangle::type_symbol(self.ws, *elem).as_str());
                let _ = write!(out, ", stride = {stride}, cap = ");
                self.write_opnd(out, *capacity);
                out.push_str("}}");
            }
            Inst::MakeVecStridedHeader {
                elem,
                stride,
                capacity,
                ..
            } => {
                out.push_str("{repr = ");
                write_lua_string(out, skull_mir::mir::VEC_STRIDED_REPR);
                out.push_str(", elem = ");
                write_lua_string(out, skull_mir::mangle::type_symbol(self.ws, *elem).as_str());
                let _ = write!(out, ", stride = {stride}, cap = ");
                self.write_opnd(out, *capacity);
                out.push('}');
            }
            Inst::MakeStruct { ty, fields } => {
                out.push('{');
                let mut first = true;
                if let Some(name) = ty {
                    out.push_str("__ty = ");
                    write_lua_string(out, skull_mir::mangle::type_symbol(self.ws, *name).as_str());
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
            Inst::MakeValueStruct {
                name,
                representation,
                form,
                fields,
            }
            | Inst::MaterializeValueStruct {
                name,
                representation,
                form,
                fields,
            } => {
                let Some(suffix) = ctor_suffix(self.func, *representation, *form) else {
                    self.ctx.reject(
                        name.to_string(),
                        CompilerError::new_no_location(format!(
                            "`{name}` value constructor has no valid supplied representation plan"
                        )),
                    );
                    return;
                };
                if value_ctor_is_spliced(self.ctx, self.func, inst) {
                    // The `<Name>_vt` body, written here: same table, same shared
                    // metatable, same field ORDER (so an effectful operand still
                    // evaluates where the argument list put it) -- without the
                    // call frame that becomes a trace root.
                    out.push_str("setmetatable({ ");
                    for (field, id) in fields.iter() {
                        write_table_key(out, field.as_str());
                        self.write_opnd(out, *id);
                        out.push_str(", ");
                    }
                    let symbol = skull_mir::mangle::type_symbol(self.ws, *name);
                    let _ = write!(out, "__ty = ");
                    write_lua_string(out, symbol.as_str());
                    let _ = write!(
                        out,
                        " }}, {})",
                        value_struct_metatable_name(symbol.as_str())
                    );
                    return;
                }
                let _ = write!(
                    out,
                    "{}{suffix}(",
                    skull_mir::mangle::type_symbol(self.ws, *name)
                );
                for (i, (_, id)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    self.write_opnd(out, *id);
                }
                out.push(')');
            }
            Inst::MakeNullableValue { present, value } => {
                out.push('(');
                self.write_opnd(out, *present);
                out.push_str(" and ");
                self.write_opnd(out, *value);
                out.push_str(" or nil)");
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
                write_lua_string(out, skull_mir::mangle::type_symbol(self.ws, *ty).as_str());
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
            Inst::MakeEnumDyn {
                tag,
                ty,
                positional,
                named,
            } => {
                self.write_dynamic_enum(out, *tag, *ty, positional, named);
            }

            Inst::FfiNewArray { ctype, count, .. } => {
                let _ = write!(
                    out,
                    "ffi.new(\"{}[?]\", ",
                    skull_mir::mangle::type_symbol(self.ws, *ctype)
                );
                self.write_opnd(out, *count);
                out.push(')');
            }
            Inst::FfiViewCast { cty, ptr } => {
                let _ = write!(
                    out,
                    "ffi.cast(\"{}*\", ",
                    skull_mir::mangle::type_symbol(self.ws, *cty)
                );
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
                | ContainerKind::VecValueStrided(_)
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
                ContainerKind::VecColumn | ContainerKind::VecStrideSlot => {
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
        out.push('(');
        self.write_bin(out, op, a, b);
        out.push_str(") % 4294967296");
    }

    fn write_u32_bitfn(&self, out: &mut String, f: &str, a: &Idx<Inst>, b: &Idx<Inst>) {
        out.push('(');
        self.write_bitfn(out, f, a, b);
        out.push_str(" % 4294967296)");
    }

    fn write_u32_shift(&self, out: &mut String, f: &str, a: &Idx<Inst>, b: &Idx<Inst>) {
        out.push_str(f);
        out.push('(');
        self.write_opnd(out, *a);
        out.push_str(", __rock_u32_shift_count(");
        self.write_opnd(out, *b);
        out.push_str(")) % 4294967296");
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

    fn write_checked_u32(&self, out: &mut String, x: Idx<Inst>) {
        if let Some(value) = checked_u32_constant(self.func, x) {
            if value == 0.0 && value.is_sign_negative() {
                out.push_str("-0.0");
            } else {
                let _ = write!(out, "{value:.0}");
            }
            return;
        }
        out.push_str("__rock_checked_u32(");
        self.write_opnd(out, x);
        out.push(')');
    }

    /// READ index `base[index]`. NativeBuffer and Vec go through the
    /// debug-mode bounds-checked stdlib helpers (`__nb_checked_get` /
    /// `__vec_checked_get`) — skull runs debug; release stripping is future
    /// release-mode work. Map indexes raw (miss returns nil; `.get` is the
    /// checked form).
    fn write_index(
        &self,
        out: &mut String,
        base: Idx<Inst>,
        index: Idx<Inst>,
        kind: ContainerKind,
    ) {
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
            ContainerKind::VecStrideSlot => {
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
            ContainerKind::VecValueStrided(elem) => {
                unreachable!(
                    "whole-element index of strided [{elem}] reached emit, in `{}`",
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

    fn cell_is_frame_field(&self, cell: Idx<Cell>) -> bool {
        self.frame.is_some_and(|frame| frame.fields.contains(&cell))
    }

    // Nil writes into absent Lua hash slots can repeatedly rehash a full frame.
    // Only compiler-owned plain frame fields use this path; user stores retain
    // their ordinary assignment and metamethod behavior. Scoped vN temporaries
    // evaluate every RHS once before any destination changes.
    fn write_frame_assignment(
        &self,
        out: &mut String,
        targets: &[(String, bool)],
        rhs: impl FnOnce(&mut String),
    ) {
        out.push_str("  do local ");
        for i in 0..targets.len() {
            if i > 0 {
                out.push_str(", ");
            }
            write!(out, "v{i}").unwrap();
        }
        out.push_str(" = ");
        rhs(out);
        out.push('\n');
        for (i, (target, frame)) in targets.iter().enumerate() {
            if *frame {
                writeln!(
                    out,
                    "    if v{i} ~= nil or {target} ~= nil then {target} = v{i} end"
                )
                .unwrap();
            } else {
                writeln!(out, "    {target} = v{i}").unwrap();
            }
        }
        out.push_str("  end\n");
    }

    fn write_effect(&self, out: &mut String, id: Idx<Inst>, inst: &Inst) {
        if let Some(site) = self.record_effect_allocation_site(id, inst, out.len()) {
            out.push_str("  __rock_alloc_hit_effect(\"");
            write!(out, "{:016x}", site.id).unwrap();
            out.push_str("\")\n");
        }
        match inst {
            Inst::Store { cell, value } => {
                if self.cell_is_frame_field(*cell) {
                    let target = cellname(self.func, self.frame, self.cell_colors, *cell);
                    self.write_frame_assignment(out, &[(target, true)], |out| {
                        self.write_opnd(out, *value)
                    });
                    return;
                }
                out.push_str("  ");
                write_cellname(out, self.func, self.frame, self.cell_colors, *cell);
                out.push_str(" = ");
                self.write_opnd(out, *value);
                out.push('\n');
            }
            Inst::StoreField { base, field, value } => {
                if let Some(frame) = self.frame {
                    if matches!(self.func.insts[*base], Inst::Load(cell) if cell == frame.s_cell) {
                        let mut target = cellname(self.func, None, self.cell_colors, frame.s_cell);
                        write_field_key(&mut target, field.as_str());
                        self.write_frame_assignment(out, &[(target, true)], |out| {
                            self.write_opnd(out, *value)
                        });
                        return;
                    }
                }
                // LuaJIT can rehash when assigning nil to an absent key.
                // Metatable writes remain observable; plain operands evaluate once.
                let guard = self.opnd_is_plain(*base)
                    && self.opnd_is_plain(*value)
                    && !matches!(&self.func.insts[*value], Inst::Const(lit) if !matches!(lit, Lit::Nil));
                if guard {
                    out.push_str("  if ");
                    self.write_opnd(out, *value);
                    out.push_str(" ~= nil or getmetatable(");
                    self.write_opnd(out, *base);
                    out.push_str(") ~= nil or ");
                    self.write_opnd(out, *base);
                    write_field_key(out, field.as_str());
                    out.push_str(" ~= nil then\n");
                }
                out.push_str("  ");
                self.write_opnd(out, *base);
                write_field_key(out, field.as_str());
                out.push_str(" = ");
                self.write_opnd(out, *value);
                out.push('\n');
                if guard {
                    out.push_str("  end\n");
                }
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
                        if self.inline_index_operands(*base, *index) {
                            self.write_vec_bounds_guard(out, *base, *index);
                            out.push_str("  ");
                            self.write_opnd(out, *base);
                            out.push_str("._data[");
                            self.write_opnd(out, *index);
                            out.push_str(" + 1] = ");
                            self.write_opnd(out, *value);
                            out.push('\n');
                        } else {
                            out.push_str("  __vec_checked_set(");
                            self.write_opnd(out, *base);
                            out.push_str(", ");
                            self.write_opnd(out, *index);
                            out.push_str(", ");
                            self.write_opnd(out, *value);
                            out.push_str(")\n");
                        }
                    }
                    // Raw 1-based write into one leaf column; the owning Vec's
                    // `len` was checked once by `VecCheckedIndex`.
                    ContainerKind::Map
                    | ContainerKind::VecColumn
                    | ContainerKind::VecStrideSlot
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
                    ContainerKind::VecValueStrided(elem) => {
                        unreachable!("whole-element store into strided [{elem}] reached emit")
                    }
                }
            }
            Inst::StoreGlobal { name, value } => {
                if self.has_state_slot(*name) {
                    out.push_str("  ");
                    out.push_str(self.ctx.state_table());
                    out.push('[');
                    self.write_state_slot(out, *name);
                    out.push_str("] = ");
                    self.write_opnd(out, *value);
                    out.push('\n');
                } else {
                    let _ = write!(out, "  {} = ", skull_mir::mangle::symbol_of_key(*name));
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
                    | ContainerKind::VecValueStrided(_)
                    | ContainerKind::VecColumn
                    | ContainerKind::VecStrideSlot
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
                if self.cell_is_frame_field(*key_cell) || self.cell_is_frame_field(*val_cell) {
                    let targets: Vec<_> = [*key_cell, *val_cell]
                        .iter()
                        .map(|c| {
                            (
                                cellname(self.func, self.frame, self.cell_colors, *c),
                                self.cell_is_frame_field(*c),
                            )
                        })
                        .collect();
                    self.write_frame_assignment(out, &targets, |out| {
                        out.push_str("next(");
                        self.write_opnd(out, *table);
                        write!(out, ", {})", targets[0].0).unwrap();
                    });
                    return;
                }
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
                if dests.iter().any(|c| self.cell_is_frame_field(*c)) {
                    let targets: Vec<_> = dests
                        .iter()
                        .map(|c| {
                            (
                                cellname(self.func, self.frame, self.cell_colors, *c),
                                self.cell_is_frame_field(*c),
                            )
                        })
                        .collect();
                    self.write_frame_assignment(out, &targets, |out| {
                        write!(out, "{}(", self.callee_name(id, *func)).unwrap();
                        self.write_csv(out, args);
                        out.push(')');
                    });
                    return;
                }
                out.push_str("  ");
                for (i, c) in dests.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write_cellname(out, self.func, self.frame, self.cell_colors, *c);
                }
                let callee = self.callee_name(id, *func);
                if !dests.is_empty() {
                    out.push_str(" = ");
                }
                let _ = write!(out, "{callee}(");
                self.write_csv(out, args);
                out.push_str(")\n");
            }
            // Indirect multi-return: `d0, d1, ... = callee(args)` -- a tuple
            // returned through a fn-value. Multi-assign so Lua doesn't truncate.
            Inst::CallValueMulti {
                dests,
                callee,
                args,
            } => {
                if dests.iter().any(|c| self.cell_is_frame_field(*c)) {
                    let targets: Vec<_> = dests
                        .iter()
                        .map(|c| {
                            (
                                cellname(self.func, self.frame, self.cell_colors, *c),
                                self.cell_is_frame_field(*c),
                            )
                        })
                        .collect();
                    self.write_frame_assignment(out, &targets, |out| {
                        self.write_opnd(out, *callee);
                        out.push('(');
                        self.write_csv(out, args);
                        out.push(')');
                    });
                    return;
                }
                out.push_str("  ");
                for (i, c) in dests.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write_cellname(out, self.func, self.frame, self.cell_colors, *c);
                }
                if !dests.is_empty() {
                    out.push_str(" = ");
                }
                self.write_opnd(out, *callee);
                out.push('(');
                self.write_csv(out, args);
                out.push_str(")\n");
            }
            // Rewritten to `s.__inner = ..` by the coroutine transform; if it
            // reaches codegen the transform didn't run (a bug).
            Inst::CoroSetInner(_) => {
                unreachable!("CoroSetInner must be removed by the coroutine transform")
            }
            // Relocated to the constructor by the coroutine transform.
            Inst::CoroHook { .. } => {
                unreachable!("CoroHook must be relocated by the coroutine transform")
            }
            _ => unreachable!("not an effect"),
        }
    }

    fn selection_arm(&self, bb: Idx<Block>) -> Option<SelectionArm> {
        let block = &self.func.blocks[bb];
        if let Term::Return(values) = &block.term {
            let [value] = values.as_slice() else {
                return None;
            };
            // A return destination can replace the old store/join sink, but
            // skipping its arm must obey the same no-effects restriction.
            if !block.insts.iter().all(|id| {
                *id == *value && matches!(self.func.insts[*id], Inst::Load(_) | Inst::Const(_))
            }) {
                return None;
            }
            return Some(SelectionArm::Return(*value));
        }
        let Term::Goto(join) = block.term else {
            return None;
        };
        let store = match block.insts.as_slice() {
            [] => None,
            [s] => match self.func.insts[*s] {
                Inst::Store { cell, value } => Some((cell, value)),
                _ => return None,
            },
            [l, s] if matches!(self.func.insts[*l], Inst::Load(_) | Inst::Const(_)) => {
                match self.func.insts[*s] {
                    Inst::Store { cell, value } if value == *l => Some((cell, value)),
                    _ => return None,
                }
            }
            _ => return None,
        };
        Some(SelectionArm::Join { store, join })
    }

    fn selection_cell_matches(
        &self,
        bb: Idx<Block>,
        cond: Idx<Inst>,
        cell: Idx<Cell>,
        operand: Idx<Inst>,
    ) -> bool {
        // A selected cell can still hold the captured comparison operand after
        // copy propagation. Do not carry that equivalence across effects.
        let insts = &self.func.blocks[bb].insts;
        for &id in insts.iter().rev() {
            match self.func.insts[id] {
                Inst::Store {
                    cell: target,
                    value,
                } if target == cell => return value == operand,
                Inst::Load(source) if id == operand && source == cell => return true,
                Inst::Load(_) | Inst::Const(_) => {}
                _ if id == cond => {}
                _ => return false,
            }
        }
        matches!(self.func.insts[operand], Inst::Load(source) if source == cell)
            && self.inlinable[raw(operand) as usize]
    }

    fn selection_matches(
        &self,
        bb: Idx<Block>,
        cond: Idx<Inst>,
        value: Idx<Inst>,
        operand: Idx<Inst>,
    ) -> bool {
        if value == operand {
            return true;
        }
        match (&self.func.insts[value], &self.func.insts[operand]) {
            (Inst::Const(Lit::Float(x)), Inst::Const(Lit::Float(y))) => x.to_bits() == y.to_bits(),
            (Inst::Const(x), Inst::Const(y)) => x == y,
            (Inst::Load(cell), _) => self.selection_cell_matches(bb, cond, *cell, operand),
            _ => false,
        }
    }

    fn numeric_selection(
        &self,
        bb: Idx<Block>,
        cond: Idx<Inst>,
        yes: Idx<Block>,
        no: Idx<Block>,
    ) -> Option<(SelectionTarget, Idx<Inst>, Idx<Inst>, &'static str)> {
        if !self.inlinable[raw(cond) as usize] {
            return None;
        }
        let (a, b, less) = match self.func.insts[cond] {
            Inst::FloatLt(a, b) | Inst::IntLt(a, b) => (a, b, true),
            Inst::FloatGt(a, b) | Inst::IntGt(a, b) => (a, b, false),
            _ => return None,
        };
        // Reversing a selection's operands must not reorder deferred effects.
        for id in [a, b] {
            if self.inlinable[raw(id) as usize]
                && !matches!(self.func.insts[id], Inst::Load(_) | Inst::Const(_))
                && !self.plain_int_to_float(id)
            {
                return None;
            }
        }
        let (target, yes_value, no_value) =
            match (self.selection_arm(yes)?, self.selection_arm(no)?) {
                (SelectionArm::Return(yes), SelectionArm::Return(no)) => {
                    (SelectionTarget::Return, Some(yes), Some(no))
                }
                (
                    SelectionArm::Join { store: yes, join },
                    SelectionArm::Join {
                        store: no,
                        join: no_join,
                    },
                ) => {
                    if join != no_join {
                        return None;
                    }
                    let cell = yes.or(no)?.0;
                    if yes.is_some_and(|s| s.0 != cell) || no.is_some_and(|s| s.0 != cell) {
                        return None;
                    }
                    (
                        SelectionTarget::Cell { cell, join },
                        yes.map(|s| s.1),
                        no.map(|s| s.1),
                    )
                }
                _ => return None,
            };
        let matches = |value: Option<Idx<Inst>>, operand: Idx<Inst>| match value {
            Some(value) => self.selection_matches(bb, cond, value, operand),
            None => match target {
                SelectionTarget::Cell { cell, .. } => {
                    self.selection_cell_matches(bb, cond, cell, operand)
                }
                SelectionTarget::Return => false,
            },
        };
        if matches(yes_value, a) && matches(no_value, b) {
            Some((target, a, b, if less { "min" } else { "max" }))
        } else if matches(yes_value, b) && matches(no_value, a) {
            // Native min/max choose the second operand on ties/unordered input.
            Some((target, b, a, if less { "max" } else { "min" }))
        } else {
            None
        }
    }

    fn numeric_clamp(
        &self,
        bb: Idx<Block>,
        cond: Idx<Inst>,
        yes: Idx<Block>,
        no: Idx<Block>,
    ) -> Option<(SelectionTarget, Idx<Inst>, f64, f64)> {
        if !self.inlinable[raw(cond) as usize] {
            return None;
        }
        let Inst::FloatLt(value, lower) = self.func.insts[cond] else {
            return None;
        };
        let (target, low_value) = self.selection_arm(yes)?.target_value()?;
        if !self.selection_matches(bb, cond, low_value, lower) {
            return None;
        }
        let block = &self.func.blocks[no];
        let Term::Branch {
            cond: inner,
            then_bb,
            else_bb,
        } = block.term
        else {
            return None;
        };
        if !block.insts.iter().all(|id| {
            *id == inner || matches!(self.func.insts[*id], Inst::Load(_) | Inst::Const(_))
        }) {
            return None;
        }
        let (inner_target, upper, inner_value, "min") =
            self.numeric_selection(no, inner, then_bb, else_bb)?
        else {
            return None;
        };
        let lo = self.selection_float_literal(bb, lower)?;
        let hi = self.selection_float_literal(no, upper)?;
        // The source tests the lower bound first; an inverted range would lose
        // that priority in a nested min/max expression. NaN bounds are unordered.
        if !matches!(
            lo.partial_cmp(&hi),
            Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
        ) || !self.selection_matches(bb, cond, inner_value, value)
        {
            return None;
        }
        if inner_target != target {
            let SelectionTarget::Cell {
                cell: inner_cell,
                join: tail,
            } = inner_target
            else {
                return None;
            };
            let (forwarded_target, forwarded) = self.selection_arm(tail)?.target_value()?;
            if forwarded_target != target
                || !matches!(self.func.insts[forwarded], Inst::Load(c) if c == inner_cell)
            {
                return None;
            }
        }
        if self.inlinable[raw(value) as usize]
            && !matches!(self.func.insts[value], Inst::Load(_) | Inst::Const(_))
        {
            return None;
        }
        Some((target, value, lo, hi))
    }

    fn selection_float_literal(&self, bb: Idx<Block>, operand: Idx<Inst>) -> Option<f64> {
        if let Inst::Const(Lit::Float(value)) = self.func.insts[operand] {
            return Some(value);
        }
        let Inst::Load(cell) = self.func.insts[operand] else {
            return None;
        };
        // Captured frame fields can change through callbacks without a cell def.
        if self.cell_is_frame_field(cell) {
            return None;
        }
        let preds = self
            .selection_preds
            .get_or_init(|| predecessors(self.func, self.rpo).0);
        let mut block = bb;
        let mut limit = self.func.blocks[bb]
            .insts
            .iter()
            .position(|id| *id == operand)?;
        let mut seen = HashSet::default();
        loop {
            if !seen.insert(block) {
                return None;
            }
            for &id in self.func.blocks[block].insts[..limit].iter().rev() {
                let mut writes = false;
                regalloc::cell_defs(&self.func.insts[id], &mut |target| writes |= target == cell);
                if writes {
                    return match self.func.insts[id] {
                        Inst::Store { value, .. } => match self.func.insts[value] {
                            Inst::Const(Lit::Float(value)) => Some(value),
                            _ => None,
                        },
                        _ => None,
                    };
                }
            }
            let [parent] = preds.get(&block)?.as_slice() else {
                return None;
            };
            block = *parent;
            limit = self.func.blocks[block].insts.len();
        }
    }

    fn write_selection_result(
        &self,
        out: &mut String,
        bb: Idx<Block>,
        target: SelectionTarget,
        value: impl FnOnce(&mut String),
    ) {
        out.push_str("  ");
        match target {
            SelectionTarget::Cell { cell, .. } => {
                write_cellname(out, self.func, self.frame, self.cell_colors, cell);
                out.push_str(" = ");
            }
            SelectionTarget::Return => out.push_str("do return "),
        }
        value(out);
        match target {
            SelectionTarget::Cell { join, .. } => {
                out.push('\n');
                // Selection skips arm entry kills; apply them at the join edge.
                self.write_kills(out, &self.kills.on_edge(bb, join));
                self.write_goto(out, join);
            }
            SelectionTarget::Return => out.push_str(" end\n"),
        }
    }

    fn emit_term(&self, out: &mut String, term: &Term, bb: Idx<Block>) {
        match term {
            Term::Return(vals) if !vals.is_empty() => self.mark_inline_operand(out, vals[0]),
            Term::Branch { cond, .. } => self.mark_inline_operand(out, *cond),
            Term::Switch { discr, .. } => self.mark_inline_operand(out, *discr),
            _ => {}
        }
        match term {
            // luajit rule 2: `return` wrapped `do ... end` so it is never the
            // block's mid-sequence statement (and is legal as the last one too).
            Term::Return(vals) if !vals.is_empty() => {
                out.push_str("  do return ");
                self.write_csv(out, vals);
                out.push_str(" end\n");
            }
            Term::Return(_) => {
                out.push_str("  do return end\n");
            }
            // Bone proves these blocks dead; reaching one is a compiler bug,
            // which a bare return would turn into a silent early exit.
            Term::Unreachable => {
                out.push_str("  error(\"rock: reached an unreachable block\")\n");
            }
            Term::Goto(b) => {
                self.write_goto(out, *b);
            }
            Term::Branch {
                cond,
                then_bb,
                else_bb,
            } => {
                if let Some((target, value, lo, hi)) =
                    self.numeric_clamp(bb, *cond, *then_bb, *else_bb)
                {
                    self.write_selection_result(out, bb, target, |out| {
                        write!(out, "math.max({lo:?}, math.min({hi:?}, ").unwrap();
                        self.write_opnd(out, value);
                        out.push_str("))");
                    });
                    return;
                }
                if let Some((target, a, b, op)) =
                    self.numeric_selection(bb, *cond, *then_bb, *else_bb)
                {
                    self.write_selection_result(out, bb, target, |out| {
                        out.push_str("math.");
                        out.push_str(op);
                        out.push('(');
                        self.write_opnd(out, a);
                        out.push_str(", ");
                        self.write_opnd(out, b);
                        out.push(')');
                    });
                    return;
                }
                out.push_str("  if ");
                self.write_opnd(out, *cond);
                out.push_str(" then goto ");
                self.write_label_ref(out, *then_bb);
                out.push_str(" else goto ");
                self.write_label_ref(out, *else_bb);
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
                    out.push_str(" then goto ");
                    self.write_label_ref(out, *b);
                    out.push('\n');
                }
                if first {
                    self.write_goto(out, *default);
                } else {
                    out.push_str("  else goto ");
                    self.write_label_ref(out, *default);
                    out.push_str(" end\n");
                }
            }
            Term::Yield { .. } => {
                unreachable!("Yield must be removed by the coroutine transform before codegen")
            }
        }
    }

    fn write_dynamic_enum(
        &self,
        out: &mut String,
        tag: Idx<Inst>,
        ty: ustr::Ustr,
        positional: &[Idx<Inst>],
        named: &[(ustr::Ustr, Idx<Inst>)],
    ) {
        if matches!(self.func.insts[tag], Inst::Const(_)) {
            self.write_enum_box(out, tag, ty, positional, named);
            return;
        }
        let cached = self.enum_box_groups.borrow().get(&ty).cloned();
        let groups = if let Some(groups) = cached {
            groups
        } else {
            let plan = skull_mir::lower::EnumPlan::for_name(self.ws, ty)
                .expect("dynamic enum has a representation plan");
            let shape = plan.flat().expect("dynamic enum has a flat shape");
            let eid = self.ws.enums_by_name[&ty];
            let mut groups = EnumBoxGroups::new();
            for variant in &self.ws.modules[eid.module.index()].enums[eid.def].variants {
                if matches!(variant.payload, bone_semantic::hir::VariantPayload::Unit) {
                    continue;
                }
                let layout = shape.box_layout(self.ws, variant.name);
                if let Some((members, _)) = groups.iter_mut().find(|(_, key)| *key == layout) {
                    members.push(variant.name);
                } else {
                    groups.push((vec![variant.name], layout));
                }
            }
            let common = groups
                .iter()
                .enumerate()
                .max_by_key(|(_, (members, _))| members.len())
                .map(|(i, _)| i)
                .expect("flat enum has payload variants");
            let last = groups.remove(common);
            groups.push(last);
            let groups = std::rc::Rc::new(groups);
            self.enum_box_groups.borrow_mut().insert(ty, groups.clone());
            groups
        };
        if groups.len() > 1 {
            out.push('(');
        }
        for (i, (members, (width, fields))) in groups.iter().enumerate() {
            if i + 1 < groups.len() {
                out.push('(');
                for (j, variant) in members.iter().enumerate() {
                    if j > 0 {
                        out.push_str(" or ");
                    }
                    self.write_opnd(out, tag);
                    out.push_str(" == ");
                    write_lua_string(out, variant.as_str());
                }
                out.push_str(") and ");
            }
            let selected: Vec<_> = named
                .iter()
                .copied()
                .filter(|(name, _)| fields.contains(name))
                .collect();
            self.write_enum_box(out, tag, ty, &positional[..*width], &selected);
            if i + 1 < groups.len() {
                out.push_str(" or ");
            }
        }
        if groups.len() > 1 {
            out.push(')');
        }
    }

    fn write_enum_box(
        &self,
        out: &mut String,
        tag: Idx<Inst>,
        ty: ustr::Ustr,
        positional: &[Idx<Inst>],
        named: &[(ustr::Ustr, Idx<Inst>)],
    ) {
        out.push_str("{__variant = ");
        self.write_opnd(out, tag);
        out.push_str(", __ty = ");
        write_lua_string(out, skull_mir::mangle::type_symbol(self.ws, ty).as_str());
        for id in positional {
            out.push_str(", ");
            self.write_opnd(out, *id);
        }
        for (name, id) in named {
            let _ = write!(out, ", {name} = ");
            self.write_opnd(out, *id);
        }
        out.push('}');
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
        Lit::Omitted => out.push_str(OMITTED_ARG),
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
    cs.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
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
