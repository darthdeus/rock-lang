//! Stone sim environment policy.

use bone_core::hir::{BodyOwner, Callee, Expr, Function};
use bone_core::prelude::{Idx, ModuleId};
use bone_core::workspace::BoundNext;
use bone_semantic::inference::Workspace;
use rustc_hash::{FxHashMap, FxHashSet};

/// Use a 16:9 window for sims that do not measure pixels.
pub(crate) const SIM_SMALL_WINDOW: &str = "320x180";

/// Readback and presented UI geometry both depend on the window size.
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

/// These calls keep the normal window size, including queries of its render targets.
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

/// The number and capacity of scene buffers also depend on the window size.
pub(crate) static SIM_CAPACITY_GLOBALS: &[&str] = &["BLOOM_LEVELS", "SCENE_CAPACITY"];

type FnKey = (ModuleId, Idx<Function>);
struct WindowDependencies<'a> {
    ws: &'a Workspace,
    known: FxHashMap<FnKey, bool>,
    visited: FxHashSet<FnKey>,
    pending: Vec<FnKey>,
}

impl<'a> WindowDependencies<'a> {
    fn new(ws: &'a Workspace) -> Self {
        Self {
            ws,
            known: FxHashMap::default(),
            visited: FxHashSet::default(),
            pending: Vec::new(),
        }
    }

    fn needs_game_window(&mut self, root: FnKey) -> bool {
        self.pending.push(root);
        while let Some(key) = self.pending.pop() {
            let found = match self.known.get(&key) {
                Some(&known) => known,
                None if self.visited.insert(key) => self.scan(key),
                None => continue,
            };
            if found {
                self.known.insert(root, true);
                self.visited.clear();
                self.pending.clear();
                return true;
            }
        }
        // A cycle can reach a match via another pending branch.
        self.known
            .extend(self.visited.drain().map(|key| (key, false)));
        false
    }

    fn scan(&mut self, (mid, fid): FnKey) -> bool {
        let module = self.ws.module(mid);
        let name = module.functions[fid].name;
        if sim_name_reads_pixels(skull_mir::mangle::declared_name(name.as_str())) {
            return true;
        }
        let owner = BodyOwner::Function(fid);
        let edges = self.pending.len();
        if let Some(inferred) = self.ws.inference(mid).and_then(|i| i.per_fn.get(&owner)) {
            self.pending
                .extend(inferred.resolved_overload_calls.values().copied());
            self.pending
                .extend(inferred.implicit_iter_args.values().map(|w| w.func));
            for info in inferred.for_iter_protocol.values() {
                self.pending.extend(info.iter.iter().map(|w| w.func));
                collect_bound_next(&info.next, &mut self.pending);
            }
            for tree in inferred.bound_call_trees.values() {
                for next in tree.next.values() {
                    collect_bound_next(next, &mut self.pending);
                }
            }
        }
        if let Some(body) = module.bodies.get(&owner) {
            for (_, expr) in body.expr_arena.iter() {
                let name = match expr {
                    Expr::Global(name) | Expr::SiblingRef(name) => name,
                    Expr::Call {
                        callee: Callee::Unresolved { name, .. },
                        ..
                    } => name,
                    _ => continue,
                };
                let before = self.pending.len();
                self.pending.extend(
                    self.ws
                        .functions_by_name
                        .get(name)
                        .into_iter()
                        .flatten()
                        .copied()
                        .filter(|(mid, _)| *mid != ModuleId::builtin()),
                );
                if self.pending.len() == before
                    && matches!(expr, Expr::Global(_) | Expr::SiblingRef(_))
                    && SIM_CAPACITY_GLOBALS.contains(&name.as_str().rsplit("::").next().unwrap())
                {
                    return true;
                }
            }
        }
        self.pending[edges..].reverse();
        false
    }
}

fn collect_bound_next(next: &BoundNext, pending: &mut Vec<FnKey>) {
    if let BoundNext::Concrete { func, sub, .. } = next {
        pending.push(*func);
        for next in sub.next.values() {
            collect_bound_next(next, pending);
        }
    }
}

pub(crate) fn apply(ws: &Workspace, sims: &mut [crate::skull_backend::SimDescriptor]) {
    let mut dependencies = WindowDependencies::new(ws);
    for sim in sims {
        if sim.render {
            sim.env_pairs
                .insert(0, ("SIM_DRAW".into(), "on_demand".into()));
        }
        if !sim.env_pairs.iter().any(|(k, _)| k == "STONE_WINDOW")
            && !dependencies.needs_game_window((sim.module, sim.function))
        {
            sim.env_pairs
                .insert(0, ("STONE_WINDOW".into(), SIM_SMALL_WINDOW.into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::OnceLock;

    fn fixture_env_by_wire() -> HashMap<String, Vec<(String, String)>> {
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/sim-small-window");
        let out = tempfile::tempdir().expect("small-window output dir");
        let main = out.path().join("main.lua");
        let project = crate::skull_backend::IncrementalProject::init_with_tests(
            fixture.to_str().unwrap(),
            &main,
            &out.path().join("incremental.lua"),
        )
        .expect("the small-window fixture must compile");
        let mut sims = crate::skull_backend::collect_sim_descriptors(project.workspace());
        super::apply(project.workspace(), &mut sims);
        sims.into_iter()
            .map(|sim| (sim.wire_name, sim.env_pairs))
            .collect()
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
}
