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
