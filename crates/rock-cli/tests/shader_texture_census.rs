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
    ("bitgun::grass_vert+bitgun::grass_frag", "mask_tex", "slot 1"),
    ("bitgun::grass_vert+bitgun::grass_frag", "uShadowmapTex", "slot 2"),
    ("bitgun::grass_vert+bitgun::grass_frag", "uTexture", "slot 0"),
    ("bitgun::heat_shimmer_frag", "scene_tex", "slot 0"),
    ("bolt_vert+glyph_scorch_frag", "uGlowTex", "slot 2"),
    ("bolt_vert+glyph_scorch_frag", "uNormalTex", "slot 3"),
    ("bolt_vert+glyph_scorch_frag", "uShadowmapTex", "slot 1"),
    ("bolt_vert+glyph_scorch_frag", "uSurfaceTex", "slot 4"),
    ("bolt_vert+glyph_scorch_frag", "uTexture", "slot 0"),
    ("calibration_display_frag", "source", "slot 0"),
    ("calibration_probe_frag", "field", "slot 0"),
    ("calibration_vert+dungeon_actor_frag", "albedo", "slot 0"),
    ("calibration_vert+dungeon_actor_frag", "uMaterialTex", "slot 3"),
    ("calibration_vert+dungeon_actor_frag", "uNormalTex", "slot 1"),
    ("calibration_vert+dungeon_actor_frag", "uSurfaceTex", "slot 2"),
    ("calibration_vert+dungeon_floor_albedo", "uAuxiliaryTex", "slot 2"),
    ("calibration_vert+dungeon_floor_albedo", "uMaterialTex", "slot 3"),
    ("calibration_vert+dungeon_floor_albedo", "uNormalTex", "slot 1"),
    ("calibration_vert+dungeon_floor_albedo", "uTexture", "slot 0"),
    ("calibration_vert+dungeon_wall_albedo", "uAuxiliaryTex", "slot 3"),
    ("calibration_vert+dungeon_wall_albedo", "uMaterialTex", "slot 4"),
    ("calibration_vert+dungeon_wall_albedo", "uNormalTex", "slot 1"),
    ("calibration_vert+dungeon_wall_albedo", "uSurfaceTex", "slot 2"),
    ("calibration_vert+dungeon_wall_albedo", "uTexture", "slot 0"),
    ("calibration_vert+wall_geometry_probe", "uAuxiliaryTex", "slot 3"),
    ("calibration_vert+wall_geometry_probe", "uMaterialTex", "slot 4"),
    ("calibration_vert+wall_geometry_probe", "uNormalTex", "slot 1"),
    ("calibration_vert+wall_geometry_probe", "uSurfaceTex", "slot 2"),
    ("calibration_vert+wall_geometry_probe", "uTexture", "slot 0"),
    ("continuous_wall_probe_vert+dungeon_wall_albedo", "uAuxiliaryTex", "slot 3"),
    ("continuous_wall_probe_vert+dungeon_wall_albedo", "uMaterialTex", "slot 4"),
    ("continuous_wall_probe_vert+dungeon_wall_albedo", "uNormalTex", "slot 1"),
    ("continuous_wall_probe_vert+dungeon_wall_albedo", "uSurfaceTex", "slot 2"),
    ("continuous_wall_probe_vert+dungeon_wall_albedo", "uTexture", "slot 0"),
    ("decoration_owner_probe", "source", "slot 0"),
    ("dungeon_atmosphere_frag", "glow_tex", "slot 1"),
    ("dungeon_atmosphere_frag", "scene_tex", "slot 0"),
    ("dungeon_atmosphere_frag", "surface_tex", "slot 2"),
    ("dungeon_atmosphere_frag", "visible_surface", "slot 3"),
    ("dungeon_contact_probe_frag", "occupancy", "slot 0"),
    ("dungeon_gi_accumulation_frag", "emission", "slot 7"),
    ("dungeon_gi_accumulation_frag", "emitters", "slot 6"),
    ("dungeon_gi_accumulation_frag", "occupancy", "slot 5"),
    ("dungeon_gi_accumulation_frag", "response0", "slot 0"),
    ("dungeon_gi_accumulation_frag", "response1", "slot 1"),
    ("dungeon_gi_accumulation_frag", "response2", "slot 2"),
    ("dungeon_gi_accumulation_frag", "response3", "slot 3"),
    ("dungeon_gi_accumulation_frag", "sources", "slot 4"),
    ("dungeon_gi_albedo_frag", "albedo", "slot 0"),
    ("dungeon_gi_detail_frag", "material", "slot 3"),
    ("dungeon_gi_detail_frag", "occupancy", "slot 0"),
    ("dungeon_gi_detail_frag", "outgoing", "slot 2"),
    ("dungeon_gi_detail_frag", "slot_map", "slot 4"),
    ("dungeon_gi_detail_frag", "sources", "slot 1"),
    ("dungeon_gi_gather_frag", "material", "slot 3"),
    ("dungeon_gi_gather_frag", "occupancy", "slot 0"),
    ("dungeon_gi_gather_frag", "outgoing", "slot 2"),
    ("dungeon_gi_gather_frag", "slot_map", "slot 4"),
    ("dungeon_gi_gather_frag", "sources", "slot 1"),
    ("dungeon_gi_hemisphere_frag", "field", "slot 0"),
    ("dungeon_gi_probe_frag", "field", "slot 0"),
    ("dungeon_gi_seed_frag", "material", "slot 2"),
    ("dungeon_gi_seed_frag", "occupancy", "slot 0"),
    ("dungeon_gi_seed_frag", "slot_map", "slot 3"),
    ("dungeon_gi_seed_frag", "sources", "slot 1"),
    ("dungeon_gi_spec_probe_frag", "data", "slot 0"),
    ("dungeon_gi_spec_probe_frag", "emission", "slot 7"),
    ("dungeon_gi_spec_probe_frag", "material", "slot 2"),
    ("dungeon_gi_spec_probe_frag", "occupancy", "slot 1"),
    ("dungeon_gi_spec_probe_frag", "outgoing0", "slot 3"),
    ("dungeon_gi_spec_probe_frag", "outgoing1", "slot 4"),
    ("dungeon_gi_spec_probe_frag", "outgoing2", "slot 5"),
    ("dungeon_gi_spec_probe_frag", "outgoing3", "slot 6"),
    ("dungeon_gi_viewport_probe_frag", "accumulated", "slot 5"),
    ("dungeon_gi_viewport_probe_frag", "emission", "slot 7"),
    ("dungeon_gi_viewport_probe_frag", "retained0", "slot 0"),
    ("dungeon_gi_viewport_probe_frag", "retained1", "slot 1"),
    ("dungeon_gi_viewport_probe_frag", "retained2", "slot 2"),
    ("dungeon_gi_viewport_probe_frag", "retained3", "slot 3"),
    ("dungeon_gi_viewport_probe_frag", "slot_map", "slot 6"),
    ("dungeon_gi_viewport_probe_frag", "sources", "slot 4"),
    ("dungeon_gi_viewport_probe_frag", "tints", "slot 8"),
    ("dungeon_material_view_frag", "material_tex", "slot 0"),
    ("dungeon_material_view_frag", "normal_tex", "slot 2"),
    ("dungeon_material_view_frag", "surface_owner", "slot 3"),
    ("dungeon_material_view_frag", "visible", "slot 1"),
    ("dungeon_pool_copy_frag", "source", "slot 0"),
    ("dungeon_receiver_vert+dungeon_relight_frag", "buffered_light", "slot 7"),
    ("dungeon_receiver_vert+dungeon_relight_frag", "emission", "slot 14"),
    ("dungeon_receiver_vert+dungeon_relight_frag", "indirect", "slot 5"),
    ("dungeon_receiver_vert+dungeon_relight_frag", "material_channels", "slot 9"),
    ("dungeon_receiver_vert+dungeon_relight_frag", "material_normal", "slot 8"),
    ("dungeon_receiver_vert+dungeon_relight_frag", "moving_table", "slot 11"),
    ("dungeon_receiver_vert+dungeon_relight_frag", "occupancy", "slot 3"),
    ("dungeon_receiver_vert+dungeon_relight_frag", "outdoor", "slot 13"),
    ("dungeon_receiver_vert+dungeon_relight_frag", "probes", "slot 6"),
    ("dungeon_receiver_vert+dungeon_relight_frag", "scene_tex", "slot 0"),
    ("dungeon_receiver_vert+dungeon_relight_frag", "shadows", "slot 1"),
    ("dungeon_receiver_vert+dungeon_relight_frag", "sources", "slot 2"),
    ("dungeon_receiver_vert+dungeon_relight_frag", "static_table", "slot 12"),
    ("dungeon_receiver_vert+dungeon_relight_frag", "surface_owner", "slot 10"),
    ("dungeon_receiver_vert+dungeon_relight_frag", "visible", "slot 4"),
    ("dungeon_shadow_frag", "occupancy", "slot 0"),
    ("dungeon_shadow_frag", "slot_map", "slot 2"),
    ("dungeon_shadow_frag", "sources", "slot 1"),
    ("dungeon_source_position_frag", "sources", "slot 0"),
    ("dungeon_static_light_probe", "emission", "slot 4"),
    ("dungeon_static_light_probe", "local_table", "slot 2"),
    ("dungeon_static_light_probe", "occupancy", "slot 0"),
    ("dungeon_static_light_probe", "sources", "slot 1"),
    ("dungeon_static_light_probe", "tints", "slot 3"),
    ("film_horizon_probe", "emission", "slot 3"),
    ("film_horizon_probe", "local_table", "slot 2"),
    ("film_horizon_probe", "occupancy", "slot 0"),
    ("film_horizon_probe", "sources", "slot 1"),
    ("film_rim_probe", "material_channels", "slot 0"),
    ("film_rim_probe", "visible", "slot 1"),
    ("furnace_resolve_frag", "field", "slot 1"),
    ("furnace_resolve_frag", "occupancy", "slot 0"),
    ("gi_accumulation_edge_probe", "accumulated", "slot 5"),
    ("gi_accumulation_edge_probe", "emission", "slot 7"),
    ("gi_accumulation_edge_probe", "retained0", "slot 0"),
    ("gi_accumulation_edge_probe", "retained1", "slot 1"),
    ("gi_accumulation_edge_probe", "retained2", "slot 2"),
    ("gi_accumulation_edge_probe", "retained3", "slot 3"),
    ("gi_accumulation_edge_probe", "slot_map", "slot 6"),
    ("gi_accumulation_edge_probe", "sources", "slot 4"),
    ("gi_accumulation_edge_probe", "tints", "slot 8"),
    ("gi_accumulation_probe", "accumulated", "slot 5"),
    ("gi_accumulation_probe", "emission", "slot 7"),
    ("gi_accumulation_probe", "retained0", "slot 0"),
    ("gi_accumulation_probe", "retained1", "slot 1"),
    ("gi_accumulation_probe", "retained2", "slot 2"),
    ("gi_accumulation_probe", "retained3", "slot 3"),
    ("gi_accumulation_probe", "slot_map", "slot 6"),
    ("gi_accumulation_probe", "sources", "slot 4"),
    ("gi_accumulation_probe", "tints", "slot 8"),
    ("gi_cache_curve_frag", "field", "slot 3"),
    ("gi_cache_curve_frag", "material", "slot 2"),
    ("gi_cache_curve_frag", "occupancy", "slot 0"),
    ("gi_cache_curve_frag", "outgoing", "slot 1"),
    ("gi_exposure_frag", "source", "slot 0"),
    ("gi_flicker_probe_frag", "accumulated", "slot 0"),
    ("gi_injection_probe_frag", "occupancy", "slot 0"),
    ("gi_loss_gather_frag", "material", "slot 3"),
    ("gi_loss_gather_frag", "occupancy", "slot 0"),
    ("gi_loss_gather_frag", "outgoing", "slot 2"),
    ("gi_loss_gather_frag", "slot_map", "slot 4"),
    ("gi_loss_gather_frag", "sources", "slot 1"),
    ("gi_loss_receiver_frag", "field", "slot 2"),
    ("gi_loss_receiver_frag", "occupancy", "slot 1"),
    ("gi_loss_receiver_frag", "visible", "slot 0"),
    ("light_slot_outgoing_probe_frag", "outgoing", "slot 0"),
    ("light_slot_probe_frag", "atlas", "slot 0"),
    ("light_slot_probe_frag", "occupancy", "slot 1"),
    ("light_slot_probe_frag", "sources", "slot 2"),
    ("light_slot_response_probe_frag", "response", "slot 0"),
    ("linear_sampler_probe_frag", "source", "slot 0"),
    ("logo::glyph_window", "core_tex", "slot 0"),
    ("logo::glyph_window", "mask_tex", "slot 1"),
    ("logo::grain_frag", "scene_tex", "slot 0"),
    ("material_checker_floor_vert+dungeon_floor_albedo", "uAuxiliaryTex", "slot 2"),
    ("material_checker_floor_vert+dungeon_floor_albedo", "uMaterialTex", "slot 3"),
    ("material_checker_floor_vert+dungeon_floor_albedo", "uNormalTex", "slot 1"),
    ("material_checker_floor_vert+dungeon_floor_albedo", "uTexture", "slot 0"),
    ("material_mip_probe", "source", "slot 0"),
    ("material_source_probe", "emission", "slot 3"),
    ("material_source_probe", "local_table", "slot 2"),
    ("material_source_probe", "occupancy", "slot 0"),
    ("material_source_probe", "sources", "slot 1"),
    ("menu_logo_reflection_probe_frag", "scene", "slot 0"),
    ("menu_logo_reflection_probe_sweep_frag", "probe", "slot 1"),
    ("menu_logo_reflection_probe_sweep_frag", "scene", "slot 0"),
    ("menu_logo_reflection_radiance_frag", "probe", "slot 1"),
    ("menu_logo_reflection_radiance_frag", "text_art", "slot 0"),
    ("menu_logo_reflection_strip_sample_frag", "probe", "slot 0"),
    ("menu_logo_reflection_wgpu_frag", "scene", "slot 1"),
    ("menu_logo_reflection_wgpu_frag", "text_art", "slot 0"),
    ("menu_logo_reflection_wgpu_radiance_frag", "scene", "slot 1"),
    ("menu_logo_reflection_wgpu_radiance_frag", "text_art", "slot 0"),
    ("menu_logo_relief_frag", "probe", "slot 1"),
    ("menu_logo_relief_frag", "text_art", "slot 0"),
    ("menu_water_source_frag", "scene_tex", "slot 1"),
    ("menu_water_source_frag", "ui_tex", "slot 0"),
    ("neutral_surface_probe_frag", "source", "slot 0"),
    ("outdoor_seam_probe", "source", "slot 0"),
    ("probe_furnace_fill_frag", "material", "slot 1"),
    ("probe_furnace_fill_frag", "occupancy", "slot 0"),
    ("probe_furnace_gather_frag", "material", "slot 1"),
    ("probe_furnace_gather_frag", "occupancy", "slot 0"),
    ("probe_furnace_gather_frag", "outgoing0", "slot 2"),
    ("probe_furnace_gather_frag", "outgoing1", "slot 3"),
    ("probe_furnace_gather_frag", "outgoing2", "slot 4"),
    ("probe_furnace_gather_frag", "outgoing3", "slot 5"),
    ("prop_pbr_transform_probe", "albedo", "slot 0"),
    ("prop_pbr_transform_probe", "uNormalTex", "slot 1"),
    ("prop_pbr_transform_probe", "uSurfaceTex", "slot 2"),
    ("prop_water_vert+water_frag", "uMaterialTex", "slot 3"),
    ("prop_water_vert+water_frag", "uNormalTex", "slot 1"),
    ("prop_water_vert+water_frag", "uSurfaceTex", "slot 2"),
    ("prop_water_vert+water_frag", "uTexture", "slot 0"),
    ("seamless_ao_domain_probe", "channels", "slot 2"),
    ("seamless_ao_domain_probe", "owner", "slot 1"),
    ("seamless_ao_domain_probe", "visible", "slot 0"),
    ("stone::blit_frag", "input_tex", "slot 0"),
    ("stone::bloom_blur_frag", "input_tex", "slot 0"),
    ("stone::bloom_down_frag", "src_tex", "slot 0"),
    ("stone::bloom_prefilter_frag", "src_tex", "slot 0"),
    ("stone::bloom_up_frag", "src_tex", "slot 0"),
    ("stone::color_grade_frag", "scene_tex", "slot 0"),
    ("stone::depth_visualize_frag", "depth_tex", "slot 0"),
    ("stone::entity_id_debug_frag", "id_buffer", "slot 0"),
    ("stone::nonfinite_frag", "src_tex", "slot 0"),
    ("stone::outline_detect_vert+stone::outline_detect_frag", "target_mask", "slot 0"),
    ("stone::outline_detect_vert+stone::outline_detect_frag", "visible_id", "slot 1"),
    ("stone::outline_target_vert+stone::outline_target_frag", "uTexture", "slot 0"),
    ("stone::present_frag", "src_tex", "slot 0"),
    ("stone::sprite_basic_vert+stone::sprite_basic_frag", "uTexture", "slot 0"),
    ("stone::sprite_batch_vert+dungeon_actor_frag", "albedo", "slot 0"),
    ("stone::sprite_batch_vert+dungeon_actor_frag", "uMaterialTex", "slot 3"),
    ("stone::sprite_batch_vert+dungeon_actor_frag", "uNormalTex", "slot 1"),
    ("stone::sprite_batch_vert+dungeon_actor_frag", "uSurfaceTex", "slot 2"),
    ("stone::sprite_batch_vert+dungeon_floor_albedo", "uAuxiliaryTex", "slot 2"),
    ("stone::sprite_batch_vert+dungeon_floor_albedo", "uMaterialTex", "slot 3"),
    ("stone::sprite_batch_vert+dungeon_floor_albedo", "uNormalTex", "slot 1"),
    ("stone::sprite_batch_vert+dungeon_floor_albedo", "uTexture", "slot 0"),
    ("stone::sprite_batch_vert+dungeon_wall_albedo", "uAuxiliaryTex", "slot 3"),
    ("stone::sprite_batch_vert+dungeon_wall_albedo", "uMaterialTex", "slot 4"),
    ("stone::sprite_batch_vert+dungeon_wall_albedo", "uNormalTex", "slot 1"),
    ("stone::sprite_batch_vert+dungeon_wall_albedo", "uSurfaceTex", "slot 2"),
    ("stone::sprite_batch_vert+dungeon_wall_albedo", "uTexture", "slot 0"),
    ("stone::sprite_batch_vert+mark_pbr_frag", "albedo", "slot 0"),
    ("stone::sprite_batch_vert+mark_pbr_frag", "uSurfaceTex", "slot 1"),
    ("stone::sprite_batch_vert+prop_emissive_frag", "uGlowTex", "slot 2"),
    ("stone::sprite_batch_vert+prop_emissive_frag", "uNormalTex", "slot 3"),
    ("stone::sprite_batch_vert+prop_emissive_frag", "uShadowmapTex", "slot 1"),
    ("stone::sprite_batch_vert+prop_emissive_frag", "uSurfaceTex", "slot 4"),
    ("stone::sprite_batch_vert+prop_emissive_frag", "uTexture", "slot 0"),
    ("stone::sprite_batch_vert+prop_flame_frag", "source", "slot 0"),
    ("stone::sprite_batch_vert+prop_pbr_frag", "albedo", "slot 0"),
    ("stone::sprite_batch_vert+prop_pbr_frag", "uNormalTex", "slot 1"),
    ("stone::sprite_batch_vert+prop_pbr_frag", "uSurfaceTex", "slot 2"),
    ("stone::sprite_batch_vert+stone::mtsdf_text_frag", "uGlowTex", "slot 2"),
    ("stone::sprite_batch_vert+stone::mtsdf_text_frag", "uNormalTex", "slot 3"),
    ("stone::sprite_batch_vert+stone::mtsdf_text_frag", "uShadowmapTex", "slot 1"),
    ("stone::sprite_batch_vert+stone::mtsdf_text_frag", "uSurfaceTex", "slot 4"),
    ("stone::sprite_batch_vert+stone::mtsdf_text_frag", "uTexture", "slot 0"),
    ("stone::sprite_batch_vert+stone::sprite_batch_frag", "uGlowTex", "slot 2"),
    ("stone::sprite_batch_vert+stone::sprite_batch_frag", "uNormalTex", "slot 3"),
    ("stone::sprite_batch_vert+stone::sprite_batch_frag", "uShadowmapTex", "slot 1"),
    ("stone::sprite_batch_vert+stone::sprite_batch_frag", "uSurfaceTex", "slot 4"),
    ("stone::sprite_batch_vert+stone::sprite_batch_frag", "uTexture", "slot 0"),
    ("stone::sprite_batch_vert+stone_material_frag", "uNormalTex", "slot 1"),
    ("stone::sprite_batch_vert+stone_material_frag", "uSurfaceTex", "slot 2"),
    ("stone::sprite_batch_vert+stone_material_frag", "uTexture", "slot 0"),
    ("stone::sprite_batch_vert+tree_canopy_frag", "uGlowTex", "slot 2"),
    ("stone::sprite_batch_vert+tree_canopy_frag", "uNormalTex", "slot 3"),
    ("stone::sprite_batch_vert+tree_canopy_frag", "uShadowmapTex", "slot 1"),
    ("stone::sprite_batch_vert+tree_canopy_frag", "uSurfaceTex", "slot 4"),
    ("stone::sprite_batch_vert+tree_canopy_frag", "uTexture", "slot 0"),
    ("stone::sprite_batch_vert+wall_frag", "uGlowTex", "slot 2"),
    ("stone::sprite_batch_vert+wall_frag", "uNormalTex", "slot 3"),
    ("stone::sprite_batch_vert+wall_frag", "uShadowmapTex", "slot 1"),
    ("stone::sprite_batch_vert+wall_frag", "uSurfaceTex", "slot 4"),
    ("stone::sprite_batch_vert+wall_frag", "uTexture", "slot 0"),
    ("stone::sprite_batch_vert+world_bar_scene_frag", "uNormalTex", "slot 1"),
    ("stone::sprite_batch_vert+world_bar_scene_frag", "uSurfaceTex", "slot 2"),
    ("stone::sprite_batch_vert+world_bar_scene_frag", "uTexture", "slot 0"),
    ("stone::text_art_resolve_frag", "base_tex", "slot 0"),
    ("stone::text_art_resolve_frag", "bloom_tex", "slot 2"),
    ("stone::text_art_resolve_frag", "coverage_tex", "slot 3"),
    ("stone::text_art_resolve_frag", "emission_tex", "slot 1"),
    ("stone::text_vert+stone::text_frag", "text", "slot 0"),
    ("stone::tonemap_frag", "bloom_tex", "slot 1"),
    ("stone::tonemap_frag", "scene_tex", "slot 0"),
    ("stone::ui_effect_composite_frag", "input_tex", "slot 0"),
    ("surface_normal_probe_frag", "source", "slot 0"),
    ("testbed::pass_pair_frag", "tex_a", "slot 0"),
    ("testbed::pass_pair_frag", "tex_b", "slot 1"),
    ("thin_shadow_probe", "source", "slot 0"),
    ("torch_wall_probe", "occupancy", "slot 1"),
    ("torch_wall_probe", "sources", "slot 2"),
    ("torch_wall_probe", "tints", "slot 3"),
    ("torch_wall_probe", "visible", "slot 0"),
    ("ui_param_vert+ui_icon_palette_frag", "uTexture", "slot 0"),
    ("ui_vert+menu_logo_reflection_motion_emission_frag", "uTexture", "slot 0"),
    ("ui_vert+menu_water_output_reflection_frag", "uTexture", "slot 0"),
    ("ui_vert+minimap_architecture_frag", "uTexture", "slot 0"),
    ("ui_vert+ui_dissolve_frag", "uTexture", "slot 0"),
    ("water_vert+water_frag", "uMaterialTex", "slot 3"),
    ("water_vert+water_frag", "uNormalTex", "slot 1"),
    ("water_vert+water_frag", "uSurfaceTex", "slot 2"),
    ("water_vert+water_frag", "uTexture", "slot 0"),
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
