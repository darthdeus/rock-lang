-- What a nil store into an absent table key costs, which is why the Lua
-- emitter guards field and frame stores it cannot prove non-nil.
--
--     luajit benchmarks/nil_store_probe.lua
--
-- Run this before changing or removing the guard, and on each target
-- architecture: the guard is worth its branch only while the raw store is
-- expensive. Reports, per table fill level, the memory and the time of a
-- nil store into an absent key, against the guarded form and a real store.
--
-- The mechanism is time, not memory. A nil store never leaves a node
-- behind, so a table whose hash part has no free node rehashes to make
-- room, drops the nil on the way, and rehashes again for the next one --
-- the same work repeated forever at no net memory growth. Measuring only
-- `collectgarbage("count")` makes the guard look pointless.

local N = 2000000

local function kb()
  collectgarbage("collect")
  return collectgarbage("count")
end

local function filled(n)
  local t = {}
  for i = 1, n do
    t["live" .. i] = i
  end
  return t
end

local absent = {}
for i = 0, 7 do
  absent[i] = "absent" .. i
end

print(jit.version)
print(string.format("%-6s %-26s %10s %12s", "live", "store", "memory", "time"))

local function row(n, label, run)
  local t = filled(n)
  collectgarbage("collect")
  local before = kb()
  local t0 = os.clock()
  run(t)
  local elapsed = os.clock() - t0
  local grew = kb() - before
  print(string.format("%-6d %-26s %+9.0f KB %9.2f ns", n, label, grew, elapsed * 1e9 / N))
end

for _, n in ipairs({ 3, 7, 15, 31, 63, 127 }) do
  row(n, "raw, absent key, nil", function(t)
    for i = 1, N do
      t[absent[i % 8]] = nil
    end
  end)
  row(n, "guarded, absent key, nil", function(t)
    for i = 1, N do
      local k = absent[i % 8]
      local v = nil
      if v ~= nil or t[k] ~= nil then
        t[k] = v
      end
    end
  end)
end

-- A real store is the control: it does occupy a node, so memory grows.
row(15, "raw, absent key, real value", function(t)
  for i = 1, N do
    t[absent[i % 8]] = i
  end
end)
