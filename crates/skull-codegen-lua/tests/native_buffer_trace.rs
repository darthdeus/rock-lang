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
