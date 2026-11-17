//! Stone sim environment policy.

/// Use a 16:9 window for sims that do not measure pixels.
pub(crate) const SIM_SMALL_WINDOW: &str = "320x180";

/// Raw FFI readers cannot trigger a render on demand.
pub(crate) static SIM_PIXEL_PREFIXES: &[&str] = &[
    "pixel_readback",
    "stone_pixel_readback",
    "imgui_test_",
    "imgui_inject_",
];

/// Match an engine pixel reader or a readback-family prefix.
fn sim_name_reads_pixels(declared: &str) -> bool {
    SIM_PIXEL_ROOTS.contains(&declared)
        || SIM_PIXEL_PREFIXES
            .iter()
            .any(|prefix| declared.starts_with(prefix))
}

/// Pixel readers require rendering and complete image data.
pub(crate) static SIM_PIXEL_ROOTS: &[&str] = &[
    "take_screenshot",
    "window_probe_new",
    "read_canvas_pixel",
    "canvas_read_pixels",
    "canvas_read_region",
    "canvas_read_rgba8",
    "canvas_read_region_i32",
    "frame_stream_begin",
    "frame_stream_record",
    "sim_submit_guard",
    "stone_read_window_rect",
    "stone_save_screenshot",
    "stone_save_screenshot_sized",
    "stone_read_canvas_pixel",
    "stone_read_canvas_pixel_channel",
    "read_depth_pixel",
    "stone_read_depth_pixel",
    "stone_get_window_size",
    "imgui_set_mouse_pos",
    "scene_scale",
    "resolve_view_for",
    "imgui_total_vtx_count",
    "imgui_body_ran",
    "imgui_bodies_with_prefix",
    "pyramid_active_step",
    "ensure_scene_capacity",
    "stone_canvas_capacity_w",
    "stone_canvas_capacity_h",
    "stone_canvas_active_w",
    "stone_canvas_active_h",
    "stone_canvas_active_is_creation",
    "stone_canvas_bound_as_target",
];

/// Scene capacity affects the shape of rendered buffers.
pub(crate) static SIM_CAPACITY_GLOBALS: &[&str] = &["BLOOM_LEVELS", "SCENE_CAPACITY"];

type FnKey = (
    bone_core::prelude::ModuleId,
    bone_core::prelude::Idx<bone_core::hir::Function>,
);
type NameMap = std::collections::HashMap<ustr::Ustr, Vec<FnKey>>;

/// Callee edges of one body: resolved calls and protocol dispatch from its
/// inference, plus function-valued references from a body scan (a named fn
/// passed as a value lowers to `Expr::Global`, never a call). Mirrors
/// `bone_lints`' edge walk; bodies without inference (extern decls, opaque
/// bodies) are leaves, matched by name at their callers instead.
fn sim_callee_edges(
    ws: &bone_semantic::inference::Workspace,
    (mid, fid): FnKey,
    name_map: &NameMap,
) -> Vec<FnKey> {
    use bone_core::hir::{BodyOwner, Callee, Expr};
    let owner = BodyOwner::Function(fid);
    let mut out = Vec::new();
    if let Some(bi) = ws.inference(mid).and_then(|inf| inf.per_fn.get(&owner)) {
        out.extend(bi.resolved_overload_calls.values().copied());
        out.extend(bi.implicit_iter_args.values().map(|w| w.func));
        for info in bi.for_iter_protocol.values() {
            if let Some(w) = &info.iter {
                out.push(w.func);
            }
            sim_collect_bound_next(&info.next, &mut out);
        }
        for tree in bi.bound_call_trees.values() {
            for bn in tree.next.values() {
                sim_collect_bound_next(bn, &mut out);
            }
        }
    }
    if let Some(body) = ws.module(mid).bodies.get(&owner) {
        for (_, expr) in body.expr_arena.iter() {
            let name = match expr {
                Expr::Global(n) | Expr::SiblingRef(n) => Some(n),
                Expr::Call {
                    callee: Callee::Unresolved { name, .. },
                    ..
                } => Some(name),
                _ => None,
            };
            if let Some(fns) = name.and_then(|n| name_map.get(n)) {
                out.extend(fns.iter().copied());
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|key| seen.insert(*key));
    out
}

fn sim_collect_bound_next(bn: &bone_core::workspace::BoundNext, out: &mut Vec<FnKey>) {
    if let bone_core::workspace::BoundNext::Concrete { func, sub, .. } = bn {
        out.push(*func);
        for bn in sub.next.values() {
            sim_collect_bound_next(bn, out);
        }
    }
}

/// True when `matches` holds for `key` or any of its transitive callees.
/// Negative results are memoized only after the whole search is exhausted.
fn sim_reaches(
    ws: &bone_semantic::inference::Workspace,
    key: FnKey,
    matches: &dyn Fn(FnKey) -> bool,
    memo: &mut std::collections::HashMap<FnKey, bool>,
    visited: &mut std::collections::HashSet<FnKey>,
    name_map: &NameMap,
) -> bool {
    if let Some(&known) = memo.get(&key) {
        return known;
    }
    let mut pending = vec![key];
    while let Some(callee) = pending.pop() {
        if !visited.insert(callee) {
            continue;
        }
        match memo.get(&callee) {
            Some(true) => {}
            Some(false) => continue,
            None if matches(callee) => {}
            None => {
                pending.extend(sim_callee_edges(ws, callee, name_map).into_iter().rev());
                continue;
            }
        }
        memo.insert(key, true);
        visited.clear();
        return true;
    }
    memo.extend(visited.drain().map(|key| (key, false)));
    false
}

/// True when the function names one of [`SIM_PIXEL_ROOTS`] or a
/// [`SIM_PIXEL_PREFIXES`] family. A new sim that captures is classified by
/// its own calls, never by a name list.
fn names_pixel_reader(ws: &bone_semantic::inference::Workspace, (mid, fid): FnKey) -> bool {
    let callee = &ws.module(mid).functions[fid];
    sim_name_reads_pixels(skull_mir::mangle::declared_name(callee.name.as_str()))
}

/// True when the body references a scene capacity global, one of
/// [`SIM_CAPACITY_GLOBALS`]. Data globals lower to `Expr::Global` exactly
/// like function-valued references; a name that resolves to no function is a
/// data reference and is matched by declared (bare) name.
fn references_capacity(
    ws: &bone_semantic::inference::Workspace,
    (mid, fid): FnKey,
    name_map: &NameMap,
) -> bool {
    use bone_core::hir::{BodyOwner, Expr};
    let Some(body) = ws.module(mid).bodies.get(&BodyOwner::Function(fid)) else {
        return false;
    };
    body.expr_arena.iter().any(|(_, expr)| {
        let (Expr::Global(name) | Expr::SiblingRef(name)) = expr else {
            return false;
        };
        let declared = name.as_str().rsplit("::").next().unwrap_or(name.as_str());
        !name_map.contains_key(name) && SIM_CAPACITY_GLOBALS.contains(&declared)
    })
}

pub(crate) fn apply(
    ws: &bone_semantic::inference::Workspace,
    sims: &mut [crate::skull_backend::SimDescriptor],
) {
    use bone_core::prelude::ModuleId;
    let mut name_map = NameMap::new();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (fid, f) in module.functions.iter() {
            name_map.entry(f.name).or_default().push((module.id, fid));
        }
    }
    let reads_pixels = |key| names_pixel_reader(ws, key);
    let reads_capacity = |key| references_capacity(ws, key, &name_map);
    let mut pixel_memo = std::collections::HashMap::new();
    let mut capacity_memo = std::collections::HashMap::new();
    let mut visited = std::collections::HashSet::new();
    for sim in sims {
        if sim.render {
            sim.env_pairs
                .insert(0, ("SIM_DRAW".to_string(), "on_demand".to_string()));
        }
        // Skipped when the sim declares any `STONE_WINDOW` (an explicit
        // empty keeps the game's own size); prepended so the pair order
        // reads default-first.
        let key = (sim.module, sim.function);
        let explicit_window = sim.env_pairs.iter().any(|(k, _)| k == "STONE_WINDOW");
        if !explicit_window
            && !sim_reaches(
                ws,
                key,
                &reads_pixels,
                &mut pixel_memo,
                &mut visited,
                &name_map,
            )
            && !sim_reaches(
                ws,
                key,
                &reads_capacity,
                &mut capacity_memo,
                &mut visited,
                &name_map,
            )
        {
            sim.env_pairs.insert(
                0,
                ("STONE_WINDOW".to_string(), SIM_SMALL_WINDOW.to_string()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::run_sim::skull_compile_game_and_collect_sims;
    use std::collections::HashMap;
    use std::sync::OnceLock;

    fn fixture_env_by_wire() -> HashMap<String, Vec<(String, String)>> {
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/sim-small-window");
        let out = tempfile::tempdir().expect("small-window output dir");
        let main = out.path().join("main.lua");
        let (_wires, env_by_wire, _sources, _stone) =
            skull_compile_game_and_collect_sims(fixture.to_str().unwrap(), &main)
                .expect("the small-window fixture must compile");
        env_by_wire
    }

    fn env_by_wire() -> &'static HashMap<String, Vec<(String, String)>> {
        static CACHE: OnceLock<HashMap<String, Vec<(String, String)>>> = OnceLock::new();
        CACHE.get_or_init(fixture_env_by_wire)
    }

    /// The env of the sim declared as `short`: the collect key is the wire
    /// name, which under modules v1 is the sim's DefKey.
    fn env_for<'a>(
        env_by_wire: &'a HashMap<String, Vec<(String, String)>>,
        short: &str,
    ) -> &'a Vec<(String, String)> {
        let mut found = None;
        for (wire, env) in env_by_wire {
            let declared = wire.rsplit("::").next().unwrap_or(wire.as_str());
            if declared == short {
                assert!(found.is_none(), "two sims share the short name `{short}`");
                found = Some(env);
            }
        }
        found.unwrap_or_else(|| {
            panic!(
                "no sim named `{short}`; wires: {:?}",
                env_by_wire.keys().collect::<Vec<_>>()
            )
        })
    }

    /// Effective STONE_WINDOW: last declaration wins, like the runner.
    fn stone_window(env: &[(String, String)]) -> Option<&str> {
        env.iter()
            .rev()
            .find(|(k, _)| k == "STONE_WINDOW")
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn reachability_through_a_cycle_is_independent_of_query_order() {
        let fixture =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sim-reaches");
        let project = crate::live_project::LiveProject::init(fixture.to_str().unwrap(), false)
            .expect("the recursive fixture must compile");
        let ws = project.workspace();
        let mut name_map = super::NameMap::new();
        let mut keys = HashMap::new();
        for module in ws.source_modules() {
            for (fid, f) in module.functions.iter() {
                let key = (module.id, fid);
                name_map.entry(f.name).or_default().push(key);
                keys.insert(skull_mir::mangle::declared_name(f.name.as_str()), key);
            }
        }
        let outer = keys["outer"];
        let inner = keys["inner"];
        let target = keys["take_screenshot"];
        // Function-valued calls keep the body-scan edges in source order.
        assert_eq!(
            super::sim_callee_edges(ws, outer, &name_map),
            [inner, target]
        );
        for reads_pixels in [true, false] {
            for order in [["outer", "inner"], ["inner", "outer"]] {
                let mut memo = HashMap::new();
                let mut visited = std::collections::HashSet::new();
                for name in order {
                    assert_eq!(
                        super::sim_reaches(
                            ws,
                            keys[name],
                            &|key| reads_pixels && super::names_pixel_reader(ws, key),
                            &mut memo,
                            &mut visited,
                            &name_map,
                        ),
                        reads_pixels,
                        "reachability from `{name}` must be independent of query order"
                    );
                    assert!(visited.is_empty());
                }
            }
        }
    }

    #[test]
    fn sims_that_read_no_pixels_get_the_small_window() {
        for short in [
            "small_window_plain_sim",
            "small_window_canvas_owner",
            "small_window_native_reader",
            "small_window_active_reader",
            "small_window_drawable_reader",
            "small_window_drawable_ffi_reader",
        ] {
            assert_eq!(
                stone_window(env_for(env_by_wire(), short)),
                Some("320x180"),
                "`{short}` calls no window-derived helper, so it boots in the small window"
            );
        }
    }

    #[test]
    fn sims_that_capture_keep_the_game_default() {
        for short in [
            "small_window_pixel_reader",
            "small_window_indirect_reader",
            "small_window_readback_reader",
            "small_window_ui_reader",
            "small_window_window_reader",
            "small_window_mouse_reader",
            "small_window_inject_reader",
            "small_window_scale_reader",
            "small_window_view_reader",
            "small_window_geometry_reader",
            "small_window_capacity_reader",
            "small_window_pyramid_reader",
            "small_window_ensure_reader",
            "small_window_bloom_reader",
            "small_window_scene_capacity_reader",
        ] {
            assert_eq!(
                stone_window(env_for(env_by_wire(), short)),
                None,
                "`{short}` reads window-derived state, so the harness must not override its window"
            );
        }
    }

    #[test]
    fn an_explicit_window_size_wins_over_the_small_default() {
        assert_eq!(
            stone_window(env_for(env_by_wire(), "small_window_explicit_size")),
            Some("800x600"),
            "an explicit @sim_env STONE_WINDOW wins"
        );
    }

    #[test]
    fn every_pixel_root_name_resolves_in_a_stone_workspace() {
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/sim-small-window");
        let project = crate::live_project::LiveProject::init(fixture.to_str().unwrap(), true)
            .expect("the small-window fixture must compile");
        let mut names = std::collections::HashSet::new();
        for module in project.workspace().modules.iter() {
            for (_fid, f) in module.functions.iter() {
                let declared = f
                    .name
                    .as_str()
                    .rsplit("::")
                    .next()
                    .unwrap_or(f.name.as_str());
                names.insert(declared.to_string());
            }
        }
        for root in super::SIM_PIXEL_ROOTS {
            assert!(
                names.contains(*root),
                "pixel root `{root}` resolves to no function; the set drifted from the engine"
            );
        }
        for prefix in super::SIM_PIXEL_PREFIXES {
            assert!(
                names.iter().any(|n| n.starts_with(prefix)),
                "pixel prefix `{prefix}` matches no function; the set drifted from the engine"
            );
        }
    }

    #[test]
    fn every_capacity_global_name_resolves_in_a_stone_workspace() {
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/sim-small-window");
        let project = crate::live_project::LiveProject::init(fixture.to_str().unwrap(), true)
            .expect("the small-window fixture must compile");
        let mut names = std::collections::HashSet::new();
        for module in project.workspace().modules.iter() {
            for (_gid, g) in module.globals.iter() {
                let declared = g
                    .name
                    .as_str()
                    .rsplit("::")
                    .next()
                    .unwrap_or(g.name.as_str());
                names.insert(declared.to_string());
            }
        }
        for root in super::SIM_CAPACITY_GLOBALS {
            assert!(
                names.contains(*root),
                "capacity global `{root}` resolves to no global; the set drifted from the engine"
            );
        }
    }
}
