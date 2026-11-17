use super::*;

static STDLIB: std::sync::LazyLock<Vec<skull_parser::Ast>> = std::sync::LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|file| {
            let id = Ustr::from(rock_stdlib::stdlib_file_id(file.name).as_str());
            skull_parser::parse_module_clean(file.contents, id).expect("stdlib parse")
        })
        .collect()
});

/// `src` parsed as `file`, after the stdlib when `stdlib` is set.
fn workspace_of(src: &str, file: &str, stdlib: bool) -> Workspace {
    let mut modules = if stdlib { STDLIB.clone() } else { Vec::new() };
    modules.push(skull_parser::parse_module_clean(src, Ustr::from(file)).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic")
}

fn module_id(ws: &Workspace, file: &str) -> ModuleId {
    *ws.module_ids.get(&Ustr::from(file)).expect("module id")
}

fn source_name_matches(name: Ustr, expected: &str) -> bool {
    bone_core::modules::display_name(name.as_str()) == expected
}

#[cfg(test)]
mod lua_definition_scope_tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// Panics when luajit cannot be spawned: a host without it must report red
    /// for a test that never ran, never green.
    fn luajit_loads(lua: &str) -> (bool, String) {
        let mut child = Command::new("luajit")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("luajit is required for this execution test but could not be spawned");
        if let Err(error) = child.stdin.take().unwrap().write_all(lua.as_bytes()) {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        }
        let output = child.wait_with_output().unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    #[test]
    fn definition_scopes_cross_the_luajit_main_constant_limit() {
        let mut raw = String::new();
        let mut definitions = LuaDefinitionWriter::new(String::new());
        for i in 0..32_769 {
            let function = format!("function generated_{i}() return {i} end\n");
            raw.push_str(&function);
            definitions.push_str(&function);
        }
        let (raw_loaded, raw_error) = luajit_loads(&raw);
        assert!(!raw_loaded, "the unpartitioned control unexpectedly loaded");
        assert!(
            raw_error.contains("more than 65536 constants"),
            "the control failed for the wrong reason: {raw_error}"
        );

        let mut scoped = definitions.finish();
        scoped.push_str("assert(generated_0() == 0 and generated_32768() == 32768)\n");
        let (scoped_loaded, scoped_error) = luajit_loads(&scoped);
        assert!(
            scoped_loaded,
            "partitioned definitions did not load: {scoped_error}"
        );
    }
}

#[cfg(test)]
mod unread_constant_tests {
    use super::*;
    use skull_mir::mir::FuncRef;

    const SRC: &str = "const FOO: int = 5\n\
        inline(lua) fn peek() -> int = \"return 0\"\n\
        fn top() -> int { peek() }\n\
        fn foo() -> int { FOO }\n\
        fn main() { top() }\n";

    fn unread_ws(source: &str) -> Workspace {
        workspace_of(source, "<unread-const>", false)
    }

    fn function(ws: &Workspace, name: &str) -> FuncRef {
        ws.source_modules()
            .find_map(|m| {
                m.functions
                    .iter()
                    .find(|(_, f)| source_name_matches(f.name, name))
                    .map(|(func, _)| FuncRef { module: m.id, func })
            })
            .expect("function")
    }

    /// The full build drops the unread `FOO`; an overlay whose edited inline
    /// Lua body now spells it must initialize the global.
    #[test]
    fn an_overlay_initializes_a_constant_an_edited_inline_body_spells() {
        let ws1 = unread_ws(SRC);
        let mid = module_id(&ws1, "<unread-const>");
        let (full, mut cache) = emit_full(&ws1, mid).expect("full");
        assert!(!full.lua.contains("FOO ="), "{}", full.lua);

        let ws2 = unread_ws(&SRC.replace("\"return 0\"", "\"return FOO\""));
        let peek = function(&ws2, "peek");
        let overlay = emit_overlay(
            &ws2,
            &mut cache,
            &[(peek.module, BodyOwner::Function(peek.func))],
        )
        .expect("overlay");
        assert!(overlay.lua.contains("FOO = "), "{}", overlay.lua);
    }
}
