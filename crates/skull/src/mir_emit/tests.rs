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

#[cfg(test)]
mod incremental_output_equality_tests {
    use super::*;

    #[derive(Clone, Copy)]
    enum EditedBody {
        Function(&'static str),
        Global(&'static str),
    }

    fn assert_sequence(file: &str, sources: &[String], edited: EditedBody, diagnostic: &str) {
        let file = format!("/tmp/rock-edit-equality/{file}.rock");
        let build = FullEmit::program().project_root(Some("/tmp/rock-edit-equality"));
        let ws = workspace_of(&sources[0], &file, true);
        let mid = module_id(&ws, &file);
        let (_, mut cache) = build.run(&ws, mid).expect("initial full emit");

        for (step, source) in sources.iter().enumerate().skip(1) {
            let ws = workspace_of(source, &file, true);
            let mid = module_id(&ws, &file);
            let owner = match edited {
                EditedBody::Function(name) => BodyOwner::Function(
                    ws.module(mid)
                        .functions
                        .iter()
                        .find(|(_, f)| source_name_matches(f.name, name))
                        .expect("edited function")
                        .0,
                ),
                EditedBody::Global(name) => BodyOwner::Global(
                    ws.module(mid)
                        .globals
                        .iter()
                        .find(|(_, g)| source_name_matches(g.name, name))
                        .expect("edited global")
                        .0,
                ),
            };
            let output = |result: Result<MirEmit, CompilerError>| {
                result.map(|emit| emit.lua).map_err(|error| {
                    (
                        error.message,
                        error
                            .labels
                            .into_iter()
                            .map(|label| (label.span, label.message))
                            .collect::<Vec<_>>(),
                    )
                })
            };
            let incremental = output(emit_incremental(&ws, &mut cache, &[(mid, owner)]));
            let fresh = build.run(&ws, mid);
            if let (Ok(_), Ok((_, fresh))) = (&incremental, &fresh) {
                let instances = |graph: &crate::instance_graph::InstanceGraph| {
                    graph
                        .instances
                        .iter()
                        .map(|i| graph.identity(&ws, i))
                        .collect::<BTreeSet<_>>()
                };
                let (incremental, fresh) = (instances(&cache.graph), instances(&fresh.graph));
                assert!(
                    incremental == fresh,
                    "{file}, edit {step}: only incremental {:?}, only fresh {:?}",
                    incremental.difference(&fresh).collect::<Vec<_>>(),
                    fresh.difference(&incremental).collect::<Vec<_>>()
                );
            }
            let fresh = output(fresh.map(|(emit, _)| emit));
            match (incremental, fresh) {
                (Err(a), Err(b)) if !diagnostic.is_empty() => {
                    assert!(a.0.contains(diagnostic), "{}", a.0);
                    assert!(b.0.contains(diagnostic), "{}", b.0);
                }
                (incremental, fresh) => assert_eq!(incremental, fresh, "{file}, edit {step}"),
            }
        }
    }

    fn shader_source(body: &str) -> String {
        format!(
            r#"
value QuadVert {{ aPos: Vec2, aTexCoord: Vec2 }}
value UiVertU {{ uProjection: Mat4, uView: Mat4, uTransform: Mat4 }}
value UiVary {{ position: Vec4, TexCoord: Vec2 }}
@shader(vertex)
fn shared_vertex(@attr a: QuadVert, @uniform u: UiVertU) -> UiVary {{
    let clip = u.uProjection * u.uView * u.uTransform * Vec4 {{ x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }}
    return UiVary {{ position: clip, TexCoord: a.aTexCoord }}
}}
@shader(fragment)
fn first_fragment(v: UiVary) -> Vec4 {{ return Vec4 {{ x: 1.0, y: 0.0, z: 0.0, w: 1.0 }} }}
@shader(fragment)
fn second_fragment(v: UiVary) -> Vec4 {{ return Vec4 {{ x: 0.0, y: 1.0, z: 0.0, w: 1.0 }} }}
fn choose() -> string {{ {body} }}
fn main() {{ let _ = choose() }}
"#
        )
    }

    #[test]
    fn incremental_edits_match_fresh_output_after_roots_and_inline_dependencies_change() {
        assert_sequence(
            "<shader-edit>",
            &[
                shader_source("return rock_shader(shared_vertex, first_fragment)"),
                shader_source("return rock_shader(shared_vertex, second_fragment)"),
                shader_source("return \"none\""),
            ],
            EditedBody::Function("choose"),
            "",
        );

        let global_source = |target: &str| {
            format!(
                "fn edit_inc(x: int) -> int {{ return x + 1 }}\n\
                 fn edit_dec(x: int) -> int {{ return x - 1 }}\n\
                 global CALLBACK: fn(int) -> int = {target}\n\
                 fn invoke(x: int) -> int {{ return CALLBACK(x) }}\n\
                 fn main() {{ let _ = invoke(2) }}"
            )
        };
        assert_sequence(
            "<fn-value-global-edit>",
            &[
                global_source("edit_inc"),
                global_source("edit_dec"),
                global_source("edit_inc"),
            ],
            EditedBody::Global("CALLBACK"),
            "",
        );

        let inspect_source = |inspect: bool| {
            let body = if inspect {
                "return inspect(xs)"
            } else {
                "return \"\""
            };
            format!(
                "value V2 {{ x: int, y: int }}\n\
                 fn caller(xs: [V2]) -> string {{ {body} }}\n\
                 fn main() {{ let xs = [V2 {{ x: 1, y: 2 }}]; let _ = caller(xs) }}"
            )
        };
        assert_sequence(
            "<inline-dependency-edit>",
            &[
                inspect_source(true),
                inspect_source(false),
                inspect_source(true),
            ],
            EditedBody::Function("caller"),
            "",
        );

        let generic_source = |call: bool| {
            let result = if call { "identity(7)" } else { "0" };
            format!(
                "fn identity<T>(x: T) -> T {{ return x }}\n\
                 fn caller() -> int {{ return {result} }}\n\
                 fn main() {{ let _ = caller() }}"
            )
        };
        assert_sequence(
            "<generic-last-call-edit>",
            &[
                generic_source(true),
                generic_source(false),
                generic_source(true),
            ],
            EditedBody::Function("caller"),
            "",
        );
    }
    #[test]
    fn pruning_shared_and_cyclic_calls_matches_fresh_output() {
        for (definitions, supported) in [
            (
                "fn branch() -> int { return leaf() } fn leaf() -> int { return 1 }",
                "branch()",
            ),
            (
                "fn branch() -> int { return leaf() + shared() } fn leaf() -> int { return 1 }",
                "branch()",
            ),
            (
                "fn branch() -> int { return cycle(2) } fn cycle(n:int) -> int { if n == 0 { return shared() } return back(n-1) } fn back(n:int) -> int { return cycle(n) }",
                "branch()",
            ),
            (
                "fn branch() -> int { return left() + right() } fn left() -> int { return shared() } fn right() -> int { return shared() }",
                "branch()",
            ),
            (
                "fn branch() -> int { return other(2) } fn other(n:int) -> int { if n == 0 { return 0 } return branch() }",
                "branch()",
            ),
            (
                "fn branch()->int { return shared()+apparent() } fn apparent()->int { return shared() }",
                "shared()",
            ),
            (
                "fn branch()->int { return lost_a()+lost_b() } fn lost_a()->int { return shared()+support_a() } fn lost_b()->int { return shared()+support_b() } fn support_a()->int { return lost_a()+join() } fn support_b()->int { return lost_b()+join() } fn join()->int { return support_a()+support_b() }",
                "shared()",
            ),
            (
                "fn branch()->int { return dead_c()+lost_b() } fn dead_c()->int { return dead_a() } fn dead_a()->int { return dead_branch() } fn dead_branch()->int { return dead_c()+target() } fn target()->int { return lost_b() } fn lost_b()->int { return 7 }",
                "target()",
            ),
            (
                "fn branch()->int { return dead_c()+lost_b() } fn target()->int { return lost_b() } fn dead_branch()->int { return dead_c()+target() } fn dead_c()->int { return dead_a() } fn dead_a()->int { return dead_branch() } fn lost_b()->int { return 7 }",
                "target()",
            ),
            (
                "fn branch()->int { return a()+dead() } fn a()->int { return b() } fn b()->int { return a() } fn dead()->int { return leaf() } fn leaf()->int { return 1 }",
                "a()",
            ),
        ] {
            for anchored in ["", "print(shared())", "print(anchor())"] {
                let source = |called: bool| {
                    format!(
                        "{definitions} fn shared() -> int {{ return 7 }} fn anchor()->int {{ return hop() }} fn hop()->int {{ return {supported} }} fn entry() -> int {{ return {} }} fn main() {{ print(entry()); {anchored} }}",
                        if called { "branch()" } else { "0" },
                    )
                };
                assert_sequence(
                    "prune",
                    &[source(true), source(false), source(true)],
                    EditedBody::Function("entry"),
                    "",
                );
            }
        }
    }

    #[test]
    fn generic_emission_failures_remain_located_diagnostics() {
        for (source, expected) in [
            (
                "fn ident<T>(x:T)->T { return x } fn apply(f:fn(int)->int,v:int)->int { return f(v) } fn main() { apply(ident,1) }",
                "generic",
            ),
            (
                "fn ident<T>(x:T)->T { return x } global F:fn(int)->int = ident fn main() { let y=F(1) }",
                "generic",
            ),
            (
                "fn make<T>()->[T] { return [] } fn main() { let xs=make(); let n=xs.len() }",
                "type",
            ),
            (
                "fn pick<T>()->int { let unused:T=1; return 1 } global G:int=pick() fn main() { let x=G }",
                "type",
            ),
        ] {
            let ws = workspace_of(source, "<generic-diagnostic>", true);
            let mid = module_id(&ws, "<generic-diagnostic>");
            let error = emit_full(&ws, mid).err().expect("emission must reject");
            assert!(error.message.contains(expected), "{}", error.message);
            assert!(
                error
                    .labels
                    .iter()
                    .any(|label| !crate::instance_graph::is_spanless(&label.span))
            );
        }
    }
    #[test]
    fn diagnostic_edits_recover_to_fresh_output() {
        let recursive = |bad: bool| {
            format!(
                "fn descend<T>(x:T)->int {{ return relay([x]) }} fn relay<T>(x:[T])->int {{ return {} }} fn main() {{ print(descend(1)) }}",
                if bad { "descend(x)" } else { "0" },
            )
        };
        assert_sequence(
            "recursion",
            &[recursive(false), recursive(true), recursive(false)],
            EditedBody::Function("relay"),
            "polymorphic recursion",
        );
    }
}
