-- usage: luajit driver.lua <compiled.lua> [rounds]
local file, rounds = arg[1], tonumber(arg[2] or 7)
dofile(file)
local N = tonumber(os.getenv("N") or 1000)
local data = {}
for i = 1, N do data[i] = i - 1 end
local v = {_data = data, len = N, __ty = "Vec"}
local K = 2000 -- larger than every element: any/find/all scan the whole Vec
local function find(prefix)
  for name, f in pairs(_G) do
    if type(f) == "function" and name:sub(1, #prefix + 1) == prefix .. "_" and not name:find("lambda") then return f end
  end
  error("missing " .. prefix)
end
local cases = {
  {"map_filter_sum", function(f) return f(v) end},
  {"map_capture_sum", function(f) return f(v, 3) end},
  {"filter_count", function(f) return f(v, 500) end},
  {"any", function(f) return f(v, K) end},
  {"all", function(f) return f(v, K) end},
  {"find", function(f) return f(v, K) end},
  {"enumerate", function(f) return f(v) end},
  {"zip", function(f) return f(v, v) end},
  {"collect", function(f) return f(v) end},
  {"range_sum", function(f) return f(N, 3) end},
  {"reduce", function(f) return f(v) end},
}
local ITERS = math.floor(2000000 / N)
local clock = os.clock
local results = {}
for _, c in ipairs(cases) do
  for _, kind in ipairs({"chain", "loop"}) do
    local f = find(kind .. "_" .. c[1])
    local call = c[2]
    local a, b = call(f), call(find((kind == "chain" and "loop_" or "chain_") .. c[1]))
    if type(a) == "table" then a, b = a.len, b.len end
    assert(a == b, c[1] .. " mismatch " .. tostring(a) .. " vs " .. tostring(b))
    results[c[1] .. "/" .. kind] = {f = f, call = call, t = {}, alloc = 0}
  end
end
for r = 1, rounds do
  for _, c in ipairs(cases) do
    for _, kind in ipairs(r % 2 == 0 and {"chain", "loop"} or {"loop", "chain"}) do
      local e = results[c[1] .. "/" .. kind]
      local f, call = e.f, e.call
      for _ = 1, 50 do call(f) end
      collectgarbage("collect"); collectgarbage("stop")
      local m0 = collectgarbage("count")
      local t0 = clock()
      for _ = 1, ITERS do call(f) end
      local t1 = clock()
      local m1 = collectgarbage("count")
      collectgarbage("restart")
      e.t[#e.t + 1] = (t1 - t0) / (ITERS * N) * 1e9
      e.alloc = (m1 - m0) * 1024 / ITERS
    end
  end
end
local function stats(t) table.sort(t); return t[1], t[math.floor((#t + 1) / 2)] end
print(string.format("%-16s %10s %10s %10s %10s %9s %9s", "case", "chain min", "chain med", "loop min", "loop med", "chain B", "loop B"))
for _, c in ipairs(cases) do
  local ch, lo = results[c[1] .. "/chain"], results[c[1] .. "/loop"]
  local cmin, cmed = stats(ch.t); local lmin, lmed = stats(lo.t)
  print(string.format("%-16s %10.3f %10.3f %10.3f %10.3f %9.0f %9.0f", c[1], cmin, cmed, lmin, lmed, ch.alloc, lo.alloc))
end
