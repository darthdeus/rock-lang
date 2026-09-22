//! Direct runtime evidence for the uint surfaces promised by the unified
//! standard library and skull's reflection/serialization boundary.

use crate::common::{run_luajit, try_emit_with_package_module};

const SERIALIZATION: &str = include_str!("../../../stone/engine/serialization.rock");
const SERIALIZATION_STUBS: &str = r#"
@nomangle inline(lua) fn stone_file_write(path: string, data: string) -> bool = "return true"
@nomangle inline(lua) fn stone_file_append(path: string, data: string) -> bool = "return true"
@nomangle inline(lua) fn stone_remove_file(path: string) -> bool = "return true"
@nomangle inline(lua) fn stone_remove_dir(path: string) -> bool = "return true"
@nomangle inline(lua) fn stone_file_sync(path: string) -> bool = "return true"
@nomangle inline(lua) fn stone_file_read(path: string) -> string? = "return null"
@nomangle inline(lua) fn stone_file_exists(path: string) -> bool = "return false"
@nomangle inline(lua) fn stone_create_directory(path: string) -> bool = "return true"
@nomangle inline(lua) fn stone_rename(src: string, dst: string) -> bool = "return true"
@nomangle inline(lua) fn __stone_host_projection_class(type_name: any) -> string = "return ''"
@nomangle inline(lua) fn __stone_host_projection_render(class: string, value: any) -> string = "return 'null'"
fn sim_mode_active() -> bool { return false }
global SIM_FIXED_EPOCH: int = 0
global time: float = 0
fn replay_is_record() -> bool { return false }
fn replay_is_replay() -> bool { return false }
fn __replay_preboot_mode() -> int { return 0 }
@nomangle inline(lua) fn os_getenv(name: string) -> string? = "return nil"
"#;

const SURFACE: &str = r#"
value UintRecord { payload: uint }

fn identity<T>(value: T) -> T { return value }

@nomangle inline(lua) fn uint_width(buf: NativeBuffer<uint>) -> int =
    "return ffi.sizeof(buf.ptr)"

fn main() {
    let record = UintRecord { payload: (4294967295 as uint) }
    let values: [uint] = [(4294967295 as uint)]
    let map: Map<uint, uint> = map_new()
    map[(4294967295 as uint)] = (2147483648 as uint)
    let maybe: uint? = (4294967295 as uint)
    let buffer = native_buffer_uint(1)
    buffer[0] = (4294967295 as uint)
    print(to_string(record.payload))
    print(inspect(record))
    print(to_json_value(record))
    print(identity(record).payload)
    print(values[0])
    print(map[(4294967295 as uint)])
    print(maybe)
    print(buffer[0])
    print(uint_width(buffer))
}
"#;

#[test]
fn uint_format_reflect_serialize_and_aggregate_surfaces() {
    let module = format!("{SERIALIZATION_STUBS}\n{SERIALIZATION}");
    let lua = try_emit_with_package_module("stone", &module, SURFACE).expect("uint surface emits");
    let descriptor = "__ROCK_TYPE_TABLE[\"UintRecord\"]";
    assert!(lua.contains(descriptor), "missing reflection descriptor: {lua}");
    assert!(lua.contains("type = \"uint\""), "uint field was not reflected as uint: {lua}");
    let lines = run_luajit(&format!("{lua}\nmain()\n"), "uint_surface");
    assert_eq!(
        lines.lines().collect::<Vec<_>>(),
        [
            "4294967295",
            "UintRecord { payload: 4294967295 }",
            r#"{"__ty":"UintRecord","payload":4294967295}"#,
            "4294967295",
            "4294967295",
            "2147483648",
            "4294967295",
            "4294967295",
            "4",
        ]
    );
}
