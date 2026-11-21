//! Remembered window placement.
//!
//! Precedence, high to low:
//!   env override > saved geometry > machine config [game.<slug>]
//!     > machine config [default] > game default (the requested size/pos)
//! then the fit clamp limits whatever this produces.
//!
//! Two files, split by who writes them (neither is checked into the repo).
//! Saved geometry is `stone/window-geometry.json` under `$XDG_STATE_HOME`
//! (default `~/.local/state`). Machine config is `stone/window.toml` under
//! `$XDG_CONFIG_HOME` (default `~/.config`).
//!
//! On Windows, absent XDG overrides use the native LocalAppData (state) and
//! RoamingAppData (config) known folders. Unix defaults remain unchanged.
//! Headless never reads or writes either file.
//!
//! The machine config also carries `imgui_scale` (game section over default)
//! for a display that misreports its density; `STONE_IMGUI_SCALE` outranks it.
//!

use std::path::{Path, PathBuf};

use crate::log_once;
use crate::window_report::{
    resolve_window_placement, Borders, DisplayReport, WindowPlacement, WindowRequest,
};

/// A source of geometry that may specify any subset of the three fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PartialGeometry {
    pub size: Option<(i32, i32)>,
    pub pos: Option<(i32, i32)>,
    pub display: Option<i32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedGeometry {
    pub size: (i32, i32),
    pub pos: (i32, i32),
    pub display: Option<i32>,
}

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

pub fn resolve_hidden_window_restore(
    saved: HiddenWindowState,
    displays: &[DisplayReport],
    borders: Borders,
) -> (WindowMode, WindowPlacement, WindowedBounds) {
    let place = |size, pos, display| {
        let placement =
            resolve_window_placement(WindowRequest { size, pos, display }, displays, borders);
        let bounds = WindowedBounds {
            pos: placement.pos,
            size: placement.size,
            display: Some(placement.display),
        };
        (placement, bounds)
    };
    if saved.mode == WindowMode::Windowed {
        let (placement, bounds) = place(saved.size, saved.pos, saved.display);
        return (saved.mode, placement, bounds);
    }
    let windowed = saved.windowed_bounds;
    let (windowed, bounds) = place(windowed.size, windowed.pos, windowed.display);
    let display = saved
        .display
        .filter(|&d| displays.iter().any(|disp| disp.index == d))
        .unwrap_or(windowed.display);
    (
        saved.mode,
        WindowPlacement {
            display,
            ..windowed
        },
        bounds,
    )
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

/// `WxH`, `WxH+X+Y`, or `WxH-X-Y` mixes -- the sign precedes each coordinate.
fn parse_window_spec(spec: &str) -> PartialGeometry {
    let (head, tail) = split_at_sign(spec).unwrap_or((spec, ""));
    let Some(size) = parse_pair(head, 'x') else {
        return PartialGeometry::default();
    };
    let pos = split_at_sign(tail).and_then(|(x, y)| {
        Some((
            x.trim_start_matches('+').parse().ok()?,
            y.trim_start_matches('+').parse().ok()?,
        ))
    });
    PartialGeometry {
        size: Some(size),
        pos,
        display: None,
    }
}

fn parse_pair(s: &str, sep: char) -> Option<(i32, i32)> {
    let (a, b) = s.split_once(sep)?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// Splits before the first `+` or `-` past the first character.
fn split_at_sign(s: &str) -> Option<(&str, &str)> {
    let split = s
        .char_indices()
        .skip(1)
        .find(|&(_, c)| c == '+' || c == '-')?
        .0;
    Some(s.split_at(split))
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MachineConfig {
    pub game: PartialGeometry,
    pub default: PartialGeometry,
    /// For a display whose reported density is wrong (X11 reporting 1x on a
    /// high-density panel). `None` keeps the engine's window-derived scale.
    pub imgui_scale: Option<f32>,
}

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

pub fn game_slug(title: &str) -> String {
    title
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn stone_dir(xdg_var: &str, native: impl FnOnce() -> Option<PathBuf>) -> Option<PathBuf> {
    (std::env::var_os(xdg_var).map(PathBuf::from))
        .or_else(native)
        .map(|dir| dir.join("stone"))
}

fn state_dir() -> Option<PathBuf> {
    stone_dir("XDG_STATE_HOME", || {
        #[cfg(target_os = "windows")]
        {
            dirs::data_local_dir()
        }
        #[cfg(not(target_os = "windows"))]
        {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        }
    })
}

fn config_dir() -> Option<PathBuf> {
    stone_dir("XDG_CONFIG_HOME", || {
        #[cfg(target_os = "windows")]
        {
            dirs::config_dir()
        }
        #[cfg(not(target_os = "windows"))]
        {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
        }
    })
}

/// Path to the engine-written saved-geometry file (may not exist yet), for
/// callers that want to tell the user where to look/reset it.
pub fn geometry_file_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("window-geometry.json"))
}

/// Path to the hand-editable machine config file (may not exist yet).
pub fn config_file_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("window.toml"))
}

/// The file's text, or None when it is absent or unreadable (logged).
fn read_optional(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            ::log::error!(target: "STONE", "reading {} failed: {}", path.display(), e);
            None
        }
    }
}

/// Read the `STONE_WINDOW` override partial (absent or malformed is empty).
pub fn env_geometry() -> PartialGeometry {
    let window = std::env::var("STONE_WINDOW").ok();
    let out = window
        .as_deref()
        .map_or_else(PartialGeometry::default, |w| parse_window_spec(w.trim()));
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

pub fn machine_config(game_slug: &str) -> MachineConfig {
    let Some(path) = config_file_path() else {
        return MachineConfig::default();
    };
    let Some(text) = read_optional(&path) else {
        return MachineConfig::default();
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

pub fn saved_geometry(
    game_slug: &str,
    topo: &str,
) -> Option<(PartialGeometry, Option<(i32, i32)>)> {
    let path = geometry_file_path()?;
    let text = read_optional(&path)?;
    let root: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            ::log::error!(target: "STONE", "saved geometry {} is corrupt: {} -- ignored", path.display(), e);
            return None;
        }
    };
    let entry = root.get(saved_key(game_slug, topo))?;
    let x = entry.get("x")?.as_i64()? as i32;
    let y = entry.get("y")?.as_i64()? as i32;
    let w = entry.get("w")?.as_i64()? as i32;
    let h = entry.get("h")?.as_i64()? as i32;
    let display = entry
        .get("display")
        .and_then(|d| d.as_i64())
        .map(|d| d as i32);
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
pub fn save_geometry(
    game_slug: &str,
    topo: &str,
    geometry: ResolvedGeometry,
    requested: (i32, i32),
) -> bool {
    let Some(path) = geometry_file_path() else {
        ::log::error!(target: "STONE", "cannot resolve user state directory for window geometry");
        return false;
    };
    save_geometry_to(&path, &saved_key(game_slug, topo), geometry, requested)
}

fn save_geometry_to(
    path: &Path,
    key: &str,
    geometry: ResolvedGeometry,
    requested: (i32, i32),
) -> bool {
    let Some(dir) = path.parent() else {
        return false;
    };
    if let Err(e) = std::fs::create_dir_all(dir) {
        ::log::error!(target: "STONE", "creating {} failed: {}", dir.display(), e);
        return false;
    }
    let mut root: serde_json::Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .filter(|v: &serde_json::Value| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    let ResolvedGeometry {
        size: (w, h),
        pos: (x, y),
        display,
    } = geometry;
    let mut entry = serde_json::json!({
        "x": x, "y": y, "w": w, "h": h,
        "req_w": requested.0, "req_h": requested.1,
    });
    if let Some(d) = display {
        entry["display"] = serde_json::json!(d);
    }
    root[key] = entry;
    let serialized = match serde_json::to_string_pretty(&root) {
        Ok(s) => s,
        Err(e) => {
            ::log::error!(target: "STONE", "serializing window geometry failed: {}", e);
            return false;
        }
    };
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, serialized.as_bytes()) {
        ::log::error!(target: "STONE", "writing {} failed: {}", tmp.display(), e);
        return false;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        ::log::error!(target: "STONE", "renaming {} -> {} failed: {}", tmp.display(), path.display(), e);
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_write_reports_failure_and_preserves_other_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("\u{6f22}\u{1f680}").join("geometry.json");
        let at = |x, y, w, h, display| ResolvedGeometry {
            size: (w, h),
            pos: (x, y),
            display,
        };
        assert!(save_geometry_to(
            &path,
            "first",
            at(1, 2, 640, 480, None),
            (640, 480)
        ));
        assert!(save_geometry_to(
            &path,
            "second",
            at(3, 4, 800, 600, Some(1)),
            (800, 600)
        ));
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(json["first"]["x"], 1);
        assert_eq!(json["second"]["x"], 3);
        assert_eq!(json["second"]["display"], 1);
        assert!(json["first"].get("display").is_none());
        let unit = at(0, 0, 1, 1, None);
        assert!(!save_geometry_to(
            &path.join("blocked.json"),
            "first",
            unit,
            (1, 1)
        ));
        assert!(!save_geometry_to(dir.path(), "first", unit, (1, 1)));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_paths_without_unix_environment() {
        const CHILD: &str = "STONE_TEST_WINDOWS_PATHS";
        if std::env::var_os(CHILD).is_some() {
            let state = state_dir().expect("Windows native state directory");
            let config = config_dir().expect("Windows native config directory");
            assert!(state.is_absolute() && config.is_absolute());
            assert_eq!(state, dirs::data_local_dir().unwrap().join("stone"));
            assert_eq!(config, dirs::config_dir().unwrap().join("stone"));
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "window_placement::tests::windows_paths_without_unix_environment",
            ])
            .env(CHILD, "1")
            .env_remove("HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn saved_at(size: (i32, i32)) -> PartialGeometry {
        PartialGeometry {
            size: Some(size),
            pos: Some((40, 50)),
            display: Some(2),
        }
    }

    #[test]
    fn a_saved_size_is_kept_only_for_the_request_it_was_saved_under() {
        let saved = saved_at((1280, 720));
        let keep = drop_stale_saved_size(saved, Some((1280, 720)), (1280, 720));
        assert_eq!((keep.0.size, keep.1), (Some((1280, 720)), false));
        for recorded in [Some((1280, 720)), None] {
            let (out, dropped) = drop_stale_saved_size(saved, recorded, (2560, 1440));
            assert_eq!((out.size, dropped), (None, true), "{recorded:?}");
            assert_eq!((out.pos, out.display), (Some((40, 50)), Some(2)));
        }
    }

    fn base() -> ResolvedGeometry {
        ResolvedGeometry {
            size: (800, 700),
            pos: (0, 0),
            display: Some(1),
        }
    }

    #[test]
    fn merge_takes_each_field_from_its_first_source() {
        let (r, prov) = merge_geometry(base(), &[]);
        assert_eq!(r, base());
        assert_eq!(
            (prov.size, prov.pos, prov.display),
            (Source::Default, Source::Default, Source::Default)
        );

        let env = PartialGeometry {
            size: Some((1280, 720)),
            ..Default::default()
        };
        let saved = PartialGeometry {
            size: Some((640, 480)),
            pos: Some((10, 20)),
            display: Some(0),
        };
        let (r, prov) = merge_geometry(base(), &[(Source::Env, env), (Source::Saved, saved)]);
        assert_eq!((r.size, r.pos, r.display), ((1280, 720), (10, 20), Some(0)));
        assert_eq!(
            (prov.size, prov.pos, prov.display),
            (Source::Env, Source::Saved, Source::Saved)
        );
    }

    #[test]
    fn env_parse_reads_size_and_position() {
        let cases = [
            ("1280x720+100+50", Some((1280, 720)), Some((100, 50))),
            ("800x600", Some((800, 600)), None),
            ("640x480-10-20", Some((640, 480)), Some((-10, -20))),
        ];
        for (geometry, size, pos) in cases {
            let g = parse_window_spec(geometry);
            assert_eq!(
                (g.size, g.pos, g.display),
                (size, pos, None),
                "{geometry:?}"
            );
        }
    }

    #[test]
    fn env_parse_malformed_is_empty() {
        assert_eq!(parse_window_spec("garbage"), PartialGeometry::default());
        assert_eq!(parse_window_spec("100").size, None);
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
        let absent = parse_config_toml(toml, "nope").unwrap().game;
        assert_eq!(absent, PartialGeometry::default());
    }

    #[test]
    fn config_imgui_scale_game_section_beats_default() {
        let toml = "[default]\nimgui_scale = 2.0\n\n[game.demo]\nimgui_scale = 1.5\n";
        assert_eq!(
            parse_config_toml(toml, "demo").unwrap().imgui_scale,
            Some(1.5)
        );
        assert_eq!(
            parse_config_toml(toml, "other").unwrap().imgui_scale,
            Some(2.0)
        );
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
    fn config_rejects_malformed_toml_and_invalid_values() {
        for toml in [
            "this is not = = toml",
            "[default]\nsize = [1]\n",
            "[default]\ndisplay = \"two\"\n",
            "[default]\nimgui_scale = 0\n",
            "[default]\nimgui_scale = -1.0\n",
            "[default]\nimgui_scale = \"2\"\n",
        ] {
            assert!(parse_config_toml(toml, "x").is_err(), "accepted {toml:?}");
        }
    }

    #[test]
    fn slug_strips_spaces_and_punct() {
        assert_eq!(game_slug("Match 3"), "match3");
        assert_eq!(game_slug("demo"), "demo");
        assert_eq!(game_slug("Two-Words!"), "twowords");
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
        let one = topology_signature(std::slice::from_ref(&a));
        let two = topology_signature(&[a.clone(), b.clone()]);
        let two_rev = topology_signature(&[b, a]);
        assert_ne!(one, two);
        assert_eq!(two, two_rev);
        assert_eq!(topology_signature(&[]), topology_signature(&[]));
    }

    fn top_border(top: i32) -> crate::window_report::Borders {
        crate::window_report::Borders {
            top,
            left: 0,
            bottom: 0,
            right: 0,
        }
    }

    fn windowed_on_second_display() -> HiddenWindowState {
        HiddenWindowState {
            mode: WindowMode::Windowed,
            pos: (2000, 100),
            size: (800, 600),
            display: Some(1),
            windowed_bounds: WindowedBounds {
                pos: (2000, 100),
                size: (800, 600),
                display: Some(1),
            },
        }
    }

    #[test]
    fn restore_when_monitor_disappears_falls_back_to_available_display() {
        let displays = vec![disp(0, (0, 0, 1920, 1080))];
        let borders = top_border(30);
        let state = windowed_on_second_display();
        let (mode, placement, bounds) = resolve_hidden_window_restore(state, &displays, borders);
        assert_eq!(mode, WindowMode::Windowed);
        assert_eq!(
            placement.display, 0,
            "must fall back to available display 0"
        );
        assert!(placement.pos.0 >= 0 && placement.pos.0 <= 1120);
        assert_eq!(placement.size, (800, 600));
        assert_eq!(bounds.display, Some(0));
    }

    #[test]
    fn fullscreen_restore_falls_back_when_monitor_disappears() {
        let displays = vec![disp(0, (0, 0, 1920, 1080))];
        let borders = top_border(0);
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
