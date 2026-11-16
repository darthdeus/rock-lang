-- Exit inside a callee, dead hoisted locals in the caller frame.
-- Usage: luajit harness.lua caller.lua <structured|goto|kill> <D> <L>
-- `kill` = goto-shaped callee, caller nils its dead locals before the call.
-- lj_snap_shrink only applies use/def pruning to the innermost frame, so
-- caller-frame slots are kept whatever the shape. The callee's own branch is
-- structured or goto-shaped; the caller is always structured here.
local shape, D, L = arg[1], tonumber(arg[2]), tonumber(arg[3])
assert(shape == "structured" or shape == "goto" or shape == "kill",
       "shape must be structured, goto or kill")
assert(D and L and D >= 0 and L >= 0 and D % 1 == 0 and L % 1 == 0 and D + L <= 180,
       "dead/live local counts must be nonnegative integers totaling at most 180")
local iters = 4000
local src = {}
local function p(s) src[#src + 1] = s end
local dn, ln = {}, {}
for k = 1, D do dn[k] = "d" .. k end
for k = 1, L do ln[k] = "l" .. k end
p("local bor = bit.bor")
p("local acc = 0")
if shape == "structured" then
  p("local function callee(x) if x > 120 then acc = acc + 1 end return x end")
else
  p("local function callee(x) if x > 120 then goto A else goto B end ::A:: acc = acc + 1 goto B ::B:: return x end")
end
p("return function(t, n)")
p("  local tot = 0")
p("  for i = 1, n do")
p("  local " .. (D > 0 and (table.concat(dn, ", ") .. ", ") or "") ..
  (L > 0 and (table.concat(ln, ", ") .. ", ") or "") .. "s, y")
p("  local x = t[i]")
for k = 1, D do p("  d" .. k .. " = bor(i, " .. k .. ")") end
p("  s = " .. (D > 0 and table.concat(dn, " + ") or "0"))
for k = 1, L do p("  l" .. k .. " = bor(i, " .. (100 + k) .. ")") end
if shape == "kill" then
  for k = 1, D do p("  d" .. k .. " = nil") end
end
p("  y = callee(x)")
p("  tot = tot + s + y" .. (L > 0 and (" + " .. table.concat(ln, " + ")) or ""))
p("  end")
p("  return tot + acc")
p("end")
local code = table.concat(src, "\n")
if os.getenv("SHOW") then print(code) end
local f = assert(loadstring(code))()
local t = {}
for i = 1, iters do t[i] = i % 150 end
local tot = 0
for rep = 1, 3 do tot = tot + f(t, iters) end
io.stderr:write(string.format("callee shape=%s D=%d L=%d tot=%g\n", shape, D, L, tot))
