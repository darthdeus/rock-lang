use std::process::Command;

fn run(body: &str) {
    let lua = format!(
        "local ffi = require('ffi')\n{}\n{body}",
        skull_codegen_lua::NB_ALLOC_PRELUDE
    );
    let output = Command::new(rock_host::project::luajit_path())
        .args(["-e", &lua])
        .output()
        .expect("run NativeBuffer prelude under LuaJIT");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn small_allocations_record_without_closing_finalizer_upvalues() {
    run(r#"
assert(jit.status())
local vm = require('jit.vmdef')
local aborts, stops = 0, 0
local function trace(what, tr, fn, pc, err)
  if what == 'abort' and vm.traceerr[err]:find('NYI: bytecode', 1, true) then
    aborts = aborts + 1
  elseif what == 'stop' then
    stops = stops + 1
  end
end
__rock_nb_alloc('uint8_t', 8)
jit.attach(trace, 'trace')
local sum = 0
for i = 1, 2000 do
  local buffer = __rock_nb_alloc('uint8_t', 8)
  buffer[0] = i % 251
  sum = sum + buffer[0]
end
jit.attach(trace)
assert(sum == 249271, sum)
assert(aborts == 0, 'small allocation NYI aborts: ' .. aborts)
assert(stops > 0, 'small allocation loop did not record')
"#);
}

#[test]
fn finalizers_keep_each_allocation_size_and_retained_buffers_alive() {
    run(r#"
collectgarbage('collect')
local stats = __ROCK_NB_OFFHEAP_STATS
local sizes = {16384, 32768, 65536}
local buffers = {}
for i, size in ipairs(sizes) do
  buffers[i] = __rock_nb_alloc('uint8_t', size)
  assert(buffers[i][size - 1] == 0)
  buffers[i][size - 1] = i
end
assert(stats.live_count == 3 and stats.live_bytes == 114688)
collectgarbage('collect')
for i, size in ipairs(sizes) do assert(buffers[i][size - 1] == i) end
buffers[2] = nil
collectgarbage('collect')
collectgarbage('collect')
assert(stats.live_count == 2 and stats.live_bytes == 81920)
buffers = nil
collectgarbage('collect')
collectgarbage('collect')
assert(stats.live_count == 0 and stats.live_bytes == 0)
"#);
}

#[test]
fn finalizer_allocations_keep_their_debt_until_their_collection_finishes() {
    run(r#"
local hard, chunk = 4 * 1024 * 1024, 65536
-- Isolate the hard backstop from the timed soft steps.
__ROCK_NB_OFFHEAP_CYCLE_MAX = hard
__ROCK_NB_OFFHEAP_CYCLE_HARD = hard
collectgarbage('collect')
collectgarbage('collect')
local stats = __ROCK_NB_OFFHEAP_STATS
local peak, callbacks = 0, 0
local function churn(n)
  for i = 1, n do
    local p = __rock_nb_alloc('uint8_t', chunk)
    p[0] = i
    peak = math.max(peak, stats.live_bytes)
  end
end
local older = newproxy(true)
getmetatable(older).__gc = function()
  callbacks = callbacks + 1
  churn(64)
end
do
  local newer = newproxy(true)
  getmetatable(newer).__gc = function()
    callbacks = callbacks + 1
    churn(48)
  end
end
-- These allocations survive the first collection. The successor sentinel
-- can run before both their finalizers and the older allocating finalizer.
collectgarbage('collect')
older = nil
collectgarbage('collect')
assert(callbacks == 2, callbacks)
assert(peak <= hard + 2 * chunk, 'peak ' .. peak)

for i = 1, 4 do collectgarbage('collect') end
-- Make the next collection come from the hard backstop, not automatic GC.
collectgarbage('stop')
local keep = {}
for i = 1, 64 do keep[i] = __rock_nb_alloc('uint8_t', chunk) end
peak = 0
local trigger = newproxy(true)
getmetatable(trigger).__gc = function()
  callbacks = callbacks + 1
  churn(48)
end
keep = nil
trigger = nil
-- Crossing the hard limit must not credit the old buffers before collection:
-- an allocating finalizer can run before their frees and need the backstop.
local incoming = __rock_nb_alloc('uint8_t', chunk)
assert(callbacks == 3, callbacks)
assert(peak <= hard + 2 * chunk, 'full collection peak ' .. peak)
assert(incoming[0] == 0)
collectgarbage('restart')
"#);
}

#[test]
fn repeated_large_allocations_record_without_creating_finalizer_closures() {
    run(r#"
assert(jit.status())
local vm = require('jit.vmdef')
local aborts, stops = 0, 0
local function trace(what, tr, fn, pc, err)
  if what == 'abort' and vm.traceerr[err]:find('NYI: bytecode', 1, true) then
    aborts = aborts + 1
  elseif what == 'stop' then
    stops = stops + 1
  end
end
local buffers = {__rock_nb_alloc('uint8_t', 16384)}
jit.attach(trace, 'trace')
for i = 2, 1000 do
  local buffer = __rock_nb_alloc('uint8_t', 16384)
  buffer[16383] = i % 251
  buffers[i] = buffer
end
jit.attach(trace)
for i = 2, 1000 do assert(buffers[i][16383] == i % 251) end
assert(aborts == 0, 'large allocation NYI aborts: ' .. aborts)
assert(stops > 0, 'large allocation loop did not record')
assert(__ROCK_NB_OFFHEAP_STATS.live_bytes == 16384000)
buffers = nil
collectgarbage('collect')
collectgarbage('collect')
assert(__ROCK_NB_OFFHEAP_STATS.live_bytes == 0)
assert(__ROCK_NB_OFFHEAP_STATS.live_count == 0)
"#);
}

#[test]
fn finalizer_reuse_does_not_keep_buffers_or_unused_callbacks_alive() {
    run(r#"
local callbacks = setmetatable({}, {__mode = 'v'})
local original_gc, registrations = ffi.gc, 0
ffi.gc = function(pointer, callback)
  registrations = registrations + 1
  callbacks[registrations] = callback
  return original_gc(pointer, callback)
end
local buffers = {}
buffers[1] = __rock_nb_alloc('uint8_t', 16384)
buffers[2] = __rock_nb_alloc('uint8_t', 16384)
buffers[3] = __rock_nb_alloc('int32_t', 4096)
ffi.gc = original_gc
buffers[1] = nil
collectgarbage('collect')
collectgarbage('collect')
assert(__ROCK_NB_OFFHEAP_STATS.live_bytes == 32768)
assert(callbacks[2] ~= nil and callbacks[3] ~= nil)
buffers[2][16383] = 17
assert(buffers[2][16383] == 17)
buffers[3][4095] = 123456
assert(buffers[3][4095] == 123456)
buffers = nil
for i = 1, 4 do collectgarbage('collect') end
assert(__ROCK_NB_OFFHEAP_STATS.live_bytes == 0)
assert(__ROCK_NB_OFFHEAP_STATS.live_count == 0)
assert(next(callbacks) == nil, 'unused finalizers must be collectible')
"#);
}

#[test]
fn storage_transition_records_with_fifteen_live_float_values() {
    run(r#"
assert(jit.status())
local primed = {}
for i = 0, 127 do primed[i] = __rock_nb_alloc('double', 32 + i * 32) end
local function build(n, a0, a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11, a12, a13, a14)
  local b = __rock_nb_alloc('double', n)
  b[0], b[1], b[2], b[3], b[4] = a0, a1, a2, a3, a4
  b[5], b[6], b[7], b[8], b[9] = a5, a6, a7, a8, a9
  b[10], b[11], b[12], b[13], b[14] = a10, a11, a12, a13, a14
  return b
end
local coalescing, stops = 0, 0
local vm = require('jit.vmdef')
local function trace(what, tr, fn, pc, err)
  if what == 'abort' and vm.traceerr[err] == 'NYI: register coalescing too complex' then
    coalescing = coalescing + 1
  elseif what == 'stop' then
    stops = stops + 1
  end
end
jit.attach(trace, 'trace')
local keep = {}
for i = 1, 3000 do
  local n = 32 + (i % 128) * 32
  local b = build(n, i * 0.15625, i * 0.28125, i * 0.40625, i * 0.53125,
    i * 0.65625, i * 0.78125, i * 0.90625, i * 1.03125, i * 1.15625,
    i * 1.28125, i * 1.40625, i * 1.53125, i * 1.65625, i * 1.78125, i * 1.90625)
  assert(b[0] == i * 0.15625)
  keep[i % 8] = b
end
jit.attach(trace)
for slot = 0, 7 do
  local i = 3000 - (3000 - slot) % 8
  for j = 0, 14 do assert(keep[slot][j] == i * ((j + 1) * 0.125 + 0.03125)) end
end
assert(coalescing == 0, 'storage transition coalescing aborts: ' .. coalescing)
assert(stops > 0, 'storage transition did not record')
"#);
}
