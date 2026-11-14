-- Run with emitted guard_proofs.rock Lua as the first argument.
-- Optional second argument selects one workload by name.
-- Fixed valid inputs only; this is not a correctness test for unchecked code.
dofile(arg[1])
assert(jit.status())
local n = 4096
local xs = {_data={},len=n}
local buf = {ptr=ffi.new('int[?]',n),len=n}
for i=1,n do xs._data[i]=i;buf.ptr[i-1]=i end
local workloads={
 {'fresh_push',function() return guard_fresh_push(n) end,10000,n},
 {'accessor_loop',function() return guard_accessor_loop(xs) end,50000,n},
 {'stride',function() return guard_stride(buf,n/4) end,50000,n},
 {'proven_stride',function() return guard_stride_proven(buf,n/4) end,100000,n},
 {'fixed_buffer',function() return guard_fixed_buffer() end,3000000,1}}
for _,w in ipairs(workloads) do
 if not arg[2] or arg[2] == w[1] then
 jit.flush()
 local aborts=0
 local function listener(what) if what=='abort' then aborts=aborts+1 end end
 jit.attach(listener,'trace')
 local sink=0
 for i=1,200 do sink=sink+w[2]() end
 local t=os.clock()
 for i=1,w[3] do sink=sink+w[2]() end
 local elapsed=os.clock()-t
 jit.attach(listener)
 print(w[1],string.format('%.4f',elapsed*1e9/(w[3]*w[4])),sink,aborts)
end
end
