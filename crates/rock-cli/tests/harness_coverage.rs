//! `autotests = false` means a new `tests/*.rs` is not a test target unless it
//! is declared in that crate's harness root -- it would compile, run nothing,
//! and read as green. This pins every merged crate's harness to its directory.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// (crate dir, harness roots that together must declare every test file)
const MERGED: &[(&str, &[&str])] = &[
    ("skull", &["tests/main.rs", "tests/main_wgpu.rs"]),
    ("skull-codegen-js", &["tests/main.rs"]),
    ("bone-query", &["tests/main.rs"]),
    ("rock-cli", &[
        "tests/main.rs",
        "tests/main_heavy.rs",
        "tests/main_js.rs",
        "tests/main_wgpu.rs",
    ]),
];

const JS_MODULES: &[&str] = &[
    "js_backend_externs_gate",
    "js_backend_ratchet",
    "js_coroutines",
    "js_test_harness_globals_sentinel",
    "comptime_build_vars",
    "mag_build_vars_web",
    "asset_overlay_defaults_web",
    "cimgui_prelude_js",
    "test_exit_status_gate_js",
    "js_run_gate",
    "js_value_tests",
    "shader_register_arity_gate",
    "imgui_web_stubs",
    "mono_js_backend_realization",
    "web_build_gate",
    "web_driver_tests",
    "web_fake_gl_tests",
    "web_package_tests",
    "web_watch_tests",
    "web_smoke_tests",
    "web_test_process",
];

const WGPU_MODULES: &[&str] = &[
    "engine_cdylib_freshness_gate",
    "frame_watchdog_gate",
    "macos_wgpu_lifecycle_gate",
    "macos_wgpu_recovery_gate",
    "bounded_summary_gate",
    "wgpu_refusal_gate",
    "wgpu_capture_gate",
    "wgpu_capture_contract_gate",
    "wgpu_gameplay_origin_gate",
    "wgpu_menu_parity_gate",
];

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/<name> has a parent")
        .to_path_buf()
}

#[test]
fn every_test_file_is_declared_in_a_harness() {
    let mut undeclared = Vec::new();
    for (krate, roots) in MERGED {
        let tests = crates_dir().join(krate).join("tests");
        let declared: HashSet<String> = roots
            .iter()
            .flat_map(|r| {
                let src = std::fs::read_to_string(crates_dir().join(krate).join(r))
                    .unwrap_or_else(|e| panic!("{krate}/{r}: {e}"));
                src.lines()
                    .filter_map(|l| {
                        let l = l.trim();
                        let rest = l.strip_prefix("#[path = \"")?;
                        Some(rest.strip_suffix("\"]")?.to_string())
                    })
                    .collect::<Vec<_>>()
            })
            .collect();

        for entry in std::fs::read_dir(&tests).expect("tests dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            // Harness roots declare the others; they are not declared themselves.
            if roots.iter().any(|r| r.ends_with(&name)) {
                continue;
            }
            if !declared.contains(&name) {
                undeclared.push(format!("{krate}/tests/{name}"));
            }
        }
    }
    undeclared.sort();
    assert!(
        undeclared.is_empty(),
        "test files not declared in any harness root (they would never run):\n  {}",
        undeclared.join("\n  ")
    );
}

#[test]
fn optional_backend_harnesses_are_separate_from_desktop_harnesses() {
    let tests = crates_dir().join("rock-cli/tests");
    let js = std::fs::read_to_string(tests.join("main_js.rs")).expect("JS harness");
    let wgpu = std::fs::read_to_string(tests.join("main_wgpu.rs")).expect("WGPU harness");
    for module in JS_MODULES {
        let declaration = format!("#[path = \"{module}.rs\"]");
        assert!(js.contains(&declaration), "JS module {module} is not in main_js.rs");
        assert!(!wgpu.contains(&declaration), "JS module {module} is in main_wgpu.rs");
    }
    for module in WGPU_MODULES {
        let declaration = format!("#[path = \"{module}.rs\"]");
        assert!(wgpu.contains(&declaration), "WGPU module {module} is not in main_wgpu.rs");
        assert!(!js.contains(&declaration), "WGPU module {module} is in main_js.rs");
    }
    for root in ["main.rs", "main_heavy.rs"] {
        let source = std::fs::read_to_string(tests.join(root)).expect("desktop harness");
        assert!(!source.contains("main_js.rs"), "{root} reaches the JS harness");
        assert!(!source.contains("main_wgpu.rs"), "{root} reaches the WGPU harness");
        assert!(!source.contains("web_smoke_tests.rs"), "{root} owns web smoke tests");
        assert!(!source.contains("wgpu_"), "{root} owns a WGPU test module");
        for module in JS_MODULES.iter().chain(WGPU_MODULES.iter()) {
            let declaration = format!("#[path = \"{module}.rs\"]");
            assert!(!source.contains(&declaration), "{root} reaches optional module {module}");
        }
    }

    for root in ["main.rs", "main_heavy.rs"] {
        let source = std::fs::read_to_string(tests.join(root)).expect("desktop harness");
        for line in source.lines() {
            let Some(path) = line.trim().strip_prefix("#[path = \"").and_then(|line| line.strip_suffix("\"]")) else {
                continue;
            };
            if matches!(path, "backend_feature_gates.rs" | "harness_coverage.rs") {
                continue;
            }
            let module_source = std::fs::read_to_string(tests.join(path)).unwrap();
            if path == "shape_oracle.rs" {
                continue;
            }
            for marker in [
                "\"--backend\", \"js\"",
                ".args([\"web\"",
                "Surface::Js",
                "\"--renderer\", \"wgpu\"",
            ] {
                assert!(!module_source.contains(marker), "{root} module {path} owns optional marker {marker}");
            }
        }
    }
}

#[test]
fn shape_oracle_has_one_canonical_owner() {
    let tests = crates_dir().join("rock-cli/tests");
    for root in ["main.rs", "main_js.rs"] {
        let source = std::fs::read_to_string(tests.join(root)).expect("harness");
        assert!(
            source.contains("#[path = \"shape_oracle.rs\"]\nmod shape_oracle;"),
            "{root} must reach the canonical shape_oracle.rs"
        );
        assert!(
            !source.contains("shape_oracle_desktop.rs"),
            "{root} must not reach a second shape-oracle owner"
        );
    }
}
