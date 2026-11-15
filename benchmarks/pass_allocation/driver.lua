-- Timing uses ordinary GC. A separate short run measures uncollected heap growth.
dofile(arg[1])
local mode, inputs = arg[2], assert(tonumber(arg[3]))
local metric, changes = arg[4], arg[5]
assert(mode == 'fresh' or mode == 'retained')
assert(metric == 'time' or metric == 'heap')
assert(changes == 'stable' or changes == 'changing' or changes == 'every_frame')
assert(jit.status(), 'JIT must be enabled')
local submit = mode == 'fresh' and pass_fresh or pass_retained
local fixture = pass_fixture(inputs)
local function frame(i)
    local period = changes == 'every_frame' and 1 or 60
    local texture = changes ~= 'stable' and (math.floor((i - 1) / period) % 2 + 1) or 1
    submit(fixture, inputs, texture)
    local sink = pass_consume(fixture)
    local expected = 12 * (100 + inputs * texture + inputs * (inputs - 1) / 2)
    assert(sink == expected, 'queued pass contents differ')
    return sink
end
for i = 1, 12000 do frame(i) end
collectgarbage('collect')
local reps = metric == 'time' and 120000 or 600
if metric == 'heap' then collectgarbage('stop') end
local before = collectgarbage('count')
local start = os.clock()
local sink = 0
for i = 1, reps do sink = sink + frame(i) end
local elapsed = os.clock() - start
local growth = (collectgarbage('count') - before) * 1024
if metric == 'heap' then collectgarbage('restart') end
assert(jit.status())
local value = metric == 'time' and elapsed * 1e9 / (12 * reps) or growth / (12 * reps)
print(string.format('%s %d %s %s %.6f %.0f', mode, inputs,
    metric, changes, value, sink))
