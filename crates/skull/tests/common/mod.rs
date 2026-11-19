use bone_semantic::inference::Workspace;
use skull::instance_graph::{self, InstanceGraph};
use ustr::Ustr;

fn with_stdlib(src: &str, file: &str) -> skull_parser::stdlib::ModuleSet {
    let user = skull_parser::parse_module_clean(src, Ustr::from(file)).expect("parse");
    skull_parser::stdlib::ModuleSet::with_stdlib([user]).expect("stdlib parses")
}

fn semantic_pass(
    modules: &mut skull_parser::stdlib::ModuleSet,
) -> Result<Workspace, skull_parser::CompilerError> {
    skull::pipeline::semantic_pass(modules, &Default::default())
}

fn assert_no_bone_errors(ws: &Workspace) {
    let errs: Vec<_> = recorded_bone_errors(ws).map(|e| &e.message).collect();
    assert!(errs.is_empty());
}

fn recorded_bone_errors(ws: &Workspace) -> impl Iterator<Item = &skull_parser::CompilerError> {
    ws.inferences
        .iter()
        .flatten()
        .flat_map(|inf| inf.errors.values().flatten())
        .chain(
            ws.modules
                .iter()
                .flat_map(|m| m.lowering_errors.values().flatten()),
        )
        .chain(ws.modules.iter().flat_map(|m| m.interface_errors.iter()))
}

pub fn bone_rejections(src: &str) -> Vec<String> {
    bone_rejection_errors(src)
        .into_iter()
        .map(|e| e.message)
        .collect()
}

pub fn bone_rejection_errors(src: &str) -> Vec<skull_parser::CompilerError> {
    let mut modules = with_stdlib(src, "<test>");
    // Fatal violations come back as `Err`; the rest are recorded on the workspace.
    let ws = match semantic_pass(&mut modules) {
        Ok(ws) => ws,
        Err(e) => return vec![e],
    };
    recorded_bone_errors(&ws).cloned().collect()
}

pub fn emit_mir(src: &str) -> skull::mir_emit::MirEmit {
    emit_at(&checked_workspace(src, "<test>"), "<test>").expect("emit")
}

fn emit_with_inputs(
    src: &str,
    file: &str,
    inputs: bone_semantic::inference::ComptimeInputs,
) -> Result<skull::mir_emit::MirEmit, skull_parser::CompilerError> {
    let mut ws = checked_workspace(src, file);
    ws.comptime_inputs = inputs;
    emit_at(&ws, file)
}

fn checked_workspace(src: &str, file: &str) -> Workspace {
    let ws = semantic_pass(&mut with_stdlib(src, file)).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    ws
}

fn emit_at(
    ws: &Workspace,
    file: &str,
) -> Result<skull::mir_emit::MirEmit, skull_parser::CompilerError> {
    skull::mir_emit::emit_workspace_mir(ws, module_id(ws, file))
}

pub fn ws_with_stdlib(src: &str) -> Workspace {
    let mut modules = with_stdlib(src, "<test>");
    semantic_pass(&mut modules).expect("semantic_pass")
}

pub fn ws_of_files(files: &[(&str, &str)]) -> Workspace {
    semantic_pass(&mut parse_files(files).expect("parse")).expect("semantic_pass")
}

fn parse_files(
    files: &[(&str, &str)],
) -> Result<skull_parser::stdlib::ModuleSet, skull_parser::CompilerError> {
    let mut modules = skull_parser::stdlib::ModuleSet::with_stdlib([]).expect("stdlib parses");
    for (file, src) in files {
        let mut ast = skull_parser::parse_module_clean(src, Ustr::from(file))?;
        if files.len() > 1 {
            ast.package = Some(shared_test_package());
        }
        modules.push(ast);
    }
    Ok(modules)
}

pub fn emit_module(src: &str) -> String {
    emit_mir(src).lua
}

pub fn ws_v1_package(package: &str, fragments: &[(&str, &str)]) -> Workspace {
    let mut modules = skull_parser::stdlib::ModuleSet::with_stdlib([]).expect("stdlib parses");
    for &(file, src) in fragments {
        let mut module = skull_parser::parse_module_clean(src, Ustr::from(file)).expect("parse");
        module.package = Some(skull_parser::PackageTag {
            id: 0,
            self_name: package.to_string(),
            identity_storage: Default::default(),
            root: String::new(),
        });
        modules.push(module);
    }
    let ws = semantic_pass(&mut modules).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    ws
}

fn shared_test_package() -> skull_parser::PackageTag {
    skull_parser::PackageTag {
        id: 0,
        self_name: format!(
            "{}<shared-test>",
            bone_core::modules::STANDALONE_PACKAGE_PREFIX
        ),
        identity_storage: Default::default(),
        root: String::new(),
    }
}

pub fn emit_v1_package(package: &str, fragments: &[(&str, &str)]) -> skull::mir_emit::MirEmit {
    let ws = ws_v1_package(package, fragments);
    let mid = module_id(&ws, fragments[0].0);
    skull::mir_emit::emit_workspace_mir(&ws, mid).expect("emit")
}

pub fn try_emit_module_with(
    src: &str,
    inputs: bone_semantic::inference::ComptimeInputs,
) -> Result<String, skull_parser::CompilerError> {
    try_emit_module_with_file(src, "<test>", inputs)
}

pub fn try_emit_module_with_file(
    src: &str,
    file: &str,
    inputs: bone_semantic::inference::ComptimeInputs,
) -> Result<String, skull_parser::CompilerError> {
    emit_with_inputs(src, file, inputs).map(|e| e.lua)
}

pub fn try_emit_module(src: &str) -> Result<String, skull_parser::CompilerError> {
    emit_at(&checked_workspace(src, "<test>"), "<test>").map(|e| e.lua)
}

pub fn emit_rejection(src: &str) -> String {
    match try_emit_module(src) {
        Ok(lua) => panic!("expected a rejection, emitted:\n{lua}"),
        Err(e) => e.message,
    }
}

pub fn try_emit_module_enforcing_identity_storage(
    src: &str,
) -> Result<String, skull_parser::CompilerError> {
    let modules = with_stdlib(src, "<test>");
    let ws = bone_semantic::semantic_passes::semantic_pass_enforcing_identity_storage(
        &mut modules.into_vec(),
        &bone_core::modules::NsAliases::default(),
    )
    .expect("semantic_pass");
    assert_no_bone_errors(&ws);
    let mid = module_id(&ws, "<test>");
    skull::mir_emit::emit_workspace_mir(&ws, mid).map(|e| e.lua)
}

pub fn emit_module_without_frame_limits(src: &str) -> String {
    emit_module_shaped(
        src,
        "<test>",
        skull::mir_emit::ShapeOverrides {
            frame_limits: Some(false),
            ..Default::default()
        },
    )
}

pub fn emit_module_shaped(
    src: &str,
    file: &str,
    shapes: skull::mir_emit::ShapeOverrides,
) -> String {
    let ws = checked_workspace(src, file);
    let mid = module_id(&ws, file);
    skull::mir_emit::FullEmit::program()
        .shapes(shapes)
        .run(&ws, mid)
        .map(|(emit, _cache)| emit.lua)
        .expect("emit under explicit shapes")
}

fn temp_lua_path() -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("skull_test_{}_{n}.lua", std::process::id()))
}

pub fn luajit_loads(prog: &str) -> Result<(), String> {
    let path = temp_lua_path();
    std::fs::write(&path, prog).expect("write program");
    let out = std::process::Command::new("luajit")
        .arg("-e")
        .arg(format!(
            "local f, e = loadfile({:?}); if not f then io.stderr:write(e) os.exit(1) end",
            path.to_string_lossy()
        ))
        .output()
        .expect("spawn luajit");
    let _ = std::fs::remove_file(&path);
    if out.status.success() {
        return Ok(());
    }
    Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
}

pub fn global_slots(lua: &str, identity: &str) -> Vec<u32> {
    lua.split(&format!("identity=\"{identity}\""))
        .nth(1)
        .unwrap_or_else(|| panic!("missing global directory entry for {identity}"))
        .split("slots={")
        .nth(1)
        .and_then(|rest| rest.split('}').next())
        .expect("global directory entry has slots")
        .split(',')
        .map(|slot| slot.parse::<u32>().expect("Lua global slot is numeric"))
        .collect()
}

pub fn function_params<'a>(lua: &'a str, name: &str) -> Vec<&'a str> {
    let body = function_body(lua, name);
    let open = body.find('(').expect("a parameter list");
    let close = body[open..].find(')').expect("a closed parameter list") + open;
    let inner = body[open + 1..close].trim();
    if inner.is_empty() {
        return Vec::new();
    }
    inner.split(',').map(str::trim).collect()
}

pub fn function_body<'a>(lua: &'a str, name: &str) -> &'a str {
    let resolved = resolve_definition(lua, name);
    let global = format!("\nfunction {resolved}(");
    let (start, terminator) = match lua.find(&global) {
        Some(at) => (at + 1, "\nend\n"),
        None => {
            let head = format!(
                "\n{}{resolved} = function(",
                skull_codegen_lua::CALLEE_FACTORY_PREFIX
            );
            let at = lua
                .find(&head)
                .unwrap_or_else(|| panic!("definition `{resolved}` has neither shape in:\n{lua}"));
            (at + 1, "\nend end\n")
        }
    };
    let rest = &lua[start..];
    let end = rest
        .find(terminator)
        .map(|e| e + terminator.len())
        .unwrap_or(rest.len());
    &rest[..end]
}

pub fn bound_callee_name(lua: &str, caller: &str, global: &str) -> String {
    bound_callee_names(lua, caller)
        .into_iter()
        .find(|(_, target)| target == global)
        .map(|(parameter, _)| parameter)
        .unwrap_or_else(|| global.to_string())
}

pub fn bound_callee_names(lua: &str, caller: &str) -> Vec<(String, String)> {
    let caller = resolve_definition(lua, caller);
    let factory_head = format!(
        "{}{caller} = function(",
        skull_codegen_lua::CALLEE_FACTORY_PREFIX
    );
    let Some(header) = lua.lines().find(|line| line.starts_with(&factory_head)) else {
        return Vec::new();
    };
    let params = header
        .strip_prefix(&factory_head)
        .and_then(|rest| rest.split_once(") return function("))
        .map(|(params, _)| params)
        .expect("callee factory header has its inner function");

    let binding_head = format!(
        "{caller} = {}{caller}(",
        skull_codegen_lua::CALLEE_FACTORY_PREFIX
    );
    let binding = lua
        .lines()
        .find(|line| line.starts_with(&binding_head))
        .unwrap_or_else(|| panic!("callee factory `{caller}` has no binding line"));
    let globals = binding
        .strip_prefix(&binding_head)
        .and_then(|rest| rest.strip_suffix(')'))
        .expect("callee binding line has a closed argument list");

    let globals: Vec<&str> = if globals.is_empty() {
        Vec::new()
    } else {
        globals.split(", ").collect()
    };
    let params: Vec<&str> = if params.is_empty() {
        Vec::new()
    } else {
        params.split(", ").collect()
    };
    assert_eq!(params.len(), globals.len());
    params
        .into_iter()
        .zip(globals)
        .map(|(parameter, global)| (parameter.to_string(), global.to_string()))
        .collect()
}

fn resolve_definition<'a>(lua: &'a str, name: &str) -> &'a str {
    let names = definition_names(lua);
    names
        .iter()
        .find(|n| **n == name)
        .or_else(|| {
            names
                .iter()
                .find(|n| n.starts_with(name) && !n[name.len()..].contains("__"))
        })
        .or_else(|| names.iter().find(|n| n.starts_with(name)))
        .copied()
        .unwrap_or_else(|| panic!("no definition of `{name}` in:\n{lua}"))
}

pub fn definition_names(lua: &str) -> Vec<&str> {
    let factory_head = format!("\n{}", skull_codegen_lua::CALLEE_FACTORY_PREFIX);
    let mut names: Vec<(usize, &str)> = Vec::new();
    for (at, m) in lua.match_indices("\nfunction ") {
        let rest = &lua[at + m.len()..];
        names.push((at, &rest[..rest.find('(').expect("fn header has `(`")]));
    }
    for (at, m) in lua.match_indices(factory_head.as_str()) {
        let rest = &lua[at + m.len()..];
        // The binding line `F = __rock_mk_F(..)` shares the prefix but is no header.
        if let Some(end) = rest.find(" = function(") {
            if !rest[..end].contains('\n') {
                names.push((at, &rest[..end]));
            }
        }
    }
    names.sort_by_key(|(at, _)| *at);
    names.into_iter().map(|(_, name)| name).collect()
}

pub fn definitions_matching<'a>(lua: &'a str, needle: &str) -> Vec<&'a str> {
    definition_names(lua)
        .into_iter()
        .filter(|n| n.contains(needle))
        .collect()
}

pub fn body_of_synthetic<'a>(lua: &'a str, needle: &str) -> &'a str {
    let names: Vec<&str> = definition_names(lua)
        .into_iter()
        .filter(|n| n.contains(needle))
        .collect();
    assert_eq!(names.len(), 1);
    function_body(lua, names[0])
}

pub fn mangled_name(lua: &str, src: &str) -> String {
    definition_names(lua)
        .into_iter()
        .find(|name| name.starts_with(src) && !name.contains("__"))
        .unwrap_or_else(|| panic!("no non-synthetic definition of `{src}` in:\n{lua}"))
        .to_string()
}

pub fn try_emit_with_stdlib_module(
    lib: &str,
    src: &str,
) -> Result<String, skull_parser::CompilerError> {
    emit_at(&ws_with_stdlib_module(lib, src), "<test>").map(|e| e.lua)
}

pub fn ws_with_stdlib_module(lib: &str, src: &str) -> Workspace {
    let mut modules = skull_parser::stdlib::ModuleSet::with_stdlib([]).expect("stdlib parses");
    modules.push(
        skull_parser::parse_module_clean(
            lib,
            Ustr::from(rock_stdlib::stdlib_file_id("probe").as_str()),
        )
        .expect("parse lib"),
    );
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = semantic_pass(&mut modules).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    ws
}

pub fn ws_with_dependency_module(lib: &str, src: &str) -> Workspace {
    let mut modules = skull_parser::stdlib::ModuleSet::with_stdlib([]).expect("stdlib parses");
    let mut library = skull_parser::parse_module_clean(
        lib,
        Ustr::from("/tmp/rock-mono-project-library/engine.rock"),
    )
    .expect("parse lib");
    library.package = Some(shared_test_package());
    modules.push(library);
    let mut main =
        skull_parser::parse_module_clean(src, Ustr::from("/tmp/rock-mono-project/main.rock"))
            .expect("parse");
    main.package = Some(shared_test_package());
    modules.push(main);
    let ws = semantic_pass(&mut modules).expect("semantic_pass");
    assert_no_bone_errors(&ws);
    ws
}

pub fn module_id(ws: &Workspace, file: &str) -> bone_semantic::prelude::ModuleId {
    *ws.module_ids.get(&Ustr::from(file)).expect("module id")
}

pub fn key(ws: &Workspace, name: &str) -> Ustr {
    let mut keys: Vec<Ustr> = ws
        .defs
        .keys()
        .chain(ws.functions_by_name.keys())
        .chain(ws.struct_by_name.keys())
        .chain(ws.enums_by_name.keys())
        .chain(ws.newtypes_by_name.keys())
        .chain(ws.globals_by_name.keys())
        .chain(ws.type_generics.keys())
        .copied()
        .collect();
    keys.sort();
    keys.dedup();
    let raw = Ustr::from(name);
    if keys.contains(&raw) {
        return raw;
    }
    let matches = |key: &&Ustr| {
        bone_core::modules::display_name(key.as_str()) == name
            || ws
                .defs
                .get(*key)
                .is_some_and(|meta| meta.name.as_str() == name)
    };
    let standalone: Vec<Ustr> = keys
        .iter()
        .filter(|key| {
            key.as_str()
                .starts_with(bone_core::modules::STANDALONE_PACKAGE_PREFIX)
                && matches(key)
        })
        .copied()
        .collect();
    let candidates = if standalone.is_empty() {
        keys.iter().filter(matches).copied().collect::<Vec<_>>()
    } else {
        standalone
    };
    match candidates.as_slice() {
        [found] => *found,
        [] => raw,
        _ => panic!("expected an unambiguous key for `{name}`, got {candidates:?}"),
    }
}

pub fn name_matches(actual: Ustr, name: &str) -> bool {
    actual.as_str() == name || bone_core::modules::display_name(actual.as_str()) == name
}

pub fn fn_ref(ws: &Workspace, name: &str) -> skull_mir::mir::FuncRef {
    let key = key(ws, name);
    let mut found = Vec::new();
    for m in ws.modules.iter() {
        for (fid, f) in m.functions.iter() {
            if f.name == key {
                found.push(skull_mir::mir::FuncRef {
                    module: m.id,
                    func: fid,
                });
            }
        }
    }
    assert_eq!(found.len(), 1);
    found[0]
}

pub fn body_id(ws: &Workspace, name: &str) -> EditedBody {
    let f = fn_ref(ws, name);
    (f.module, bone_semantic::hir::BodyOwner::Function(f.func))
}

pub fn emit_full_of(
    ws: &Workspace,
    what: &str,
) -> (skull::mir_emit::MirEmit, skull::mir_emit::SkullEmitCache) {
    let mid = skull::user_module_id(ws).unwrap();
    skull::mir_emit::emit_full(ws, mid).unwrap_or_else(|e| panic!("{what}: {e:?}"))
}

pub fn full_lua(ws: &Workspace) -> String {
    emit_full_of(ws, "emit_full").0.lua
}

pub type EditedBody = (
    bone_semantic::prelude::ModuleId,
    bone_semantic::hir::BodyOwner,
);

pub fn global_body(ws: &Workspace, name: &str) -> EditedBody {
    ws.modules
        .iter()
        .find_map(|m| {
            m.globals
                .iter()
                .find(|(_, g)| name_matches(g.name, name))
                .map(|(id, _)| (m.id, bone_semantic::hir::BodyOwner::Global(id)))
        })
        .unwrap_or_else(|| panic!("no global `{name}`"))
}

pub fn check_incremental(
    ws: &Workspace,
    cache: &mut skull::mir_emit::SkullEmitCache,
    edited: &[EditedBody],
) -> skull::mir_emit::MirEmit {
    let incremental =
        skull::mir_emit::emit_incremental(ws, cache, edited).expect("incremental emit");
    assert_eq!(incremental.lua, full_lua(ws));
    incremental
}

pub struct Edit {
    pub base: skull::mir_emit::MirEmit,
    pub incremental: skull::mir_emit::MirEmit,
    pub overlay: Result<skull::mir_emit::MirEmit, skull_parser::CompilerError>,
}

pub fn edit(before: &Workspace, after: &Workspace, edited: &[EditedBody]) -> Edit {
    let (base, mut cache) = emit_full_of(before, "base emit");
    let (_, mut overlay_cache) = emit_full_of(before, "overlay base emit");
    Edit {
        base,
        incremental: check_incremental(after, &mut cache, edited),
        overlay: skull::mir_emit::emit_overlay(after, &mut overlay_cache, edited),
    }
}

pub fn overlay_or_restart(
    overlay: Result<skull::mir_emit::MirEmit, skull_parser::CompilerError>,
) -> Option<skull::mir_emit::MirEmit> {
    match overlay {
        Ok(overlay) => Some(overlay),
        Err(e) => {
            assert!(
                e.message.contains("compatibility") && e.message.contains("restart"),
                "{e:?}"
            );
            None
        }
    }
}

pub fn graph_for(ws: &Workspace) -> (InstanceGraph, Vec<skull_mir::mir::FuncRef>) {
    let roots = skull::mir_emit::emission_roots(ws, None);
    (instance_graph::build(ws, roots.clone(), false), roots)
}

pub fn body_calls(body: &str, symbol: &str) -> bool {
    body.match_indices(symbol).any(|(at, _)| {
        let before = body[..at].chars().next_back();
        let after = body[at + symbol.len()..].chars().next();
        after == Some('(') && before.is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_')
    })
}

pub fn callees_named_in(lua: &str, caller: &str, callee: &str) -> Vec<String> {
    let body = function_body(lua, caller);
    let mut out: Vec<String> = definitions_of(lua, callee)
        .into_iter()
        .filter(|global| body_calls(body, &bound_callee_name(lua, caller, global)))
        .collect();
    out.sort();
    out.dedup();
    out
}

pub fn definition_symbols(lua: &str) -> Vec<String> {
    definition_names(lua)
        .into_iter()
        .map(str::to_string)
        .collect()
}

pub fn definitions_of(lua: &str, name: &str) -> Vec<String> {
    definition_symbols(lua)
        .into_iter()
        .filter(|s| s.starts_with(name) && !s.contains("__"))
        .collect()
}

pub fn ws_of(src: &str) -> Workspace {
    let module = skull_parser::parse_module_clean(src, Ustr::from("<t>")).expect("parse");
    bone_semantic::semantic_passes::semantic_pass(&mut [module]).expect("semantic_pass")
}

pub fn emit_without_stdlib(src: &str) -> String {
    let ws = ws_of(src);
    let mid = skull::user_module_id(&ws).unwrap();
    skull::mir_emit::emit_workspace_mir(&ws, mid)
        .expect("emit")
        .lua
}

fn luajit_output(prog: &str) -> std::process::Output {
    let path = temp_lua_path();
    std::fs::write(&path, prog).expect("write program");
    let out = std::process::Command::new("luajit")
        .arg(&path)
        .output()
        .expect("spawn luajit");
    let _ = std::fs::remove_file(&path);
    out
}

pub fn run_luajit_capturing(prog: &str) -> (bool, String) {
    let out = luajit_output(prog);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), combined)
}

pub fn run_luajit(prog: &str) -> String {
    let out = luajit_output(prog);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "luajit failed:\n{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout.trim().replace("\r\n", "\n")
}

pub fn run_main(lua: &str) -> String {
    run_fn(lua, "main")
}

pub fn run_fn(lua: &str, name: &str) -> String {
    let resolved = resolve_definition(lua, name);
    run_luajit(&format!("{lua}\n{resolved}()\n"))
}

pub fn compile_and_run(src: &str) -> String {
    run_main(&emit_module(src))
}

pub use skull::allocation_report::{activation_scope, scope as allocation_scope};
pub use skull_transform::inline_off_guard;

pub fn has_vec_header(lua: &str, repr: &str, elem: Option<&str>) -> bool {
    let (own, shared) = match elem {
        Some(elem) => (
            format!("repr = \"{repr}\", elem = \"{elem}\""),
            format!("__rock_vec_header(\"{repr}\", \"{elem}\""),
        ),
        None => (
            format!("repr = \"{repr}\""),
            format!("__rock_vec_header(\"{repr}\""),
        ),
    };
    lua.contains(&own) || lua.contains(&shared)
}
