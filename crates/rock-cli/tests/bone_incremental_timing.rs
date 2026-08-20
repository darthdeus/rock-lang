//! Premise measurement for bone per-body eviction (Track 2,
//! `docs/bone-incremental-plan.md`). NOT a correctness gate -- a throwaway
//! profiler. Reads a real game project off disk and warms the real stdlib, so
//! it is `#[ignore]`'d; run explicitly:
//!
//!   cargo test -p rock-cli --test bone_incremental_timing -- --ignored --nocapture
//!
//! Question it answers: on a single-body edit, where does `rebuild_module`'s
//! time actually go? `expand_and_desugar` runs over the WHOLE combined set
//! (warm stdlib + every user module) every edit; `lower_and_infer` runs over
//! only the edited module. Track 2 shrinks the latter to one body -- but only
//! pays off if the latter dominates. Dump `phase_timings` to find out before
//! designing the src change.

use std::time::Instant;

fn warm_stdlib() -> Vec<skull_parser::Ast> {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = ustr::Ustr::from(format!("<bone-stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id)
                .unwrap_or_else(|e| panic!("stdlib {} must parse: {e:?}", f.name))
        })
        .collect()
}

/// Discover a project the same way `rock watch --backend bone` does: resolve
/// `stick.toml` deps (the stone engine etc.), so cross-module types like `Vec2`
/// are present. Mirrors `bone_watch`'s file-set construction.
fn project_files(path: &str) -> Vec<(String, String)> {
    let source_spec = rock_cli::common::path_to_source_spec(path);
    let sources =
        rock_cli::unified_compile::discover_sources(&source_spec).expect("discovery succeeds");
    sources
        .files
        .iter()
        .map(|f| {
            let f = f.borrow();
            (f.file.to_id_path(), f.contents.clone())
        })
        .collect()
}

#[test]
#[ignore = "measurement, not a gate; run with --ignored --nocapture"]
fn measure_body_edit_phase_breakdown() {
    let warm = warm_stdlib();
    let proj_rel = std::env::var("BONE_BENCH_PROJ").unwrap_or_else(|_| "mag".into());
    let file_suffix =
        std::env::var("BONE_BENCH_FILE").unwrap_or_else(|_| "combat.rock".into());
    let needle = std::env::var("BONE_BENCH_NEEDLE")
        .unwrap_or_else(|_| "return reagent_data(kind).name".into());

    let proj = format!("{}/../../{proj_rel}", env!("CARGO_MANIFEST_DIR"));
    let files = project_files(&proj);
    assert!(files.len() > 1, "{proj_rel} should have several modules");

    let mut session = bone_incremental::session::Session::new(warm, files.clone())
        .unwrap_or_else(|e| panic!("session warm failed: {}", e.message));

    // The module we will edit, and a single-body edit inside it (signature
    // untouched -> BodyOnly tier). Appending a comment inside one fn body flips
    // that body's hash and nothing else's.
    let target = files
        .iter()
        .find(|(p, _)| p.ends_with(&file_suffix))
        .unwrap_or_else(|| panic!("{file_suffix} present in {proj_rel}"));
    let needle = needle.as_str();
    assert_eq!(target.1.matches(needle).count(), 1, "edit anchor must be unique");
    let edited = target.1.replace(needle, &format!("{needle} // bench-edit"));

    // Warm the update path once (first edit allocates caches, ramps clocks),
    // then measure a second, content-distinct edit.
    let _ = session.update_file(&target.0, &edited).unwrap();
    let edited2 = target.1.replace(needle, &format!("{needle} // bench-edit-2"));

    let t = Instant::now();
    let result = session.update_file(&target.0, &edited2).unwrap();
    let wall = t.elapsed().as_secs_f64() * 1000.0;

    println!("\n=== bone body-edit phase breakdown ({proj_rel}/{file_suffix}) ===");
    println!("tier        : {:?}", result.tier);
    println!("reinferred  : {} bodies", result.reinferred.len());
    println!("wall        : {wall:.3} ms");
    let mut total = 0.0;
    for (label, dur) in &session.ws.phase_timings {
        let ms = dur.as_secs_f64() * 1000.0;
        total += ms;
        println!("  {label:<22} {ms:>8.3} ms");
    }
    println!("  {:<22} {total:>8.3} ms (sum of phases)", "TOTAL");
    println!("warm modules: {} | user files: {}", "stdlib", files.len());
}

/// Verify the incremental OVERLAY (incremental.lua) shape: emit_full -> body edit
/// -> emit_overlay produces (a) the changed function's definition, (b) the
/// CodeRef slot publication, and (c) accumulates
/// across edits (a second edit's overlay still carries the first). Run:
///   cargo test -p rock-cli --test bone_incremental_timing emit_overlay -- --ignored --nocapture
#[test]
#[ignore = "reads a real project off disk; run with --ignored"]
fn emit_overlay_changed_fn_plus_code_slot_accumulates() {
    let warm = warm_stdlib();
    let proj = concat!(env!("CARGO_MANIFEST_DIR"), "/../../games/match3");
    let files = project_files(proj);
    let mut session = bone_incremental::session::Session::new(warm, files.clone())
        .unwrap_or_else(|e| panic!("session warm failed: {}", e.message));

    let (_baseline, mut cache) = {
        let ws = session.workspace();
        let mid = skull::user_module_id(ws).expect("user module");
        skull::mir_emit::emit_full(ws, mid).expect("emit_full")
    };

    let target = files
        .iter()
        .find(|(p, _)| p.ends_with("main.rock"))
        .expect("match3 main.rock");
    let needle = "return random_int(0, NUM_TILE_TYPES)";
    assert_eq!(target.1.matches(needle).count(), 1, "edit anchor unique");

    // First body edit -> overlay carries the changed fn + its slot publication.
    let edit1 = target.1.replace(needle, &format!("{needle} // overlay-test-1"));
    let r1 = session.update_file(&target.0, &edit1).expect("update 1");
    assert_eq!(r1.tier, bone_incremental::session::UpdateTier::BodyOnly, "body edit");
    let ov1 = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &r1.reinferred)
        .expect("emit_overlay");

    assert!(ov1.lua.contains("\nfunction "), "overlay must redefine a fn:\n{}", &ov1.lua[..ov1.lua.len().min(400)]);
    assert!(ov1.lua.contains("__rock_code_slot_stage("), "overlay must publish CodeRef slots");
    assert!(!ov1.lua.contains("__rock_rebind_system"), "overlay must not emit a name-based registry repair");
    let lines1 = ov1.lua.lines().count();

    // Second body edit in a DIFFERENT spot -> overlay accumulates (still has the
    // first fn's redefinition too).
    let needle2 = "return grid[y * GRID_W + x]";
    if target.1.matches(needle2).count() == 1 {
        let edit2 = edit1.replace(needle2, &format!("{needle2} // overlay-test-2"));
        let r2 = session.update_file(&target.0, &edit2).expect("update 2");
        let ov2 = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &r2.reinferred)
            .expect("emit_overlay");
        assert!(
            ov2.lua.lines().count() >= lines1,
            "overlay should accumulate, not shrink: {} -> {}",
            lines1,
            ov2.lua.lines().count()
        );
    }

    eprintln!("overlay 1: {lines1} lines (vs full main.lua ~{} lines)", _baseline.lua.lines().count());
    eprintln!("--- overlay head ---\n{}", &ov1.lua[..ov1.lua.len().min(600)]);
}
