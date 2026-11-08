//! Require native health from the pinned host; optionally check a stock runtime.
#[cfg(target_os = "linux")]
#[test]
fn loaded_luajit_reports_its_native_health_capability() {
    use std::process::Command;
    let released = gamelibs::host_dir().join("bin/luajit");
    let stone = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join("libstone_rs.so");
    let script = r#"
local ffi = require('ffi')
ffi.cdef[[int stone_lua_jit_health_sample(void); const char *stone_lua_jit_health_json(void); int stone_lua_alloc_split_available(void); int stone_lua_alloc_counter_begin(void); int stone_lua_jit_health_frame_delta(float *out, size_t len);]]
local path = assert(os.getenv('STONE_HEALTH_TEST_LIB'))
local stone = ffi.load(path)
assert(package.loadlib(path, 'luaopen_stone_lua_jit_health'))()
-- Health installation must leave the allocation counter uninstalled.
assert(stone.stone_lua_alloc_counter_begin() == 0)
assert(package.loadlib(path, 'luaopen_stone_lua_alloc_counter'))()
local available = tonumber(os.getenv('HEALTH_EXPECT_AVAILABLE'))
assert(stone.stone_lua_jit_health_sample() == available)
local json = ffi.string(stone.stone_lua_jit_health_json())
if available == 0 then
  assert(json == '{"available":false}', json)
  assert(stone.stone_lua_alloc_split_available() == 0)
  local buf0 = ffi.new("float[?]", 18)
  assert(stone.stone_lua_jit_health_frame_delta(buf0, 18) == 0)
  assert(buf0[0] == 0, 'unavailable delta export must zero-fill')
else
  assert(json:find('"available":true', 1, true), json)
  assert(json:find('a4c064539ea931274d407e1249b4bb575424a0f4+rock-health-1', 1, true), json)
  assert(stone.stone_lua_alloc_split_available() == 1)
  local x=0; for i=1,5000 do x=x+i end
  assert(stone.stone_lua_jit_health_sample() == 1)
  local after = ffi.string(stone.stone_lua_jit_health_json())
  assert(after ~= json, 'executed workload must change the native snapshot')
  local n = 18
  local buf = ffi.new("float[?]", n)
  assert(stone.stone_lua_jit_health_frame_delta(buf, n) == 1)
  for i=0,n-1 do assert(buf[i] == buf[i], 'delta column must be finite') end
  assert(buf[4] >= 0, 'abort total must not go negative')
  for k=0,2 do local r = buf[5+k*2]
    assert(r == -1 or (r >= 0 and r < 64 and r == math.floor(r)), 'reason must be -1 or a traceerr index')
  end
  local x2=0; for i=1,20000 do x2=x2+i end
  assert(stone.stone_lua_jit_health_sample() == 1)
  assert(stone.stone_lua_jit_health_frame_delta(buf, n) == 1, 'consecutive calls must delimit consecutive spans')
  assert(stone.stone_lua_jit_health_frame_delta(nil, 0) == 0, 'null buffer must not write')
  assert(stone.stone_lua_jit_health_frame_delta(buf, 17) == 0, 'short buffer must not write')
end
print(json)
"#;
    let mut runtimes = vec![(released, "1")];
    if let Some(stock) = std::env::var_os("STONE_TEST_STOCK_LUAJIT") {
        runtimes.push((stock.into(), "0"));
    }
    for (runtime, expected) in runtimes {
        let output = Command::new(&runtime)
            .args(["-e", script])
            .env("STONE_HEALTH_TEST_LIB", &stone)
            .env("HEALTH_EXPECT_AVAILABLE", expected)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}: {}\n{}",
            runtime.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
