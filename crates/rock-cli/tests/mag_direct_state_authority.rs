use std::fs;
use std::path::{Path, PathBuf};

fn rock_sources(root: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(root).expect("read MAG source directory") {
        let entry = entry.expect("read MAG source entry");
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|name| name.to_str()) != Some("tests") {
                rock_sources(&path, out);
            }
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rock") {
            out.push(path);
        }
    }
}

fn rock_function_body<'a>(source: &'a str, signature: &str) -> &'a str {
    let start = source
        .find(signature)
        .unwrap_or_else(|| panic!("missing Rock function {signature}"));
    let open = start
        + source[start..]
            .find('{')
            .unwrap_or_else(|| panic!("missing body for Rock function {signature}"));
    let mut depth = 0;
    for (offset, ch) in source[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &source[open + 1..open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("unterminated body for Rock function {signature}");
}

fn rock_struct_body<'a>(source: &'a str, name: &str) -> &'a str {
    source
        .split_once(&format!("pub struct {name} {{"))
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .unwrap_or_else(|| panic!("missing Rock struct body {name}"))
}

#[test]
fn quest_state_field_pins_ignore_function_parameters() {
    let source = include_str!("../../../mag-simulation/quest.rock");
    let scratch = format!("{source}\npub fn unrelated(bank_gold: int, pending_events: [string]) {{}}\n");
    for (name, field) in [
        ("ProgressState", "bank_gold: int"),
        ("QuestRuntimeState", "pending_events: [string]"),
    ] {
        assert!(scratch.matches(field).count() > 1);
        assert_eq!(rock_struct_body(&scratch, name).matches(field).count(), 1);
        let declaration = format!("pub struct {name} {{");
        let duplicate = scratch.replacen(
            &declaration, &format!("{declaration}\n    {field},"), 1);
        assert_eq!(rock_struct_body(&duplicate, name).matches(field).count(), 2);
    }
}

#[test]
fn mag_run_readiness_predicate_has_no_replay_or_upload_side_effects() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("rock workspace root");
    let run = fs::read_to_string(repo.join("mag/run.rock")).expect("read MAG run source");
    let predicate = rock_function_body(&run, "fn run_ready() -> bool {");
    for forbidden in ["replay_job_ready", "stone_flush_image_uploads"] {
        assert!(
            !predicate.contains(forbidden),
            "run_ready is a predicate and must not call `{forbidden}`:\n{predicate}"
        );
    }

    let transition = rock_function_body(&run, "fn run_admission_try_finish() {");
    assert_eq!(
        transition.matches("replay_job_ready(current_tick, \"admission\", 1,").count(),
        1,
        "one admission transition authority must record readiness exactly once per attempt"
    );
    assert_eq!(
        run.matches("replay_job_ready(current_tick, \"admission\", 1,").count(),
        1,
        "admission replay publication must have one production call site"
    );
}

#[test]
fn mag_has_no_aggregate_projection_bridge_names() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("rock workspace root");
    let mut sources = Vec::new();
    rock_sources(&repo.join("mag"), &mut sources);
    let forbidden = [
        ["run_projection", "_bind"].concat(),
        ["run_projection", "_capture"].concat(),
        ["MAG_BUILDING", "_RUN"].concat(),
    ];
    let mut findings = Vec::new();
    for source in &sources {
        let text = fs::read_to_string(&source).expect("read MAG source");
        for name in &forbidden {
            if text.contains(name) {
                findings.push(format!("{}: {name}", source.display()));
            }
        }
    }
    assert!(findings.is_empty(), "projection bridge census is nonzero:\n{}", findings.join("\n"));
}

#[test]
fn mag_world_environment_state_is_owned_by_the_scene_graph() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("rock workspace root");
    let mut sources = Vec::new();
    rock_sources(&repo.join("mag"), &mut sources);

    let forbidden = [
        "global restorative_ready_at:",
        "global harvest_ready:",
        "global harvest_node_tiles:",
        "global harvest_node_spaces:",
        "global harvest_scan_t:",
        "global hazard_ready_at:",
        "global hazard_last_settled_place:",
        "global gate_lock_seen:",
        "struct HarvestSpaceNodes",
        "fn reset_restoration()",
        "fn reset_harvest()",
        "fn reset_hazards()",
    ];
    let mut findings = Vec::new();
    for source in &sources {
        let text = fs::read_to_string(source).expect("read MAG source");
        for (line_number, line) in text.lines().enumerate() {
            for declaration in forbidden {
                if line.contains(declaration) {
                    findings.push(format!(
                        "{}:{}: {declaration}",
                        source.display(),
                        line_number + 1
                    ));
                }
            }
        }
    }
    assert!(
        findings.is_empty(),
        "world-environment state still has Session-global/reset authority:\n{}",
        findings.join("\n")
    );

    let world = fs::read_to_string(repo.join("mag/world_levels.rock"))
        .expect("read MAG world source");
    let space_state = world
        .split_once("struct SpaceState {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .expect("read SpaceState body");
    for field in [
        "harvest: HarvestVisualState",
        "layout: Entity<WorldSpaceLayout>",
    ] {
        assert_eq!(
            space_state.matches(field).count(),
            1,
            "SpaceState must own `{field}` exactly once"
        );
    }
    for field in ["restoration:", "hazards:", "gates:", "teleport_entry:", "ticks:"] {
        assert!(
            !space_state.lines().any(|line| line.trim_start().starts_with(field)),
            "SpaceState must keep `{field}` under its package simulation state"
        );
    }
    assert!(
        !space_state.lines().any(|line| line.trim_start().starts_with("simulation:")),
        "SpaceState must share the package root's simulation record through its layout"
    );
    let root = fs::read_to_string(repo.join("mag-world/world_root.rock"))
        .expect("read package world root");
    let root_state = root
        .split_once("pub struct MagWorldRoot<M> {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .expect("read MagWorldRoot body");
    let layout_state = root
        .split_once("pub struct WorldSpaceLayout {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .expect("read WorldSpaceLayout body");
    assert_eq!(root_state.matches("spaces: [Entity<WorldSpaceLayout>]").count(), 1);
    assert_eq!(layout_state.matches("simulation: SpaceSimulationState").count(), 1);
    assert!(world.contains("root.spaces = layouts"));
    assert_eq!(root_state.matches("space_layouts: Arena<WorldSpaceLayout>").count(), 1);
    let build = rock_function_body(&world, "fn world_state_build(");
    for shared_handle in ["let layout = space_layouts.insert(WorldSpaceLayout {",
        "layouts.push(layout)", "spaces.push(space_states.insert(SpaceState {\n            layout,"] {
        assert!(build.contains(shared_handle), "world constructor must share layout handle `{shared_handle}`");
    }
    let advance = rock_function_body(&world, "fn world_tick_all(");
    for call in ["for space_handle in world.root.spaces", "let space = space_handle.get().unwrap()",
        "simulation_space_advance(space.simulation)"] {
        assert!(advance.contains(call), "world tick must advance each root-owned layout: `{call}`");
    }
    let simulation = fs::read_to_string(repo.join("mag-simulation/world_space.rock"))
        .expect("read package space state");
    let simulation_state = simulation
        .split_once("pub struct SpaceSimulationState {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .expect("read SpaceSimulationState body");
    for field in [
        "ticks: int",
        "harvest_ready_at: Map<int, float>",
        "restoration: RestorationState",
        "hazards: HazardState",
        "gates: GateObservationState",
        "teleport_entry: TeleportEntryState",
    ] {
        assert_eq!(
            simulation_state.matches(field).count(),
            1,
            "SpaceSimulationState must own `{field}` exactly once"
        );
    }
    let harvest = fs::read_to_string(repo.join("mag/harvest.rock"))
        .expect("read MAG harvest source");
    assert_eq!(
        harvest.matches("fn harvest_ready_key(g: IVec2)").count(),
        1,
        "per-Space harvest state must not pack SpaceId into local tile keys"
    );
    let hazards = fs::read_to_string(repo.join("mag-simulation/hazards.rock"))
        .expect("read simulation hazards source");
    assert_eq!(
        hazards.matches("fn hazard_ready_key(grid: IVec2)").count(),
        1,
        "per-Space hazard state must not pack SpaceId into local tile keys"
    );

    let player = fs::read_to_string(repo.join("mag/player.rock"))
        .expect("read MAG player source");
    assert_eq!(
        player.matches("hazard_entry: HazardEntryState = HazardEntryState {}").count(),
        1,
        "Player must own its package hazard entry state"
    );
    assert_eq!(
        hazards.matches("last_settled_place: Place? = null").count(),
        1,
        "the package must own the previous settled hazard tile"
    );

    let run = fs::read_to_string(repo.join("mag/run.rock"))
        .expect("read MAG run source");
    for assignment in [
        "restorative_ready_at =",
        "harvest_ready =",
        "harvest_node_tiles =",
        "harvest_node_spaces =",
        "harvest_scan_t =",
        "hazard_ready_at =",
        "hazard_entry =",
        "hazard_hits =",
        "gate_lock_seen =",
    ] {
        assert_eq!(
            run.matches(assignment).count(),
            0,
            "run_reset_direct_state must not reset Scene-owned `{assignment}`"
        );
    }
}

#[test]
fn mag_uses_scene_owned_player_and_direct_session_state_without_an_ambient_accessor() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("rock workspace root");
    let mut sources = Vec::new();
    rock_sources(&repo.join("mag"), &mut sources);
    let aggregate_type = ["Run", "State"].concat();
    let run_accessor = ["mag", "_run", "_state"].concat();
    let forbidden = [
        format!("struct {aggregate_type}"),
        format!("Scene<{aggregate_type}>"),
        ["MAG", "_ACTIVE", "_RUN"].concat(),
        ["mag", "_run", "_scene", "_for", "_access"].concat(),
        format!("fn {run_accessor}"),
        format!("{run_accessor}()"),
        ["mag", "_world", "_scene"].concat(),
        ["fn current", "_run"].concat(),
        "global player:".to_string(),
        ["fn current", "_player()"].concat(),
        ["fn mag", "_player()"].concat(),
        ["fn player", "_for", "_access"].concat(),
        ["fn mag", "_progress"].concat(),
        ["fn mag", "_journal"].concat(),
        ["fn journal", "_state"].concat(),
        "global player_dead:".to_string(),
        "fn player_dead(".to_string(),
        "player.player_dead".to_string(),
        "global player_attack_target:".to_string(),
        "fn reset_player_death_state()".to_string(),
        "global player_gesture:".to_string(),
        "global player_gesture_timer:".to_string(),
        "global player_idle_ticks:".to_string(),
        "global player_loco:".to_string(),
        "global player_stop_ru:".to_string(),
        "global player_stride_hops:".to_string(),
        "global PLAYER_START_SPR:".to_string(),
        "global player_cast_words:".to_string(),
        "global branched_ritual_cast:".to_string(),
        "global player_cast_acceleration:".to_string(),
        "global branched_ritual_acquisition_event_seq:".to_string(),
        "global PLAYER_NOTORIETY:".to_string(),
        "global PLAYER_COMBAT_LAST_CONTACT:".to_string(),
        "fn player_is_murderer()".to_string(),
        "fn clear_player_combat_latch()".to_string(),
        "fn refresh_player_combat_latch()".to_string(),
        "fn player_in_combat()".to_string(),
        "fn reset_player_notoriety()".to_string(),
        "fn reset_player_combat_latch()".to_string(),
        "fn reset_branched_ritual_cast()".to_string(),
        "fn reset_player_cast_acceleration()".to_string(),
        "global POTION_READY_AT:".to_string(),
        "reset_potion_cooldown(".to_string(),
        "global bandage_slips:".to_string(),
        "global summon_slots:".to_string(),
        "global outline_last_t:".to_string(),
        "global outline_last_color:".to_string(),
        "global spell_pump_outline_until:".to_string(),
        "global spell_pump_outline_color:".to_string(),
        "reset_spell_pump_feedback(".to_string(),
        "global bank_gold:".to_string(),
        "global events:".to_string(),
        "global quest_runs:".to_string(),
        "global quest_tracked:".to_string(),
        "global quest_collapsed:".to_string(),
        "global quest_quarantined:".to_string(),
        "global journal: [JournalLine]".to_string(),
        "global journal_next_row_id:".to_string(),
        "global journal_next_cast_id:".to_string(),
        "global journal_active_cast_id:".to_string(),
        "global journal_submitted_contributions:".to_string(),
        "global journal_retired_contributions:".to_string(),
        "global rmb_consumed:".to_string(),
        "global drag_nameplate_from:".to_string(),
        "global drag_nameplate_start:".to_string(),
        "global last_target:".to_string(),
        "global target_cycle:".to_string(),
        "global target_cycle_idx:".to_string(),
        "global target_jump_active:".to_string(),
        "global target_jump_targets:".to_string(),
        "global cursor_stack:".to_string(),
        "global hover_nameplate_mob:".to_string(),
        "global selection:".to_string(),
        "global dead_last_click_target:".to_string(),
        "global dead_last_click_at:".to_string(),
        "global hover_entity_id:".to_string(),
        "global hovered_mob:".to_string(),
        "global hovered_corpse:".to_string(),
        "global hovered_npc:".to_string(),
        "global hovered_door:".to_string(),
        "global hovered_ground_item:".to_string(),
        "global hovered_prop_tile:".to_string(),
        "global hovered_wall_tile:".to_string(),
        "global quest_pending_events:".to_string(),
        "global quest_reconciling:".to_string(),
        "global quest_reconcile_queued:".to_string(),
        "global quest_event_subscribers:".to_string(),
        "global quest_state_subscribers:".to_string(),
        "global quest_region_subscribers:".to_string(),
        "global quest_dependents:".to_string(),
        "global quest_needs_reconcile:".to_string(),
        "global quest_last_region_place:".to_string(),
        "global quest_state_changed:".to_string(),
        "global bank_open:".to_string(),
        "global shop_open:".to_string(),
        "global inventory_open:".to_string(),
        "global spell_panel_open:".to_string(),
        "global journal_open:".to_string(),
        "global quest_log_open:".to_string(),
        "global quest_tracker_open:".to_string(),
        "global quest_log_tab:".to_string(),
        "global quest_log_scroll:".to_string(),
        "global quest_tracker_scroll:".to_string(),
        "global quest_giver_open_id:".to_string(),
        "global quest_giver_confirm_id:".to_string(),
        "global quest_giver_open_npc:".to_string(),
        "global ui_menu_is_open:".to_string(),
        "global COMBAT_REPORT_OPEN:".to_string(),
        "global COMBAT_REPORT_DETAILS:".to_string(),
        "global COMBAT_REPORT_SCROLL:".to_string(),
        "global COMBAT_REPORT_EXPANDED:".to_string(),
        "global precast_tutorial_dummy:".to_string(),
        "global precast_tutorial_dummy_hp:".to_string(),
        "global target_practice_dummy_hp:".to_string(),
        "global precast_dummy_tile:".to_string(),
        "global precast_entrance_tiles:".to_string(),
        "global precast_barrier_tiles:".to_string(),
        "global precast_highlight_next:".to_string(),
        "global target_practice_dummies:".to_string(),
        "global target_practice_dummy_tiles:".to_string(),
        "global target_practice_rewarded:".to_string(),
        "global target_practice_entrance_tiles:".to_string(),
        "global target_practice_exit_tiles:".to_string(),
        "global tutorial_space:".to_string(),
        "global tutorial_space_ready:".to_string(),
        "fn tutorial_reset(".to_string(),
        "global MAG_PROGRESS:".to_string(),
        "global MAG_QUEST_RUNTIME:".to_string(),
    ];
    let required = [
        "global MAG_JOURNAL: JournalState",
        "global MAG_INTERACTION: InteractionState",
        "global MAG_RUN_UI: RunUiState",
        "global history: [SnapshotHistoryNode]",
        "global MAG_ACTIVE_WORLD: Scene<WorldState>?",
    ];
    let mut findings = Vec::new();
    let mut definitions = required.map(|_| Vec::new());
    for source in &sources {
        let text = fs::read_to_string(&source).expect("read MAG source");
        for (line_number, line) in text.lines().enumerate() {
            for name in &forbidden {
                if line.contains(name) {
                    findings.push(format!("{}:{}: {name}", source.display(), line_number + 1));
                }
            }
            for (index, declaration) in required.iter().enumerate() {
                if line.trim_start().starts_with(declaration) {
                    definitions[index].push(format!("{}:{}", source.display(), line_number + 1));
                }
            }
            if line.contains("() => render_pos(player.movable)")
                || line.contains("() => player.movable.render_pos()")
            {
                findings.push(format!(
                    "{}:{}: deferred callback captures raw Player",
                    source.display(),
                    line_number + 1
                ));
            }
        }
    }
    for (field, qualified, declaration) in [
        ("potion_ready_at", "player.body.potion_ready_at", "potion_ready_at:"),
        ("bandage_slips", "player.combat.bandage_slips", "bandage_slips:"),
        ("summon_slots", "player.summon_slots", "summon_slots:"),
    ] {
        for source in &sources {
            let text = fs::read_to_string(source).expect("read MAG source");
            for (line_number, line) in text.lines().enumerate() {
                // Save-file JSON keys name the field as data, not as an access.
                let code = without_string_literals(line);
                if code.contains(field)
                    && !code.contains(qualified)
                    && !code.trim_start().starts_with(declaration)
                {
                    findings.push(format!(
                        "{}:{}: unqualified Player-owned field `{field}`",
                        source.display(),
                        line_number + 1
                    ));
                }
            }
        }
    }
    assert!(findings.is_empty(), "superseded run-root/accessor census is nonzero:\n{}", findings.join("\n"));
    for (declaration, found) in required.iter().zip(definitions) {
        assert_eq!(found.len(), 1, "`{declaration}` must have one production definition: {found:?}");
    }

    let world = fs::read_to_string(repo.join("mag/world_levels.rock"))
        .expect("read MAG world source");
    let world_state = world
        .split_once("struct WorldState {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .expect("read WorldState body");
    for field in [
        "players: Arena<Player>",
        "player: Entity<Player>",
        "wisp_motes: Arena<Mote>",
        "wisps: [MagicWisp]",
        "wisp_scatter_t: float",
        "wisp_rush_t: float",
        "wisp_effectors: [MoteEffector]",
        "wisp_mood: MoteMood",
        "tutorial: TutorialState",
    ] {
        assert!(world_state.contains(field), "WorldState is missing Scene-owned `{field}`");
    }
    assert!(world_state.contains("root: MagWorldRoot<Mob>"));
    assert!(!world_state.lines().any(|line| line.trim() == "tasks: CoroutinePoller,"));
    let root = fs::read_to_string(repo.join("mag-world/world_root.rock"))
        .expect("read package world root");
    let root_state = root
        .split_once("pub struct MagWorldRoot<M> {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .expect("read MagWorldRoot body");
    assert_eq!(root_state.matches("tasks: CoroutinePoller").count(), 1);

    let tutorial = fs::read_to_string(repo.join("mag-simulation/tutorial_state.rock"))
        .expect("read package tutorial state");
    let tutorial_state = tutorial
        .split_once("pub struct TutorialState {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .expect("read TutorialState body");
    for field in [
        "precast_dummy_tile: IVec2",
        "precast_highlight_next: float",
        "practice_dummy_tiles: [IVec2]",
        "space: SpaceId",
        "space_ready: bool",
    ] {
        assert_eq!(
            tutorial_state.matches(field).count(),
            1,
            "TutorialState must contain `{field}` exactly once"
        );
    }
    for former_field in [
        "precast_dummy:",
        "practice_dummies:",
        "precast_entrance_tiles:",
        "precast_barrier_tiles:",
        "practice_rewarded:",
        "practice_entrance_tiles:",
        "practice_exit_tiles:",
    ] {
        assert!(
            !tutorial_state.lines().any(|line| line.trim_start().starts_with(former_field)),
            "TutorialState must not duplicate removed `{former_field}` authority"
        );
    }
    let tutorial_runtime = fs::read_to_string(repo.join("mag/tutorial.rock"))
        .expect("read MAG tutorial runtime");
    let mob_lookup = rock_function_body(&tutorial_runtime, "fn tutorial_mob_at_in(");
    for predicate in [
        "world.root.mobs",
        "place.space, space",
        "place.grid == g",
        "mob.objective_tag == tag",
    ] {
        assert!(mob_lookup.contains(predicate), "tutorial mob lookup is missing `{predicate}`");
    }
    assert!(tutorial_runtime.contains("\"tutorial.precast_dummy\", tutorial.space"));
    let quests = fs::read_to_string(repo.join("mag/quests.rock"))
        .expect("read MAG quest source");
    let receipt_owner = rock_function_body(&quests, "fn quest_run_receipt_seen_in(");
    assert!(receipt_owner.contains("quest_run(world.progress, id)"));
    let quest_logic = fs::read_to_string(repo.join("mag-simulation/quest.rock"))
        .expect("read quest simulation source");
    let run_lookup = rock_function_body(&quest_logic, "pub fn quest_run(");
    assert!(run_lookup.contains("progress.quest_runs.get(id)"));
    assert!(run_lookup.contains("entity.get()"));
    let doors = fs::read_to_string(repo.join("mag/doors.rock"))
        .expect("read MAG gate source");
    let gate_lock = rock_function_body(&doors, "fn gate_locked_in(");
    assert!(gate_lock.contains("world_gate_locked(world.root, gate, space)"));
    let package_doors = fs::read_to_string(repo.join("mag-world/world_doors.rock"))
        .expect("read package gate source");
    let gate_members = rock_function_body(&package_doors, "pub fn world_gate_locked<M>(");
    for predicate in ["gate_index::world_gate_members(root, gate, space)",
        "space_id_equal(spawner.place.space, space)",
        "spawner.spec.gate != gate", "spawner.lifecycle.members.len() > 0", "if members == 0"] {
        assert!(gate_members.contains(predicate), "root-owned gate lock is missing `{predicate}`");
    }
    let crypt: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo.join("mag/levels/crypt.json"))
            .expect("read authored Crypt level"),
    )
    .expect("parse authored Crypt level");
    let authored_spawners = crypt["spawners"].as_array().expect("Crypt spawners");
    let authored_doors = crypt["doors"].as_array().expect("Crypt doors");
    for (tag, gate, expected) in [
        ("tutorial.precast_dummy", 1, 1),
        ("tutorial.practice_dummy:", 2, 3),
    ] {
        assert_eq!(
            authored_spawners
                .iter()
                .filter(|spawner| {
                    spawner["objective_tag"].as_str().is_some_and(|value| value.starts_with(tag))
                        && spawner["gate"].as_i64() == Some(gate)
                })
                .count(),
            expected,
            "Crypt must bind {tag} targets to authored gate {gate}"
        );
        assert!(
            authored_doors.iter().any(|door| door["gate_id"].as_i64() == Some(gate)),
            "Crypt must have a door for authored gate {gate}"
        );
    }

    let run_ui = fs::read_to_string(repo.join("mag/quest_ui.rock"))
        .expect("read MAG run UI source");
    for field in [
        "bank_open: bool",
        "shop_open: bool",
        "inventory_open: bool",
        "spells_open: bool",
        "journal_open: bool",
        "quest_log_open: bool",
        "quest_tracker_open: bool",
        "quest_log_tab: string",
        "quest_log_scroll: float",
        "quest_tracker_scroll: float",
        "quest_giver_open_id: string",
        "quest_giver_confirm_id: string",
        "quest_giver_open_npc: Entity<Mob>?",
        "menu_open: bool",
        "combat_report_open: bool",
        "combat_report_details: bool",
        "combat_report_scroll: float",
        "combat_report_expanded: Map<string, bool>",
    ] {
        assert_eq!(
            run_ui.matches(field).count(),
            1,
            "RunUiState must contain `{field}` exactly once"
        );
    }
    let panels = fs::read_to_string(repo.join("mag/panels.rock"))
        .expect("read MAG panel source");
    assert!(
        panels.contains("is_open: fn(RunUiState) -> bool"),
        "panel openness must consume the typed run UI value"
    );
    assert_eq!(
        panels.matches("let ui = MAG_RUN_UI").count(),
        5,
        "each panel scan must bind the run UI value once"
    );
    assert_eq!(
        panels.matches(".is_open(ui)").count(),
        5,
        "every production panel scan must use its bound run UI value"
    );
    assert_eq!(
        panels.matches("is_open: () => MAG_RUN_UI").count(),
        0,
        "panel definitions must not reload Session state per catalog entry"
    );
    let attach_player = rock_function_body(&world, "fn WorldState.attach_player(");
    assert!(
        attach_player.contains("entity.get().unwrap().entity = entity"),
        "attaching a Player must publish its self identity"
    );

    let player = fs::read_to_string(repo.join("mag/player.rock"))
        .expect("read MAG player source");
    let player_state = player
        .split_once("struct Player {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .expect("read Player body");
    assert!(
        player.contains("entity: Entity<Player>?"),
        "Player must carry its generation-safe Scene identity for deferred work"
    );
    for field in [
        "body: PlayerBody",
        "combat: PlayerCombatState",
        "gesture: PlayerGesture",
        "gesture_timer: float",
        "idle_ticks: int",
        "loco: LocoState",
        "stop_ru: bool",
        "stride_hops: int",
        "loco_start_variant: PlayerStartVariant",
        "cast_words: CastWords",
        "ritual_cast: BranchedRitualCast",
        "ritual_acquisition_event_seq: int",
        "law: PlayerLaw",
        "combat_latch: CombatLatchState",
        "summon_slots: [MobId?]",
        "cast_outline: PlayerCastOutline",
    ] {
        assert_eq!(
            player_state.matches(field).count(),
            1,
            "scene-owned Player must contain `{field}` exactly once"
        );
    }
    let law = fs::read_to_string(repo.join("mag-simulation/notoriety.rock"))
        .expect("read simulation player law source");
    let law_state = law.split_once("pub struct PlayerLaw {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body).expect("read PlayerLaw body");
    for field in ["notoriety: PlayerNotoriety", "criminal_remaining: float"] {
        assert_eq!(law_state.matches(field).count(), 1, "PlayerLaw must own `{field}` exactly once");
        assert!(!player_state.contains(field), "Player must borrow the shared law instead of copying `{field}`");
    }
    let body = fs::read_to_string(repo.join("mag-simulation/player_body.rock"))
        .expect("read simulation player body source");
    for field in [
        "dead: bool", "invulnerable: bool", "status: [StatusEffect<DamageSource>]",
        "health_cur: float", "health_max: float", "mana_cur: float", "mana_max: float",
        "stamina_cur: float", "stamina_max: float", "potion_ready_at: float",
    ] {
        assert_eq!(body.matches(field).count(), 1, "PlayerBody must own `{field}` exactly once");
        assert!(!player_state.contains(field), "Player must not duplicate `{field}`");
    }
    assert_eq!(body.matches("potion_ready_at: float = 0.0").count(), 1,
        "PlayerBody must initialize its potion deadline exactly once");
    let combat = fs::read_to_string(repo.join("mag-simulation/player_combat.rock"))
        .expect("read simulation player combat source");
    for field in [
        "spell_state: SpellState", "cast_acceleration: PlayerCastAcceleration",
        "attack_target: MobId?", "attack_timer: float",
        "bandage_slips: int", "cast_equipment_damage_mult: float",
    ] {
        assert_eq!(combat.matches(field).count(), 1, "PlayerCombatState must own `{field}` exactly once");
        assert!(!player_state.contains(field), "Player must not duplicate `{field}`");
    }
    assert!(
        combat.contains("pub fn player_bandage_heal_left(state: PlayerCombatState)"),
        "PlayerCombatState must own the bandage heal-left rule"
    );
    for field in [
        "combat_latch: CombatLatchState = CombatLatchState {}",
        "combat: PlayerCombatState = PlayerCombatState {}",
        "summon_slots: [MobId?] = [null, null, null, null]",
        "cast_outline: PlayerCastOutline = PlayerCastOutline {}",
    ] {
        assert_eq!(
            player_state.matches(field).count(),
            1,
            "scene-owned Player must initialize `{field}` exactly once"
        );
    }
    let combat_latch = fs::read_to_string(repo.join("mag-simulation/combat_latch.rock"))
        .expect("read simulation combat latch source");
    assert_eq!(
        combat_latch.matches("last_contact: float = -1.0").count(),
        1,
        "the package must own the combat contact timer"
    );
    let cast_outline = player
        .split_once("struct PlayerCastOutline {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .expect("read PlayerCastOutline body");
    for field in [
        "last_t: float = 0.0",
        "last_color: Color = WHITE",
        "pump_until: float = 0.0",
        "pump_color: Color = WHITE",
    ] {
        assert_eq!(
            cast_outline.matches(field).count(),
            1,
            "PlayerCastOutline must initialize `{field}` exactly once"
        );
    }
    for method in [
        "fn Player.summon_slot_pet(player",
        "fn Player.max_pets(player)",
        "fn Player.summon_label_spans(player",
        "fn Player.party_any_pet(player)",
        "fn Player.spell_pump_highlights_player(player)",
        "fn Player.spell_pump_outline(player",
        "fn Player.channel_outline(player",
    ] {
        assert!(
            sources.iter().any(|source| fs::read_to_string(source)
                .expect("read MAG source")
                .contains(method)),
            "missing Player-owned receiver declaration `{method}`"
        );
    }

    let main = fs::read_to_string(repo.join("mag/main.rock"))
        .expect("read MAG main source");
    for field in [
        "rmb_consumed: bool",
        "last_target: Entity<Mob>?",
        "target_cycle: [Entity<Mob>]",
        "target_jump_targets: [Entity<Mob>]",
        "selection: SelectionStore",
        "hovered_ground_item: Entity<GroundItem>?",
    ] {
        assert_eq!(
            main.matches(field).count(),
            1,
            "InteractionState must contain `{field}` exactly once"
        );
    }

    let quests = fs::read_to_string(repo.join("mag/quests.rock"))
        .expect("read MAG quest source");
    let quest_state = fs::read_to_string(repo.join("mag-simulation/quest.rock"))
        .expect("read simulation quest source");
    let quest_runtime = rock_struct_body(&quest_state, "QuestRuntimeState");
    for field in [
        "pending_events: [string]",
        "event_subscribers: Map<string, [string]>",
        "state_subscribers: Map<string, [string]>",
        "region_subscribers: [string]",
        "dependents: Map<string, [string]>",
        "last_region_place: Place?",
    ] {
        assert_eq!(
            quest_runtime.matches(field).count(),
            1,
            "QuestRuntimeState must contain `{field}` exactly once"
        );
    }
    for method in [
        "fn Player.is_murderer(player)",
        "fn Player.clear_combat_latch(player)",
        "fn Player.refresh_combat_latch(player)",
        "fn Player.in_combat(player)",
    ] {
        assert!(
            sources.iter().any(|source| fs::read_to_string(source)
                .expect("read MAG source")
                .contains(method)),
            "missing Player-owned receiver declaration `{method}`"
        );
    }
    let feedback = fs::read_to_string(repo.join("mag/combat_text.rock"))
        .expect("read MAG feedback source");
    assert!(
        feedback.contains("fn mag_player_feedback_follower_in(world: WorldState, target: Entity<Player>)"),
        "deferred Player feedback must re-resolve the Scene entity"
    );

    let progress = rock_struct_body(&quest_state, "ProgressState");
    for field in [
        "bank_gold: int",
        "events: Map<string, EventRecord>",
        "quest_rows: Arena<QuestProgress>",
        "quest_runs: Map<string, Entity<QuestProgress>>",
        "quest_tracked: [string]",
        "quest_collapsed: Map<string, bool>",
        "quest_quarantined: [JsonValue]",
    ] {
        assert_eq!(
            progress.matches(field).count(),
            1,
            "ProgressState must contain `{field}` exactly once"
        );
    }

    let run = fs::read_to_string(repo.join("mag/run.rock")).expect("read MAG run source");
    let world_levels = fs::read_to_string(repo.join("mag/world_levels.rock"))
        .expect("read MAG world source");
    for (constructed, domain) in [
        ("progress: ProgressState { quest_rows: b.arena() },", "progress"),
        ("quest_runtime: QuestRuntimeState {},", "quest runtime"),
    ] {
        assert_eq!(
            world_levels.matches(constructed).count(),
            1,
            "each new world must construct its own {domain} domain exactly once"
        );
    }
    for stale in ["ProgressState {", "quest_runtime_reset()"] {
        assert!(
            !run.contains(stale),
            "run reset still replaces world-owned state with `{stale}`"
        );
    }
    assert_eq!(
        run.matches("MAG_INTERACTION = InteractionState {}").count(),
        1,
        "run reset must replace the complete interaction domain exactly once"
    );
    assert_eq!(
        run.matches("run_ui_reset()").count(),
        1,
        "run reset must use the run UI's shared reset authority exactly once"
    );
    assert_eq!(
        run_ui.matches("MAG_RUN_UI = RunUiState {}").count(),
        1,
        "run UI reset must replace the complete domain exactly once"
    );
    for stale_reset in [
        "bank_open =",
        "shop_open =",
        "inventory_open =",
        "spell_panel_open =",
        "journal_open =",
        "quest_log_open =",
        "quest_tracker_open =",
        "quest_log_tab =",
        "quest_log_scroll =",
        "quest_tracker_scroll =",
        "quest_giver_open_id =",
        "quest_giver_confirm_id =",
        "quest_giver_open_npc =",
        "ui_menu_is_open =",
        "COMBAT_REPORT_OPEN =",
        "COMBAT_REPORT_DETAILS =",
    ] {
        assert!(
            !run.contains(stale_reset),
            "run reset still owns run UI field `{stale_reset}`"
        );
    }
    assert_eq!(
        quests.matches(".quest_runtime = QuestRuntimeState {}").count(),
        1,
        "quest runtime reset must replace the complete domain exactly once"
    );
    for stale_reset in [
        "player_gesture =",
        "player_gesture_timer =",
        "player_idle_ticks =",
        "player_loco =",
        "player_stop_ru =",
        "player_stride_hops =",
        "player_cast_words =",
        "branched_ritual_cast =",
        "player_cast_acceleration =",
        "branched_ritual_acquisition_event_seq =",
    ] {
        assert!(
            !run.contains(stale_reset),
            "run reset still owns Player field `{stale_reset}`"
        );
    }
    for stale_reset in [
        "potion_ready_at =",
        "bandage_slips =",
        "summon_slots =",
        "cast_outline =",
        "reset_potion_cooldown(",
        "reset_spell_pump_feedback(",
    ] {
        assert!(
            !run.contains(stale_reset),
            "run reset still owns migrated Player state `{stale_reset}`"
        );
    }

    let journal = fs::read_to_string(repo.join("mag/journal.rock"))
        .expect("read MAG journal source");
    for field in [
        "lines: [JournalLine]",
        "next_row_id: int",
        "next_cast_id: int",
        "active_cast_id: int",
        "submitted_contributions: int",
        "retired_contributions: int",
    ] {
        assert_eq!(
            journal.matches(field).count(),
            1,
            "JournalState must contain `{field}` exactly once"
        );
    }
    let journal_state = journal
        .split_once("struct JournalState {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .expect("read JournalState body");
    for derived in ["JOURNAL_DRAW_ORDER", "JOURNAL_DRAW_ROWS"] {
        assert!(
            !journal_state.contains(derived),
            "derived cache `{derived}` must not become semantic JournalState"
        );
    }
    assert_eq!(journal.matches("global journal_open: bool").count(), 0,
        "journal visibility belongs to RunUiState");
    for cache in [
        "global JOURNAL_DRAW_ORDER: [int]",
        "global JOURNAL_DRAW_ROWS: Map<int, JournalDrawRow>",
    ] {
        assert_eq!(
            journal.matches(cache).count(),
            1,
            "derived journal cache `{cache}` must remain separately owned"
        );
    }
    assert_eq!(
        journal.matches("MAG_JOURNAL = JournalState {}").count(),
        1,
        "the journal reset authority must replace the complete domain exactly once"
    );
    assert_eq!(
        run.matches("journal_reset()").count(),
        1,
        "run reset must consume the journal reset authority exactly once"
    );

    let save = fs::read_to_string(repo.join("mag/save.rock")).expect("read MAG save source");
    assert!(
        save.contains("fn Player.progress_to_json(player, progress: ProgressState,"),
        "save serialization must consume the typed progress domain explicitly"
    );
    let async_save = fs::read_to_string(repo.join("mag/save_async.rock"))
        .expect("read MAG asynchronous save source");
    assert_eq!(
        async_save
            .matches("save_async_metadata(world, player,")
            .count(),
        2,
        "both persistence paths must pass the world that owns the progress domain"
    );
    assert!(
        save.contains("fn Player.save_progress_migration(player, progress: ProgressState,"),
        "migration must forward an explicitly typed progress domain"
    );
    for (line_number, line) in save.lines().enumerate() {
        if line.contains("save_progress(player") && !line.contains("fn Player.save_progress") {
            assert!(
                line.contains("save_progress(player, world.progress")
                    || line.contains("save_progress(player, progress,"),
                "save caller at mag/save.rock:{} bypasses the progress domain: {line}",
                line_number + 1
            );
        }
    }
}

#[test]
fn mag_player_queries_are_receiver_methods() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("rock workspace root");
    let source = fs::read_to_string(repo.join("mag/player.rock")).expect("read MAG player source");
    let required = [
        "fn Player.is_paralyzed(player)",
        "fn Player.effective_armor(player)",
        "fn Player.shaped_progress(player)",
        "fn Player.sprite(player)",
        "fn Player.mana_per_sec(player)",
        "fn Player.tint(player)",
    ];
    for declaration in required {
        assert!(source.contains(declaration), "missing Player receiver declaration `{declaration}`");
    }
    let forbidden = [
        "fn player_is_paralyzed()",
        "fn player_effective_armor()",
        "fn player_shaped_progress()",
        "fn player_sprite()",
        "fn player_stamina_regen_per_sec()",
        "fn player_mana_per_sec()",
        "fn player_tint()",
    ];
    for declaration in forbidden {
        assert!(!source.contains(declaration), "prefix-only Player query remains `{declaration}`");
    }
}

#[test]
fn mag_world_presentation_state_has_explicit_owners() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("rock workspace root");
    let mut sources = Vec::new();
    rock_sources(&repo.join("mag"), &mut sources);
    let forbidden = [
        "global teleport_arrive_t:",
        "global teleport_arrive_at:",
        "global teleport_arrive_color:",
        "global world_marks:",
        "global world_mark_head:",
        "global DROP_PARTICLE_SEQ:",
        "teleport_arrive_t =",
        "teleport_arrive_at =",
        "teleport_arrive_color =",
        "world_marks =",
        "world_mark_head =",
        "    cloud = []",
        "struct Cloud {",
        "const CLOUD_SIZE:",
        "fn init_cloud()",
        "fn Player.update_cloud(player)",
        "fn Player.draw_cloud(player)",
        "decals: [WorldMark]",
        "decal_head: int",
        "let decals = state.decals",
    ];
    for source in &sources {
        let text = fs::read_to_string(source).expect("read MAG source");
        for declaration in forbidden {
            assert!(!text.contains(declaration), "ambient presentation state remains in {}: {declaration}", source.display());
        }
    }
    let player = fs::read_to_string(repo.join("mag/player.rock")).expect("read Player source");
    assert!(player.contains("teleport_arrival: TeleportArrivalState"));
    let teleport = fs::read_to_string(repo.join("mag/teleport.rock")).expect("read teleport source");
    let arrival_state = teleport
        .split_once("struct TeleportArrivalState {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .expect("read TeleportArrivalState body");
    for field in ["place: Place", "remaining: float", "color: Color"] {
        assert_eq!(arrival_state.matches(field).count(), 1, "TeleportArrivalState owns {field}");
    }
    assert!(teleport.contains("player.teleport_arrival.place = dest"));
    assert!(teleport.contains("player.teleport_arrival.remaining"));
    assert!(teleport.contains("editor_presented_tile_center(player.teleport_arrival.place.space"));
    let run = fs::read_to_string(repo.join("mag/run.rock")).expect("read run source");
    assert!(run.contains("player.update_teleport_arrival()"));
    let world = fs::read_to_string(repo.join("mag/world_levels.rock")).expect("read world source");
    let presentation = fs::read_to_string(repo.join("mag/world_presentation.rock"))
        .expect("read MAG world presentation source");
    assert!(presentation.contains(
        "@state(ownership=host_projection, world=exclude, realization=rebuild_host_projection)\n\
         global MAG_WORLD_PRESENTATION: MagWorldPresentation"
    ));

    let world_state = world
        .split_once("struct WorldState {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .expect("read WorldState body");
    let root = fs::read_to_string(repo.join("mag-world/world_root.rock"))
        .expect("read package world root");
    let root_state = root
        .split_once("pub struct MagWorldRoot<M> {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .expect("read MagWorldRoot body");
    let presentation_state = presentation
        .split_once("struct MagWorldPresentation {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .expect("read MagWorldPresentation body");
    let presentation_fields = [
        ("tasks", "presentation_tasks"),
        ("projectiles", "projectiles"),
        ("bolts", "bolts"),
        ("strikes", "strikes"),
        ("glyphs", "glyphs"),
        ("fractures", "fractures"),
        ("embers", "embers"),
        ("sparks", "sparks"),
        ("flashes", "flashes"),
        ("drop_particles", "drop_particles"),
        ("ritual_acquisition_motes", "ritual_acquisition_motes"),
        ("greater_heal_release_pulses", "greater_heal_release_pulses"),
        ("cast_hand_vfx", "cast_hand_vfx"),
        ("drop_particle_seq", "drop_particle_seq"),
    ];
    for (field, former_world_field) in presentation_fields {
        assert!(
            !world_state
                .lines()
                .any(|line| line.trim_start().starts_with(&format!("{former_world_field}:"))),
            "presentation field `{former_world_field}` remains in WorldState"
        );
        assert_eq!(
            presentation_state
                .lines()
                .filter(|line| line.trim_start().starts_with(&format!("{field}:")))
                .count(),
            1,
            "MagWorldPresentation must own `{field}` exactly once"
        );
    }

    for (field, record, projection) in [
        ("ground_items", "Arena<GroundItem>", "[[GroundItemPresentation]?]"),
        ("corpses", "Arena<Corpse>", "[[CorpsePresentation]?]"),
    ] {
        assert!(
            root_state
                .lines()
                .any(|line| line.trim() == format!("{field}: {record},")),
            "MagWorldRoot must own the authoritative {field} arena"
        );
        assert!(
            !world_state.lines().any(|line| line.trim_start().starts_with(&format!("{field}:"))),
            "WorldState must not duplicate the package root's {field} arena"
        );
        assert!(
            presentation_state
                .lines()
                .any(|line| line.trim() == format!("{field}: {projection} = [],")),
            "MagWorldPresentation must own the {field} host projection"
        );
    }

    let digest = fs::read_to_string(repo.join("mag/simulation_digest.rock"))
        .expect("read MAG simulation digest");
    let fields: Vec<_> = world_state
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with("//") || line.is_empty() {
                return None;
            }
            line.split_once(':').map(|(field, _)| field.trim())
        })
        .collect();
    assert_eq!(fields.len(), 26, "WorldState field census changed: {fields:?}");
    assert!(digest.contains("for space_handle in world.spaces"));
    assert!(digest.contains("let space = space_handle.get().unwrap()"));
    for field in fields {
        let digested = digest.contains(&format!("world.{field}"));
        let excluded = world.contains(&format!("DIGEST_EXCLUDE({field})"));
        assert!(
            digested || excluded,
            "WorldState field `{field}` is neither digested nor explicitly excluded"
        );
        assert!(
            !(digested && excluded),
            "WorldState field `{field}` is both digested and explicitly excluded"
        );
    }

    let blood = fs::read_to_string(repo.join("mag/blood.rock")).expect("read blood source");
    assert!(blood.contains("presentation.drop_particle_seq"));
}

#[test]
fn mag_has_one_canonical_simulation_tick_boundary() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("rock workspace root");
    let run = fs::read_to_string(repo.join("mag/run.rock")).expect("read MAG run source");

    assert_eq!(
        run.matches("fn mag_simulation_tick(").count(),
        1,
        "MAG must define exactly one canonical simulation tick"
    );
    let tick = rock_function_body(&run, "fn mag_simulation_tick(");
    let prepare = rock_function_body(&run, "fn mag_tick_prepare(");
    for call in [
        "coroutine_poller_update(world.root.tasks)",
        "flow_frame_reset()",
        "world_tick_all(world)",
        "player.apply_inventory_command_in(world, intents.inventory)",
    ] {
        assert!(prepare.contains(call), "tick preparation is missing authoritative call `{call}`");
    }
    for call in ["mag_world_tick_runner_in(world)", "world_tick_advance(tick, dt)"] {
        assert!(tick.contains(call), "canonical tick is missing runner delegation `{call}`");
    }
    let host_runner = rock_function_body(&run, "fn mag_world_tick_runner_in(");
    for call in ["prepare: mag_tick_prepare", "world_tick_runner(runtime.rows, world.root, context, hooks, shells)",
        "runtime.runner.unwrap().get().unwrap().actor_host = mag_tick_actor_host()"] {
        assert!(host_runner.contains(call), "shared runner construction is missing delegation `{call}`");
    }
    assert!(!host_runner.contains("world_tick_advance("), "acquiring a runner must not advance another tick");
    let package_tick = fs::read_to_string(repo.join("mag-world/world_tick.rock"))
        .expect("read package tick runner");
    let advance = rock_function_body(&package_tick, "pub fn world_tick_advance<M, H>(");
    assert!(advance.find("prepare(runner.host, dt)").unwrap()
        < advance.find("world_expedition_schedule_tick(runner, runner.stages.unwrap())").unwrap(),
        "runner preparation must precede the ordered package schedule");
    let runner = rock_function_body(&package_tick, "pub fn world_tick_runner<M, H>(");
    let actor_host = rock_function_body(&run, "fn mag_tick_actor_host(");
    assert!(actor_host.contains("player_input:") && actor_host.contains("mag_tick_actor_pre_mob(r.host)"));
    for call in ["input(r)", "world_tick_actors(r)",
        "world_doors_update(r.root.get().unwrap(), r.host, r.door_host, player, r.dt)"] {
        assert!(runner.contains(call), "package runner is missing authoritative stage `{call}`");
    }
    let phases = [
        ("mag_tick_actor_pre_mob", &["world_player_cast_intent_enqueue(queue, record, tick, ctx.intents.cast)",
            "WorldPlayerActionKind::FrameTarget", "WorldPlayerActionKind::FrameMovement",
            "world_player_actions_dispatch(world.root,"][..]),
        ("mag_tick_rows_pickup", &[][..]),
        ("mag_tick_spawners", &[][..]),
        ("mag_tick_player_recovery", &[][..]),
        ("mag_tick_player_actions_harvest_hazards", &[
            "player.update_player_statuses_in(world, dt)",
            "player.apply_bandage_intent_in(world, ctx.intents.consumables)",
            "mag_capture_status_presentation(world)",
            "player.update_walls_in(world, dt)",
            "world_ground_pickup_command(world.root,",
            "WorldGroundPickupCommand::Player(WorldGroundPickupPlayer {"][..]),
        ("mag_tick_death_cull", &["cull_dead_mobs_in(ctx.world)"][..]),
        ("mag_tick_corpses_respawns", &["tick_corpses(ctx.world, ctx.dt)"][..]),
        ("mag_tick_wisp_sacrifice", &[
            "active.player.tick_wisp_intent_in(ctx.world, ctx.dt)",
            "wisp_motes_tick_in(ctx.world, ctx.dt)"][..]),
    ];
    let schedule = fs::read_to_string(repo.join("mag-world/expedition_schedule.rock"))
        .expect("read package expedition schedule");
    let schedule_tick = rock_function_body(&schedule, "pub fn world_expedition_schedule_tick<W>(");
    let mut previous = 0;
    for (index, stage) in ["actor_pre_mob", "actors", "rows_pickup",
        "spawners", "player_recovery", "doors", "player_actions_harvest_hazards", "death_cull",
        "corpses_respawns", "wisp_sacrifice"].iter().enumerate() {
        let marker = format!("{stage}(context)");
        let position = schedule_tick.find(&marker)
            .unwrap_or_else(|| panic!("package schedule omits `{marker}`"));
        assert!(index == 0 || previous < position, "package schedule changed phase order");
        previous = position;
    }
    for (phase, calls) in &phases {
        if *phase != "mag_tick_actor_pre_mob" {
            assert!(host_runner.contains(phase), "shared runner does not register phase `{phase}`");
        }
        let body = rock_function_body(&run, &format!("fn {phase}("));
        for call in *calls {
            assert!(body.contains(call), "phase `{phase}` is missing authoritative call `{call}`");
        }
    }
    let player_slot = rock_function_body(&run, "fn mag_tick_actor_pre_mob(");
    assert_eq!(player_slot.matches("world_player_actions_dispatch(").count(), 1,
        "MAG must dispatch its player queue exactly once");
    let combat = fs::read_to_string(repo.join("mag/combat.rock"))
        .expect("read MAG cast intent facade");
    let cast_facade = rock_function_body(&combat, "fn PlayerActor.update_casting_in(");
    assert!(cast_facade.contains("world_player_cast_intent_enqueue("),
        "direct cast intents must enqueue on the shared FIFO");
    assert_eq!(cast_facade.matches("world_player_actions_dispatch(").count(), 1,
        "direct cast intents must use the shared dispatcher");
    for former in ["active.update_casting_in(", "player.apply_target_intent_in(", "player.update_movement_in("] {
        assert!(!player_slot.contains(former), "player input bypasses the shared queue: {former}");
    }
    let package_player = fs::read_to_string(repo.join("mag-world/expedition_player.rock"))
        .expect("read package player command host");
    let package_slot = rock_function_body(&package_player, "pub fn expedition_player_input(");
    assert_eq!(package_slot.matches("world_player_actions_dispatch(").count(), 1,
        "package steps must dispatch the same player queue exactly once");
    for presentation in [
        "update_projectiles()",
        "update_bolts()",
        "world_damage_canvas_blood_query_tick(player)",
        "present_wall_effects()",
        "update_world_ambient_fx()",
        "update_drop_particles()",
        "update_forked_ritual_presentation()",
        "player.update_wisp_intent()",
        "mag_present_outcomes(world)",
        "cast_glyphs_present(world)",
        "cast_hand_vfx_present(world)",
    ] {
        for body in [prepare, advance, runner, actor_host] {
            assert!(!body.contains(presentation), "presentation call `{presentation}` must stay outside the runner");
        }
        assert!(
            !tick.contains(presentation),
            "presentation call `{presentation}` must remain outside the canonical tick"
        );
        for (phase, _) in phases {
            let body = rock_function_body(&run, &format!("fn {phase}("));
            assert!(
                !body.contains(presentation),
                "presentation call `{presentation}` must remain outside phase `{phase}`"
            );
        }
    }
    let player_actions = rock_function_body(&run, "fn mag_tick_player_actions_harvest_hazards(");
    let status_owner = player_actions
        .find("player.update_player_statuses_in(world, dt)")
        .expect("status owner in canonical tick");
    assert!(
        !player_actions.contains("mag_present_status_effects(world)"),
        "simulation must not call the status presenter"
    );
    let status_sample = player_actions
        .find("mag_capture_status_presentation(world)")
        .expect("status sample in canonical tick");
    let late_wall_contact = player_actions
        .find("player.update_walls_in(world, dt)")
        .expect("late wall contact in canonical tick");
    assert!(
        status_owner < status_sample && status_sample < late_wall_contact,
        "status sampling must stay between its owner tick and late wall contact"
    );
    let wisp_phase = rock_function_body(&run, "fn mag_tick_wisp_sacrifice(");
    let wisp_intent = wisp_phase
        .find("active.player.tick_wisp_intent_in(ctx.world, ctx.dt)")
        .expect("authoritative wisp intent in canonical tick");
    let wisp_tick = wisp_phase
        .find("wisp_motes_tick_in(ctx.world, ctx.dt)")
        .expect("authoritative wisp integration in canonical tick");
    assert!(wisp_intent < wisp_tick,
        "wisp effectors must be prepared before authoritative integration");

    let update = rock_function_body(&run, "fn mag_run_update()");
    assert_eq!(
        update.matches("let tick_intent = if player_input { mag_tick_intent_from_input(player) } else { GRAPHICS_DETECT_IDLE_INTENT }").count(),
        1,
        "the host must construct the retained tick intent once when input is admitted"
    );
    assert_eq!(
        update.matches("mag_present_status_effects(world)").count(),
        1,
        "the host must present the sampled status state once"
    );
    assert_eq!(
        update.matches("mag_simulation_tick(world, tick_intent, dt)").count(),
        1,
        "the host must consume the canonical tick exactly once"
    );
    assert_eq!(
        update
            .matches("if !world_frozen() {\n        mag_simulation_tick(world, tick_intent, dt)\n        wisp_stage_presentation_in(world)\n    }")
            .count(),
        1,
        "the host must admit the canonical tick exactly once"
    );
    let host_tick = update.find("mag_simulation_tick(world, tick_intent, dt)").unwrap();
    let host_presenter = update.find("mag_present_status_effects(world)").unwrap();
    assert!(host_tick < host_presenter,
        "the host must present the pre-contact sample after simulation returns");
}

#[test]
fn mag_tick_intent_is_overwritten_before_the_live_tick_consumes_it() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("rock workspace root");
    let input = fs::read_to_string(repo.join("mag/input_intent.rock"))
        .expect("read MAG input adapter source");
    assert_eq!(
        input
            .matches("@state(ownership=host_projection, world=exclude, realization=rebuild_host_projection)\nglobal MAG_TICK_INTENT")
            .count(),
        1,
        "the retained per-frame input record must be host projection state"
    );
    let adapter = rock_function_body(&input, "fn mag_tick_intent_from_input(");
    for filler in [
        "mag_target_intent_fill_from_input(player)",
        "mag_cast_intent_fill_from_input(player)",
        "mag_movement_intent_fill_from_input(player)",
        "mag_consumable_intent_fill_from_input()",
        "mag_equipment_intent_fill_from_input()",
        "mag_summon_intent_fill_from_input()",
        "mag_inventory_command_fill_from_input()",
    ] {
        assert_eq!(
            adapter.matches(filler).count(),
            1,
            "the live input adapter must overwrite lane `{filler}` before returning"
        );
    }
    assert_eq!(
        adapter.matches("return MAG_TICK_INTENT").count(),
        1,
        "the adapter must return the one overwritten record"
    );

    let run = fs::read_to_string(repo.join("mag/run.rock")).expect("read MAG run source");
    let tick = rock_function_body(&run, "fn mag_simulation_tick(");
    assert!(
        !tick.contains("MAG_TICK_INTENT"),
        "the canonical tick must consume its argument, not the host input buffer"
    );
    let update = rock_function_body(&run, "fn mag_run_update()");
    let fill = update
        .find("let tick_intent = if player_input { mag_tick_intent_from_input(player) } else { GRAPHICS_DETECT_IDLE_INTENT }")
        .expect("host input adapter call");
    let consume = update
        .find("mag_simulation_tick(world, tick_intent, dt)")
        .expect("canonical tick call");
    assert!(fill < consume, "input must be filled before the live tick consumes it");
}

fn without_string_literals(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_string = false;
    let mut escaped = false;
    for ch in line.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
                out.push('"');
            }
        } else {
            if ch == '"' {
                in_string = true;
            }
            out.push(ch);
        }
    }
    out
}
