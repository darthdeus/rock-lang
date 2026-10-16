//! Ordinary WorldImage ownership and explicit restore contracts.

use std::path::Path;
use crate::stone_harness::{HeavyCommandExt, SpawnBudget};

// Cold MAG assets need scheduling margin in the concurrent native fixture lane.
const STATE_PHASE_TIMEOUT_SECS: &str = "180";

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
}

#[test]
fn mag_world_state_has_no_host_resources() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let output = crate::command::rock_bin()
        .args(["lint", "mag", "--lints=host_resource_in_game_memory"])
        .current_dir(workspace_root())
        .output_with_deadline()
        .expect("run MAG host-resource lint");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "MAG host-resource lint failed: {text}");
    assert!(text.contains("[LINT] mag: no findings"), "MAG still captures a host resource: {text}");
}

#[test]
fn bitgun_world_props_have_no_host_resources() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let output = crate::command::rock_bin()
        .args(["lint", "bitgun", "--lints=host_resource_in_game_memory"])
        .current_dir(workspace_root())
        .output_with_deadline()
        .expect("run Bitgun host-resource lint");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "Bitgun host-resource lint failed: {text}");
    assert!(text.contains("[LINT] bitgun: no findings"), "Bitgun still captures a host resource: {text}");
}

#[test]
fn bitgun_grass_autosave_is_owned_by_the_host_canvas() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let world = sim_state_names("bitgun", 3);
    assert!(!world.is_empty(), "the production checkpoint census must be populated");
    for name in ["BLOOD_CANVAS", "GRASS_AUTOSAVE_TIMER"] {
        assert!(!world.contains(name), "{name} must follow the rebuilt host canvas");
    }
}

/// Sim-state global names = the engine predicate's output, resolved to the
/// logical names used by dump-state rather than re-listed in Rust.
fn sim_state_names(game: &str, frames: u32) -> std::collections::HashSet<String> {
    let state = run_plain_dump(game, frames, &[("STONE_DUMP_WORLD_MEMBERS", "1")]);
    let names = state["__stone_diag"]["STONE_WORLD_MEMBERS"].as_str().expect("production world membership");
    names.lines().map(str::to_string).collect()
}

const MAG_HOST_PROJECTION_WORLD_EXCLUDED: &[&str] = &[
    "AGENT_ACCESS_ENABLED",
    "AGENT_SESSION_ID",
    "AGENT_GENERATION",
    "AGENT_REGISTERED",
    "AGENT_CONNECTION_SEEN",
    "AGENT_NEXT_RETRY_AT",
    "AGENT_QUEUE",
    "AGENT_METADATA_LEVEL",
    "AGENT_METADATA_VISIBLE",
    "AGENT_METADATA_FULLSCREEN",
    "AGENT_METADATA_ACCESS",
    "PENDING_CAPTURE",
    "AGENT_SESSION_ID_SERIAL",
    "AGENT_WINDOW_POS",
    "AGENT_WINDOW_SIZE",
    "AGENT_CHECKBOX_POS",
    "AGENT_FORGIVE_BUTTON_POS",
    "AGENT_RESURRECT_BUTTON_POS",
    "AGENT_RESTORE_BUTTON_POS",
    "AGENT_INVULNERABLE_BUTTON_POS",
    "AGENT_GOLD_BUTTON_POS",
    "AGENT_REAGENTS_BUTTON_POS",
    "AGENT_STOP_REQUESTED",
    "CAST_GLYPHS",
    "CAST_GLYPHS_WORLD",
    "CAST_GLYPH_CHILD_POLLER",
    "DUNGEON_MATERIAL_CHANNELS",
    "DUNGEON_MATERIAL_CHANNELS_KEY",
    "DUNGEON_MATERIAL_WALL_CHANNELS",
    "DUNGEON_MATERIAL_WALL_CHANNELS_KEY",
    "DUNGEON_WALL_FLOOR_STRIP",
    "DUNGEON_WALL_FLOOR_STRIP_KEY",
    "DUNGEON_MATERIAL_COLOR",
    "DUNGEON_MATERIAL_NORMAL",
    "DUNGEON_MATERIAL_REFLECTANCE_KEY",
    "DUNGEON_MATERIAL_COLOR_KEY",
    "DUNGEON_MATERIAL_NORMAL_KEY",
    "DUNGEON_MATERIAL_WALL_COLOR",
    "DUNGEON_MATERIAL_WALL_NORMAL",
    "DUNGEON_MATERIAL_WALL_COLOR_KEY",
    "DUNGEON_MATERIAL_WALL_NORMAL_KEY",
    "DUNGEON_MATERIAL_RETIRED",
    "DUNGEON_RELIGHT_OUTPUT",
    "DUNGEON_RELIGHT_CACHES",
    "DUNGEON_GI_MATERIAL_VERSION",
    "DUNGEON_SPRITE_ASSETS",
    "DUNGEON_SPRITE_DRAWS",
    "FLOOR_MATERIAL_VISUALS",
    "MAG_CHARACTER_RELIGHT_SURFACES",
    "MAG_PLAYER_VISUALS",
    "MAG_WORLD_VISUAL_ASSETS",
    "WORLD_EQUIPMENT_SURFACES",
];

const MAG_S3_PLATFORM_WORLD_EXCLUDED: &[&str] = &[
    "UI_PANEL_BG",
    "UI_TEXT",
    "UI_TEXT_BRIGHT",
    "UI_TITLE",
    "UI_TEXT_DIM",
    "UI_ACCENT",
    "UI_SELECTED",
    "PAUSE_DIM",
    "SKIN_BUTTON_SQUARE",
    "SKIN_BUTTON_VERT",
    "SKIN_BUTTON_LITTLE",
    "SKIN_BUTTON_LONG",
    "SKIN_BUTTON_ROUND",
    "SKIN_BUTTON_MEDIUM",
    "SKIN_MENU_FRAME",
    "SKIN_POPUP_WINDOW",
    "SKIN_MAIN_UI_BAR",
    "SKIN_BOSS_HP",
    "SKIN_ROUND_FRAME",
    "MAG_VISUAL_BANK",
    "MAG_VISUALS",
    "MAG_PLAYER_VISUALS",
    "MAG_CHARACTER_RELIGHT_SURFACES",
    "MAG_WORLD_VISUAL_ASSETS",
    "MAG_RESIDENT_VISUALS",
    "MAG_EDITOR_VISUAL_THUMBNAILS",
    "MAG_EDITOR_VISUAL_ATLAS",
    "MAG_RUN_VISUAL_CHARACTERS",
    "MAG_WALL_ARCHITECTURE",
    "MAG_WALL_ARCHITECTURE_SURFACES",
    "tile_architecture_atlas",
    "MAG_RUN_VISUAL_RESIDENCY_HANDLES",
    "MAG_RUN_VISUAL_RESIDENCY_CURSOR",
    "DUNGEON_MATERIAL_STUDY",
    "DUNGEON_MATERIAL_PBR",
    "DUNGEON_MATERIAL_ROUGHNESS",
    "DUNGEON_PROP_ROUGHNESS",
    "DUNGEON_MATERIAL_AUTHORED",
    "DUNGEON_MATERIAL_WORN",
    "DUNGEON_MATERIAL_METALLIC",
    "DUNGEON_MATERIAL_CHANNELS",
    "DUNGEON_MATERIAL_CHANNELS_KEY",
    "DUNGEON_MATERIAL_WALL_CHANNELS",
    "DUNGEON_MATERIAL_WALL_CHANNELS_KEY",
    "DUNGEON_FLOOR_REPEAT",
    "DUNGEON_WALL_REPEAT",
    "DUNGEON_MATERIAL_MINIFY",
    "DUNGEON_MATERIAL_GEOMETRIC",
    "DUNGEON_WALL_FLOOR_MATERIAL",
    "DUNGEON_WALL_FLOOR_STRIP",
    "DUNGEON_WALL_FLOOR_STRIP_KEY",
    "DUNGEON_MATERIAL_NORMAL_STRENGTH",
    "DUNGEON_MATERIAL_COLOR",
    "DUNGEON_MATERIAL_NORMAL",
    "DUNGEON_MATERIAL_REFLECTANCE_KEY",
    "DUNGEON_MATERIAL_COLOR_KEY",
    "DUNGEON_MATERIAL_NORMAL_KEY",
    "DUNGEON_MATERIAL_WALL_COLOR",
    "DUNGEON_MATERIAL_WALL_NORMAL",
    "DUNGEON_MATERIAL_WALL_COLOR_KEY",
    "DUNGEON_MATERIAL_WALL_NORMAL_KEY",
    "DUNGEON_MATERIAL_RESOURCES_READY",
    "DUNGEON_MATERIAL_UI",
    "DUNGEON_MATERIAL_UI_SIZED",
    "DUNGEON_MATERIAL_RETIRED",
    "MINIMAP_LEVEL",
    "MINIMAP_LEVEL_READY",
    "MINIMAP_WORLD_CARDS",
    "GAME_MINIMAP_PANEL",
    "GAME_MINIMAP_PANEL_RECT",
    "editor_minimap_sized",
    "editor_world_sized",
    "editor_world_focus_left",
    "BANK_PANEL",
    "COMBAT_REPORT_PANEL",
    "EQUIPMENT_ICONS",
    "ITEM_ASSETS",
    "GOLD_COIN_ASSET",
    "ITEM_ICONS",
    "BACKPACK_SATCHEL",
    "INVENTORY_PANEL",
    "JOURNAL_PANEL",
    "tile_floor_atlas",
    "tile_water_surface",
    "tile_water_normal",
    "tile_water_edge_stone",
    "tile_wall_atlas",
    "prop_door_closed",
    "prop_door_open",
    "prop_gate_l_closed",
    "prop_gate_l_open",
    "prop_gate_r_closed",
    "prop_gate_r_open",
    "DOOR_PROP_ASSETS",
    "MENU_LOGO_TEXT_ART",
    "MENU_LOGO_TEXT_STATE",
    "MENU_LOGO_REFLECTION_PROBE",
    "PAPERDOLL_PANEL",
    "PAPERDOLL_ART",
    "arrow_img",
    "ARROW_PBR",
    "PROP_ASSETS",
    "QUEST_LOG_PANEL",
    "QUEST_TRACKER_PANEL",
    "QUEST_GIVER_PANEL",
    "RUNEBOOK_PANEL",
    "MENU_WATER_SOURCE_CANVAS",
    "LOGO_RUST",
    "LOGO_SDL",
    "LOGO_OPENGL",
    "MENU_OUTPUT_TEXT_ART",
    "SHOP_PANEL",
    "SPELL_CELL_ART",
    "SPELL_ICON_ATLAS",
    "SPELL_PANEL",
    "TUTORIAL_GUIDE_PANEL",
    "CURSOR_EID_SLOT",
    "CURSOR_DEPTH_SLOT",
    "ghost_surface",
    "WORLD_DAMAGE_BLOOD_QUERY_SLOT",
    "WORLD_DAMAGE_BLOOD_QUERY_TARGETS",
    "WORLD_DAMAGE_BLOOD_QUERY_LAST_CANVAS",
];

#[test]
fn world_image_includes_effective_pad_state_only() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let names = sim_state_names("bitgun", 3);
    assert!(names.contains("pad_down"), "effective pad buttons must be replay state: {names:?}");
    assert!(names.contains("pad_axes"), "effective pad axes must be replay state: {names:?}");
    assert!(names.contains("ACTION_RUNTIME"), "action runtime must be replay state: {names:?}");
    assert!(!names.contains("physical_pad_down"), "physical pad lane must not be snapshotted: {names:?}");
    assert!(!names.contains("physical_pad_active_instance"), "controller handles must not be snapshotted: {names:?}");
    assert!(!names.contains("replay_pad_down"), "replay scratch must not be snapshotted: {names:?}");
    assert!(!names.contains("replay_scancodes_down"), "replay keyboard scratch must not be snapshotted: {names:?}");
    assert!(!names.contains("pad_axis_motion_this_frame"), "capture scratch must not be snapshotted: {names:?}");
    assert!(!names.contains("ACTIONS"), "binding configuration must not be snapshotted: {names:?}");
    assert!(!names.contains("ACTION_NAMES"), "binding registry scratch must not be snapshotted: {names:?}");
    assert!(!names.contains("BINDING_RAW_ACTIONS"), "unknown binding scratch must not be snapshotted: {names:?}");
    for name in [
        "BINDING_RAW_ACTION_BYTES",
        "BINDING_RAW_ACTION_KEYS",
        "REBIND_CAPTURE",
        "REBIND_CAPTURE_CANCELLED",
        "REBIND_SNAPSHOT_SCANCODES_DOWN",
        "REBIND_SNAPSHOT_SCANCODES_PRESSED",
        "REBIND_SNAPSHOT_MOUSE_DOWN",
        "REBIND_SNAPSHOT_MOUSE_PRESSED",
        "REBIND_SNAPSHOT_PAD_DOWN",
        "REBIND_SNAPSHOT_PAD_PRESSED",
        "REBIND_SNAPSHOT_AXES",
        "REBIND_SNAPSHOT_AXIS_MOTION",
    ] {
        assert!(!names.contains(name), "input host projection {name} was snapshotted: {names:?}");
    }
    assert!(!names.contains("LAST_INPUT_DEVICE"), "glyph modality must not be snapshotted: {names:?}");
    assert!(!names.contains("UI_INPUT_CAPTURE"), "capture mode must not be snapshotted: {names:?}");
    for name in [
        "RENDER_BATCH_SORT_KEYS",
        "UI_RENDER_SORT_KEYS",
        "UI_BATCH_KEY_BUILDS",
        "WORLD_BATCH_KEY_BUILDS",
    ] {
        assert!(!names.contains(name), "renderer flush scratch {name} was snapshotted: {names:?}");
    }
    assert!(
        !names.contains("TEXT_MISSING_GLYPH_POLICY"),
        "process-derived TEXT_MISSING_GLYPH_POLICY was snapshotted: {names:?}"
    );
}

#[test]
fn mag_audio_and_vfx_projections_are_not_world_image_state() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let names = sim_state_names("mag", 3);
    assert!(!names.is_empty(), "the Mag checkpoint census must examine a nonzero population");
    assert!(names.contains("pad_down"), "the Mag census must include effective input state: {names:?}");
    assert!(names.contains("tally"), "the Mag census must include gameplay tally state: {names:?}");
    assert!(!names.contains("tally_perf"), "the frame histogram must remain runner diagnostic state: {names:?}");
    for name in [
        "SFX_SPELL_FLAMESTRIKE_HIT",
        "SFX_CAST_LIGHTNING",
        "AMBIENCE_VOICE",
        "STONE_BUILD_SHA",
        "ui_effect_composite_shader",
        "UI_RASTER_CANVAS",
        "UI_OUTPUT_CANVAS",
        "UI_EFFECT_COMPOSITE_PIPELINE",
        "RENDER_BATCH_SORT_KEYS",
        "UI_RENDER_SORT_KEYS",
        "UI_BATCH_KEY_BUILDS",
        "WORLD_BATCH_KEY_BUILDS",
    ] {
        assert!(!names.contains(name), "host projection {name} leaked into WorldImage: {names:?}");
    }
    assert!(
        !names.contains("TEXT_MISSING_GLYPH_POLICY"),
        "process-derived TEXT_MISSING_GLYPH_POLICY leaked into WorldImage: {names:?}"
    );
}

#[test]
fn mag_host_projections_are_not_world_image_state() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let names = sim_state_names("mag", 3);
    assert!(!names.is_empty(), "the Mag checkpoint census must examine a nonzero population");
    for name in MAG_HOST_PROJECTION_WORLD_EXCLUDED {
        assert!(
            !names.contains(*name),
            "host projection {name} must be explicit world-excluded state: {names:?}"
        );
    }
}

#[test]
fn mag_s3_presentation_globals_are_not_world_image_state() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let names = sim_state_names("mag", 3);
    assert!(!names.is_empty(), "the Mag S3 census must examine a nonzero population");
    for name in MAG_S3_PLATFORM_WORLD_EXCLUDED {
        assert!(
            !names.contains(*name),
            "S3 presentation global {name} was captured in WorldImage: {names:?}"
        );
    }
}

#[test]
fn mag_world_image_admits_aggregate_owners_not_superseded_direct_globals() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let names = sim_state_names("mag", 3);
    assert!(!names.is_empty(), "the Mag checkpoint census must examine a nonzero population");
    assert_mag_seek_census_owners(&names);
}

fn assert_mag_panel_plan_census(sim: &std::collections::HashSet<String>) {
    assert!(
        !sim.contains("PANEL_DRAW_PLAN"),
        "PANEL_DRAW_PLAN is a rebuildable catalog cache, not WorldImage state"
    );
    assert!(
        !sim.contains("PANEL_DRAW_PLAN_BUILDS"),
        "PANEL_DRAW_PLAN_BUILDS is a diagnostic rebuild counter, not WorldImage state"
    );
    assert!(
        sim.contains("PANEL_STATES"),
        "PANEL_STATES must remain semantic panel ownership"
    );
    assert!(
        sim.contains("PANEL_SEQ"),
        "PANEL_SEQ must remain semantic panel ownership"
    );
}

#[test]
fn mag_panel_draw_plan_is_derived_not_world_image() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let names = sim_state_names("mag", 3);
    assert!(!names.is_empty(), "the Mag panel-plan census must examine a nonzero population");
    assert_mag_panel_plan_census(&names);
}

/// The sfx browser's replay handle is the HOST's, not the simulation's.
/// `SFX_REPLAY_VOICE` holds a live `Voice` minted by a tool button, so a
/// WorldImage that captured it would carry a native handle and the desync
/// hash would compare two allocation counters. Its `SFX_LOG_*` neighbours
/// are already `host_projection`; testbed boots the engine without a game
/// that plays anything, so the census reaches the declaration alone.
#[test]
fn sfx_browser_replay_voice_is_not_world_image_state() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let names = sim_state_names("testbed", 3);
    assert!(names.contains("time"), "the census must reach Stone's own globals: {names:?}");
    assert!(!names.contains("dt"), "native frame delta is host projection: {names:?}");
    assert!(
        !names.contains("SFX_REPLAY_VOICE"),
        "SFX_REPLAY_VOICE is a tool-owned native voice handle, not WorldImage state: {names:?}"
    );
    assert!(
        !names.contains("SFX_LOG_SUPPRESS"),
        "the sfx log's own state is host projection: {names:?}"
    );
}

#[test]
fn skin_gallery_seed_is_platform_state_outside_world_image() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let names = sim_state_names("testbed", 3);
    assert!(names.contains("time"), "the census must reach Stone's own globals: {names:?}");
    assert!(!names.contains("dt"), "native frame delta is host projection: {names:?}");
    assert!(
        !names.contains("SKIN_GALLERY_SEED"),
        "the seeded gallery map must not be WorldImage state: {names:?}"
    );

    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("testbed.lua");
    let compiled = crate::command::rock_bin()
        .args(["compile", "games/testbed", "--output", &output.to_string_lossy()])
        .current_dir(workspace_root())
        .output_with_deadline()
        .expect("compile testbed");
    assert!(
        compiled.status.success(),
        "testbed compile failed: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let lua = std::fs::read_to_string(output).expect("read testbed artifact");
    let plan = lua
        .lines()
        .find(|line| line.starts_with("_G.__ROCK_STATE_IMAGE_PLAN = {"))
        .expect("the emitter publishes a StateImagePlan");
    let entry = plan
        .split("{identity=\"")
        .find(|chunk| chunk.starts_with("global:") && chunk.contains("::SKIN_GALLERY_SEED\""))
        .expect("the gallery seed must have a StateImagePlan entry");
    assert!(
        entry.contains("kind=\"host_projection\""),
        "SKIN_GALLERY_SEED must be host-owned so boot seeds survive a worker boundary: {entry}"
    );
    assert!(
        entry.contains("capture_policy=\"host-owned\"")
            && entry.contains("realization_policy=\"rebuild-host-projection\"")
            && entry.contains("world_policy=\"exclude\""),
        "SKIN_GALLERY_SEED must remain live across WorldImage restore: {entry}"
    );
}

/// The five Stone globals whose class CHANGES when the package declares
/// `[language] memory = "platform"` (`docs/state-ownership-plan.md` S3).
///
/// Each was an unannotated world member sitting among annotated
/// `host_projection` neighbours -- the same shape as `SFX_REPLAY_VOICE` in S0,
/// found by reading the class change out instead of by a later divergence:
///
///  - `REBIND_INPUT_CONSUMED` -- one bool among twelve `host_projection`
///    siblings in `input/rebind.rock`, reset every frame by `ui/mouse.rock`.
///  - `SCREENSHOT_PENDING` -- the request queue, whose seven neighbours carry
///    a comment saying they are deliberately host-classified.
///  - `EXACT_FILE_WATCHES` -- dev-mode watch registrations holding `fn`
///    callbacks, among 22 `host_projection` neighbours in `debug/watcher.rock`.
///  - `UI_BAKE_EPOCH` -- the UI bake cache's invalidation epoch, which is only
///    meaningful against the host-side bake records it is compared with.
///  - `UI_LAYOUT_OVERFLOW` -- an overflow diagnostic beside the already-host
///    `UI_TEXT_OVERFLOW_N`.
#[test]
fn stones_platform_class_keeps_its_unannotated_host_state_out_of_the_world_image() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let names = sim_state_names("testbed", 3);
    assert!(names.contains("time"), "the census must reach Stone's own globals: {names:?}");
    assert!(!names.contains("dt"), "native frame delta is host projection: {names:?}");
    for name in [
        "REBIND_INPUT_CONSUMED",
        "SCREENSHOT_PENDING",
        "EXACT_FILE_WATCHES",
        "UI_BAKE_EPOCH",
        "UI_LAYOUT_OVERFLOW",
    ] {
        assert!(
            !names.contains(name),
            "{name} is host state Stone's package class owns, not WorldImage state: {names:?}"
        );
    }
}

/// The package class is a DEFAULT, not an override: Stone's explicitly
/// `session` globals stay in the world image after the declaration.
#[test]
fn stones_platform_class_does_not_swallow_its_explicit_session_globals() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let names = sim_state_names("testbed", 3);
    for name in ["time", "TIME_SCALE", "WORLD_MODE"] {
        assert!(
            names.contains(name),
            "{name} carries an explicit @state(ownership=session) and must stay WorldImage state: {names:?}"
        );
    }
}

/// `rock frame <game> -n N --dump-state [env]` -> parsed JSON, with NEITHER

/// `rock frame <game> -n N --dump-state [env]` -> parsed JSON, with NEITHER
/// --record nor --replay: ordinary play, the mode a shipped build runs in.
fn run_plain_dump(game: &str, frames: u32, env: &[(&str, &str)]) -> serde_json::Value {
    let scratch = tempfile::tempdir().unwrap();
    let out = crate::command::rock_bin()
        .args([
            "frame", game, "-n", &frames.to_string(),
            "--dump-state", "--output", &scratch.path().to_string_lossy(),
        ])
        .envs(env.iter().copied())
        .env("ROCK_FRAME_TIMEOUT_SECS", STATE_PHASE_TIMEOUT_SECS)
        .current_dir(workspace_root())
        .output_with_deadline()
        .expect("spawn plain run");
    assert!(out.status.success(), "plain run failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).expect("dump-state JSON")
}

/// Current MAG seek census owners, plus the superseded direct globals they
/// replaced. Both MAG seek census consumers must call this rather than
/// re-listing the names.
fn assert_mag_seek_census_owners(sim: &std::collections::HashSet<String>) {
    for name in [
        "MAG_ACTIVE_WORLD",
        "MAG_JOURNAL",
        "MAG_RUN_UI",
        "history",
    ] {
        assert!(sim.contains(name), "{name} must be admitted as simulation state");
    }
    for (name, owner) in [
        ("MAG_PROGRESS", "MAG_ACTIVE_WORLD"),
        ("MAG_QUEST_RUNTIME", "MAG_ACTIVE_WORLD"),
        ("bank_gold", "MAG_ACTIVE_WORLD"),
        ("journal", "MAG_JOURNAL"),
        ("quest_log_open", "MAG_RUN_UI"),
    ] {
        assert!(
            !sim.contains(name),
            "{name} is a superseded direct global; sim-state belongs to {owner}"
        );
    }
    assert!(
        !sim.contains("player"),
        "Player ownership must remain inside MAG_ACTIVE_WORLD, not a duplicate global"
    );
    assert!(
        !sim.contains("MAG_TICK_INTENT"),
        "the consumed per-frame input record is host projection state, not WorldImage state"
    );
    for name in [
        "MAG_PENDING_CAST_KIND",
        "MAG_PENDING_TARGET_JUMP",
        "MAG_PENDING_TARGET_SELECTION",
        "MAG_PENDING_INVENTORY_COMMAND",
    ] {
        assert!(
            sim.contains(name),
            "pending semantic input command {name} must remain checkpointed"
        );
    }
}

/// Loaded audio resources (MultiSound/Sound handles) are NOT simulation state:
/// the plan excludes audio from keyframes ("never rewinds; mute during scrub,
/// resume live"), the same carve-out render resources already get. bitgun's
/// `sfx_shotgun` is a `Sound` handle -- it must be absent from the sim-state
/// predicate's output (so it is neither snapshotted nor desync-hashed), while a
/// real gameplay global (`MOBS`) stays present.
#[test]
fn audio_resources_excluded_from_sim_state() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let sim = sim_state_names("bitgun", 3);
    assert!(
        sim.contains("MOBS"),
        "control: gameplay global MOBS must be sim state (got {} names)",
        sim.len()
    );
    assert!(
        !sim.contains("sfx_shotgun"),
        "audio Sound handle sfx_shotgun must be excluded from sim state"
    );
}

/// A keyframe taken while image publication is HELD carries the requests that
/// were still outstanding, and restoring it puts them back.
///
/// A test-only image request under `STONE_IMAGE_HOLD_AT_FRAME` creates the
/// outstanding queue through the same loader and publication hold used by a
/// game session. Ordinary headless frames finish their decode before a
/// cadence keyframe can capture it.
///
/// The control arm is the denominator: without the hold the same snapshot and
/// restore sees an EMPTY queue, so a green here is the hold's doing and not a
/// queue that happened to be busy.
#[test]
fn a_restore_reinstalls_the_image_requests_that_were_still_outstanding() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("queue.png");
    std::fs::copy(workspace_root().join("mag/assets/cursors/color/01.png"), &image)
        .expect("copy an owned image to a unique request path");
    let image_s = image.to_string_lossy().into_owned();
    let common: Vec<(&str, &str)> = vec![
        ("STONE_IMAGE_REQUEST_AT_FRAME", "2"),
        ("STONE_IMAGE_REQUEST_PATH", &image_s),
        ("STONE_SNAPSHOT_AT_TICK", "3"),
        ("STONE_RESTORE_AT_TICK", "6"),
    ];

    let mut held = common.clone();
    held.push(("STONE_IMAGE_HOLD_AT_FRAME", "1"));
    held.push(("STONE_IMAGE_RELEASE_AT_FRAME", "30"));
    let held = run_plain_dump("testbed", 40, &held);
    assert_eq!(held["__stone_diag"]["STONE_LAST_RESTORE_TICK"], 3,
        "the held arm must restore the keyframe it took");
    let pending = held["__stone_diag"]["STONE_RESTORE_IMAGE_PENDING"]
        .as_u64().expect("a restore reports the queue it reinstalled");
    assert!(pending > 0,
        "restoring a keyframe taken under a publication hold must reinstall its \
         outstanding requests, got {pending}");

    let plain = run_plain_dump("testbed", 40, &common);
    assert_eq!(plain["__stone_diag"]["STONE_LAST_RESTORE_TICK"], 3,
        "the control arm must restore the same keyframe");
    assert_eq!(plain["__stone_diag"]["STONE_RESTORE_IMAGE_PENDING"], 0,
        "control: without the hold the same keyframe has nothing outstanding, \
         so the assertion above is the hold's doing");
}

fn current_tick(s: &serde_json::Value) -> i64 {
    s["stone"]["current_tick"].as_i64().expect("current_tick")
}

/// Assert every sim-state global (`sim`) in `got` matches `want`, and at least
/// one was compared.
///
/// Resource ids, when present, are rendered as host projections by the
/// serializer (`stone/engine/host_projection.rock`), since these dumps can
/// come from separate `rock frame` processes.
fn assert_sim_state_matches(
    got: &serde_json::Value,
    want: &serde_json::Value,
    sim: &std::collections::HashSet<String>,
    label: &str,
) {
    let mut compared = 0;
    let mut mismatches = Vec::new();
    for (pkg, globals) in crate::stone_harness::dump_packages(got) {
        let want_pkg = &want[pkg];
        for (name, val) in globals {
            if !sim.contains(name) {
                continue;
            }
            let mut got_val = val.clone();
            let mut want_val = want_pkg[name].clone();
            if pkg == "stone" && name == "UI_FOCUS" {
                normalize_focus_pool_tails(&mut got_val);
                normalize_focus_pool_tails(&mut want_val);
            }
            collect_f32_state_mismatches(
                &got_val,
                &want_val,
                &format!("{pkg}.{name} ({label})"),
                &mut mismatches,
            );
            compared += 1;
        }
    }
    assert!(compared > 0, "must compare at least one sim-state global");
    assert!(
        mismatches.is_empty(),
        "sim state must match; first {} difference(s):\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

fn normalize_focus_pool_tails(value: &mut serde_json::Value) {
    // Focus pools retain stale tail items; only the counted prefix is active.
    for (pool_name, count_name) in [("cur", "cur_count"), ("last", "last_count")] {
        let Some(count) = value
            .get(count_name)
            .and_then(serde_json::Value::as_u64)
        else {
            continue;
        };
        let Some(pool) = value
            .get_mut(pool_name)
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };
        let Some(data) = pool
            .get_mut("data")
            .and_then(serde_json::Value::as_array_mut)
        else {
            continue;
        };
        let active = (count as usize).min(data.len());
        data.truncate(active);
        pool.insert("len".to_string(), serde_json::Value::from(active as u64));
    }
}

fn collect_f32_state_mismatches(
    got: &serde_json::Value,
    want: &serde_json::Value,
    path: &str,
    mismatches: &mut Vec<String>,
) {
    if mismatches.len() >= 32 { return }
    match (got, want) {
        (serde_json::Value::Number(a), serde_json::Value::Number(b)) => {
            if let (Some(a), Some(b)) = (a.as_i64(), b.as_i64()) {
                if a != b { mismatches.push(format!("{path}: integer {a} != {b}")); }
            } else {
                let a = a.as_f64().expect("JSON number");
                let b = b.as_f64().expect("JSON number");
                if (a as f32).to_bits() != (b as f32).to_bits() {
                    mismatches.push(format!("{path}: f32 {a} != {b}"));
                }
            }
        }
        (serde_json::Value::Array(a), serde_json::Value::Array(b)) => {
            if a.len() != b.len() {
                mismatches.push(format!("{path}: array length {} != {}", a.len(), b.len()));
            }
            for (i, (a, b)) in a.iter().zip(b).enumerate() {
                collect_f32_state_mismatches(a, b, &format!("{path}[{i}]"), mismatches);
            }
        }
        (serde_json::Value::Object(a), serde_json::Value::Object(b)) => {
            let a_keys = a.keys().collect::<std::collections::BTreeSet<_>>();
            let b_keys = b.keys().collect::<std::collections::BTreeSet<_>>();
            if a_keys != b_keys {
                mismatches.push(format!("{path}: object keys {a_keys:?} != {b_keys:?}"));
            }
            for (name, a) in a {
                if let Some(b) = b.get(name) {
                    collect_f32_state_mismatches(a, b, &format!("{path}.{name}"), mismatches);
                }
            }
        }
        _ if got != want => mismatches.push(format!("{path}: {got} != {want}")),
        _ => {}
    }
}

#[test]
fn restore_returns_to_snapshot_tick_state() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let env = [("ROCK_TEST_SEED", "42"), ("STONE_TEST_FIXED_DT", "0.016666667")];
    let mut reference_env = env.to_vec();
    reference_env.extend([("STONE_START_PAUSED", "1"), ("STONE_STEP_FRAMES", "10")]);
    let reference = run_plain_dump("bitgun", 30, &reference_env);
    assert_eq!(current_tick(&reference), 10);
    let mut restore_env = env.to_vec();
    restore_env.extend([("STONE_SNAPSHOT_AT_TICK", "10"), ("STONE_RESTORE_AT_TICK", "20")]);
    let restored = run_plain_dump("bitgun", 30, &restore_env);
    assert_eq!(current_tick(&restored), 10);
    assert_sim_state_matches(&restored, &reference, &sim_state_names("bitgun", 3), "tick-10");
}
