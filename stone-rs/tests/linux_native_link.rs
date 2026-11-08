#[cfg(target_os = "linux")]
#[test]
fn native_host_links_stone_without_a_lua_runtime() {
    let loader: unsafe extern "C" fn(*mut stone_rs::LuaState) -> std::ffi::c_int =
        stone_rs::luaopen_stone_lua_alloc_counter;

    assert_eq!(unsafe { loader(std::ptr::null_mut()) }, 0);
}

#[cfg(target_os = "linux")]
#[test]
fn subprocess_liveness_has_a_canonical_byte_abi() {
    use std::process::Command;

    let luajit = gamelibs::host_dir().join("bin/luajit");
    let stone = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join("libstone_rs.so");
    let script = r#"
local ffi = require("ffi")
ffi.cdef[[uint8_t stone_proc_alive(int handle);]]
local stone = ffi.load(assert(os.getenv("STONE_COUNTER_TEST_LIB")))
for handle = 0, 8 do
    assert(stone.stone_proc_alive(handle) == 0, "an empty subprocess slot must return byte zero")
end
"#;
    let output = Command::new(luajit)
        .args(["-e", script])
        .env("STONE_COUNTER_TEST_LIB", stone)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "subprocess byte ABI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn bundled_luajit_enters_the_stone_allocator_callback() {
    use std::process::Command;

    let luajit = gamelibs::host_dir().join("bin/luajit");
    let stone = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join("libstone_rs.so");
    assert!(
        luajit.is_file(),
        "bundled LuaJIT is missing: {}",
        luajit.display()
    );
    assert!(
        stone.is_file(),
        "fresh Stone cdylib is missing: {}",
        stone.display()
    );

    let script = r#"
local ffi = require("ffi")
ffi.cdef[[
    int stone_lua_alloc_counter_begin(void);
    int stone_lua_alloc_counter_end(void);
    double stone_lua_alloc_gross_request_kb(void);
    uint64_t stone_lua_alloc_call_count(void);
]]
local path = assert(os.getenv("STONE_COUNTER_TEST_LIB"))
local stone = ffi.load(path)
local loader = assert(package.loadlib(path, "luaopen_stone_lua_alloc_counter"))
loader()
assert(stone.stone_lua_alloc_counter_begin() == 1)
local values = {}
for i = 1, 2048 do
    values[i] = tostring(i) .. ":" .. string.rep("x", 64)
end
local calls = tonumber(stone.stone_lua_alloc_call_count())
local gross_kb = tonumber(stone.stone_lua_alloc_gross_request_kb())
assert(stone.stone_lua_alloc_counter_end() == 1)
assert(#values == 2048)
assert(calls > 0, "forced allocations did not reach the callback")
assert(gross_kb > 0, "forced allocations recorded no request traffic")
print(string.format("allocator callback: %d calls, %.3f KiB", calls, gross_kb))
"#;
    let output = Command::new(&luajit)
        .args(["-e", script])
        .env("STONE_COUNTER_TEST_LIB", &stone)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "bundled LuaJIT allocator callback failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("allocator callback:"),
        "callback verdict is missing from stdout:\n{}",
        String::from_utf8_lossy(&output.stdout),
    );
}
