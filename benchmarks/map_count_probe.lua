-- Prototype the cost of maintaining map cardinality under controlled writes.
-- This is not the Map runtime: raw Lua writes bypass the count, so the
-- representation is only sound when every mutation is known to the compiler.
-- Run with LuaJIT enabled: luajit benchmarks/map_count_probe.lua

local function size(data)
  local n = 0
  for _ in next, data do n = n + 1 end
  return n
end

local function create(n)
  local data = {}
  for i = 1, n do data['k' .. i] = i end
  local keys, transient = {}, {}
  for i = 1, 64 do
    keys[i] = 'k' .. i
    transient[i] = 'transient' .. i
  end
  return { data = data, count = n, keys = keys, transient = transient }
end

local function put(state, key, value)
  local old = state.data[key]
  if old == nil then
    if value ~= nil then state.count = state.count + 1 end
  elseif value == nil then
    state.count = state.count - 1
  end
  if value ~= nil or old ~= nil then state.data[key] = value end
end

local function remove(state, key)
  local old = state.data[key]
  if old ~= nil then
    state.data[key] = nil
    state.count = state.count - 1
  end
  return old
end

local function verify()
  local s = create(8)
  put(s, 'k1', false)
  assert(s.count == 8 and s.data.k1 == false)
  put(s, 'extra', 9)
  assert(s.count == 9)
  put(s, 'extra', nil)
  assert(s.count == 8)
  put(s, 'missing', nil)
  assert(s.count == 8)
  assert(remove(s, 'k1') == false and s.count == 7)
  assert(remove(s, 'k1') == nil and s.count == 7)
  for k in next, s.data do remove(s, k) end
  assert(s.count == 0 and size(s.data) == 0)
  s.data.external = 1
  assert(s.count ~= size(s.data), 'external writes must expose the stale count')
end

local function lengths(s, n, counted)
  local sum = 0
  for _ = 1, n do sum = sum + (counted and s.count or size(s.data)) end
  return sum
end

local function overwrites(s, n, counted)
  for i = 1, n do
    local key = s.keys[i % 64 + 1]
    if counted then put(s, key, i) else s.data[key] = i end
  end
  return s.data[s.keys[n % 64 + 1]]
end

local function toggles(s, n, counted, frequency)
  local sum = 0
  for i = 1, n do
    local key = s.transient[i % 64 + 1]
    local old = s.data[key]
    if old == nil then
      if counted then put(s, key, i) else s.data[key] = i end
      sum = sum + i
    else
      if counted then remove(s, key) else s.data[key] = nil end
      sum = sum + old
    end
    if frequency > 0 and i % frequency == 0 then
      sum = sum + (counted and s.count or size(s.data))
    end
  end
  return sum
end

local function absent_removes(s, n, counted)
  local sum = 0
  for _ = 1, n do
    local old
    if counted then
      old = remove(s, 'absent')
    else
      old = s.data.absent
      if old ~= nil then s.data.absent = nil end
    end
    if old == nil then sum = sum + 1 end
  end
  return sum
end

local function churn(s, n, counted, frequency)
  local sum = 0
  for i = 1, n do
    if counted then
      put(s, 'transient', i)
    else
      s.data.transient = i
    end
    if frequency > 0 and i % frequency == 0 then
      sum = sum + (counted and s.count or size(s.data))
    end
    if counted then
      sum = sum + remove(s, 'transient')
    else
      local old = s.data.transient
      if old ~= nil then s.data.transient = nil end
      sum = sum + old
    end
  end
  return sum
end

local function clear_refill(s, n, counted)
  local keys = { 'k1', 'k2', 'k3', 'k4', 'k5', 'k6', 'k7', 'k8' }
  local sum = 0
  for _ = 1, n do
    for k, value in next, s.data do
      sum = sum + value
      if counted then remove(s, k) else s.data[k] = nil end
    end
    for i, k in ipairs(keys) do
      if counted then put(s, k, i) else s.data[k] = i end
    end
  end
  return sum
end

local function measure(run, entries, reps, counted, frequency)
  jit.flush()
  local warm = create(entries)
  run(warm, 1000, counted, frequency)
  local s = create(entries)
  collectgarbage('collect')
  local t = os.clock()
  local sink = run(s, reps, counted, frequency)
  local ns = (os.clock() - t) * 1e9 / reps
  assert(size(s.data) == entries)
  if counted then assert(s.count == entries) end
  return ns, sink
end

verify()
assert(jit.status(), 'measure with the JIT enabled')
print(jit.version .. ' ' .. jit.arch)
local cases = {
  { 'len_8', lengths, 8, 500000 },
  { 'len_1000', lengths, 1000, 20000 },
  { 'overwrite', overwrites, 200, 500000 },
  { 'remove_absent', absent_removes, 200, 500000 },
  { 'insert_remove', churn, 200, 500000, 0 },
  { 'insert_len_remove', churn, 200, 100000, 1 },
  { 'insert_len_every_16_remove', churn, 200, 500000, 16 },
  { 'toggle_64', toggles, 200, 512000, 0 },
  { 'toggle_64_len_every_16', toggles, 200, 512000, 16 },
  { 'clear_refill_8', clear_refill, 8, 100000 },
}
for _, case in ipairs(cases) do
  local times = { {}, {} }
  local expected
  for round = 1, 9 do
    local order = round % 2 == 1 and { 1, 2 } or { 2, 1 }
    for _, variant in ipairs(order) do
      local ns, sink = measure(case[2], case[3], case[4], variant == 2, case[5])
      if expected then assert(sink == expected) else expected = sink end
      times[variant][round] = ns
    end
  end
  for _, t in ipairs(times) do table.sort(t) end
  print(string.format('%s raw_min_ns=%.3f raw_median_ns=%.3f counted_min_ns=%.3f counted_median_ns=%.3f sink=%.0f',
    case[1], times[1][1], times[1][5], times[2][1], times[2][5], expected))
end
