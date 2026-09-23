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
        transition.matches("replay_job_ready(\"admission\", 1,").count(),
        1,
        "one admission transition authority must record readiness exactly once per attempt"
    );
    assert_eq!(
        run.matches("replay_job_ready(\"admission\", 1,").count(),
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
        "simulation: SpaceSimulationState",
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
        "player_dead".to_string(),
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
    ];
    let required = [
        "global MAG_PROGRESS: ProgressState",
        "global MAG_JOURNAL: JournalState",
        "global MAG_INTERACTION: InteractionState",
        "global MAG_QUEST_RUNTIME: QuestRuntimeState",
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
                if line.contains(field)
                    && !line.contains(qualified)
                    && !line.trim_start().starts_with(declaration)
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
    for field in [
        "players: Arena<Player>",
        "player: Entity<Player>",
        "tasks: CoroutinePoller",
        "wisp_motes: Arena<Mote>",
        "wisps: [MagicWisp]",
        "wisp_scatter_t: float",
        "wisp_rush_t: float",
        "wisp_effectors: [MoteEffector]",
        "wisp_mood: MoteMood",
        "tutorial: TutorialState",
    ] {
        assert!(world.contains(field), "WorldState is missing Scene-owned `{field}`");
    }

    let tutorial = fs::read_to_string(repo.join("mag/tutorial.rock"))
        .expect("read MAG tutorial source");
    let tutorial_state = tutorial
        .split_once("struct TutorialState {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .expect("read TutorialState body");
    for field in [
        "precast_dummy: Entity<Mob>?",
        "precast_dummy_tile: IVec2",
        "precast_entrance_tiles: [IVec2]",
        "precast_barrier_tiles: [IVec2]",
        "precast_highlight_next: float",
        "practice_dummies: [Entity<Mob>?]",
        "practice_dummy_tiles: [IVec2]",
        "practice_rewarded: [bool]",
        "practice_entrance_tiles: [IVec2]",
        "practice_exit_tiles: [IVec2]",
        "space: SpaceId",
        "space_ready: bool",
    ] {
        assert_eq!(
            tutorial_state.matches(field).count(),
            1,
            "TutorialState must contain `{field}` exactly once"
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
    assert!(
        world.contains("player.get().unwrap().entity = player"),
        "WorldState construction must publish the Player's self identity"
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
        "notoriety: PlayerNotoriety",
        "combat_latch: CombatLatchState",
        "summon_slots: [Entity<Mob>?]",
        "cast_outline: PlayerCastOutline",
    ] {
        assert_eq!(
            player_state.matches(field).count(),
            1,
            "scene-owned Player must contain `{field}` exactly once"
        );
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
        "attack_target: MobId?", "attack_timer: float", "bandage_active: bool",
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
        "summon_slots: [Entity<Mob>?] = [null, null, null, null]",
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
    for field in [
        "pending_events: [string]",
        "event_subscribers: Map<string, [string]>",
        "state_subscribers: Map<string, [string]>",
        "region_subscribers: [string]",
        "dependents: Map<string, [string]>",
        "last_region_place: Place?",
    ] {
        assert_eq!(
            quest_state.matches(field).count(),
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
        feedback.contains("fn mag_player_feedback_pos(target: Entity<Player>)"),
        "deferred Player feedback must re-resolve the Scene entity"
    );

    let progress = &quest_state;
    for field in [
        "bank_gold: int",
        "events: Map<string, EventRecord>",
        "quest_runs: Map<string, QuestProgress>",
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
    assert_eq!(
        run.matches("MAG_PROGRESS = ProgressState {}").count(),
        1,
        "run reset must replace the complete progress domain exactly once"
    );
    assert_eq!(
        run.matches("MAG_INTERACTION = InteractionState {}").count(),
        1,
        "run reset must replace the complete interaction domain exactly once"
    );
    assert_eq!(
        run.matches("quest_runtime_reset()").count(),
        1,
        "run reset must use the quest runtime's shared reset authority exactly once"
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
        quests.matches("MAG_QUEST_RUNTIME = QuestRuntimeState {}").count(),
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
            .matches("save_async_metadata(world, player, MAG_PROGRESS")
            .count(),
        2,
        "both persistence paths must pass the owning world and progress domain"
    );
    assert!(
        save.contains("fn Player.save_progress_migration(player, progress: ProgressState,"),
        "migration must forward an explicitly typed progress domain"
    );
    for (line_number, line) in save.lines().enumerate() {
        if line.contains("save_progress(player") && !line.contains("fn Player.save_progress") {
            assert!(
                line.contains("save_progress(player, MAG_PROGRESS")
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
        "CLOUD_SIZE",
        "fn init_cloud()",
        "fn Player.update_cloud(player)",
        "fn Player.draw_cloud(player)",
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
    for field in ["place: Place", "remaining: float", "color: Color"] {
        assert_eq!(teleport.matches(field).count(), 1, "TeleportArrivalState owns {field}");
    }
    assert!(teleport.contains("player.teleport_arrival.place = dest"));
    assert!(teleport.contains("player.teleport_arrival.remaining"));
    assert!(teleport.contains("editor_presented_tile_center(player.teleport_arrival.place.space"));
    let run = fs::read_to_string(repo.join("mag/run.rock")).expect("read run source");
    assert!(run.contains("player.update_teleport_arrival()"));
    let world = fs::read_to_string(repo.join("mag/world_levels.rock")).expect("read world source");
    assert!(world.contains("decals: [WorldMark]"));
    assert!(world.contains("decal_head: int"));
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
            world_state
                .lines()
                .any(|line| line.trim() == format!("{field}: {record},")),
            "WorldState must own the authoritative {field} arena"
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
    assert_eq!(fields.len(), 29, "WorldState field census changed: {fields:?}");
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
    assert!(blood.contains("world_space_index(world, space)"));
    assert!(blood.contains("world.spaces[index].decals"));
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
    for call in [
        "coroutine_poller_update(actor.tasks)",
        "flow_frame_reset()",
        "world_tick_all(world)",
        "actor.update_casting_in(world, intents.cast)",
        "player.apply_target_intent_in(world, intents.target)",
        "player.update_movement_in(world, intents.movement, dt)",
        "player.apply_inventory_command_in(world, intents.inventory)",
        "player.apply_bandage_intent_in(world, intents.consumables)",
        "player.update_walls_in(world, dt)",
        "actor.update_ground_pickup(world)",
        "actor.cull_dead_mobs(world)",
        "tick_corpses(world, dt)",
        "player.tick_wisp_intent_in(world, dt)",
        "wisp_motes_tick_in(world, dt)",
    ] {
        assert!(tick.contains(call), "canonical tick is missing authoritative call `{call}`");
    }
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
        assert!(
            !tick.contains(presentation),
            "presentation call `{presentation}` must remain outside the canonical tick"
        );
    }
    let status_owner = tick
        .find("player.update_player_statuses_in(world, dt)")
        .expect("status owner in canonical tick");
    assert!(
        !tick.contains("mag_present_status_effects(world)"),
        "simulation must not call the status presenter"
    );
    let status_sample = tick
        .find("mag_capture_status_presentation(world)")
        .expect("status sample in canonical tick");
    let late_wall_contact = tick
        .find("player.update_walls_in(world, dt)")
        .expect("late wall contact in canonical tick");
    assert!(
        status_owner < status_sample && status_sample < late_wall_contact,
        "status sampling must stay between its owner tick and late wall contact"
    );
    let wisp_intent = tick
        .find("player.tick_wisp_intent_in(world, dt)")
        .expect("authoritative wisp intent in canonical tick");
    let wisp_tick = tick
        .find("wisp_motes_tick_in(world, dt)")
        .expect("authoritative wisp integration in canonical tick");
    assert!(wisp_intent < wisp_tick,
        "wisp effectors must be prepared before authoritative integration");

    let update = rock_function_body(&run, "fn mag_run_update()");
    assert_eq!(
        update.matches("let tick_intent = mag_tick_intent_from_input(player)").count(),
        1,
        "the host must construct the retained tick intent once"
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
        .find("let tick_intent = mag_tick_intent_from_input(player)")
        .expect("host input adapter call");
    let consume = update
        .find("mag_simulation_tick(world, tick_intent, dt)")
        .expect("canonical tick call");
    assert!(fill < consume, "input must be filled before the live tick consumes it");
}
