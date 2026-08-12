//! `resizable` decides whether the LOGICAL extent follows the container.
//!
//! `__sw_sync_canvas_size` runs once per rAF. Adopting the container's size is
//! what a resizable game wants and exactly what a non-resizable one must not get:
//! a game that does not opt in may lay out against its configured
//! `screen_size()`, and a responsive container must not silently move it.
//!
//! dpr is orthogonal and tracked for both -- it scales the buffer's resolution,
//! not the extent games measure against, and the output-density font atlas
//! depends on it reaching the runtime.
//!
//! Driven directly against the runtime under node -- the fn under test reads only
//! `__SW`, `window.devicePixelRatio` and the canvas element, so a stub of each
//! pins both arms without a browser.

use std::process::Command;

fn node_available() -> bool {
    Command::new("node").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

const CONFIG_W: i32 = 1280;
const CONFIG_H: i32 = 720;
// The container the page actually lays the canvas out at -- deliberately not the
// configured size, which is the whole point of the gate.
const CSS_W: i32 = 500;
const CSS_H: i32 = 300;

/// Boot `__SW` as if `stone_create_window_ex` had just run at CONFIG_WxCONFIG_H
/// at `boot_dpr`, then run one `__sw_sync_canvas_size` with the window reporting
/// `now_dpr`. Passing the two separately is what makes a dpr arm assert a real
/// rewrite instead of the sync's no-change early return.
fn sync_once(resizable: bool, boot_dpr: f64, now_dpr: f64) -> String {
    let program = format!(
        r#"
{runtime}
globalThis.window = {{ devicePixelRatio: {now_dpr} }};
__SW.canvas = {{
  width: {CONFIG_W} * {boot_dpr}, height: {CONFIG_H} * {boot_dpr}, style: {{}},
  getBoundingClientRect: function () {{ return {{ width: {CSS_W}, height: {CSS_H} }}; }},
}};
__SW.logicalW = {CONFIG_W}; __SW.logicalH = {CONFIG_H}; __SW.dpr = {boot_dpr};
__SW.resizable = {resizable};
__sw_sync_canvas_size();
process.stdout.write(JSON.stringify({{
  bufW: __SW.canvas.width, bufH: __SW.canvas.height,
  logicalW: __SW.logicalW, logicalH: __SW.logicalH,
  resized: !!__SW.resized,
}}));
"#,
        runtime = skull_codegen_js::JS_WEB_RUNTIME,
    );
    let out = Command::new("node").arg("-e").arg(&program).output().expect("spawn node");
    assert!(out.status.success(), "node failed:\n{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_non_resizable_game_keeps_its_configured_extent() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let json = sync_once(false, 1.0, 1.0);
    assert!(
        json.contains(&format!("\"logicalW\":{CONFIG_W}"))
            && json.contains(&format!("\"logicalH\":{CONFIG_H}")),
        "a non-resizable game must keep its configured {CONFIG_W}x{CONFIG_H} extent, \
         not adopt the {CSS_W}x{CSS_H} container: {json}"
    );
    assert!(
        json.contains(&format!("\"bufW\":{CONFIG_W}")),
        "and at dpr 1 its buffer stays the configured size: {json}"
    );
    assert!(
        json.contains("\"resized\":false"),
        "nothing changed, so no resize event: {json}"
    );
}

// dpr is NOT gated: a non-resizable game still gets a HiDPI buffer, because the
// output-density font atlas reads its density from exactly this. Without this
// arm the gate above passes on a sync that ignores dpr entirely -- which is what
// gating the whole function on `resizable` would have done, and the non-resizable
// arm would then have been the only thing left to catch it.
#[test]
fn a_non_resizable_game_still_tracks_dpr() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let json = sync_once(false, 1.0, 2.0);
    assert!(
        json.contains(&format!("\"logicalW\":{CONFIG_W}")),
        "dpr must not move the logical extent: {json}"
    );
    assert!(
        json.contains(&format!("\"bufW\":{}", CONFIG_W * 2))
            && json.contains(&format!("\"bufH\":{}", CONFIG_H * 2)),
        "a 2x display must still get a 2x buffer: {json}"
    );
}

// The positive arm: without it the gate above passes on a sync that does nothing
// at all, which would be a different bug wearing the same green.
#[test]
fn a_resizable_game_still_follows_its_container() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let json = sync_once(true, 1.0, 1.0);
    assert!(
        json.contains(&format!("\"bufW\":{CSS_W}")) && json.contains(&format!("\"bufH\":{CSS_H}")),
        "a resizable game must adopt the {CSS_W}x{CSS_H} container: {json}"
    );
    assert!(json.contains("\"resized\":true"), "and must report the resize: {json}");
}

// DPR still multiplies the buffer for a resizable game; the gate is about
// `resizable`, not about dropping HiDPI.
#[test]
fn a_resizable_game_scales_its_container_by_dpr() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let json = sync_once(true, 1.0, 2.0);
    assert!(
        json.contains(&format!("\"bufW\":{}", CSS_W * 2))
            && json.contains(&format!("\"logicalW\":{CSS_W}")),
        "a resizable game's buffer is the container times dpr, its logical extent the container: {json}"
    );
}
