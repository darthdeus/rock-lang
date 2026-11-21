//! Shared test helpers: build the stdlib workspace and emit an intrinsic's
//! scalar Rock-fallback body to JS.

use skull_mir::mir::FuncRef;

/// Every `node` spawn in this crate's tests goes through here.
///
/// `console.log` of a NUMBER is formatted by `util.inspect`, which colours it,
/// and `FORCE_COLOR` forces that on even when stdout is a pipe. A harness that
/// parses stdout then receives `"\x1b[33m9901\x1b[39m"` and fails to read an
/// int -- the value is right, the bytes are not.
///
/// **`NO_COLOR` alone does not work**: node ignores it whenever `FORCE_COLOR`
/// is set, and says so on stderr. The variable has to be REMOVED, so a fix that
/// only sets `NO_COLOR` reads correctly and does nothing.
pub fn node() -> std::process::Command {
    tool_command("node")
}

/// A command for `name` with the same colour scrub as [`node`].
fn tool_command(name: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(name);
    cmd.env_remove("FORCE_COLOR").env("NO_COLOR", "1");
    cmd
}

/// Run `program` under `node -e` and return its stdout. Panics, showing the
/// program and stderr, when node exits non-zero.
pub fn run_node(program: &str) -> String {
    let out = node().arg("-e").arg(program).output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run `body` under node after the concatenated `runtime` pieces.
pub fn run_js(runtime: &[&str], body: &str) -> String {
    run_node(&format!("{}\n{body}", runtime.concat()))
}

/// Write `program` to a file, run `tool` on it and return trimmed stdout.
pub fn run_file(tool: &str, program: &str) -> String {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "skull_codegen_js_{}_{serial}_{tool}",
        std::process::id()
    ));
    std::fs::write(&path, program).expect("write program");
    let out = tool_command(tool).arg(&path).output().expect("run tool");
    let _ = std::fs::remove_file(path);
    assert!(
        out.status.success(),
        "{tool} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8 stdout")
        .trim()
        .to_string()
}

/// The id of the `<test>` module.
pub fn test_mid(ws: &bone_core::workspace::Workspace) -> bone_core::prelude::ModuleId {
    *ws.module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module")
}

/// Emit the `<test>` module as a whole JS program.
pub fn emit_program_js(ws: &bone_core::workspace::Workspace) -> Result<String, String> {
    skull_codegen_js::emit::emit_program(ws, test_mid(ws))
        .map(|p| p.program)
        .map_err(|e| e.message)
}

/// The `<test>` module's fn named `name`.
pub fn test_fn_ref(ws: &bone_core::workspace::Workspace, name: &str) -> FuncRef {
    let mid = test_mid(ws);
    for (fid, f) in ws.module(mid).functions.iter() {
        if skull_mir::mangle::declared_name(f.name.as_str()) == name {
            return FuncRef {
                module: mid,
                func: fid,
            };
        }
    }
    panic!("no fn `{name}` in <test> module");
}

/// Emit the `<test>` fn `root` and its callee closure to JS.
pub fn emit_closure(ws: &bone_core::workspace::Workspace, root: &str) -> Result<String, String> {
    skull_codegen_js::emit::emit_with_closure(ws, &[(test_fn_ref(ws, root), root.to_string())])
        .map_err(|e| e.message)
}

/// Build the stdlib workspace (parse + semantic pass).
pub fn stdlib_ws() -> bone_core::workspace::Workspace {
    ws_with("fn main() {}")
}

/// Stdlib workspace plus `src` as the `<test>` module.
pub fn ws_with(src: &str) -> bone_core::workspace::Workspace {
    ws_with_named("<test>", src)
}

pub fn ws_with_named(file: &str, src: &str) -> bone_core::workspace::Workspace {
    try_ws_with_named(file, src).expect("semantic_pass")
}

pub fn try_ws_with_named(
    file: &str,
    src: &str,
) -> Result<bone_core::workspace::Workspace, skull_parser::CompilerError> {
    let module = skull_parser::parse_module_clean(src, ustr::Ustr::from(file)).expect("parse");
    let mut modules = skull_parser::stdlib::ModuleSet::with_stdlib([module])?;
    skull::pipeline::semantic_pass(&mut modules, &Default::default())
}

/// `file` parsed as a module of package `package`.
pub fn package_module(package: &str, file: &str, src: &str) -> skull_parser::Ast {
    let mut module = skull_parser::parse_module_clean(src, ustr::Ustr::from(file)).expect("parse");
    module.package = Some(skull_parser::PackageTag {
        id: 0,
        self_name: package.to_string(),
        identity_storage: Default::default(),
        root: String::new(),
    });
    module
}

/// Stdlib workspace plus `users`, failing on any body or interface error.
pub fn ws_of_modules(users: Vec<skull_parser::Ast>) -> bone_core::workspace::Workspace {
    let mut modules = skull_parser::stdlib::ModuleSet::with_stdlib(users).expect("stdlib");
    let ws = skull::pipeline::semantic_pass(&mut modules, &Default::default()).expect("workspace");
    let errors: Vec<_> = ws
        .inferences
        .iter()
        .flatten()
        .flat_map(|inference| inference.errors.values().flatten())
        .chain(
            ws.modules
                .iter()
                .flat_map(|module| module.lowering_errors.values().flatten()),
        )
        .chain(
            ws.modules
                .iter()
                .flat_map(|module| module.interface_errors.iter()),
        )
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    ws
}
