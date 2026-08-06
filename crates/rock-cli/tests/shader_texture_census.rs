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

/// Walk `projects` and return every distinct `(program, sampler, slot|bare)`,
/// sorted. Deduped across projects, because the engine set re-registers per
/// project while the binding contract is per PROGRAM.
fn census_over(projects: &[&str]) -> Vec<(String, String, String)> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::env::set_current_dir(&root).expect("chdir to repo root");

    let mut uniq: std::collections::BTreeMap<(String, String), String> =
        std::collections::BTreeMap::new();

    for dir in projects {
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

            let slots: Vec<String> =
                em.abi.texture_units.iter().map(|u| u.as_str().to_string()).collect();
            for name in declared_samplers(&em.source) {
                let kind = match slots.iter().position(|slot_name| slot_name == &name) {
                    Some(slot) => format!("slot {slot}"),
                    None => "bare".to_string(),
                };
                let key = (pair.key.to_string(), name);
                if let Some(previous) = uniq.insert(key.clone(), kind.clone()) {
                    assert_eq!(
                        previous, kind,
                        "program `{}` sampler `{}` changed binding across projects",
                        key.0, key.1
                    );
                }
            }
        }
    }
    uniq.into_iter().map(|((p, s), k)| (p, s, k)).collect()
}

#[test]
#[ignore = "analysis artifact for the @textures port, not a gate"]
fn shader_texture_census() {
    let got = census_over(PROJECTS);
    println!("\n{:-<100}", "");
    for (prog, samp, kind) in &got {
        println!("{prog:<42} {samp:<22} {kind}");
    }
    println!("{:-<100}", "");
    let programs: std::collections::BTreeSet<&str> =
        got.iter().map(|(program, _, _)| program.as_str()).collect();
    let grouped = got.iter().filter(|(_, _, k)| k.starts_with("slot ")).count();
    println!(
        "examined {} program(s), {} distinct (program, sampler) pair(s): \
         {grouped} in a @textures group, {} BARE",
        programs.len(),
        got.len(),
        got.len() - grouped
    );

    // The exact set makes an omitted project, sampler kind, or slot-order change
    // fail instead of hiding behind a still-nonzero summary.
    let want: Vec<(String, String, String)> = EXPECTED
        .iter()
        .map(|(p, s, k)| (p.to_string(), s.to_string(), k.to_string()))
        .collect();
    assert_eq!(
        got, want,
        "the sampler surface moved. A NEW row is fine -- add it. A row that \
         VANISHED means the census stopped seeing something it used to."
    );
}

/// Every `(program, sampler, slot|bare)` on the registered surface, sorted.
/// Deduped across projects: the engine set re-registers per project, and the
/// binding contract is per PROGRAM.
const EXPECTED: &[(&str, &str, &str)] = &[
    ("blit_frag", "input_tex", "slot 0"),
    ("bloom_blur_frag", "input_tex", "slot 0"),
    ("bloom_down_frag", "src_tex", "slot 0"),
    ("bloom_prefilter_frag", "src_tex", "slot 0"),
    ("bloom_up_frag", "src_tex", "slot 0"),
    ("color_grade_frag", "scene_tex", "slot 0"),
    ("depth_visualize_frag", "depth_tex", "slot 0"),
    ("entity_id_debug_frag", "id_buffer", "slot 0"),
    ("glyph_window", "core_tex", "slot 0"),
    ("glyph_window", "mask_tex", "slot 1"),
    ("grain_frag", "scene_tex", "slot 0"),
    ("grass_vert+grass_frag", "mask_tex", "slot 1"),
    ("grass_vert+grass_frag", "uShadowmapTex", "slot 2"),
    ("grass_vert+grass_frag", "uTexture", "slot 0"),
    ("heat_shimmer_frag", "scene_tex", "slot 0"),
    ("nonfinite_frag", "src_tex", "slot 0"),
    ("outline_detect_vert+outline_detect_frag", "target_mask", "slot 0"),
    ("outline_detect_vert+outline_detect_frag", "visible_id", "slot 1"),
    ("outline_target_vert+outline_target_frag", "uTexture", "slot 0"),
    ("pass_pair_frag", "tex_a", "slot 0"),
    ("pass_pair_frag", "tex_b", "slot 1"),
    ("present_frag", "src_tex", "slot 0"),
    ("sprite_basic_vert+sprite_basic_frag", "uTexture", "slot 0"),
    ("sprite_batch_vert+sprite_batch_frag", "uGlowTex", "slot 2"),
    ("sprite_batch_vert+sprite_batch_frag", "uNormalTex", "slot 3"),
    ("sprite_batch_vert+sprite_batch_frag", "uShadowmapTex", "slot 1"),
    ("sprite_batch_vert+sprite_batch_frag", "uTexture", "slot 0"),
    ("text_vert+text_frag", "text", "slot 0"),
    ("tonemap_frag", "bloom_tex", "slot 1"),
    ("tonemap_frag", "scene_tex", "slot 0"),
    ("ui_vert+ui_dissolve_frag", "uTexture", "slot 0"),
];

/// Dropping a project with unique shader registrations must lose rows.
#[test]
#[ignore = "analysis artifact for the @textures port, not a gate"]
fn census_omitting_a_project_loses_rows() {
    let full = census_over(PROJECTS);
    let without_logo: Vec<&str> = PROJECTS.iter().copied().filter(|p| *p != "logo").collect();
    let partial = census_over(&without_logo);
    assert!(
        partial.len() < full.len(),
        "dropping projects must shrink the surface, else the sweep is not \
         reading them (full={}, partial={})",
        full.len(),
        partial.len()
    );
    assert!(
        full.iter().any(|(p, _, _)| p == "glyph_window"),
        "the full sweep must see logo's live glyph shader"
    );
}

/// Matching only a first-token `sampler2D` drops integer and precision-qualified
/// sampler declarations.
#[test]
#[ignore = "analysis artifact for the @textures port, not a gate"]
fn a_first_token_type_match_would_drop_the_integer_samplers() {
    let src = "uniform isampler2D visible_id;\nuniform highp sampler2D scene_tex;\n";
    assert_eq!(declared_samplers(src), vec!["visible_id", "scene_tex"]);

    // First-token matching cannot recognize another sampler type or a qualifier.
    let first_token_only: Vec<String> = src
        .lines()
        .filter_map(|l| l.trim().strip_prefix("uniform "))
        .filter_map(|r| r.split_once(char::is_whitespace))
        .filter(|(ty, _)| *ty == "sampler2D")
        .map(|(_, n)| n.trim().trim_end_matches(';').to_string())
        .collect();
    assert_eq!(
        first_token_only,
        Vec::<String>::new(),
        "first-token sampler2D matching must miss both declarations"
    );
}
