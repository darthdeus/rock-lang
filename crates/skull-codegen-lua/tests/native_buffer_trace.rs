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
