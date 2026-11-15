dofile(arg[1])
local mode, metric = arg[2], arg[3]
assert(mode == 'local' or mode == 'direct')
assert(metric == 'time' or metric == 'heap' or metric == 'trace')
assert(jit.status(), 'JIT must be enabled')
local select_row = mode == 'local' and select_local or select_direct
local left, right = make(10, 101), make(30, 202)
local function check(row, seed, sequence)
    assert(row.sequence == sequence and row.landing ~= nil)
    assert(row.landing.environment == nil)
    for _, name in ipairs({'a', 'b', 'c', 'd', 'e', 'f', 'g'}) do
        for i = 0, 6 do
            assert(row.landing[name .. '.f' .. i] == seed + i)
        end
    end
end
-- Check both branches, independent input values and value snapshot semantics.
for _, flag in ipairs({true, false}) do
    local result = select_row(flag, left, right)
    local edited = changed(flag and left or right)
    check(result, flag and 10 or 30, flag and 101 or 202)
    check(edited, flag and 10 or 30, 999)
    check(left, 10, 101)
    check(right, 30, 202)
end
local starts, stops, aborts = 0, 0, 0
if metric == 'trace' then
    jit.attach(function(event)
        if event == 'start' then starts = starts + 1
        elseif event == 'stop' then stops = stops + 1
        elseif event == 'abort' then aborts = aborts + 1 end
    end, 'trace')
end
local ring = {}
local function run(n)
    for i = 1, n do
        ring[(i - 1) % 256 + 1] = select_row(i % 2 == 0, left, right)
    end
end
run(50000)
collectgarbage('collect')
local n = metric == 'heap' and 5000 or 500000
if metric == 'heap' then collectgarbage('stop') end
local before, start = collectgarbage('count'), os.clock()
run(n)
local elapsed = os.clock() - start
local growth = (collectgarbage('count') - before) * 1024
if metric == 'heap' then collectgarbage('restart') end
local sink = 0
for i = 1, 256 do
    local flag = (n - ((n - i) % 256)) % 2 == 0
    check(ring[i], flag and 10 or 30, flag and 101 or 202)
    sink = sink + ring[i].sequence
end
assert(jit.status())
print(string.format('%s %s %.6f %.6f %d %d %d %d', mode, metric,
    elapsed * 1e9 / n, growth / n, sink, starts, stops, aborts))
