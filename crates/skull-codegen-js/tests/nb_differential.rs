//! The JS NativeBuffer checked-store shim (`__nb_checked_set`), run under
//! node. Gated on `node` being present -- absent, skip WITH A LOG.

use bone_semantic::inference::Workspace;
use skull_codegen_js::JS_NB_CHECK_SHIMS;
use skull_mir::mir::FuncRef;

const FIXTURE: &str = include_str!("fixtures/nb_differential.rock");
const FIXTURE_MOD: &str = "<nb_differential>";

#[derive(Clone, Copy, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Null,
}

fn parse_tok(s: &str) -> Tok {
    match s {
        "nil" | "null" => Tok::Null,
        _ => Tok::Num(
            s.parse::<f64>()
                .unwrap_or_else(|_| panic!("unparseable result token: {s:?}")),
        ),
    }
}

fn toks_agree(a: Tok, b: Tok) -> bool {
    match (a, b) {
        (Tok::Null, Tok::Null) => true,
        (Tok::Num(x), Tok::Num(y)) => (x - y).abs() < 1e-9,
        _ => false,
    }
}

const F32_OF_0_1: f64 = 0.10000000149011612;
const VALUE_CASES: &[(&str, Tok)] = &[
    ("test_nb_len_float", Tok::Num(3.0)),
    ("test_nb_len_int", Tok::Num(5.0)),
    ("test_nb_len_u8", Tok::Num(7.0)),
    ("test_nb_len_i8", Tok::Num(2.0)),
    ("test_nb_len_u16", Tok::Num(4.0)),
    ("test_nb_len_i16", Tok::Num(6.0)),
    ("test_nb_len_u32", Tok::Num(8.0)),
    ("test_nb_sugar_float", Tok::Num(4.0)),
    ("test_nb_sugar_int", Tok::Num(42.0)),
    ("test_nb_f32_rounding", Tok::Num(F32_OF_0_1)),
    ("test_nb_direct_float", Tok::Num(2.5)),
    ("test_nb_direct_int", Tok::Num(42.0)),
    ("test_nb_direct_u8", Tok::Num(44.0)),
    ("test_nb_direct_i8", Tok::Num(-127.0)),
    ("test_nb_direct_u16", Tok::Num(60000.0)),
    ("test_nb_direct_i16", Tok::Num(-300.0)),
    ("test_nb_direct_u32", Tok::Num(4000000000.0)),
    ("test_nb_get_hit_float", Tok::Num(4.5)),
    ("test_nb_get_hit_index0", Tok::Num(3.25)),
    ("test_nb_get_miss_low_float", Tok::Null),
    ("test_nb_get_miss_high_float", Tok::Null),
    ("test_nb_get_hit_int", Tok::Num(77.0)),
    ("test_nb_get_miss_int", Tok::Null),
    ("test_nb_get_hit_u8", Tok::Num(12.0)),
    ("test_nb_get_miss_u8", Tok::Null),
    ("test_nb_get_hit_i8", Tok::Num(13.0)),
    ("test_nb_get_miss_i8", Tok::Null),
    ("test_nb_get_hit_u16", Tok::Num(14.0)),
    ("test_nb_get_miss_u16", Tok::Null),
    ("test_nb_get_hit_i16", Tok::Num(15.0)),
    ("test_nb_get_miss_i16", Tok::Null),
    ("test_nb_get_hit_u32", Tok::Num(16.0)),
    ("test_nb_get_miss_u32", Tok::Null),
    ("test_nb_wrap_u8_256", Tok::Num(0.0)),
    ("test_nb_wrap_u8_300", Tok::Num(44.0)),
    ("test_nb_wrap_u8_neg1", Tok::Num(255.0)),
    ("test_nb_wrap_i8_128", Tok::Num(-128.0)),
    ("test_nb_wrap_i8_neg129", Tok::Num(127.0)),
    ("test_nb_wrap_u16_65536", Tok::Num(0.0)),
    ("test_nb_wrap_u16_neg1", Tok::Num(65535.0)),
    ("test_nb_wrap_i16_32768", Tok::Num(-32768.0)),
    ("test_nb_wrap_u32_2p32", Tok::Num(0.0)),
    ("test_nb_wrap_u32_2p32p5", Tok::Num(5.0)),
    ("test_nb_wrap_u32_neg1", Tok::Num(4294967295.0)),
    ("test_nb_wrap_int_2p31", Tok::Num(-2147483648.0)),
    ("test_nb_wrap_int_2p31p5", Tok::Num(-2147483643.0)),
    ("test_nb_wrap_int_neg", Tok::Num(2147483642.0)),
];

const ERROR_CASES: &[&str] = &[
    "test_nb_oob_read",
    "test_nb_oob_read_neg",
    "test_nb_oob_write",
];

fn tool_available(tool: &str, arg: &str) -> bool {
    crate::common::tool(tool)
        .arg(arg)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn both_backends_available(test: &str) -> bool {
    for (tool, arg) in [("node", "--version"), ("luajit", "-v")] {
        if !tool_available(tool, arg) {
            eprintln!("SKIP {test}: `{tool}` not on PATH");
            return false;
        }
    }
    true
}

fn fixture_ws() -> Workspace {
    let mut modules: Vec<skull_parser::Ast> = rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = ustr::Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect();
    modules.push(
        skull_parser::parse_module_clean(FIXTURE, ustr::Ustr::from(FIXTURE_MOD))
            .expect("fixture parse"),
    );
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

fn find_fn(ws: &Workspace, name: &str) -> FuncRef {
    let mut found = None;
    for &mid in ws.module_ids.values() {
        for (fid, f) in ws.module(mid).functions.iter() {
            if f.name.as_str() == name && f.inline_body.is_none() {
                assert!(found.is_none(), "ambiguous fn `{name}`");
                found = Some(FuncRef {
                    module: mid,
                    func: fid,
                });
            }
        }
    }
    found.unwrap_or_else(|| panic!("no fn `{name}` in workspace"))
}

fn emit_js_program(ws: &Workspace, names: &[&str]) -> String {
    let roots: Vec<(FuncRef, String)> = names
        .iter()
        .map(|&n| (find_fn(ws, n), n.to_string()))
        .collect();
    let mut out = String::new();
    out.push_str(JS_NB_CHECK_SHIMS);
    out.push('\n');
    out.push_str(
        &skull_codegen_js::emit::emit_with_closure(ws, &roots)
            .unwrap_or_else(|e| panic!("closure emit: {}", e.message)),
    );
    out
}

fn emit_lua_program(ws: &Workspace) -> String {
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from(FIXTURE_MOD))
        .expect("fixture module id");
    skull::mir_emit::emit_workspace_mir(ws, mid)
        .expect("lua emit")
        .lua
}

fn run_tool(tool: &str, path: &std::path::Path) -> std::process::Output {
    crate::common::tool(tool)
        .arg(path)
        .output()
        .unwrap_or_else(|e| panic!("spawn {tool}: {e}"))
}

fn write_temp(tag: &str, ext: &str, contents: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "skull_js_nbdiff_{}_{tag}.{ext}",
        std::process::id()
    ));
    std::fs::write(&path, contents).expect("write temp program");
    path
}

fn run_lines(tool: &str, tag: &str, ext: &str, prog: &str) -> Vec<String> {
    let path = write_temp(tag, ext, prog);
    let out = run_tool(tool, &path);
    let _ = std::fs::remove_file(&path);
    assert!(
        out.status.success(),
        "{tool} exited non-zero.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .lines()
        .map(|l| l.trim().to_string())
        .collect()
}

#[test]
fn nb_value_differential() {
    if !both_backends_available("nb_value_differential") {
        return;
    }
    let ws = fixture_ws();
    let names: Vec<&str> = VALUE_CASES.iter().map(|(n, _)| *n).collect();

    let mut lua = emit_lua_program(&ws);
    for &name in &names {
        lua.push_str(&format!("\nprint(tostring({name}()))"));
    }
    let lua_lines = run_lines("luajit", "values", "lua", &lua);

    let mut js = emit_js_program(&ws, &names);
    for &name in &names {
        js.push_str(&format!("\nconsole.log({name}());"));
    }
    let js_lines = run_lines("node", "values", "js", &js);

    assert_eq!(lua_lines.len(), VALUE_CASES.len(), "luajit line count");
    assert_eq!(js_lines.len(), VALUE_CASES.len(), "node line count");

    for (i, &(name, expected)) in VALUE_CASES.iter().enumerate() {
        let lua_tok = parse_tok(&lua_lines[i]);
        let js_tok = parse_tok(&js_lines[i]);
        assert!(
            toks_agree(lua_tok, expected),
            "{name}: luajit = {lua_tok:?}, expected {expected:?}"
        );
        assert!(
            toks_agree(js_tok, expected),
            "{name}: node = {js_tok:?}, expected {expected:?}"
        );
        assert!(
            toks_agree(js_tok, lua_tok),
            "{name}: node = {js_tok:?} disagrees with luajit = {lua_tok:?}"
        );
    }
}

#[test]
fn nb_checked_oob_errors_on_both_backends() {
    if !both_backends_available("nb_checked_oob_errors_on_both_backends") {
        return;
    }
    let ws = fixture_ws();
    let lua_base = emit_lua_program(&ws);

    for &name in ERROR_CASES {
        let lua = format!("{lua_base}\n{name}()");
        let path = write_temp(&format!("err_{name}"), "lua", &lua);
        let out = run_tool("luajit", &path);
        let _ = std::fs::remove_file(&path);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(!out.status.success(), "{name}: luajit did NOT abort");
        assert!(
            stderr.contains("out of bounds"),
            "{name}: luajit aborted without a bounds message:\n{stderr}"
        );

        let js = format!("{}\n{name}();", emit_js_program(&ws, &[name]));
        let path = write_temp(&format!("err_js_{name}"), "js", &js);
        let out = run_tool("node", &path);
        let _ = std::fs::remove_file(&path);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(!out.status.success(), "{name}: node did NOT abort");
        assert!(
            stderr.contains("out of bounds"),
            "{name}: node aborted without a bounds message:\n{stderr}"
        );
    }
}

// The pinned NaN store edge: NaN into an INTEGER buffer is a programmer error -- the JS checked-set
// shim rejects it (debug check; currently unconditional, the JS harness has no
// release mode). NaN into a FLOAT buffer stores normally. JS-side only: on
// luajit the store is backend-native, so there is nothing to compare.
#[test]
fn nb_js_checked_set_rejects_nan_into_int_buffer() {
    if !tool_available("node", "--version") {
        eprintln!("SKIP nb_js_checked_set_rejects_nan_into_int_buffer: `node` not on PATH");
        return;
    }
    let prog = format!(
        "{JS_NB_CHECK_SHIMS}\n\
         var ib = {{ ptr: new Int32Array(1), len: 1 }};\n\
         var thrown = false;\n\
         try {{ __nb_checked_set(ib, 0, NaN); }} catch (e) {{ thrown = String(e).indexOf('NaN') >= 0; }}\n\
         var fb = {{ ptr: new Float32Array(1), len: 1 }};\n\
         __nb_checked_set(fb, 0, NaN);\n\
         console.log(thrown && (fb.ptr[0] !== fb.ptr[0]));"
    );
    let path = write_temp("nan_check", "js", &prog);
    let out = run_tool("node", &path);
    let _ = std::fs::remove_file(&path);
    assert!(
        out.status.success(),
        "node failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "true");
}
