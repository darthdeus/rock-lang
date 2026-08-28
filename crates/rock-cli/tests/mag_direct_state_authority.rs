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
    ];
    let required = [
        "global MAG_PROGRESS: ProgressState",
        "global MAG_JOURNAL: JournalState",
        "global history: [SnapshotHistoryNode]",
        "global spell_panel_open: bool",
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
    ] {
        assert!(world.contains(field), "WorldState is missing Scene-owned `{field}`");
    }
    assert!(
        world.contains("player.get().unwrap().entity = player"),
        "WorldState construction must publish the Player's self identity"
    );

    let player = fs::read_to_string(repo.join("mag/player.rock"))
        .expect("read MAG player source");
    assert!(
        player.contains("entity: Entity<Player>?"),
        "Player must carry its generation-safe Scene identity for deferred work"
    );
    for field in [
        "dead: bool",
        "attack_target: Entity<Mob>?",
        "gesture: PlayerGesture",
        "gesture_timer: float",
        "idle_ticks: int",
        "loco: LocoState",
        "stop_ru: bool",
        "stride_hops: int",
        "loco_start_variant: PlayerStartVariant",
        "cast_words: CastWords",
        "ritual_cast: BranchedRitualCast",
        "cast_acceleration: PlayerCastAcceleration",
        "ritual_acquisition_event_seq: int",
        "notoriety: PlayerNotoriety",
        "combat_last_contact: float",
    ] {
        assert_eq!(
            player.matches(field).count(),
            1,
            "scene-owned Player must contain `{field}` exactly once"
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

    let progress = fs::read_to_string(repo.join("mag/progress.rock"))
        .expect("read MAG progress source");
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
    assert_eq!(
        journal.matches("global journal_open: bool").count(),
        1,
        "journal window visibility remains separate UI state"
    );
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
        save.contains("fn Player.progress_to_json(player, progress: ProgressState)"),
        "save serialization must consume the typed progress domain explicitly"
    );
    assert!(
        save.contains("save_progress(player, MAG_PROGRESS)"),
        "the persistence boundary must pass the authoritative progress domain"
    );
    for (line_number, line) in save.lines().enumerate() {
        if line.contains("save_progress(player") && !line.contains("fn Player.save_progress") {
            assert!(
                line.contains("save_progress(player, MAG_PROGRESS"),
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
        "fn Player.stamina_regen_per_sec(player)",
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
