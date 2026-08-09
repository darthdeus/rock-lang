//! Phase-4 GLSL corpus oracle.
//!
//! This is the source-side census before changing production ownership: every
//! registered program is emitted for both target profiles, and its GLSL,
//! logical interface and physical ABI are recorded per project and shader key.
//! The real-driver compile/link gate remains `shader-validate`.

use rock_cli::glsl_test::validate_workspace;
use skull_codegen_glsl::GlslProfile;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const PROJECTS: &[&str] = &[
    "anvil",
    "benches/pixel_upload",
    "bitgun",
    "demos/decorator_systems",
    "demos/hot_reload_test",
    "demos/imgui",
    "demos/outline_debug",
    "demos/quit_from_late_draw",
    "demos/silent_errors",
    "demos/testbed",
    "demos/ui-proto",
    "demos/wisps",
    "games/bopt",
    "games/chess",
    "games/sample-game-b",
    "games/drawbench",
    "games/fm",
    "games/sample-game",
    "games/little-knights",
    "games/match3",
    "games/pixel-spirit",
    "games/spark",
    "games/survivors",
    "games/testbed",
    "games/vfxlab",
    "games/void",
    "logo",
    "mag",
];

const PROFILES: &[(&str, GlslProfile)] = &[
    ("330core", GlslProfile::Core330),
    ("300es", GlslProfile::Es300),
];

const EXPECTED_PROFILE_ROWS: &[(&str, usize)] = &[
    ("anvil", 36),
    ("benches/pixel_upload", 36),
    ("bitgun", 54),
    ("demos/decorator_systems", 36),
    ("demos/hot_reload_test", 36),
    ("demos/imgui", 36),
    ("demos/outline_debug", 36),
    ("demos/quit_from_late_draw", 36),
    ("demos/silent_errors", 36),
    ("demos/testbed", 36),
    ("demos/ui-proto", 36),
    ("demos/wisps", 36),
    ("games/bopt", 38),
    ("games/chess", 36),
    ("games/sample-game-b", 36),
    ("games/drawbench", 36),
    ("games/fm", 0),
    ("games/sample-game", 36),
    ("games/little-knights", 38),
    ("games/match3", 36),
    ("games/pixel-spirit", 66),
    ("games/spark", 36),
    ("games/survivors", 36),
    ("games/testbed", 44),
    ("games/vfxlab", 36),
    ("games/void", 36),
    ("logo", 42),
    ("mag", 46),
];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RowKey {
    project: String,
    shader: String,
    profile: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ShaderRow {
    key: RowKey,
    stage_count: usize,
    glsl_fingerprint: u64,
    interface_fingerprint: u64,
    uniform_fingerprint: u64,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn discovered_bin_projects(root: &Path) -> BTreeSet<String> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| {
            !matches!(
                entry.file_name().to_str(),
                Some(".git") | Some("target") | Some("node_modules") | Some("tmp")
            )
        })
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name() == "stick.toml")
        .filter_map(|entry| {
            let dir = entry.path().parent()?;
            rock_host::stick::StickConfig::is_bin_at(dir).then(|| {
                dir.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
        })
        .collect()
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        h ^= *byte as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

fn fingerprint_debug<T: std::fmt::Debug>(value: &T) -> u64 {
    fnv1a(format!("{value:?}").as_bytes())
}

fn uniform_fingerprint(abi: &skull_codegen_glsl::ShaderAbiPlan) -> u64 {
    let mut bytes = Vec::new();
    for (binding, block) in &abi.uniform_blocks {
        bytes.extend_from_slice(&binding.to_le_bytes());
        bytes.extend_from_slice(&block.fingerprint().to_le_bytes());
    }
    for (binding, block) in &abi.uniform_array_blocks {
        bytes.extend_from_slice(&binding.to_le_bytes());
        bytes.extend_from_slice(&block.fingerprint().to_le_bytes());
    }
    fnv1a(&bytes)
}

fn source_stages(source: &str) -> usize {
    let vertex = source.matches("#ifdef VERTEX").count();
    let fragment = source.matches("#ifdef PIXEL").count();
    assert_eq!(
        vertex, 1,
        "every emitted shader must have one VERTEX section"
    );
    assert_eq!(
        fragment, 1,
        "every emitted shader must have one PIXEL section"
    );
    vertex + fragment
}

fn snapshot() -> Vec<ShaderRow> {
    let root = repo_root();
    let discovered = discovered_bin_projects(&root);
    assert_eq!(
        discovered,
        PROJECTS.iter().copied().map(str::to_string).collect(),
        "the explicit shader corpus list must partition the current bin corpus"
    );
    let mut rows = Vec::new();

    for project in PROJECTS {
        let path = root.join(project);
        assert!(
            path.join("stick.toml").is_file(),
            "not a corpus project: {project}"
        );
        let ws = validate_workspace(path.to_str().unwrap()).expect("workspace");
        let pairs = skull_mir::lower::collect_shader_pairs(&ws).expect("shader pairs");
        for pair in pairs {
            let fragment = ws.module(pair.frag.module).functions[pair.frag.func].name;
            for (profile_name, profile) in PROFILES {
                let emission = match pair.vert {
                    Some(vertex) => {
                        let vertex_name = ws.module(vertex.module).functions[vertex.func].name;
                        skull_codegen_glsl::emit_shader_pipeline_with_dependencies(
                            &ws,
                            vertex.module,
                            vertex_name.as_str(),
                            fragment.as_str(),
                            *profile,
                        )
                    }
                    None => skull_codegen_glsl::emit_shader_entry_with_dependencies(
                        &ws,
                        pair.frag.module,
                        fragment.as_str(),
                        *profile,
                    ),
                }
                .unwrap_or_else(|e| {
                    panic!("{project}: `{}` [{profile_name}] failed: {e}", pair.key)
                });

                rows.push(ShaderRow {
                    key: RowKey {
                        project: (*project).to_string(),
                        shader: pair.key.clone(),
                        profile: *profile_name,
                    },
                    stage_count: source_stages(&emission.source),
                    glsl_fingerprint: fnv1a(emission.source.as_bytes()),
                    interface_fingerprint: fingerprint_debug(&emission.interface),
                    uniform_fingerprint: uniform_fingerprint(&emission.abi),
                });
            }
        }
    }

    rows.sort_by(|a, b| a.key.cmp(&b.key));
    rows
}

fn changed_rows(before: &[ShaderRow], after: &[ShaderRow]) -> Vec<RowKey> {
    assert_eq!(before.len(), after.len(), "mutation comparison lost a row");
    before
        .iter()
        .zip(after)
        .filter(|(a, b)| *a != *b)
        .map(|(a, _)| a.key.clone())
        .collect()
}

#[test]
#[ignore = "real 28-project Phase-4 corpus oracle"]
fn shader_corpus_has_per_shader_profile_and_interface_evidence() {
    let rows = snapshot();
    assert_eq!(PROJECTS.len(), 28, "the shader corpus denominator moved");
    assert_eq!(rows.len(), 524 * PROFILES.len(), "profile rows moved");

    let shader_keys: BTreeSet<(&str, &str)> = rows
        .iter()
        .map(|r| (r.key.project.as_str(), r.key.shader.as_str()))
        .collect();
    assert_eq!(shader_keys.len(), 524, "shader population moved");
    assert_eq!(
        rows.iter().map(|r| r.stage_count).sum::<usize>(),
        524 * 2 * 2
    );
    assert_eq!(
        rows.iter().map(|r| r.key.profile).collect::<BTreeSet<_>>(),
        BTreeSet::from(["300es", "330core"]),
        "both GLSL profiles must be represented"
    );

    let mut observed_projects: BTreeMap<&str, usize> = PROJECTS
        .iter()
        .copied()
        .map(|project| (project, 0))
        .collect();
    for row in &rows {
        *observed_projects.get_mut(row.key.project.as_str()).unwrap() += 1;
    }
    let expected_projects: BTreeMap<&str, usize> = EXPECTED_PROFILE_ROWS.iter().copied().collect();
    assert_eq!(
        observed_projects, expected_projects,
        "per-project shader rows moved"
    );
    assert_eq!(
        observed_projects
            .values()
            .filter(|count| **count > 0)
            .count(),
        27
    );
    assert_eq!(
        observed_projects
            .values()
            .filter(|count| **count == 0)
            .count(),
        1
    );

    let mut by_shader: BTreeMap<(&str, &str), Vec<&ShaderRow>> = BTreeMap::new();
    for row in &rows {
        by_shader
            .entry((row.key.project.as_str(), row.key.shader.as_str()))
            .or_default()
            .push(row);
        assert_ne!(
            row.glsl_fingerprint, 0,
            "empty GLSL fingerprint: {:?}",
            row.key
        );
        assert_ne!(
            row.interface_fingerprint, 0,
            "empty interface fingerprint: {:?}",
            row.key
        );
        assert_ne!(
            row.uniform_fingerprint, 0,
            "empty uniform fingerprint: {:?}",
            row.key
        );
    }
    assert!(by_shader.values().all(|rows| rows.len() == PROFILES.len()));
    for rows in by_shader.values() {
        assert_eq!(rows[0].stage_count, 2);
        assert_eq!(rows[0].interface_fingerprint, rows[1].interface_fingerprint);
        assert_eq!(rows[0].uniform_fingerprint, rows[1].uniform_fingerprint);
        assert_ne!(rows[0].glsl_fingerprint, rows[1].glsl_fingerprint);
    }

    let mut mutated = rows.clone();
    let target = mutated
        .iter_mut()
        .find(|row| {
            row.key.project == "mag"
                && row.key.shader == "ui_vert+ui_dissolve_frag"
                && row.key.profile == "330core"
        })
        .expect("the mutation target must be in the real corpus");
    let expected = target.key.clone();
    target.glsl_fingerprint ^= 1;
    assert_eq!(changed_rows(&rows, &mutated), vec![expected]);
}
