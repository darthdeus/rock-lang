//! The canvas-view capacity contract, on the JS canvas paths.
//!
//! A scene target is allocated at CAPACITY and rendered into its ACTIVE rect,
//! which is anchored at the GL origin. stone-rs `canvas.rs` therefore bounds-
//! checks and flips top-left-to-GL-row against the ACTIVE extent, never the
//! backing texture. The JS runtime has to agree, or a correct pixel reads the
//! wrong row -- which is exactly how web picking was broken.
//!
//! The Chrome smoke covers this end to end, but only through the ASYNC PBO
//! path (`stone_pixel_readback_submit`), which is what mag's picking uses. The
//! synchronous `stone_read_canvas_pixel` is never reached by it. These drive
//! both directly against a recorded-call GL stub with capacity strictly larger
//! than the active extent, so each one's bounds check and Y-flip are pinned
//! independently of the game.

fn node_available() -> bool {
    crate::common::node().arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

const CAP_W: i32 = 1920;
const CAP_H: i32 = 1080;
const ACT_W: i32 = 1624;
const ACT_H: i32 = 914;

/// Boot the web runtime against a fake GL that records `readPixels`, register
/// one canvas at capacity, set its active extent, then run `body`.
fn run(body: &str) -> String {
    let program = format!(
        r#"
{runtime}
// Drive the runtime's canvas fns directly against a recorded-call GL. No boot:
// these are the functions under test, and a real context would only add noise.
var __reads = [];
__SW.gl = new Proxy({{
  RGBA: 6408, FLOAT: 5126, UNSIGNED_BYTE: 5121, RGBA_INTEGER: 36249, INT: 5124,
  readPixels: function(x, y) {{ __reads.push([x, y]); }},
}}, {{
  get: function(t, k) {{
    if (k in t) return t[k];
    // Any other GL entry point is an unused no-op; any other enum is a number.
    return typeof k === "string" && k === k.toUpperCase()
      ? 0
      : function() {{ return {{}}; }};
  }},
}});
__SW.canvases[7] = {{ fbo: {{}}, tex: 1, w: {CAP_W}, h: {CAP_H}, format: 3 }};
stone_set_canvas_active(7, {ACT_W}, {ACT_H});
var out = {{}};
{body}
process.stdout.write(JSON.stringify(out));
"#,
        runtime = skull_codegen_js::JS_WEB_RUNTIME,
    );
    let out = crate::common::node().arg("-e").arg(&program).output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The row `readPixels` was asked for, for a top-left `y`, flipped against the
/// ACTIVE height the way stone-rs does.
fn expected_gl_row(top_left_y: i32) -> i32 {
    ACT_H - 1 - top_left_y
}

#[test]
fn sync_read_flips_against_the_active_height_not_the_backing_texture() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let json = run(
        "stone_read_canvas_pixel(7, 100, 200);\n\
         out.reads = __reads;",
    );
    let want = format!("[[100,{}]]", expected_gl_row(200));
    assert!(
        json.contains(&want),
        "sync readback must flip against the active height ({ACT_H}): wanted row {} in {json}\n\
         (flipping against the backing {CAP_H} would read row {})",
        expected_gl_row(200),
        CAP_H - 1 - 200,
    );
}

#[test]
fn web_remove_file_reports_missing_paths() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let json = run(
        "__ROCK_WEB_FILES.present = 'ok';\n\
         out.missing = stone_remove_file('missing');\n\
         out.present = stone_remove_file('present');\n\
         out.present_after = stone_file_exists('present');",
    );
    assert!(json.contains("\"missing\":false"), "missing web files must report failure: {json}");
    assert!(json.contains("\"present\":true"), "present web files must report success: {json}");
    assert!(json.contains("\"present_after\":false"), "successful removal must delete the file: {json}");
}

#[test]
fn web_audio_init_reports_unavailable_context() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let json = run(
        "out.audio = stone_audio_init();\n\
         out.unload_missing = stone_unload_sound(999);\n\
         __SW.audio.sounds[7] = {};\n\
         out.unload_present = stone_unload_sound(7);",
    );
    assert!(json.contains("\"audio\":false"), "missing AudioContext must be reported: {json}");
    assert!(json.contains("\"unload_missing\":false"), "missing sounds must report failure: {json}");
    assert!(json.contains("\"unload_present\":true"), "present sounds must unload successfully: {json}");
}

#[test]
fn sync_read_bounds_checks_against_the_active_extent() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    // Inside the backing texture but OUTSIDE the active rect: the pixel holds
    // no rendered content, so it must read as a miss and issue no GL read.
    let json = run(&format!(
        "out.justInsideX = stone_read_canvas_pixel(7, {}, 10);\n\
         out.outsideX = stone_read_canvas_pixel(7, {}, 10);\n\
         out.outsideY = stone_read_canvas_pixel(7, 10, {});\n\
         out.negative = stone_read_canvas_pixel(7, -1, 10);\n\
         out.reads = __reads;",
        ACT_W - 1,
        ACT_W,
        ACT_H,
    ));
    assert!(json.contains("\"outsideX\":0"), "x == active_w must miss: {json}");
    assert!(json.contains("\"outsideY\":0"), "y == active_h must miss: {json}");
    assert!(json.contains("\"negative\":0"), "negative x must miss: {json}");
    // Only the in-bounds probe may reach GL.
    let reads = json.split("\"reads\":").nth(1).unwrap_or("");
    assert_eq!(
        reads.matches('[').count(),
        2,
        "exactly one readPixels (the in-bounds probe) should reach GL: {json}"
    );
}

#[test]
fn async_submit_flips_and_bounds_check_against_the_active_extent() {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let json = run(&format!(
        "var slot = stone_pixel_readback_new(0);\n\
         stone_pixel_readback_submit(slot, 7, 100, 200);\n\
         stone_pixel_readback_submit(slot, 7, 10, {});\n\
         out.reads = __reads;",
        ACT_H,
    ));
    let want = format!("[100,{}]", expected_gl_row(200));
    assert!(
        json.contains(&want),
        "async submit must flip against the active height: wanted {want} in {json}"
    );
    // The out-of-active submit must not reach GL at all.
    let reads = json.split("\"reads\":").nth(1).unwrap_or("");
    assert_eq!(
        reads.matches('[').count(),
        2,
        "the submit outside the active rect must issue no readPixels: {json}"
    );
}
