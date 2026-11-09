//! Stone sim environment policy.

/// The headless window sims that never read pixels boot in. 320x180 is 16:9
/// like the 1920x1080 game default at a thirty-sixth of the pixels, so scene
/// render targets shrink to a few megabytes. A game's live window can have a
/// different aspect: window-derived camera framing must refresh on resize
/// before a worker entry runs. Four capture sims already run this size
/// by explicit choice (`frame_stream_capture`, `low_fps_nudge`,
/// `meme_capture_sound`), so it is a blessed sim size, not a new extreme.
pub(crate) const SIM_SMALL_WINDOW: &str = "320x180";

/// Declared-name prefixes of the engine's async pixel-readback family and its
/// FFI, plus the imgui test-geometry and synthetic-input families: every
/// `pixel_readback_*` wrapper, `stone_pixel_readback_*` call, `imgui_test_*`
/// UI automation helper (scroll/click/drag by presented position), and
/// `imgui_inject_*` synthetic input call (mouse buttons, text), present and
/// future. The families are large, uniformly prefixed, and call no shared
/// funnel, so a prefix covers their growth; over-matching stays on the safe
/// side (a lifecycle call keeps the window). The imgui families read or drive
/// presented geometry (item rects, visibility, widget input) rather than
/// pixels, but that geometry is window-derived exactly like a readback: the
/// seam proof showed a palette-click sim failing at every size below 1080p
/// while its siblings pass at 320x180, and the full proof showed a
/// synthetic-mouse toggle sim failing 6/20 at 320x180.
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

/// Declared names of the engine's pixel readers: captures, canvas, window and
/// depth readback and frame streams, plus the FFI
/// roots sims may call directly and `sim_submit_guard`, which every guarded
/// reader funnels through. The tail of the list reads presented-output-derived
/// state rather than pixels, which the full proof showed is equally
/// window-derived: the window size (`stone_get_window_size`, asserted 1280x800
/// by an agent sim), the live view scale (`scene_scale` reads 64 at 1080p but
/// 6.3 at 320x180; `resolve_view_for` resolves it), the presented UI geometry
/// census (`imgui_bodies_with_prefix`, `imgui_body_ran`, `imgui_total_vtx_count`;
/// a prop grid draws zero cards at 320x180), the synthetic mouse position
/// (`imgui_set_mouse_pos`, the non-inject-prefixed half of the input pair),
/// and the render-target capacity readers: the native storage queries
/// (`stone_canvas_capacity_*`, `stone_canvas_boot_*`, `stone_canvas_active_*`,
/// `stone_canvas_bound_as_target`), the pyramid step (`pyramid_active_step`
/// derives pyramid geometry from the capacity), and the capacity
/// re-specification (`ensure_scene_capacity`). Existence and the Rock
/// active-rect readers are deliberately not listed: `canvas_is_live` funnels
/// through `stone_canvas_exists`, and `canvas_active` is called by shared
/// helpers, so matching either keeps hundreds of sims that own fixed-size
/// canvases. Each name is unique workspace-wide (D-form methods desugar to
/// bare names, so no method is listed: a sim reading probe bytes created its
/// probe through `window_probe_new` in the same entry). The async readback,
/// imgui test-geometry and imgui synthetic-input families are covered by
/// [`SIM_PIXEL_PREFIXES`]; the scene capacity globals are covered by
/// [`SIM_CAPACITY_GLOBALS`]. A sim whose transitive callees name one of these
/// keeps the game's window; anything else boots in [`SIM_SMALL_WINDOW`]
/// unless it declares its own `STONE_WINDOW`.
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

/// Declared names of the scene capacity globals: the bloom pyramid levels and
/// the capacity the scene target set was built for
/// (`stone/engine/rendering/draw.rock`). Both follow the boot window: sample
/// boots 6 bloom levels at 1280x720 but 4 at 320x180, so a sim asserting a
/// level count or a capacity panics or fails small. A sim whose transitive
/// bodies reference one of these keeps the game's window, like a pixel reader.
/// Matched by declared (bare) name. The drawable size is matched by call
/// (`drawable_size`, `stone_get_drawable_size` in [`SIM_PIXEL_ROOTS`]), not by
/// its `DRAWABLE_SIZE` global, which shared helpers read: matching the global
/// keeps hundreds of sims that never query the size. Liveness and ownership
/// queries (`canvas_is_live`, `new_canvas`, `destroy_canvas`) do not match and
/// stay small.
pub(crate) static SIM_CAPACITY_GLOBALS: &[&str] = &["BLOOM_LEVELS", "SCENE_CAPACITY"];

/// Callee edges of one body: resolved calls and protocol dispatch from its
/// inference, plus function-valued references from a body scan (a named fn
/// passed as a value lowers to `Expr::Global`, never a call). Mirrors
/// `bone_lints`' edge walk; bodies without inference (extern decls, opaque
/// bodies) are leaves, matched by name at their callers instead.
fn sim_callee_edges(
    ws: &bone_semantic::inference::Workspace,
    mid: bone_core::prelude::ModuleId,
    fid: bone_core::prelude::Idx<bone_core::hir::Function>,
    name_map: &std::collections::HashMap<
        ustr::Ustr,
        Vec<(
            bone_core::prelude::ModuleId,
            bone_core::prelude::Idx<bone_core::hir::Function>,
        )>,
    >,
) -> Vec<(
    bone_core::prelude::ModuleId,
    bone_core::prelude::Idx<bone_core::hir::Function>,
)> {
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
    out
}

fn sim_collect_bound_next(
    bn: &bone_core::workspace::BoundNext,
    out: &mut Vec<(
        bone_core::prelude::ModuleId,
        bone_core::prelude::Idx<bone_core::hir::Function>,
    )>,
) {
    if let bone_core::workspace::BoundNext::Concrete { func, sub, .. } = bn {
        out.push(*func);
        for bn in sub.next.values() {
            sim_collect_bound_next(bn, out);
        }
    }
}

/// True when the sim at `(mid, fid)` can read pixels: one of
/// [`SIM_PIXEL_ROOTS`] is among its transitive callees (memoized across sims,
/// so one compile walks the shared call graph once). A new sim that captures
/// is classified by its own calls, never by a name list.
fn sim_reads_pixels(
    ws: &bone_semantic::inference::Workspace,
    mid: bone_core::prelude::ModuleId,
    fid: bone_core::prelude::Idx<bone_core::hir::Function>,
    memo: &mut std::collections::HashMap<
        (
            bone_core::prelude::ModuleId,
            bone_core::prelude::Idx<bone_core::hir::Function>,
        ),
        bool,
    >,
    visiting: &mut std::collections::HashSet<(
        bone_core::prelude::ModuleId,
        bone_core::prelude::Idx<bone_core::hir::Function>,
    )>,
    name_map: &std::collections::HashMap<
        ustr::Ustr,
        Vec<(
            bone_core::prelude::ModuleId,
            bone_core::prelude::Idx<bone_core::hir::Function>,
        )>,
    >,
) -> bool {
    if let Some(&known) = memo.get(&(mid, fid)) {
        return known;
    }
    if !visiting.insert((mid, fid)) {
        return false;
    }
    let callee = &ws.module(mid).functions[fid];
    let mut reads = sim_name_reads_pixels(skull_mir::mangle::declared_name(callee.name.as_str()));
    if !reads {
        for (callee_mid, callee_fid) in sim_callee_edges(ws, mid, fid, name_map) {
            if sim_reads_pixels(ws, callee_mid, callee_fid, memo, visiting, name_map) {
                reads = true;
                break;
            }
        }
    }
    visiting.remove(&(mid, fid));
    memo.insert((mid, fid), reads);
    reads
}

/// True when the sim at `(mid, fid)` references a scene capacity global: one
/// of [`SIM_CAPACITY_GLOBALS`] is named by its transitive bodies (memoized
/// like [`sim_reads_pixels`]). Data globals lower to `Expr::Global` exactly
/// like function-valued references; a name that resolves to no function is a
/// data reference and is matched by declared (bare) name.
fn sim_reads_capacity(
    ws: &bone_semantic::inference::Workspace,
    mid: bone_core::prelude::ModuleId,
    fid: bone_core::prelude::Idx<bone_core::hir::Function>,
    memo: &mut std::collections::HashMap<
        (
            bone_core::prelude::ModuleId,
            bone_core::prelude::Idx<bone_core::hir::Function>,
        ),
        bool,
    >,
    visiting: &mut std::collections::HashSet<(
        bone_core::prelude::ModuleId,
        bone_core::prelude::Idx<bone_core::hir::Function>,
    )>,
    name_map: &std::collections::HashMap<
        ustr::Ustr,
        Vec<(
            bone_core::prelude::ModuleId,
            bone_core::prelude::Idx<bone_core::hir::Function>,
        )>,
    >,
) -> bool {
    use bone_core::hir::{BodyOwner, Expr};
    if let Some(&known) = memo.get(&(mid, fid)) {
        return known;
    }
    if !visiting.insert((mid, fid)) {
        return false;
    }
    let mut reads = false;
    if let Some(body) = ws.module(mid).bodies.get(&BodyOwner::Function(fid)) {
        for (_, expr) in body.expr_arena.iter() {
            let name = match expr {
                Expr::Global(n) | Expr::SiblingRef(n) => Some(n),
                _ => None,
            };
            let Some(name) = name else { continue };
            if name_map.contains_key(name) {
                continue;
            }
            let declared = name.as_str().rsplit("::").next().unwrap_or(name.as_str());
            if SIM_CAPACITY_GLOBALS.contains(&declared) {
                reads = true;
                break;
            }
        }
    }
    if !reads {
        for (callee_mid, callee_fid) in sim_callee_edges(ws, mid, fid, name_map) {
            if sim_reads_capacity(ws, callee_mid, callee_fid, memo, visiting, name_map) {
                reads = true;
                break;
            }
        }
    }
    visiting.remove(&(mid, fid));
    memo.insert((mid, fid), reads);
    reads
}

pub(crate) fn apply(
    ws: &bone_semantic::inference::Workspace,
    sims: &mut [crate::skull_backend::SimDescriptor],
) {
    use bone_core::prelude::ModuleId;
    let mut name_map: std::collections::HashMap<
        ustr::Ustr,
        Vec<(ModuleId, bone_core::prelude::Idx<bone_core::hir::Function>)>,
    > = std::collections::HashMap::new();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (fid, f) in module.functions.iter() {
            name_map.entry(f.name).or_default().push((module.id, fid));
        }
    }
    let mut pixel_memo = std::collections::HashMap::new();
    let mut capacity_memo = std::collections::HashMap::new();
    let mut visiting = std::collections::HashSet::new();
    for sim in sims {
        if sim.render {
            sim.env_pairs
                .insert(0, ("SIM_DRAW".to_string(), "on_demand".to_string()));
        }
        // Skipped when the sim declares any `STONE_WINDOW` (an explicit
        // empty keeps the game's own size); prepended so the pair order
        // reads default-first.
        let explicit_window = sim.env_pairs.iter().any(|(k, _)| k == "STONE_WINDOW");
        if !explicit_window
            && !sim_reads_pixels(
                ws,
                sim.module,
                sim.function,
                &mut pixel_memo,
                &mut visiting,
                &name_map,
            )
            && !sim_reads_capacity(
                ws,
                sim.module,
                sim.function,
                &mut capacity_memo,
                &mut visiting,
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
