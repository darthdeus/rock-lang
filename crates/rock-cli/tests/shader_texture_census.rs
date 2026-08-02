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
fn census_over(projects: &[&str]) -> Vec<(String, String, &'static str)> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::env::set_current_dir(&root).expect("chdir to repo root");

    let mut uniq: std::collections::BTreeMap<(String, String), &'static str> =
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
                em.texture_slots.iter().map(|u| u.as_str().to_string()).collect();
            for name in declared_samplers(&em.source) {
                let kind = if slots.contains(&name) { "slot" } else { "bare" };
                uniq.insert((pair.key.to_string(), name), kind);
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
    let grouped = got.iter().filter(|(_, _, k)| *k == "slot").count();
    println!(
        "examined {} distinct (program, sampler) pair(s): {grouped} in a @textures group, {} BARE",
        got.len(),
        got.len() - grouped
    );

    // The EXACT set, not a count. `programs > 0` passed with `logo/` omitted
    // and passed again while every `ISampler2D` was being dropped -- a census
    // is only as strong as the space it measured, so the space is the
    // assertion. Both defects fail here, and both are mutation-controlled below.
    let want: Vec<(String, String, &'static str)> = EXPECTED
        .iter()
        .map(|(p, s, k)| (p.to_string(), s.to_string(), *k))
        .collect();
    assert_eq!(
        got, want,
        "the sampler surface moved. A NEW row is fine -- add it. A row that \
         VANISHED means the census stopped seeing something it used to."
    );
}

/// Every `(program, sampler, grouped|bare)` on the registered surface, sorted.
/// Deduped across projects: the engine set re-registers per project, and the
/// binding contract is per PROGRAM.
const EXPECTED: &[(&str, &str, &str)] = &[
    ("blit_frag", "input_tex", "bare"),
    ("bloom_blur_frag", "input_tex", "bare"),
    ("bloom_down_frag", "src_tex", "bare"),
    ("bloom_prefilter_frag", "src_tex", "bare"),
    ("bloom_up_frag", "src_tex", "bare"),
    ("color_grade_frag", "scene_tex", "bare"),
    ("depth_visualize_frag", "uTexture", "bare"),
    ("entity_id_debug_frag", "id_buffer", "bare"),
    ("grass_vert+grass_frag", "mask_tex", "slot"),
    ("grass_vert+grass_frag", "shadowmap_tex", "slot"),
    ("grass_vert+grass_frag", "uTexture", "slot"),
    ("heat_shimmer_frag", "scene_tex", "bare"),
    ("nonfinite_frag", "src_tex", "bare"),
    ("outline_detect_vert+outline_detect_frag", "target_mask", "bare"),
    ("outline_detect_vert+outline_detect_frag", "visible_id", "bare"),
    ("outline_target_vert+outline_target_frag", "uTexture", "bare"),
    ("present_frag", "src_tex", "bare"),
    ("sprite_basic_vert+sprite_basic_frag", "uTexture", "bare"),
    ("sprite_batch_vert+sprite_batch_frag", "uGlowTex", "bare"),
    ("sprite_batch_vert+sprite_batch_frag", "uNormalTex", "bare"),
    ("sprite_batch_vert+sprite_batch_frag", "uShadowmapTex", "bare"),
    ("sprite_batch_vert+sprite_batch_frag", "uTexture", "bare"),
    ("text_vert+text_frag", "text", "bare"),
    ("tonemap_frag", "bloom_tex", "bare"),
    ("tonemap_frag", "scene_tex", "bare"),
    ("ui_vert+ui_dissolve_frag", "uTexture", "bare"),
];

/// Mutation control for the OMITTED-PROJECT defect: the first census never
/// looked in `logo/`, and `programs > 0` was happy. `ui_dissolve` is mag's, so
/// dropping mag must lose rows the full sweep has.
#[test]
#[ignore = "analysis artifact for the @textures port, not a gate"]
fn census_omitting_a_project_loses_rows() {
    let full = census_over(PROJECTS);
    let partial = census_over(&["bitgun", "games/testbed"]);
    assert!(
        partial.len() < full.len(),
        "dropping projects must shrink the surface, else the sweep is not \
         reading them (full={}, partial={})",
        full.len(),
        partial.len()
    );
    assert!(
        full.iter().any(|(p, _, _)| p.starts_with("ui_vert")),
        "the full sweep must see mag-only programs"
    );
}

/// Mutation control for the OMITTED-SAMPLER-KIND defect: matching on the first
/// token drops `isampler2D` (and any precision-qualified ES declaration). Those
/// are outline_detect's two and entity_id_debug's one.
#[test]
#[ignore = "analysis artifact for the @textures port, not a gate"]
fn a_first_token_type_match_would_drop_the_integer_samplers() {
    let src = "uniform isampler2D visible_id;\nuniform highp sampler2D scene_tex;\n";
    assert_eq!(declared_samplers(src), vec!["visible_id", "scene_tex"]);

    // The old, broken form, kept HERE so its blindness is demonstrated rather
    // than described.
    let first_token_only: Vec<String> = src
        .lines()
        .filter_map(|l| l.trim().strip_prefix("uniform "))
        .filter_map(|r| r.split_once(char::is_whitespace))
        .filter(|(ty, _)| ty.contains("sampler"))
        .map(|(_, n)| n.trim().trim_end_matches(';').to_string())
        .collect();
    assert_eq!(
        first_token_only,
        vec!["visible_id"],
        "the old form must miss the qualified declaration -- if it does not, \
         this control no longer demonstrates the defect it exists for"
    );
}
