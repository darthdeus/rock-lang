//! The user guide's examples are runnable files under `examples/guide`, and
//! every ```rock fence in `docs/guide.md` is one of those files byte for
//! byte, so the guide cannot drift from what the compiler accepts.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn guide_dir() -> PathBuf {
    repo_root().join("examples").join("guide")
}

fn invoke(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(cwd)
        .args(args)
        .env("ROCK_DIR", cwd)
        .env("ROCK_INVOCATION_LOG", "off")
        .env_remove("ROCK_STDLIB_DIR")
        .env_remove("LUA_PATH")
        .env_remove("LUA_CPATH")
        .output()
        .expect("start rock")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// One runnable example: a single file, or a project directory, plus the
/// stdout `rock run` must print.
struct RunCase {
    name: String,
    path: PathBuf,
    expected: Vec<u8>,
}

/// One shader example: compiled, never run. Roots come from the sibling
/// `<stem>.roots` file (one fn per line); one root is a fullscreen fragment
/// entry, two roots are a vertex+fragment pipeline.
struct ShaderCase {
    name: String,
    path: PathBuf,
    roots: Vec<String>,
}

fn inventory() -> (Vec<RunCase>, Vec<ShaderCase>, Vec<PathBuf>) {
    let dir = guide_dir();
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("read examples/guide")
        .map(|e| e.expect("dir entry").path())
        .collect();
    entries.sort();
    let mut runs = Vec::new();
    let mut shaders = Vec::new();
    let mut sources = Vec::new();
    let mut problems = Vec::new();
    for entry in &entries {
        let name = entry.file_name().unwrap().to_string_lossy().into_owned();
        if entry.is_dir() {
            let out = dir.join(format!("{name}.out"));
            if out.is_file() {
                sources.extend(rock_files(entry));
                runs.push(RunCase {
                    name,
                    path: entry.clone(),
                    expected: std::fs::read(&out).expect("read .out"),
                });
            } else {
                problems.push(format!("project dir without a sibling .out: {name}"));
            }
            continue;
        }
        let Some(ext) = entry.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        match ext {
            "rock" => {
                sources.push(entry.clone());
                let stem = entry.file_stem().unwrap().to_string_lossy().into_owned();
                let roots_file = dir.join(format!("{stem}.roots"));
                let out_file = dir.join(format!("{stem}.out"));
                match (roots_file.is_file(), out_file.is_file()) {
                    (true, false) => shaders.push(ShaderCase {
                        name: stem,
                        path: entry.clone(),
                        roots: roots_of(&roots_file),
                    }),
                    (false, true) => runs.push(RunCase {
                        name: stem,
                        path: entry.clone(),
                        expected: std::fs::read(&out_file).expect("read .out"),
                    }),
                    (true, true) => problems.push(format!("{stem} has both .roots and .out")),
                    (false, false) => {
                        problems.push(format!("{stem}.rock has neither .out nor .roots"))
                    }
                }
            }
            "out" => {
                let stem = entry.file_stem().unwrap().to_string_lossy().into_owned();
                if !dir.join(format!("{stem}.rock")).is_file() && !dir.join(&stem).is_dir() {
                    problems.push(format!("orphan expected output: {stem}.out"));
                }
            }
            "roots" => {
                let stem = entry.file_stem().unwrap().to_string_lossy().into_owned();
                if !dir.join(format!("{stem}.rock")).is_file() {
                    problems.push(format!("orphan roots file: {stem}.roots"));
                }
            }
            _ => problems.push(format!("unexpected file in examples/guide: {name}")),
        }
    }
    if !problems.is_empty() {
        panic!("examples/guide inventory:\n  {}", problems.join("\n  "));
    }
    assert!(
        !runs.is_empty() && !shaders.is_empty() && !sources.is_empty(),
        "examples/guide must hold runnable examples, shader examples and sources"
    );
    sources.sort();
    (runs, shaders, sources)
}

fn rock_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("read example project dir")
            .map(|e| e.expect("dir entry").path())
            .collect();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                pending.push(entry);
            } else if entry.extension().and_then(|e| e.to_str()) == Some("rock") {
                out.push(entry);
            }
        }
    }
    out
}

fn roots_of(path: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).expect("read .roots");
    let roots: Vec<String> = text.split_whitespace().map(str::to_string).collect();
    assert!(
        roots.len() == 1 || roots.len() == 2,
        "{}: want one fullscreen root or a vertex+fragment pair, got {roots:?}",
        path.display()
    );
    roots
}

#[test]
fn guide_runnable_examples_print_their_expected_output() {
    let (runs, _, _) = inventory();
    let mut failures = Vec::new();
    for case in &runs {
        let scratch = tempfile::tempdir().expect("scratch dir");
        let target = case.path.to_string_lossy().into_owned();
        let out = invoke(scratch.path(), &["run", &target]);
        if !out.status.success() {
            failures.push(format!("{}: `rock run` failed:\n{}", case.name, text(&out)));
            continue;
        }
        if out.stdout != case.expected {
            failures.push(format!(
                "{}: stdout differs:\n--- expected ---\n{}\n--- actual ---\n{}",
                case.name,
                String::from_utf8_lossy(&case.expected),
                String::from_utf8_lossy(&out.stdout)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} runnable example(s) failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// Fenced ```rock blocks in the guide, in order. The fence content is the
/// lines between the fences plus one trailing newline.
fn guide_fences() -> Vec<String> {
    let guide = repo_root().join("docs").join("guide.md");
    let text = std::fs::read_to_string(&guide).expect("read docs/guide.md");
    let mut fences = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in text.lines() {
        if line.trim_end() == "```rock" {
            assert!(current.is_none(), "docs/guide.md nests a ```rock fence");
            current = Some(Vec::new());
        } else if line.trim_end() == "```" {
            if let Some(lines) = current.take() {
                fences.push(lines.join("\n") + "\n");
            }
        } else if let Some(lines) = current.as_mut() {
            lines.push(line);
        }
    }
    assert!(
        current.is_none(),
        "docs/guide.md leaves a ```rock fence open"
    );
    assert!(!fences.is_empty(), "docs/guide.md has no ```rock fences");
    fences
}

#[test]
fn guide_fences_match_example_files_byte_for_byte() {
    let (_, _, sources) = inventory();
    let bodies: Vec<(PathBuf, String)> = sources
        .iter()
        .map(|p| {
            (
                p.clone(),
                std::fs::read_to_string(p).expect("read example source"),
            )
        })
        .collect();
    for (path, body) in &bodies {
        assert!(
            body.ends_with('\n') && !body.ends_with("\n\n"),
            "{}: example files end with exactly one newline",
            path.display()
        );
    }
    let fences = guide_fences();
    let mut failures = Vec::new();
    for (index, fence) in fences.iter().enumerate() {
        if !bodies.iter().any(|(_, body)| body == fence) {
            let head: String = fence.lines().take(3).collect::<Vec<_>>().join("\n");
            failures.push(format!(
                "fence {} matches no file under examples/guide:\n{head}",
                index + 1
            ));
        }
    }
    for (path, body) in &bodies {
        if !fences.iter().any(|f| f == body) {
            failures.push(format!(
                "{} appears in no ```rock fence",
                path.strip_prefix(repo_root()).unwrap().display()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "guide/example drift:\n  {}",
        failures.join("\n  ")
    );
}

#[test]
fn guide_shader_examples_compile_to_glsl() {
    let (_, shaders, _) = inventory();
    let mut failures = Vec::new();
    for case in &shaders {
        let scratch = tempfile::tempdir().expect("scratch dir");
        let target = case.path.to_string_lossy().into_owned();
        let mut args = vec!["shader", target.as_str()];
        args.extend(case.roots.iter().map(String::as_str));
        let out = invoke(scratch.path(), &args);
        if !out.status.success() {
            failures.push(format!(
                "{}: `rock shader` failed:\n{}",
                case.name,
                text(&out)
            ));
            continue;
        }
        let glsl = String::from_utf8_lossy(&out.stdout);
        for marker in ["#version", "void main"] {
            if !glsl.contains(marker) {
                failures.push(format!("{}: GLSL lacks `{marker}`:\n{glsl}", case.name));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} shader example(s) failed GLSL:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

fn wgsl_workspace(
    src: &str,
) -> (
    bone_semantic::inference::Workspace,
    bone_semantic::prelude::ModuleId,
) {
    let mut modules: Vec<skull_parser::Ast> = rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            skull_parser::parse_module_clean(
                f.contents,
                ustr::Ustr::from(format!("<stdlib:{}>", f.name).as_str()),
            )
            .expect("stdlib parse")
        })
        .collect();
    let mut module = skull_parser::parse_module_clean_with_dialect(
        src,
        ustr::Ustr::from("<test>"),
        skull_parser::Dialect::V1,
    )
    .expect("shader example parse");
    module.package = Some(skull_parser::PackageTag {
        id: 0,
        self_name: "guide_fixture".to_string(),
        identity_storage: Default::default(),
    });
    modules.push(module);
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic pass");
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("example module");
    (ws, mid)
}

fn wgsl_root_key(
    ws: &bone_semantic::inference::Workspace,
    mid: bone_semantic::prelude::ModuleId,
    name: &str,
) -> String {
    if ws.functions_by_name.contains_key(&ustr::Ustr::from(name)) {
        return name.to_string();
    }
    ws.module(mid)
        .functions
        .iter()
        .map(|(_, f)| f.name.as_str())
        .find(|key| key.rsplit("::").next() == Some(name))
        .unwrap_or_else(|| panic!("root `{name}` is not defined in the shader example"))
        .to_string()
}

#[test]
fn guide_shader_examples_compile_to_wgsl() {
    use skull_codegen_wgsl::{WgslAbiPlan, emit_explicit_shader, emit_fullscreen_shader};
    let (_, shaders, _) = inventory();
    let mut failures = Vec::new();
    for case in &shaders {
        let src = std::fs::read_to_string(&case.path).expect("read shader example");
        let (ws, mid) = wgsl_workspace(&src);
        let keys: Vec<String> = case
            .roots
            .iter()
            .map(|r| wgsl_root_key(&ws, mid, r))
            .collect();
        let emitted = (|| -> Result<String, skull_parser::CompilerError> {
            if keys.len() == 1 {
                let fref = skull_codegen_shader::resolve_root(&ws, mid, &keys[0])?;
                let neutral = skull_codegen_wgsl::plan_fullscreen_artifact(&ws, fref)?;
                let abi =
                    WgslAbiPlan::wgsl(&neutral.interface, bone_semantic::prelude::Span::default())?;
                Ok(emit_fullscreen_shader(
                    &ws,
                    &neutral.ir,
                    &neutral.interface,
                    &abi,
                    bone_semantic::prelude::Span::default(),
                )?
                .source)
            } else {
                let glsl = skull_codegen_glsl::emit_shader_pipeline_with_dependencies(
                    &ws,
                    mid,
                    &keys[0],
                    &keys[1],
                    skull_codegen_glsl::GlslProfile::Core330,
                )?;
                let abi = WgslAbiPlan::explicit(
                    &glsl.interface,
                    "vertex_main",
                    "fragment_main",
                    bone_semantic::prelude::Span::default(),
                )?;
                Ok(emit_explicit_shader(
                    &ws,
                    &glsl.ir,
                    &glsl.interface,
                    &abi,
                    bone_semantic::prelude::Span::default(),
                )?
                .source)
            }
        })();
        match emitted {
            Ok(wgsl) if wgsl.contains("@fragment") => {}
            Ok(wgsl) => failures.push(format!("{}: WGSL lacks `@fragment`:\n{wgsl}", case.name)),
            Err(e) => failures.push(format!("{}: WGSL emit failed: {e:?}", case.name)),
        }
    }
    assert!(
        failures.is_empty(),
        "{} shader example(s) failed WGSL:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
