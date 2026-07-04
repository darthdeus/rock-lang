// Tests for the shared `_G.__rock_serialize_value` walker installed by
// `__rock_install_serialize_value` in `stone/engine/serialization.rock`.
// Both `to_json_value` and `dump_globals_json` route through it; these
// tests pin the branches that the existing snapshot suite only exercises
// implicitly.
//
// Coverage targeted:
//   - cdata value-struct branch (`ffi.istype` + `__ROCK_TYPE_TABLE`)
//   - multiple distinct value-struct types in the same dump (IVec2 vs Vec2)
//   - empty `Vec<T>` envelope (regression: empty `{}` -> "len on object")
//   - non-empty `Vec` metatable branch with content
//   - boolean type preservation
//   - integer-valued field round-trips without precision loss
//   - max-depth guard not tripping on normal game states
//
// All assertions run against a single `rock frame match3 --dump-state`
// invocation. match3 has IVec2 globals (anim_swap_a etc.) and stone
// exports Vec2 pivot constants, covering both cdata types without needing
// the heavier mag game.

use std::path::Path;
use std::process::Command;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
}

fn dump_match3_state() -> serde_json::Value {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let output = Command::new(rock_bin)
        .args(["frame", "games/match3", "-n", "1", "--dump-state"])
        .env("ROCK_TEST_SEED", "42")
        .current_dir(workspace_root())
        .output()
        .expect("failed to spawn rock frame");
    assert!(
        output.status.success(),
        "rock frame match3 exited with {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout)
        .expect("dump-state stdout was not valid JSON")
}

#[test]
fn serializer_branch_coverage() {
    let state = dump_match3_state();

    // ── cdata value-struct: IVec2 ────────────────────────────────────────
    // match3 keeps anim_swap_a as a global IVec2. The cdata branch must
    // find the type via ffi.istype + __ROCK_TYPE_TABLE and emit
    // {"__ty":"IVec2","x":<int>,"y":<int>}.
    let swap = &state["match3"]["anim_swap_a"];
    assert!(swap.is_object(), "anim_swap_a should be a JSON object, got: {swap}");
    assert_eq!(swap["__ty"], "IVec2", "anim_swap_a.__ty must be IVec2; got: {swap}");
    assert!(
        swap["x"].is_i64() || swap["x"].is_u64(),
        "IVec2.x must be integer, got: {}",
        swap["x"]
    );
    assert!(
        swap["y"].is_i64() || swap["y"].is_u64(),
        "IVec2.y must be integer, got: {}",
        swap["y"]
    );
    let extra: Vec<&String> = swap
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| !["__ty", "x", "y"].contains(&k.as_str()))
        .collect();
    assert!(extra.is_empty(), "IVec2 has unexpected fields: {extra:?}");

    // ── cdata value-struct: two distinct types in one dump ───────────────
    // stone exports Vec2 pivot constants alongside match3's IVec2 globals.
    // The cdata branch iterates __ROCK_C_STRUCT with ffi.istype; if two
    // types alias the same C layout the wrong name could win.
    let pivot = &state["stone"]["PIVOT_CENTER"];
    assert_eq!(pivot["__ty"], "Vec2", "PIVOT_CENTER.__ty must be Vec2; got: {pivot}");
    assert_ne!(
        swap["__ty"], pivot["__ty"],
        "IVec2 and Vec2 must have distinct __ty values"
    );

    // ── empty Vec envelope ───────────────────────────────────────────────
    // Without the metatable check, an empty Vec serializes as `{}` and
    // downstream .data.len accesses hit "len on object".
    let include_stack = &state["stone"]["SHADER_PREPROCESSOR"]["include_stack"];
    assert!(
        include_stack.is_object(),
        "empty Vec must be a struct-envelope object, got: {include_stack}"
    );
    assert_eq!(include_stack["__ty"], "Vec", "empty Vec missing __ty=\"Vec\": {include_stack}");
    assert_eq!(include_stack["len"], 0, "empty Vec must have len=0: {include_stack}");
    assert!(
        include_stack["data"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "empty Vec.data must be [], got: {}",
        include_stack["data"]
    );

    // ── non-empty Vec envelope ───────────────────────────────────────────
    // Every Vec envelope in the dump must have len == data.len().
    fn check_vecs(v: &serde_json::Value, path: &str, count: &mut usize) {
        match v {
            serde_json::Value::Object(m) => {
                if m.get("__ty").and_then(|t| t.as_str()) == Some("Vec") {
                    let len = m["len"]
                        .as_u64()
                        .unwrap_or_else(|| panic!("Vec at {path} missing numeric `len`: {v}"));
                    let data_len = m["data"]
                        .as_array()
                        .unwrap_or_else(|| panic!("Vec at {path} has non-array `data`: {v}"))
                        .len() as u64;
                    assert_eq!(
                        len, data_len,
                        "Vec at {path}: len ({len}) != data.len ({data_len})"
                    );
                    *count += 1;
                }
                for (k, val) in m {
                    check_vecs(val, &format!("{path}.{k}"), count);
                }
            }
            serde_json::Value::Array(arr) => {
                for (i, val) in arr.iter().enumerate() {
                    check_vecs(val, &format!("{path}[{i}]"), count);
                }
            }
            _ => {}
        }
    }
    let mut count = 0;
    check_vecs(&state, "root", &mut count);
    assert!(count > 0, "expected at least one Vec envelope in the dump");

    // ── boolean type preservation ────────────────────────────────────────
    // Lua tostring(true) == "true" — booleans must stay JSON booleans.
    let auto_play = &state["match3"]["auto_play"];
    assert!(
        auto_play.is_boolean(),
        "auto_play must serialize as a JSON boolean, got: {auto_play}"
    );

    // ── integer field round-trip ─────────────────────────────────────────
    // IVec2.x/y are integer cdata fields; pin that they parse as integers.
    for field in ["anim_swap_a", "anim_swap_b", "queued_sel", "queued_swap_b"] {
        let v = &state["match3"][field];
        assert!(
            v["x"].is_i64() || v["x"].is_u64(),
            "{field}.x must be a JSON integer, got: {}",
            v["x"]
        );
        assert!(
            v["y"].is_i64() || v["y"].is_u64(),
            "{field}.y must be a JSON integer, got: {}",
            v["y"]
        );
    }

    // ── max-depth guard ──────────────────────────────────────────────────
    let s = state.to_string();
    assert!(
        !s.contains("<max_depth>"),
        "dump contains <max_depth> sentinel — recursion exceeded MAX_DEPTH"
    );
    assert!(
        !s.contains("<circular>"),
        "dump contains <circular> sentinel — unexpected cycle in globals"
    );
}
