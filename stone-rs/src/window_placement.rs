//! Remembered window placement.
//!
//! Precedence, high to low:
//!   env override > saved geometry > machine config [game.<slug>]
//!     > machine config [default] > game default (the requested size/pos)
//! then the fit clamp limits whatever this produces.
//!
//! Two files, split by who writes them (neither is checked into the repo):
//!   - saved geometry (engine-written state):
//!       $XDG_STATE_HOME|~/.local/state/stone/window-geometry.json
//!   - machine config (hand-written): $XDG_CONFIG_HOME|~/.config/stone/window.toml
//! One convention on every platform (macOS included), so there is a single
//! path to reason about. Headless never reads or writes either file.
//!
//! The machine config also carries `imgui_scale` (game section over default)
//! for a display that misreports its density; `STONE_IMGUI_SCALE` outranks it.
//!
//! The precedence merge + parsers are pure and unit-tested; the file/dir I/O is
//! a thin layer on top.

use std::path::PathBuf;

use crate::log_once;
use crate::window_report::DisplayReport;

/// A source of geometry that may specify any subset of the three fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PartialGeometry {
    pub size: Option<(i32, i32)>,
    pub pos: Option<(i32, i32)>,
    pub display: Option<i32>,
}

/// The concrete geometry request handed to window creation (before the fit).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedGeometry {
    pub size: (i32, i32),
    pub pos: (i32, i32),
    pub display: Option<i32>,
}

/// Window display mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowMode {
    Windowed,
    Maximized,
    Fullscreen(i32),
}

/// Windowed bounds remembered for returning from fullscreen/maximized or across hide/show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowedBounds {
    pub pos: (i32, i32),
    pub size: (i32, i32),
    pub display: Option<i32>,
}

/// Captured window state when hidden, preserving placement, size, and display mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HiddenWindowState {
    pub mode: WindowMode,
    pub pos: (i32, i32),
    pub size: (i32, i32),
    pub display: Option<i32>,
    pub windowed_bounds: WindowedBounds,
}

/// Resolve restored window placement when showing a previously hidden window.
/// Preserves monitor, position, size, and mode; if a monitor disappeared,
/// falls back to an available monitor using existing placement policy.
pub fn resolve_hidden_window_restore(
    saved: HiddenWindowState,
    displays: &[crate::window_report::DisplayReport],
    borders: crate::window_report::Borders,
) -> (
    WindowMode,
    crate::window_report::WindowPlacement,
    WindowedBounds,
) {
    let wb_req = crate::window_report::WindowRequest {
        size: saved.windowed_bounds.size,
        pos: saved.windowed_bounds.pos,
        display: saved.windowed_bounds.display,
    };
    let wb_placement = crate::window_report::resolve_window_placement(wb_req, displays, borders);
    let resolved_windowed_bounds = WindowedBounds {
        pos: wb_placement.pos,
        size: wb_placement.size,
        display: Some(wb_placement.display),
    };

    match saved.mode {
        WindowMode::Windowed => {
            let req = crate::window_report::WindowRequest {
                size: saved.size,
                pos: saved.pos,
                display: saved.display,
            };
            let placement = crate::window_report::resolve_window_placement(req, displays, borders);
            let bounds = WindowedBounds {
                pos: placement.pos,
                size: placement.size,
                display: Some(placement.display),
            };
            (WindowMode::Windowed, placement, bounds)
        }
        WindowMode::Maximized => {
            let target_display = saved
                .display
                .filter(|&d| displays.iter().any(|disp| disp.index == d))
                .unwrap_or(resolved_windowed_bounds.display.unwrap_or(0));
            let placement = crate::window_report::WindowPlacement {
                pos: resolved_windowed_bounds.pos,
                size: resolved_windowed_bounds.size,
                display: target_display,
            };
            (WindowMode::Maximized, placement, resolved_windowed_bounds)
        }
        WindowMode::Fullscreen(mode_code) => {
            let target_display = saved
                .display
                .filter(|&d| displays.iter().any(|disp| disp.index == d))
                .unwrap_or(resolved_windowed_bounds.display.unwrap_or(0));
            let placement = crate::window_report::WindowPlacement {
                pos: resolved_windowed_bounds.pos,
                size: resolved_windowed_bounds.size,
                display: target_display,
            };
            (
                WindowMode::Fullscreen(mode_code),
                placement,
                resolved_windowed_bounds,
            )
        }
    }
}

/// Which source won each field -- logged so a dev can see where a window's
/// placement came from without guessing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GeometryProvenance {
    pub size: Source,
    pub pos: Source,
    pub display: Source,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Env,
    Saved,
    ConfigGame,
    ConfigDefault,
    Default,
}

/// Merge the sources over a concrete base (the game default). `sources` is
/// ordered high precedence first; the first source that specifies a field wins,
/// otherwise the base value is kept. `labels` names each source for provenance.
pub fn merge_geometry(
    base: ResolvedGeometry,
    sources: &[(Source, PartialGeometry)],
) -> (ResolvedGeometry, GeometryProvenance) {
    let pick_size = sources.iter().find_map(|(s, g)| g.size.map(|v| (*s, v)));
    let pick_pos = sources.iter().find_map(|(s, g)| g.pos.map(|v| (*s, v)));
    let pick_display = sources.iter().find_map(|(s, g)| g.display.map(|v| (*s, v)));

    let (size_src, size) = pick_size.unwrap_or((Source::Default, base.size));
    let (pos_src, pos) = pick_pos.unwrap_or((Source::Default, base.pos));
    let (display_src, display) = match pick_display {
        Some((s, v)) => (s, Some(v)),
        None => (Source::Default, base.display),
    };

    (
        ResolvedGeometry { size, pos, display },
        GeometryProvenance {
            size: size_src,
            pos: pos_src,
            display: display_src,
        },
    )
}

/// Parse `STONE_WINDOW` (`WxH+X+Y` or `WxH`) and `STONE_WINDOW_DISPLAY` (`<i>`).
/// A malformed value yields None for that field (caller logs); this never
/// panics.
pub fn parse_env_geometry(window: Option<&str>, display: Option<&str>) -> PartialGeometry {
    let mut out = PartialGeometry::default();
    if let Some(w) = window {
        out = parse_window_spec(w.trim());
    }
    if let Some(d) = display {
        if let Ok(i) = d.trim().parse::<i32>() {
            out.display = Some(i);
        }
    }
    out
}

/// `WxH`, `WxH+X+Y`, or `WxH-X-Y` mixes -- the sign precedes each coordinate.
fn parse_window_spec(spec: &str) -> PartialGeometry {
    let mut out = PartialGeometry::default();
    // Split the WxH head from an optional signed +X+Y / -X-Y tail.
    let bytes = spec.as_bytes();
    let mut split = spec.len();
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (b == b'+' || b == b'-') {
            split = i;
            break;
        }
    }
    let (head, tail) = spec.split_at(split);
    if let Some((w, h)) = parse_pair(head, 'x') {
        out.size = Some((w, h));
    } else {
        return PartialGeometry::default();
    }
    if !tail.is_empty() {
        if let Some((x, y)) = parse_signed_pair(tail) {
            out.pos = Some((x, y));
        }
    }
    out
}

fn parse_pair(s: &str, sep: char) -> Option<(i32, i32)> {
    let (a, b) = s.split_once(sep)?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// Parse `+X+Y` / `-X-Y` (each coordinate carries its own leading sign).
fn parse_signed_pair(s: &str) -> Option<(i32, i32)> {
    // Find the sign that starts the second coordinate (skip index 0's sign).
    let bytes = s.as_bytes();
    let mut split = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (b == b'+' || b == b'-') {
            split = i;
            break;
        }
    }
    if split == 0 {
        return None;
    }
    let (xs, ys) = s.split_at(split);
    let x: i32 = xs.trim_start_matches('+').parse().ok()?;
    let y: i32 = ys.trim_start_matches('+').parse().ok()?;
    Some((x, y))
}

/// The hand-written machine config for one game: the `[game.<slug>]` and
/// `[default]` geometry partials (merged later with the other geometry sources)
/// and the ImGui scale, already resolved game-over-default.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MachineConfig {
    pub game: PartialGeometry,
    pub default: PartialGeometry,
    /// For a display whose reported density is wrong (X11 reporting 1x on a
    /// high-density panel). `None` keeps the engine's window-derived scale.
    pub imgui_scale: Option<f32>,
}

/// Parse the machine config TOML for `game_slug`. Returns an error string on
/// malformed TOML so the caller can log it (a present-but-broken config must
/// never be silently ignored).
pub fn parse_config_toml(toml_str: &str, game_slug: &str) -> Result<MachineConfig, String> {
    let value: toml::Value = toml::from_str(toml_str).map_err(|e| e.to_string())?;
    let default_table = value.get("default");
    let game_table = value.get("game").and_then(|g| g.get(game_slug));
    let partial = |t: Option<&toml::Value>| {
        t.map(partial_from_toml_table)
            .transpose()
            .map(Option::unwrap_or_default)
    };
    let scale = |t: Option<&toml::Value>| t.map(imgui_scale_from_toml_table).transpose();
    Ok(MachineConfig {
        game: partial(game_table)?,
        default: partial(default_table)?,
        imgui_scale: scale(game_table)?
            .flatten()
            .or(scale(default_table)?.flatten()),
    })
}

fn imgui_scale_from_toml_table(v: &toml::Value) -> Result<Option<f32>, String> {
    let Some(raw) = v.get("imgui_scale") else {
        return Ok(None);
    };
    let scale = raw
        .as_float()
        .or_else(|| raw.as_integer().map(|i| i as f64))
        .ok_or_else(|| "imgui_scale must be a number".to_string())?;
    if !(scale.is_finite() && scale > 0.0) {
        return Err(format!("imgui_scale must be positive, got {scale}"));
    }
    Ok(Some(scale as f32))
}

fn partial_from_toml_table(v: &toml::Value) -> Result<PartialGeometry, String> {
    let mut out = PartialGeometry::default();
    if let Some(p) = v.get("position") {
        out.pos = Some(toml_pair(p, "position")?);
    }
    if let Some(s) = v.get("size") {
        out.size = Some(toml_pair(s, "size")?);
    }
    if let Some(d) = v.get("display") {
        out.display = Some(
            d.as_integer()
                .ok_or_else(|| "display must be an integer".to_string())? as i32,
        );
    }
    Ok(out)
}

fn toml_pair(v: &toml::Value, field: &str) -> Result<(i32, i32), String> {
    let arr = v
        .as_array()
        .ok_or_else(|| format!("{field} must be a [x, y] array"))?;
    if arr.len() != 2 {
        return Err(format!("{field} must have exactly two integers"));
    }
    let a = arr[0]
        .as_integer()
        .ok_or_else(|| format!("{field}[0] must be an integer"))?;
    let b = arr[1]
        .as_integer()
        .ok_or_else(|| format!("{field}[1] must be an integer"))?;
    Ok((a as i32, b as i32))
}

/// A stable per-topology key: display count + each display's bounds (order
/// independent), FNV-1a hashed to hex. A laptop docked vs undocked hashes
/// differently, so each keeps its own remembered geometry.
pub fn topology_signature(displays: &[DisplayReport]) -> String {
    let mut bounds: Vec<(i32, i32, i32, i32)> = displays.iter().map(|d| d.bounds).collect();
    bounds.sort_unstable();
    let mut s = format!("n{}", displays.len());
    for b in bounds {
        s.push_str(&format!(";{},{},{},{}", b.0, b.1, b.2, b.3));
    }
    fnv1a_hex(&s)
}

fn fnv1a_hex(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in s.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", h)
}

/// Slug for a game key: lowercase alphanumerics only (spaces/punctuation
/// dropped). "Match 3" -> "match3". This is the `<slug>` in `[game.<slug>]`
/// and in the saved-geometry key.
pub fn game_slug(title: &str) -> String {
    title
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

// ============================================================================
// I/O layer (dirs + read/write). Kept thin; the logic above is what's tested.
// ============================================================================

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn state_dir() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_STATE_HOME") {
        return Some(PathBuf::from(x).join("stone"));
    }
    home_dir().map(|h| h.join(".local").join("state").join("stone"))
}

fn config_dir() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(x).join("stone"));
    }
    home_dir().map(|h| h.join(".config").join("stone"))
}

fn geometry_file() -> Option<PathBuf> {
    state_dir().map(|d| d.join("window-geometry.json"))
}

fn config_file() -> Option<PathBuf> {
    config_dir().map(|d| d.join("window.toml"))
}

/// Path to the engine-written saved-geometry file (may not exist yet), for
/// callers that want to tell the user where to look/reset it.
pub fn geometry_file_path() -> Option<PathBuf> {
    geometry_file()
}

/// Path to the hand-editable machine config file (may not exist yet).
pub fn config_file_path() -> Option<PathBuf> {
    config_file()
}

/// Read the env override partial (both vars, either may be absent/malformed).
pub fn env_geometry() -> PartialGeometry {
    let window = std::env::var("STONE_WINDOW").ok();
    let display = std::env::var("STONE_WINDOW_DISPLAY").ok();
    let out = parse_env_geometry(window.as_deref(), display.as_deref());
    if window
        .as_deref()
        .map(str::trim)
        .is_some_and(|w| !w.is_empty())
        && out.size.is_none()
    {
        log_once!(
            "STONE",
            "STONE_WINDOW='{}' is malformed (want WxH or WxH+X+Y); ignored",
            window.as_deref().unwrap_or("")
        );
    }
    out
}

/// Read the machine config for this game. Missing file: skip silently (the one
/// sanctioned absent-file fallback). Present-but-malformed: log an error and
/// skip -- never silent.
pub fn machine_config(game_slug: &str) -> MachineConfig {
    let Some(path) = config_file() else {
        return MachineConfig::default();
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return MachineConfig::default();
        }
        Err(e) => {
            ::log::error!(target: "STONE", "reading {} failed: {}", path.display(), e);
            return MachineConfig::default();
        }
    };
    match parse_config_toml(&text, game_slug) {
        Ok(config) => config,
        Err(e) => {
            ::log::error!(
                target: "STONE",
                "machine config {} is malformed: {} -- ignored",
                path.display(), e,
            );
            MachineConfig::default()
        }
    }
}

/// Read the saved geometry for (game, topology). Missing file / missing entry /
/// unreadable JSON: None (a corrupt state file is logged, then ignored -- it is
/// engine-owned and self-heals on the next write).
pub fn saved_geometry(
    game_slug: &str,
    topo: &str,
) -> Option<(PartialGeometry, Option<(i32, i32)>)> {
    let path = geometry_file()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            ::log::error!(target: "STONE", "reading {} failed: {}", path.display(), e);
            return None;
        }
    };
    let root: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            ::log::error!(target: "STONE", "saved geometry {} is corrupt: {} -- ignored", path.display(), e);
            return None;
        }
    };
    let entry = root.get(&saved_key(game_slug, topo))?;
    let x = entry.get("x")?.as_i64()? as i32;
    let y = entry.get("y")?.as_i64()? as i32;
    let w = entry.get("w")?.as_i64()? as i32;
    let h = entry.get("h")?.as_i64()? as i32;
    let display = entry
        .get("display")
        .and_then(|d| d.as_i64())
        .map(|d| d as i32);
    // The size the GAME asked for when this was written. Absent on entries
    // saved before it was recorded -- see drop_stale_saved_size.
    let req = match (
        entry.get("req_w").and_then(|v| v.as_i64()),
        entry.get("req_h").and_then(|v| v.as_i64()),
    ) {
        (Some(rw), Some(rh)) => Some((rw as i32, rh as i32)),
        _ => None,
    };
    Some((
        PartialGeometry {
            size: Some((w, h)),
            pos: Some((x, y)),
            display,
        },
        req,
    ))
}

/// A saved entry records the size the GAME asked for when it was written. If
/// the game asks for a different default now, a developer changed it in code
/// and that has to win over the remembered size -- otherwise editing
/// GameConfig does nothing at all and the only cure is deleting a state file
/// nobody remembers exists.
///
/// Only the SIZE is dropped. Position and display stay: changing a default
/// size is no reason to move a window off the monitor its user put it on.
///
/// A legacy entry (written before the request was recorded) carries no request
/// and cannot be shown to be fresh, so it is treated as stale. That costs one
/// re-placement per game and then self-heals on the next save.
pub fn drop_stale_saved_size(
    saved: PartialGeometry,
    saved_req: Option<(i32, i32)>,
    requested: (i32, i32),
) -> (PartialGeometry, bool) {
    if saved_req == Some(requested) {
        return (saved, false);
    }
    let dropped = saved.size.is_some();
    (
        PartialGeometry {
            size: None,
            ..saved
        },
        dropped,
    )
}

fn saved_key(game_slug: &str, topo: &str) -> String {
    format!("{}|{}", game_slug, topo)
}

/// Write the current window geometry for (game, topology), merging into the
/// existing file. Atomic (temp + rename). Errors are logged, never swallowed.
#[allow(clippy::too_many_arguments)]
pub fn save_geometry(
    game_slug: &str,
    topo: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    display: Option<i32>,
    requested: (i32, i32),
) {
    let Some(path) = geometry_file() else {
        ::log::error!(target: "STONE", "no HOME/XDG_STATE_HOME: cannot save window geometry");
        return;
    };
    let Some(dir) = path.parent() else { return };
    if let Err(e) = std::fs::create_dir_all(dir) {
        ::log::error!(target: "STONE", "creating {} failed: {}", dir.display(), e);
        return;
    }
    // Merge into whatever is there so other games'/topologies' entries survive.
    let mut root: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .filter(|v: &serde_json::Value| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    let mut entry = serde_json::json!({
        "x": x, "y": y, "w": w, "h": h,
        "req_w": requested.0, "req_h": requested.1,
    });
    if let Some(d) = display {
        entry["display"] = serde_json::json!(d);
    }
    root[saved_key(game_slug, topo)] = entry;
    let serialized = match serde_json::to_string_pretty(&root) {
        Ok(s) => s,
        Err(e) => {
            ::log::error!(target: "STONE", "serializing window geometry failed: {}", e);
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, serialized.as_bytes()) {
        ::log::error!(target: "STONE", "writing {} failed: {}", tmp.display(), e);
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        ::log::error!(target: "STONE", "renaming {} -> {} failed: {}", tmp.display(), path.display(), e);
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved_at(size: (i32, i32)) -> PartialGeometry {
        PartialGeometry {
            size: Some(size),
            pos: Some((40, 50)),
            display: Some(2),
        }
    }

    #[test]
    fn saved_size_survives_an_unchanged_request() {
        let (out, dropped) =
            drop_stale_saved_size(saved_at((1280, 720)), Some((1280, 720)), (1280, 720));
        assert_eq!(out.size, Some((1280, 720)));
        assert!(!dropped);
    }

    // The bug this exists for: the game's default moved and the remembered
    // size silently outranked it, so a code edit did nothing.
    #[test]
    fn a_changed_request_drops_the_saved_size_and_keeps_the_place() {
        let (out, dropped) =
            drop_stale_saved_size(saved_at((1280, 720)), Some((1280, 720)), (2560, 1440));
        assert_eq!(out.size, None);
        assert!(dropped);
        assert_eq!(
            out.pos,
            Some((40, 50)),
            "a resized default must not move the window"
        );
        assert_eq!(out.display, Some(2), "nor send it to another monitor");
    }

    #[test]
    fn a_legacy_entry_without_a_recorded_request_is_stale() {
        let (out, dropped) = drop_stale_saved_size(saved_at((1280, 720)), None, (2560, 1440));
        assert_eq!(out.size, None);
        assert!(dropped);
    }

    // End to end through the merge: the game default wins the SIZE while the
    // saved entry still wins position and display.
    #[test]
    fn a_dropped_size_falls_through_to_the_game_default() {
        let base = ResolvedGeometry {
            size: (2560, 1440),
            pos: (0, 0),
            display: None,
        };
        let (saved, _) = drop_stale_saved_size(saved_at((1280, 720)), Some((1280, 720)), base.size);
        let (res, prov) = merge_geometry(base, &[(Source::Saved, saved)]);
        assert_eq!(res.size, (2560, 1440));
        assert_eq!(prov.size, Source::Default);
        assert_eq!(res.pos, (40, 50));
        assert_eq!(prov.pos, Source::Saved);
    }

    fn base() -> ResolvedGeometry {
        ResolvedGeometry {
            size: (800, 700),
            pos: (0, 0),
            display: Some(1),
        }
    }

    #[test]
    fn merge_uses_base_when_no_source() {
        let (r, prov) = merge_geometry(base(), &[]);
        assert_eq!(r, base());
        assert_eq!(prov.size, Source::Default);
        assert_eq!(prov.pos, Source::Default);
        assert_eq!(prov.display, Source::Default);
    }

    #[test]
    fn merge_respects_precedence_per_field() {
        let env = PartialGeometry {
            size: Some((1280, 720)),
            pos: None,
            display: None,
        };
        let saved = PartialGeometry {
            size: Some((640, 480)),
            pos: Some((10, 20)),
            display: Some(0),
        };
        let (r, prov) = merge_geometry(base(), &[(Source::Env, env), (Source::Saved, saved)]);
        // env wins size; saved provides pos + display (env is silent on them).
        assert_eq!(r.size, (1280, 720));
        assert_eq!(r.pos, (10, 20));
        assert_eq!(r.display, Some(0));
        assert_eq!(prov.size, Source::Env);
        assert_eq!(prov.pos, Source::Saved);
        assert_eq!(prov.display, Source::Saved);
    }

    #[test]
    fn merge_config_game_beats_config_default() {
        let game = PartialGeometry {
            pos: Some((5, 5)),
            ..Default::default()
        };
        let default = PartialGeometry {
            pos: Some((99, 99)),
            size: Some((1024, 768)),
            ..Default::default()
        };
        let (r, prov) = merge_geometry(
            base(),
            &[(Source::ConfigGame, game), (Source::ConfigDefault, default)],
        );
        assert_eq!(r.pos, (5, 5));
        assert_eq!(prov.pos, Source::ConfigGame);
        assert_eq!(r.size, (1024, 768));
        assert_eq!(prov.size, Source::ConfigDefault);
    }

    #[test]
    fn env_parse_size_and_pos() {
        let g = parse_env_geometry(Some("1280x720+100+50"), None);
        assert_eq!(g.size, Some((1280, 720)));
        assert_eq!(g.pos, Some((100, 50)));
    }

    #[test]
    fn env_parse_size_only() {
        let g = parse_env_geometry(Some("800x600"), Some("2"));
        assert_eq!(g.size, Some((800, 600)));
        assert_eq!(g.pos, None);
        assert_eq!(g.display, Some(2));
    }

    #[test]
    fn env_parse_negative_pos() {
        let g = parse_env_geometry(Some("640x480-10-20"), None);
        assert_eq!(g.size, Some((640, 480)));
        assert_eq!(g.pos, Some((-10, -20)));
    }

    #[test]
    fn env_parse_malformed_is_empty() {
        assert_eq!(
            parse_env_geometry(Some("garbage"), None),
            PartialGeometry::default()
        );
        assert_eq!(parse_env_geometry(Some("100"), None).size, None);
    }

    #[test]
    fn config_parses_game_and_default() {
        let toml = r#"
[default]
position = [10, 20]
size = [1920, 1080]
display = 0

[game.match3]
position = [640, 480]
display = 1
"#;
        let MachineConfig { game, default, .. } = parse_config_toml(toml, "match3").unwrap();
        assert_eq!(default.pos, Some((10, 20)));
        assert_eq!(default.size, Some((1920, 1080)));
        assert_eq!(default.display, Some(0));
        assert_eq!(game.pos, Some((640, 480)));
        assert_eq!(game.display, Some(1));
        assert_eq!(game.size, None);
    }

    #[test]
    fn config_absent_game_yields_empty_game_partial() {
        let toml = "[default]\ndisplay = 0\n";
        let MachineConfig { game, default, .. } = parse_config_toml(toml, "nope").unwrap();
        assert_eq!(game, PartialGeometry::default());
        assert_eq!(default.display, Some(0));
    }

    #[test]
    fn config_malformed_is_error() {
        assert!(parse_config_toml("this is not = = toml", "x").is_err());
        assert!(parse_config_toml("[default]\nsize = [1]\n", "x").is_err());
        assert!(parse_config_toml("[default]\ndisplay = \"two\"\n", "x").is_err());
    }

    #[test]
    fn config_imgui_scale_game_section_beats_default() {
        let toml = "[default]\nimgui_scale = 2.0\n\n[game.mag]\nimgui_scale = 1.5\n";
        assert_eq!(
            parse_config_toml(toml, "mag").unwrap().imgui_scale,
            Some(1.5)
        );
        assert_eq!(
            parse_config_toml(toml, "other").unwrap().imgui_scale,
            Some(2.0)
        );
        // An integer is a scale too; a hand-written `2` must not be rejected.
        assert_eq!(
            parse_config_toml("[default]\nimgui_scale = 2\n", "x")
                .unwrap()
                .imgui_scale,
            Some(2.0)
        );
        assert_eq!(
            parse_config_toml("[default]\ndisplay = 2\n", "x")
                .unwrap()
                .imgui_scale,
            None
        );
    }

    #[test]
    fn config_imgui_scale_must_be_a_positive_number() {
        assert!(parse_config_toml("[default]\nimgui_scale = 0\n", "x").is_err());
        assert!(parse_config_toml("[default]\nimgui_scale = -1.0\n", "x").is_err());
        assert!(parse_config_toml("[default]\nimgui_scale = \"2\"\n", "x").is_err());
    }

    #[test]
    fn slug_strips_spaces_and_punct() {
        assert_eq!(game_slug("Match 3"), "match3");
        assert_eq!(game_slug("mag"), "mag");
        assert_eq!(game_slug("Bit-Gun!"), "bitgun");
    }

    fn disp(index: i32, bounds: (i32, i32, i32, i32)) -> DisplayReport {
        DisplayReport {
            index,
            name: String::new(),
            bounds,
            usable: bounds,
            dpi: (0.0, 0.0, 0.0),
            current_mode: None,
            mode_count: 0,
            modes: vec![],
        }
    }

    #[test]
    fn topology_signature_is_order_independent_and_count_sensitive() {
        let a = disp(0, (0, 0, 1440, 900));
        let b = disp(1, (1440, 0, 2560, 1440));
        let one = topology_signature(&[a.clone()]);
        let two = topology_signature(&[a.clone(), b.clone()]);
        let two_rev = topology_signature(&[b, a]);
        assert_ne!(one, two);
        assert_eq!(two, two_rev);
        assert_eq!(topology_signature(&[]), topology_signature(&[]));
    }

    #[test]
    fn windowed_restore_preserves_display_and_bounds() {
        let displays = vec![disp(0, (0, 0, 1920, 1080)), disp(1, (1920, 0, 1920, 1080))];
        let borders = crate::window_report::Borders {
            top: 30,
            left: 0,
            bottom: 0,
            right: 0,
        };
        let state = HiddenWindowState {
            mode: WindowMode::Windowed,
            pos: (2000, 100),
            size: (800, 600),
            display: Some(1),
            windowed_bounds: WindowedBounds {
                pos: (2000, 100),
                size: (800, 600),
                display: Some(1),
            },
        };
        let (mode, placement, bounds) = resolve_hidden_window_restore(state, &displays, borders);
        assert_eq!(mode, WindowMode::Windowed);
        assert_eq!(placement.display, 1);
        assert_eq!(placement.pos, (2000, 100));
        assert_eq!(placement.size, (800, 600));
        assert_eq!(bounds.display, Some(1));
        assert_eq!(bounds.pos, (2000, 100));
    }

    #[test]
    fn restore_when_monitor_disappears_falls_back_to_available_display() {
        // Window was on display 1 (at 2000, 100), but display 1 disappeared: only display 0 remains (0..1920)
        let displays = vec![disp(0, (0, 0, 1920, 1080))];
        let borders = crate::window_report::Borders {
            top: 30,
            left: 0,
            bottom: 0,
            right: 0,
        };
        let state = HiddenWindowState {
            mode: WindowMode::Windowed,
            pos: (2000, 100),
            size: (800, 600),
            display: Some(1),
            windowed_bounds: WindowedBounds {
                pos: (2000, 100),
                size: (800, 600),
                display: Some(1),
            },
        };
        let (mode, placement, bounds) = resolve_hidden_window_restore(state, &displays, borders);
        assert_eq!(mode, WindowMode::Windowed);
        assert_eq!(
            placement.display, 0,
            "must fall back to available display 0"
        );
        // Must clamp into display 0 (max_x = 1920 - 800 = 1120)
        assert!(placement.pos.0 >= 0 && placement.pos.0 <= 1120);
        assert_eq!(placement.size, (800, 600));
        assert_eq!(bounds.display, Some(0));
    }

    #[test]
    fn fullscreen_restore_falls_back_when_monitor_disappears() {
        let displays = vec![disp(0, (0, 0, 1920, 1080))];
        let borders = crate::window_report::Borders {
            top: 0,
            left: 0,
            bottom: 0,
            right: 0,
        };
        let state = HiddenWindowState {
            mode: WindowMode::Fullscreen(1),
            pos: (1920, 0),
            size: (1920, 1080),
            display: Some(1),
            windowed_bounds: WindowedBounds {
                pos: (2000, 100),
                size: (800, 600),
                display: Some(1),
            },
        };
        let (mode, placement, bounds) = resolve_hidden_window_restore(state, &displays, borders);
        assert_eq!(mode, WindowMode::Fullscreen(1));
        assert_eq!(
            placement.display, 0,
            "fullscreen must fall back to available display 0"
        );
        assert_eq!(bounds.display, Some(0));
    }
}
