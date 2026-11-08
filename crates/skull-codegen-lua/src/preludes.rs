//! Runtime preludes and shims prepended to emitted chunks.

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
function __vec_checked_get(vec, i)\n\
  if i < 0 or i >= vec.len then error('Vec index out of bounds') end\n\
  return vec._data[i + 1]\n\
end\n\
function __vec_checked_set(vec, i, v)\n\
  if i < 0 or i >= vec.len then error('Vec index out of bounds') end\n\
  vec._data[i + 1] = v\n\
end";
