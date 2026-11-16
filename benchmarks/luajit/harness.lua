-- Abort-counting harness. Usage:
--   luajit harness.lua <bench.lua> [args...]
-- Installs a jit.attach("trace") callback, runs the bench file
-- with `arg` rebased so arg[1] is the first bench argument, then prints
-- a table of trace starts/stops/aborts split root/side and abort buckets
-- keyed by "reason @ location (parent trace/exit)".
local jutil = require("jit.util")
local vmdef = require("jit.vmdef")
local traceerr, ffnames, funcinfo = vmdef.traceerr, vmdef.ffnames, jutil.funcinfo

local bench = assert(arg[1], "usage: luajit harness.lua <workload.lua> [args...]")
assert(jit.status(), "run this diagnostic with the JIT enabled")
local bargs = {}
for i = 2, #arg do bargs[#bargs + 1] = arg[i] end
bargs[0] = bench

local stats = {
  root_start = 0, side_start = 0, root_stop = 0, side_stop = 0,
  root_abort = 0, side_abort = 0, flush = 0,
}
local buckets, order = {}, {}
local parent_of = {}   -- tr -> "parent/exit" for side traces, nil for root

local function loc_of(func, pc)
  if not func then return "(?)" end
  local info = funcinfo(func, pc)
  local line = info.currentline or info.linedefined
  if info.loc then return info.loc end
  if info.source then return tostring(info.source) .. ":" .. tostring(line or 0) end
  if info.ffid then return "ff:" .. (ffnames[info.ffid] or tostring(info.ffid)) end
  if info.addr then return string.format("C:%x", info.addr) end
  return "(?)"
end

local function cb(what, tr, func, pc, otr, oex)
  if what == "start" then
    if otr then
      parent_of[tr] = otr .. "/" .. oex
      stats.side_start = stats.side_start + 1
    else
      parent_of[tr] = false
      stats.root_start = stats.root_start + 1
    end
  elseif what == "stop" then
    if parent_of[tr] then stats.side_stop = stats.side_stop + 1
    else stats.root_stop = stats.root_stop + 1 end
  elseif what == "flush" then
    stats.flush = stats.flush + 1
  elseif what == "abort" then
    local err = traceerr[otr]
    if err then
      local a = oex
      if type(oex) == "number" and err:find("bytecode") then
        local n = oex * 6 + 1
        a = vmdef.bcnames:sub(n, n + 5):gsub(" +$", "")
      end
      err = err:gsub("%%[sd]", function() return tostring(a) end)
    else
      err = tostring(otr)
    end
    local p = parent_of[tr]
    local key
    if p then
      stats.side_abort = stats.side_abort + 1
      key = err .. " @ " .. loc_of(func, pc) .. " (side of " .. p .. ")"
    else
      stats.root_abort = stats.root_abort + 1
      key = err .. " @ " .. loc_of(func, pc) .. " (root)"
    end
    if not buckets[key] then order[#order + 1] = key; buckets[key] = 0 end
    buckets[key] = buckets[key] + 1
  end
end
jit.attach(cb, "trace")

local chunk = assert(loadfile(bench))
arg = bargs  -- benches read the global arg, like a top-level script
local ok, err = pcall(chunk, unpack(bargs))
jit.attach(cb)  -- detach so the report itself is not counted
jit.off()

-- Aggregate NYICOAL count once so callers can grep one line.
local nyicoal = 0
for k, n in pairs(buckets) do
  if k:find("coalescing", 1, true) then nyicoal = nyicoal + n end
end

if not ok then io.stderr:write("bench error: " .. tostring(err) .. "\n") end
print(string.format("--- %s %s", bench, table.concat(bargs, " ")))
print(string.format("root: start=%d stop=%d abort=%d | side: start=%d stop=%d abort=%d | flush=%d | NYICOAL=%d",
  stats.root_start, stats.root_stop, stats.root_abort,
  stats.side_start, stats.side_stop, stats.side_abort, stats.flush, nyicoal))
table.sort(order, function(a, b) return buckets[a] > buckets[b] end)
for _, k in ipairs(order) do
  print(string.format("%6d  %s", buckets[k], k))
end
if not ok then os.exit(1) end
