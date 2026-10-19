//! WGPU shader registration and projection tests.

use bone_incremental::session::{NsAliases, Session, SessionFile};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use crate::common::ws_with_stdlib;
use skull::mir_emit::{emit_full_in_for_renderer, emit_overlay as emit_overlay_result, RendererTarget};
use skull_codegen_shader::ConcreteStageInput;
use skull_mir::shader_ir::ShaderStage;
use std::sync::Arc;

const TYPED_PROJECTION_REFUSAL_SRC: &str = r#"
struct EntityIdTex { id_buffer: ISampler2D }

@shader(fragment)
fn entity_id_debug_frag(@textures t: EntityIdTex, tc: Vec2) -> Vec4 {
    let id = texture_id(t.id_buffer, tc)
    if id <= 0 { discard() }
    let x = int_to_float(id)
    return v(x, x, x, 1.0)
}

value Quad { aPos: Vec2, aTexCoord: Vec2 }
value Vary { position: Vec4, TexCoord: Vec2 }
struct OutlineTex { target_mask: ISampler2D, visible_id: ISampler2D }

@shader(vertex)
fn outline_detect_vert(@attr a: Quad) -> Vary {
    return Vary {
        position: v(a.aPos, 0.0, 1.0),
        TexCoord: a.aTexCoord,
    }
}

@shader(fragment)
fn outline_detect_frag(vary: Vary, @textures t: OutlineTex) -> Vec4 {
    let target = texture_id(t.target_mask, vary.TexCoord)
    let visible = texture_id(t.visible_id, vary.TexCoord)
    if target == visible { return v(0.0, 1.0, 0.0, 1.0) }
    return v(1.0, 0.0, 0.0, 1.0)
}

fn use_all() -> string {
    let entity = rock_shader(entity_id_debug_frag)
    let outline = rock_shader(outline_detect_vert, outline_detect_frag)
    return entity + outline
}
"#;

const NONFINITE_PROJECTION_SRC: &str = r#"
struct SrcTex { src_tex: Sampler2D }

@shader(fragment)
fn nonfinite_frag(@textures t: SrcTex, tc: Vec2) -> Vec4 {
    let c = texture(t.src_tex, tc)
    let bad = bvec3(
        isinf(c.r) || isnan(c.r),
        isinf(c.g) || isnan(c.g),
        isinf(c.b) || isnan(c.b),
    )
    if any(bad) { return v(to_vec3(bad), 1.0) }
    if any(greater_than(c.rgb.abs(), v(60000.0, 60000.0, 60000.0))) {
        return v(1.0, 1.0, 0.0, 1.0)
    }
    return v(0.0, 0.0, 0.0, 1.0)
}

fn use_all() -> string { return rock_shader(nonfinite_frag) }
"#;

fn test_mid(ws: &Workspace) -> ModuleId {
    *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("<test> module id")
}

const WGPU_FULLSCREEN_SRC: &str = r#"
value EngineU {
    uProjection: Mat4,
    uView: Mat4,
    uYBounds: Vec2,
    uScreenBounds: Vec2,
    uEngineTime: float,
}
struct MainTex { uTexture: Sampler2D }
value SpriteBasicVert {
    aPos: Vec2,
    aTexCoord: Vec2,
    aColor: Vec4,
}
value SpriteBasicVary {
    position: Vec4,
    TexCoord: Vec2,
    VertexColor: Vec4,
}
@shader(vertex)
fn sprite_basic_vert(@attr a: SpriteBasicVert, @engine e: EngineU) -> SpriteBasicVary {
    return SpriteBasicVary {
        position: e.uProjection * e.uView * v(a.aPos, 0.0, 1.0),
        TexCoord: a.aTexCoord,
        VertexColor: a.aColor,
    }
}
@shader(fragment)
fn sprite_basic_frag(vary: SpriteBasicVary, @textures t: MainTex) -> Vec4 {
    return texture(t.uTexture, vary.TexCoord) * vary.VertexColor
}
fn shader_channel(x: float) -> float { return x }
value SpriteBatchVert { aPos: Vec2 }
value SpriteBatchVary { position: Vec4, TexCoord: Vec2 }
struct SpriteBatchTex { uTexture: Sampler2D }
@shader(vertex)
fn sprite_batch_vert(@attr a: SpriteBatchVert) -> SpriteBatchVary {
    return SpriteBatchVary { position: v(a.aPos, 0.0, 1.0), TexCoord: a.aPos }
}
@shader(fragment)
fn sprite_batch_frag(vary: SpriteBatchVary, @textures t: SpriteBatchTex) -> Vec4 {
    return texture(t.uTexture, vary.TexCoord)
}
@shader(fragment)
fn sampler_value_frag(src: Sampler2D, tc: Vec2) -> Vec4 {
    return texture(src, tc)
}
value CustomVert { aPos: Vec2 }
value CustomVary { position: Vec4, TexCoord: Vec2 }
@shader(vertex)
fn custom_vert(@attr a: CustomVert) -> CustomVary {
    return CustomVary { position: v(a.aPos, 0.0, 1.0), TexCoord: a.aPos }
}
@shader(fragment)
fn custom_frag(_vary: CustomVary) -> Vec4 {
    return Vec4 { x: 1.0, y: 0.0, z: 1.0, w: 1.0 }
}
@shader(fragment)
fn blit_frag(tc: Vec2) -> Vec4 { return Vec4 { x: shader_channel(tc.x), y: 0.0, z: 1.0, w: 1.0 } }
@shader(fragment)
fn present_frag(tc: Vec2) -> Vec4 { return Vec4 { x: 0.0, y: tc.y, z: 1.0, w: 1.0 } }
@shader(fragment)
fn tonemap_frag(tc: Vec2) -> Vec4 { return Vec4 { x: 1.0, y: 1.0, z: tc.x, w: 1.0 } }

fn use_all() -> string {
    let a = rock_shader(blit_frag)
    let b = rock_shader(present_frag)
    let c = rock_shader(tonemap_frag)
    let d = rock_shader(sprite_basic_vert, sprite_basic_frag)
    let e = rock_shader(sprite_batch_vert, sprite_batch_frag)
    let f = rock_shader(sampler_value_frag)
    return rock_shader(custom_vert, custom_frag)
}
"#;

#[test]
fn wgpu_registration_follows_graph_owned_shader_pairs() {
    let ws = ws_with_stdlib(WGPU_FULLSCREEN_SRC);
    let mid = test_mid(&ws);
    let (emit, cache) = emit_full_in_for_renderer(&ws, mid, None, RendererTarget::Wgpu)
        .expect("selected artifacts emit");
    assert_eq!(
        emit.lua.matches("__rock_wgpu_shader_register(").count(),
        6,
        "the selected renderer admits six of the seven graph pairs"
    );
    assert!(emit.lua.contains("__rock_shader_register(\"sampler_value_frag\""),
        "GL keeps the raw compatibility registration for the omitted WGSL projection");
    let census = cache.wgpu_projection_census().expect("WGPU projection census");
    assert_eq!(census.examined, 7);
    assert_eq!(census.admitted, 6);
    assert_eq!(census.omitted, 1);
    assert_eq!(census.admitted + census.omitted, census.examined);
    assert_eq!(census.omissions.keys().map(String::as_str).collect::<Vec<_>>(), ["sampler_value_frag"]);
    for root in [
        "blit_frag",
        "present_frag",
        "tonemap_frag",
        "sprite_basic_vert+sprite_basic_frag",
        "sprite_batch_vert+sprite_batch_frag",
        "custom_vert+custom_frag",
    ] {
        assert!(
            emit.lua.contains(&format!("__rock_wgpu_shader_register(\"{root}\"")),
            "selected root {root} is missing from WGSL registration"
        );
    }
}

const CODEREF_WGPU_OVERLAY_SRC: &str = r#"
global CALLBACK: fn() -> int = callback
global SHADER_KEY: string = rock_shader(late_frag)

@nomangle
fn callback() -> int { return 1 }

@shader(fragment)
fn late_frag(tc: Vec2) -> Vec4 {
    return v(tc.x, 0.0, 1.0, 1.0)
}

@nomangle
fn run_callback() { print(CALLBACK()) }
"#;

const CODEREF_WGPU_OVERLAY_EDIT: &str = r#"
global CALLBACK: fn() -> int = callback
global SHADER_KEY: string = rock_shader(late_frag)

@nomangle
fn callback() -> int { return 2 }

@shader(fragment)
fn late_frag(tc: Vec2) -> Vec4 {
    return v(1.0, tc.y, 0.0, 1.0)
}

@nomangle
fn run_callback() { print(CALLBACK()) }
"#;

fn wgpu_overlay_session(source: &str) -> Session {
    let stdlib = rock_stdlib::stdlib_files()
        .iter()
        .map(|file| {
            let id = ustr::Ustr::from(format!("<stdlib:{}>", file.name).as_str());
            skull_parser::parse_module_clean(file.contents, id).expect("stdlib parse")
        })
        .collect();
    Session::new(stdlib, vec![SessionFile::legacy("main.rock", source)], &NsAliases::default())
        .expect("session")
}

#[test]
fn a_failed_wgpu_overlay_cannot_publish_a_staged_coderef() {
    let mut session = wgpu_overlay_session(CODEREF_WGPU_OVERLAY_SRC);
    let mid = skull::user_module_id(session.workspace()).expect("user module");
    let (full, mut cache) = emit_full_in_for_renderer(
        session.workspace(),
        mid,
        None,
        RendererTarget::Wgpu,
    )
    .expect("combined CodeRef/WGPU full emit");
    let escaping = full.code_artifact.entries.values().filter(|entry| entry.escaping).count();
    assert!(escaping > 0, "CodeArtifact escaping population must be nonzero");
    let census = cache.wgpu_projection_census().expect("WGPU projection census");
    assert!(census.examined > 0 && census.admitted > 0, "WGPU projection population must be nonzero: {census:?}");

    let update = session
        .update_file("main.rock", CODEREF_WGPU_OVERLAY_EDIT)
        .expect("compatible body edit");
    let overlay = emit_overlay_result(session.workspace(), &mut cache, &update.reinferred)
        .expect("combined CodeRef/WGPU overlay");
    assert!(overlay.lua.contains("__rock_code_slot_stage"));
    assert!(overlay.lua.contains("__rock_wgpu_shader_register("));
    assert!(
        overlay.lua.find("__rock_wgpu_shader_register(").unwrap()
            < overlay.lua.find("__rock_code_slot_publish").unwrap(),
        "WGPU registration must precede CodeRef publication"
    );

    let script = format!(
        "function __rock_shader_register(...) end\n\
         _G.FAIL_WGPU = false\n\
         _G.WGPU_PENDING_FAILED = false\n\
         function __rock_wgpu_shader_publish_pending()\n\
             return not _G.WGPU_PENDING_FAILED\n\
         end\n\
         function __rock_wgpu_shader_discard_pending()\n\
             _G.WGPU_PENDING_FAILED = false\n\
         end\n\
         function __rock_wgpu_shader_register(_, _, _, _)\n\
             if _G.FAIL_WGPU then _G.WGPU_PENDING_FAILED = true end\n\
         end\n\
         {}\n\
         run_callback()\n\
         _G.FAIL_WGPU = true\n\
         local ok = xpcall(function()\n{}end, debug.traceback)\n\
         if not ok then __rock_code_slot_abort() end\n\
         print(ok)\n\
         print(_G.__rock_code_slot_pending == nil)\n\
         run_callback()\n\
         _G.FAIL_WGPU = false\n\
         {}\n\
         print(_G.__rock_code_slot_pending == nil)\n\
         run_callback()\n",
        full.lua, overlay.lua, overlay.lua
    );
    let output = crate::common::run_luajit(&script, "coderef_wgpu_overlay");
    assert_eq!(output, "1\nfalse\ntrue\n1\ntrue\n2", "failed WGPU load must retain the old CodeRef, then the control publishes the edit");
}

const BLOOM_PROJECTION_SRC: &str = r#"
struct InputTex { input_tex: Sampler2D }
struct SrcTex { src_tex: Sampler2D }
value BloomDownU { gain: float }

@shader(fragment)
fn bloom_down_13(src: Sampler2D, tc: Vec2) -> Vec4 {
    return texture(src, tc)
}

@shader(fragment)
fn blit_frag(@textures t: InputTex, tc: Vec2) -> Vec4 {
    return texture(t.input_tex, tc)
}

@shader(fragment)
fn bloom_down_frag(@textures t: SrcTex, @uniform u: BloomDownU, tc: Vec2) -> Vec4 {
    return bloom_down_13(t.src_tex, tc) * u.gain
}

fn use_all() -> string {
    let a = rock_shader(blit_frag)
    let b = rock_shader(bloom_down_frag)
    return b
}
"#;

#[test]
fn bloom_sampler_helper_projection_is_admitted_from_the_graph() {
    let ws = ws_with_stdlib(BLOOM_PROJECTION_SRC);
    let mid = test_mid(&ws);
    let (emit, cache) = emit_full_in_for_renderer(&ws, mid, None, RendererTarget::Wgpu)
        .expect("selected artifacts emit");
    let census = cache.wgpu_projection_census().expect("WGPU projection census");
    assert_eq!(census.examined, 2, "the fixture must exercise both graph roots: {census:?}");
    assert_eq!(census.admitted, 2, "bloom sampler/helper projection was omitted: {census:?}");
    assert_eq!(census.omitted, 0, "a valid graph pair entered the omitted population: {census:?}");
    assert!(
        emit.lua.contains("__rock_wgpu_shader_register(\"bloom_down_frag\""),
        "the admitted bloom artifact never reached the registration output"
    );
}

#[test]
fn selected_wgpu_planning_reads_the_exact_graph_instance_and_concrete_plan() {
    let ws = ws_with_stdlib(WGPU_FULLSCREEN_SRC);
    let mid = test_mid(&ws);
    let (emit, cache) = emit_full_in_for_renderer(&ws, mid, None, RendererTarget::Wgpu)
        .expect("selected artifacts emit");
    assert_eq!(emit.lua.matches("__rock_wgpu_shader_register(").count(), 6);

    let blit = cache
        .shader_view()
        .roots
        .iter()
        .find(|instance| {
            ws.module(instance.func.module).functions[instance.func.func].name.as_str()
                == "blit_frag"
        })
        .expect("blit root");
    let program_view = cache
        .shader_view()
        .program_views
        .iter()
        .find(|view| view.program.frag == blit.func)
        .expect("blit program view");
    let stage = program_view
        .stages
        .iter()
        .find(|stage| stage.stage == ShaderStage::Fragment)
        .expect("blit fragment view");
    assert!(cache.graph().instances.contains(&stage.entry));
    assert_eq!(stage.retained.len(), stage.helpers.len() + 1);
    assert_eq!(stage.order.len(), stage.helpers.len());

    let entry_plan = program_view
        .concrete_plans
        .get(&stage.entry)
        .expect("the retained entry has a concrete body plan");
    assert_eq!(entry_plan.function_instance(), Some(&stage.entry));
    assert!(!entry_plan.signature.params.is_empty());
    assert!(!entry_plan.callees.is_empty());

    let interner = Arc::clone(cache.graph().interner());
    let scx = skull_mir::lower::ScalarCtx::new_values_first_class(&ws)
        .sharing(Arc::clone(&interner));
    assert!(Arc::ptr_eq(&interner, &scx.interner_handle()));
    let input = ConcreteStageInput {
        stage: stage.stage,
        entry: stage.entry.clone(),
        retained: stage.retained.iter().cloned().collect(),
        helpers: stage.helpers.iter().cloned().collect(),
        builtins: stage.builtins.iter().cloned().collect(),
        order: stage.order.clone(),
        dependencies: stage.dependencies.clone(),
        concrete_plans: Arc::clone(&program_view.concrete_plans),
    };
    let neutral = skull_codegen_shader::plan_fullscreen_fragment_from_concrete(
        &ws,
        &scx,
        &input,
        &skull_codegen_wgsl::WgslShaderPolicy,
    )
    .expect("the selected registration uses the graph-backed concrete plan");
    assert_eq!(neutral.entry.inst, stage.entry);
    assert!(Arc::ptr_eq(&neutral.ir.interner, &interner));
    assert_eq!(neutral.dependencies, stage.dependencies);
    let abi = skull_codegen_wgsl::WgslAbiPlan::wgsl(
        &neutral.interface,
        bone_semantic::prelude::Span::default(),
    )
    .expect("WGSL ABI");
    assert!(!abi.wire().expect("WGSL ABI wire").is_empty());
    assert!(!abi.fragment_outputs.is_empty());

    let mut missing = (*program_view.concrete_plans).clone();
    assert!(missing.remove(&stage.entry).is_some());
    let rejected = match skull_codegen_shader::plan_fullscreen_fragment_from_concrete(
        &ws,
        &scx,
        &ConcreteStageInput {
            concrete_plans: Arc::new(missing),
            ..input
        },
        &skull_codegen_wgsl::WgslShaderPolicy,
    ) {
        Ok(_) => panic!("a missing concrete plan must fail closed"),
        Err(error) => error,
    };
    assert!(rejected.message.contains("no concrete plan"), "{rejected}");
}

#[test]
fn typed_projection_census_admits_the_current_entity_and_outline_projections() {
    let ws = ws_with_stdlib(TYPED_PROJECTION_REFUSAL_SRC);
    let mid = test_mid(&ws);
    let (emit, cache) = emit_full_in_for_renderer(&ws, mid, None, RendererTarget::Wgpu)
        .expect("the typed refusal census must compile its GL controls");
    let census = cache.wgpu_projection_census().expect("WGPU projection census");
    println!("typed projection census: {census:?}");
    assert_eq!(census.examined, 2);
    assert_eq!(census.admitted, 2, "entity-id and outline projections must be admitted: {census:?}");
    assert_eq!(census.omitted, 0, "typed projection omissions remain: {census:?}");
    assert!(census.omissions.is_empty());
    assert!(emit.lua.contains("__rock_shader_register(\"entity_id_debug_frag\""));
    assert!(emit.lua.contains("__rock_shader_register(\"outline_detect_vert+outline_detect_frag\""));
    assert!(emit.lua.contains("__rock_wgpu_shader_register(\"entity_id_debug_frag\""));
    assert!(emit.lua.contains("__rock_wgpu_shader_register(\"outline_detect_vert+outline_detect_frag\""));
}

#[test]
fn selected_wgpu_entity_id_projection_is_admitted() {
    let ws = ws_with_stdlib(TYPED_PROJECTION_REFUSAL_SRC);
    let mid = test_mid(&ws);
    let (emit, cache) = emit_full_in_for_renderer(&ws, mid, None, RendererTarget::Wgpu)
        .expect("the entity-id fixture must reach projection census");
    let census = cache.wgpu_projection_census().expect("WGPU projection census");
    assert_eq!(census.examined, 2);
    assert_eq!(census.admitted, 2, "entity-id and its real outline consumer must be admitted: {census:?}");
    assert_eq!(census.omitted, 0, "typed projection omissions remain: {census:?}");
    assert!(emit.lua.contains("__rock_wgpu_shader_register(\"entity_id_debug_frag\""));
}

#[test]
fn nonfinite_projection_reaches_selected_wgpu() {
    let ws = ws_with_stdlib(NONFINITE_PROJECTION_SRC);
    let mid = test_mid(&ws);
    let (emit, cache) = emit_full_in_for_renderer(&ws, mid, None, RendererTarget::Wgpu)
        .expect("the nonfinite shader must have a GL control");
    let census = cache.wgpu_projection_census().expect("WGPU projection census");
    assert_eq!(census.examined, 1);
    assert_eq!(census.admitted, 1, "nonfinite shader remains omitted: {census:?}");
    assert_eq!(census.omitted, 0, "nonfinite shader projection failed: {census:?}");
    assert!(emit.lua.contains("__rock_wgpu_shader_register(\"nonfinite_frag\""));
}
