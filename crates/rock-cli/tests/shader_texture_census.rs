//! Entry-point-aware census of every sampler on every registered shader
//! PROGRAM, with its slot and how it is currently bound.
//!
//! Not a grep over `Sampler2D` params: that counts helper-fn params that are
//! not entry samplers, and it cannot see that one vertex entry serves several
//! programs (so its samplers appear once per program, which is what the binding
//! contract is scoped to). This walks `collect_shader_pairs` and reads the real
//! emission -- `ShaderEmission.texture_slots` for the compiler-assigned slots,
//! and the emitted GLSL's own `uniform sampler2D` declarations for the whole
//! set, so a bare sampler is exactly one the group does not claim.
//!
//! `#[ignore]`d: it is an analysis artifact for the `@textures` port, run on
//! demand rather than a gate.
//!
//!   cargo test -p rock-cli --test rock_cli_heavy -- --ignored --nocapture census

use rock_cli::glsl_test::validate_workspace;
use skull_codegen_glsl::GlslProfile;

const PROJECTS: &[&str] =
    &["mag", "bitgun", "logo", "games/testbed", "games/match3", "games/drawbench"];

/// Every sampler uniform, in declaration order. Matches on the WHOLE
/// declaration and takes the last token as the name, because the type is not
/// one token: `isampler2D` (outline_detect's id attachments) and a
/// precision-qualified `uniform highp sampler2D x;` on the ES profile both slip
/// past a first-token check, and a census that silently drops a sampler kind
/// reports a clean tree it never looked at.
fn declared_samplers(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in src.lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("uniform ") else { continue };
        let decl = rest.trim_end_matches(';').trim();
        // A block declaration (`uniform Blk { .. }`) is not a sampler.
        if decl.contains('{') || !decl.contains("sampler") {
            continue;
        }
        let Some(name) = decl.split_whitespace().last() else { continue };
        if !out.iter().any(|n: &String| n == name) {
            out.push(name.to_string());
        }
    }
    out
}

#[test]
#[ignore = "analysis artifact for the @textures port, not a gate"]
fn shader_texture_census() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::env::set_current_dir(&root).expect("chdir to repo root");

    let mut programs = 0usize;
    let mut grouped = 0usize;
    let mut bare = 0usize;
    let mut rows: Vec<String> = Vec::new();

    for dir in PROJECTS {
        let ws = validate_workspace(dir).expect("workspace");
        let pairs = skull_mir::lower::collect_shader_pairs(&ws).expect("pairs");
        for pair in &pairs {
            let fname = ws.module(pair.frag.module).functions[pair.frag.func].name;
            let em = match pair.vert {
                Some(vert) => {
                    let vname = ws.module(vert.module).functions[vert.func].name;
                    skull_codegen_glsl::emit_shader_pipeline_with_dependencies(
                        &ws, vert.module, vname.as_str(), fname.as_str(), GlslProfile::Core330,
                    )
                }
                None => skull_codegen_glsl::emit_shader_entry_with_dependencies(
                    &ws, pair.frag.module, fname.as_str(), GlslProfile::Core330,
                ),
            }
            .expect("emit");

            let all = declared_samplers(&em.source);
            if all.is_empty() {
                continue;
            }
            programs += 1;
            let slots: Vec<String> =
                em.texture_slots.iter().map(|u| u.as_str().to_string()).collect();
            for name in &all {
                match slots.iter().position(|s| s == name) {
                    Some(i) => {
                        grouped += 1;
                        rows.push(format!("{dir:<16} {:<34} {name:<22} slot {i}", pair.key));
                    }
                    None => {
                        bare += 1;
                        rows.push(format!(
                            "{dir:<16} {:<34} {name:<22} BARE (cpu unit)",
                            pair.key
                        ));
                    }
                }
            }
        }
    }

    println!("\n{:-<100}", "");
    for r in &rows {
        println!("{r}");
    }
    println!("{:-<100}", "");
    println!(
        "examined {programs} program(s) declaring samplers: {grouped} in a @textures group, \
         {bare} BARE",
    );
    assert!(programs > 0, "census examined nothing -- the walk is broken, not the tree clean");
}
