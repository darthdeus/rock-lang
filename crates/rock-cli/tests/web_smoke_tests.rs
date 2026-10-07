//! stone-web Arc 2 MB gates, headless-Chrome tier (game-integration tier,
//! excluded from the default nextest profile like game_snapshot_tests -- runs
//! under `-P full`):
//!
//! - browser screenshots wait for runtime observables, then must not be blank
//!   (>= 8 distinct colors: board tiles + frame + background). Skips with a
//!   log when chrome is absent.
//!
//! The fast node/fake-GL half lives in `web_fake_gl_tests` and runs in the
//! DEFAULT profile -- see that file's header for why the split exists.

use std::path::PathBuf;

use crate::web_test_process;

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn build_web(game: &str, out: &std::path::Path) {
    let status = web_test_process::command(rock_bin())
        .current_dir(repo_root())
        .args(["web", game, "--out", out.to_str().unwrap()])
        .status()
        .expect("spawn rock web");
    assert!(status.success(), "rock web {game} failed");
    assert!(out.join("game.js").is_file(), "game.js not emitted");
    assert!(out.join("index.html").is_file(), "index.html not emitted");
}

#[test]
fn web_chrome_screenshot_nonblank_with_text() {
    run_cdp_screenshot_when_ready(
        "games/match3",
        "web_chrome_screenshot_nonblank_with_text",
        "match3 tiles",
    );
}

// Chess in real headless Chrome: 14 MB of book/opening/sound assets + font are
// fetched before main; the rendered board (light/dark tiles + pieces + side
// panels) must clear the color bar.
#[test]
fn web_chrome_screenshot_nonblank_chess() {
    run_cdp_screenshot_when_ready(
        "games/chess",
        "web_chrome_screenshot_nonblank_chess",
        "the chess board",
    );
}

// Build `game`, serve it, and drive it with tools/web/chrome_cdp_smoke.js.
// Returns the tempdir (keep it alive -- the screenshot lives in it) and the
// screenshot path, or None when node/chrome is absent.
fn run_cdp(game: &str, test: &str, args: &[&str]) -> Option<(tempfile::TempDir, PathBuf)> {
    if web_test_process::node()
        .arg("--version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("SKIP {test}: node not on PATH");
        return None;
    }
    if !web_test_process::chrome_available(&repo_root()) {
        eprintln!("SKIP {test}: no chrome on PATH");
        return None;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    build_web(game, dir.path());

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut server = web_test_process::command("python3")
        .args(["-m", "http.server", &port.to_string(), "-d"])
        .arg(dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn http.server");
    std::thread::sleep(std::time::Duration::from_millis(300));

    let shot = dir.path().join("shot.png");
    let out = web_test_process::node()
        .arg(repo_root().join("tools/web/chrome_cdp_smoke.js"))
        .arg(format!("http://127.0.0.1:{port}/index.html"))
        .arg(&shot)
        .args(args)
        .output()
        .expect("spawn cdp driver");
    let _ = server.kill();
    let _ = server.wait();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("SKIP: no chrome") {
        eprintln!("SKIP {test}: no chrome on PATH");
        return None;
    }
    assert!(
        out.status.success() && stdout.contains("PASS"),
        "{test} CDP smoke failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    Some((dir, shot))
}

fn run_cdp_screenshot_when_ready(game: &str, test: &str, subject: &str) {
    let Some((_dir, shot)) = run_cdp(
        game,
        test,
        &[
            "--frames",
            // The counter advances before frameFn, so 3 witnesses two returned frames.
            "3",
            "--ready-expr",
            "Object.keys((globalThis.__SW && __SW.fonts) || {}).length > 0",
        ],
    ) else {
        return;
    };
    assert_not_blank(&shot, subject);
}

fn assert_not_blank(shot: &std::path::Path, subject: &str) {
    let img = image::open(shot).expect("read screenshot").to_rgba8();
    let mut colors = std::collections::HashSet::new();
    for p in img.pixels() {
        colors.insert((p[0] / 16, p[1] / 16, p[2] / 16));
        if colors.len() >= 8 {
            break;
        }
    }
    assert!(
        colors.len() >= 8,
        "screenshot looks blank ({} distinct quantized colors); {subject} should produce many",
        colors.len()
    );
}

// Arc C box 1, WebGL2 leg. The same fixture the native sim drives
// (games/testbed/sampling.rock, sim explicit_sampling): one two-texel image
// drawn nearest AND linear in one frame, plus a load-time-linear control quad,
// measured by the ramp width at the magnified texel boundary. The verdict is
// computed in Rock so both backends assert identical claims -- the driver only
// flips the fixture on and compares one string.
//
// It reads back through stone_read_window_rect, which is why that has a WebGL2
// twin at all; before this the extern(js) decl had no implementation and any
// WindowProbe on the web threw.
#[test]
fn web_sampling_nearest_and_linear() {
    let Some((_dir, shot)) = run_cdp(
        "games/testbed",
        "web_sampling_nearest_and_linear",
        &[
            "--frames",
            "10",
            "--setup-expr",
            "__G.SAMPLING_REQUEST = true",
            "--expect",
            "__G.SAMPLING_VERDICT",
            "--value",
            "PASS",
        ],
    ) else {
        return;
    };
    assert_not_blank(&shot, "the three sampling quads");
}

// Arc C box 5, WebGL2 leg. Caller-owned rect batches and immediate draws bind
// their own sampler instead of inheriting the unit's, asserted through the same
// Rock-side verdict the native sim reads. See
// games/testbed/default_sampling.rock.
#[test]
fn web_default_sampling() {
    let Some((_dir, shot)) = run_cdp(
        "games/testbed",
        "web_default_sampling",
        &[
            "--frames",
            "10",
            "--setup-expr",
            "__G.DEFAULT_SAMPLING_REQUEST = true",
            "--expect",
            "__G.DEFAULT_SAMPLING_VERDICT",
            "--value",
            "PASS",
        ],
    ) else {
        return;
    };
    assert_not_blank(&shot, "the rect-batch and immediate quads");
}

// Arc 1 of the rich-text plan, WebGL2 leg. The same fixture the native sim
// drives (games/testbed/rich_text.rock, sim rich_text_spans): colored spans
// through one atlas and one batch, with the vertex-level structural claims and
// the two-colour pixel readback both computed in Rock so the two backends
// assert identical claims. Fake GL cannot stand in here -- its fallback font is
// glyphless, so every glyph population would be zero over zero.
#[test]
fn web_rich_text_spans() {
    let Some((_dir, shot)) = run_cdp(
        "games/testbed",
        "web_rich_text_spans",
        &[
            "--frames",
            "30",
            // The fixture walks RT_PHASE_A_STEPS structural steps at one per
            // frame, then needs the plate presented and the probe read back.
            // The default 10-frame settle questions it mid-walk.
            "--settle-frames",
            "40",
            "--setup-expr",
            "__G.RICH_REQUEST = true",
            "--expect",
            "__G.RICH_VERDICT",
            "--value",
            "PASS",
        ],
    ) else {
        return;
    };
    assert_not_blank(&shot, "the two-colour rich-text line");
}

#[test]
fn web_font_density_resize() {
    let Some((_dir, shot)) = run_cdp(
        "games/testbed",
        "web_font_density_resize",
        &[
            "--frames",
            "10",
            "--resize-dpr",
            "2",
            "--setup-expr",
            "__G.FONT_DENSITY_REQUEST = true",
            "--expect",
            "__G.FONT_DENSITY_VERDICT",
            "--value",
            "PASS",
        ],
    ) else {
        return;
    };
    assert_not_blank(&shot, "the density-aware font fixture");
}

#[test]
fn web_selected_font_world_metrics() {
    let Some(_) = run_cdp(
        "games/testbed",
        "web_selected_font_world_metrics",
        &[
            "--frames",
            "4",
            "--settle-frames",
            "3",
            "--setup-expr",
            "__G.FONT_METRICS_REQUEST = true",
            "--expect",
            "__G.FONT_METRICS_VERDICT",
            "--value",
            "PASS",
        ],
    ) else {
        return;
    };
}

// mag in real headless Chrome, CDP-driven: the virtual-time screenshot path
// deterministically races mag's first present (~175 MB preload; frame 1 does
// every shader compile + texture upload under swiftshader), so the driver
// (tools/web/chrome_cdp_smoke.js) waits for 30 REAL frames instead, then
// verifies PICKING end-to-end -- an injected click on the player must flow
// MRT FragId write -> R32I attachment -> async PBO readback -> SELECTION ->
// `last_click_target == ClickTarget::Player` -- and captures the screenshot
// for the non-blank color bar.
// Arc C box 4, WebGL2 leg. One canvas bound twice in a single pass -- slot 0
// nearest, slot 1 linear -- with the verdict computed in Rock so both backends
// assert identical claims. See games/testbed/pass_sampling.rock.
#[test]
fn web_pass_sampling() {
    // No screenshot check: this fixture renders into offscreen canvases, so the
    // window stays the empty testbed scene. The verdict IS the oracle.
    let Some(_) = run_cdp(
        "games/testbed",
        "web_pass_sampling",
        &[
            "--frames",
            "10",
            "--setup-expr",
            "__G.PASS_SAMPLING_REQUEST = true",
            "--expect",
            "__G.PASS_SAMPLING_VERDICT",
            "--value",
            "PASS",
        ],
    ) else {
        return;
    };
}

// mag's ARRAY BLOCK, in a real WebGL2 context, against a real driver.
//
// Nothing else reaches it in a browser: `just t`'s fake-GL harness records call
// shapes and STUBS the layout check outright, because that stub models no std140
// layout -- so it can say a block was bound, never that a driver agrees with the
// offsets the compiler planned, and never that anything filled it. Passing here
// is also the only confirmation that an array member's `name[0]` query resolves
// on a real driver: registration is transactional and publishes nothing when the
// layout check fails.
//
// **`unfilled == 0` alone proves nothing**, which is the trap this shape exists
// to avoid. That counter reports blocks DRAWN WITH but never filled; a block
// nobody draws is never stamped `drawn` and so can never contribute, and a build
// that registered binding 4 and never rendered through it reads zero and looks
// clean. `registered` and `drawn` are its denominators.
//
// All four numbers come from ONE run, so they describe the same frames. Split
// across two runs they would rebuild and re-render mag twice -- real cost
// against a full profile that already has contention timeouts -- while letting
// the premises drift apart.
//
// Read out of `__SW`, the JS runtime's OWN per-block record, which is the state
// the gate itself consults rather than a re-derivation of it.
//
// Separate from `web_chrome_screenshot_and_click_pick_mag` deliberately. That
// one is red (`docs/known-failing-tests.md`), and an already-red gate cannot
// certify anything underneath it -- an unfilled block would hide inside a
// failure the suite already tolerates.
#[test]
fn web_chrome_mag_array_block_is_registered_drawn_and_filled() {
    let Some(_) = run_cdp(
        "mag",
        "web_chrome_mag_array_block_is_registered_drawn_and_filled",
        &[
            "--frames",
            "30",
            // The two premises report as booleans and the two failure counts as
            // NUMBERS, so a mismatch names which one broke and by how much
            // rather than collapsing to `false`.
            "--expect",
            "(function(){var reg=0,drawn=0,unfilled=0,S=__SW.shaders;\
             for (var h in S) { var sh=S[h]; if(!sh||!sh.blocks) continue;\
             for (var i=0;i<sh.blocks.length;i++){ var b=sh.blocks[i];\
             if(!b||b.binding!==4) continue; reg++;\
             if(b.drawn) drawn++; if(b.drawn&&!b.uploaded) unfilled++; } }\
             return (reg>0)+\":\"+(drawn>0)+\":\"+unfilled+\":\"+\
             stone_unfilled_uniform_block_count();})()",
            // registered : drawn : unfilled-at-binding-4 : unfilled-anywhere
            "--value",
            "true:true:0:0",
        ],
    ) else {
        return;
    };
}

#[test]
fn web_chrome_screenshot_and_click_pick_mag() {
    let Some((_dir, shot)) = run_cdp(
        "mag",
        "web_chrome_screenshot_and_click_pick_mag",
        &[
            "--frames",
            "30",
            // mag's web canvas is 1920x1080, so the default 1280x720 window
            // leaves the right third of the scene outside the viewport -- and a
            // click there is a silent miss rather than an error.
            "--window-size",
            "1920,1080",
            // Derive the click from the player's LIVE projection, never a
            // literal pixel: a hard-coded point silently becomes a test of the
            // current framing and goes stale whenever camera framing or present
            // scaling moves (it did -- Arc A's FrameView work invalidated the
            // old 950,465). `render_pos + 1.5` is the sprite body rather than
            // the feet anchor, matching mag's own `player_body_probe_world`
            // test hook, and logical output units map to CSS through the canvas
            // element's rect so the expression holds at any devicePixelRatio.
            // A nobox `Vec2` return does NOT arrive as an array: leaf 0 is the
            // JS return value and leaf 1 rides `__rock_ret1`, so each slot must
            // be captured immediately after its own call, before the next call
            // republishes the bank. Indexing the result (`rp[0]`/`rp[1]`) reads
            // undefined and the whole expression collapses to [null, null].
            "--click-expr",
            "(function(){               var rpx=render_pos_Movable(__G.player.movable), rpy=__rock_ret1;               var sx=world_to_screen_Vec2(rpx,rpy+1.5), sy=__rock_ret1;               var c=document.querySelector('canvas');var r=c.getBoundingClientRect();               return [r.left+sx*(r.width/__SW.logicalW),                       r.top+sy*(r.height/__SW.logicalH)];})()",
            // Ask the game's OWN accessor rather than a mirror global: this is
            // the same call `pick_at_cursor` makes, so it exercises the real
            // path (MRT id write -> PBO readback -> SELECTION -> ClickTarget)
            // and answers `None` on a miss rather than undefined.
            //
            // Report whether anything was picked AT ALL, so the two failures are
            // told apart from the message alone:
            //   "nohover:None" -> the click landed off the sprite (a projection
            //                     or framing fault, since the point is derived).
            //   "hover:None"   -> something was picked and the click path
            //                     dropped it; that is a real pipeline defect.
            // A PAYLOADLESS variant emits as a bare string ("None", "Player");
            // only a payload-carrying one is an object with `__variant`. Read
            // both, or a unit variant reads as undefined.
            "--expect",
            "(function(){var t=pick_from_id_int(__G.HOVER_ENTITY_ID); \
             return (__G.HOVER_ENTITY_ID ? \"hover\" : \"nohover\") + \":\" + \
             (typeof t === \"string\" ? t : t.__variant);})()",
            "--value",
            "hover:Player",
        ],
    ) else {
        return;
    };
    assert_not_blank(&shot, "the mag scene");
}

#[test]
fn web_chrome_hot_reload_preserves_state() {
    if web_test_process::node()
        .arg("--version")
        .output()
        .map(|output| !output.status.success())
        .unwrap_or(true)
    {
        eprintln!("SKIP web_chrome_hot_reload_preserves_state: node not on PATH");
        return;
    }
    if !web_test_process::chrome_available(&repo_root()) {
        eprintln!("SKIP web_chrome_hot_reload_preserves_state: no chrome on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let project = dir.path().join("project");
    let out = dir.path().join("out");
    let ready = dir.path().join("ready");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("stick.toml"),
        "[package]\nname = \"hot_reload_smoke\"\n",
    )
    .unwrap();
    std::fs::write(
        project.join("main.rock"),
        "global STATE: int = 1\n@nomangle fn hot() -> int { 1 }\nfn main() { hot() }\n",
    )
    .unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut server = web_test_process::command(rock_bin())
        .current_dir(repo_root())
        .args([
            "web",
            project.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--watch",
            "--port",
            &port.to_string(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn web watch");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !out.join("index.html").is_file() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(out.join("index.html").is_file(), "web watch did not build");

    let driver = repo_root().join("tools/web/chrome_hot_reload_smoke.js");
    let mut smoke = web_test_process::node()
        .arg(driver)
        .arg(format!("http://127.0.0.1:{port}/index.html"))
        .arg(&ready)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn hot-reload smoke");
    while !ready.is_file() && std::time::Instant::now() < deadline {
        if smoke.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if !ready.is_file() {
        let output = smoke.wait_with_output().unwrap();
        let _ = server.kill();
        let _ = server.wait();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("SKIP: no chrome") {
            eprintln!("SKIP web_chrome_hot_reload_preserves_state: no chrome on PATH");
            return;
        }
        panic!("hot-reload smoke did not become ready:\n{stderr}");
    }

    std::fs::write(
        project.join("main.rock"),
        "global STATE: int = 1\n@nomangle fn hot() -> int { 2 }\nfn main() { hot() }\n",
    )
    .unwrap();
    let output = smoke.wait_with_output().unwrap();
    let _ = server.kill();
    let _ = server.wait();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("PASS"),
        "browser hot reload failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// `rock web --single-file` in a real browser, loaded from `file://` -- the one
/// place the single-file promise can actually be checked. The structural gates
/// in `web_package_tests` prove the page has no external references; only a
/// browser proves the embedded bytes are USABLE.
///
/// No HTTP server here on purpose. Every other test in this file serves over
/// HTTP because a fetch of a `file://` sibling is blocked; a single-file build
/// issues no fetch at all, so `file://` is exactly the case worth pinning and
/// it is what a player double-clicking the file gets.
///
/// chesslings rather than match3: match3 stages only the fallback font, so it
/// would pass with image embedding broken.
///
/// Failure mode to expect: if the prelude stops preferring an inlined asset
/// over a fetch, this does not fail fast -- the file:// fetch never resolves and
/// the run burns the driver's full 120s boot deadline before reporting. Measured
/// on that exact mutation (2.3s PASS -> 120s timeout).
#[test]
fn web_chrome_single_file_boots_from_a_file_url() {
    if web_test_process::node()
        .arg("--version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("SKIP single_file_boots: node not on PATH");
        return;
    }
    if !web_test_process::chrome_available(&repo_root()) {
        eprintln!("SKIP single_file_boots: no chrome on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let status = web_test_process::command(rock_bin())
        .current_dir(repo_root())
        .args(["web", "games/chesslings", "--single-file", "--out",
               dir.path().join("index.html").to_str().unwrap()])
        .status()
        .expect("spawn rock web --single-file");
    assert!(status.success(), "rock web --single-file failed");
    let page = dir.path().join("index.html");
    assert!(page.is_file(), "single-file page not emitted");

    let shot = dir.path().join("shot.png");
    let out = web_test_process::node()
        .arg(repo_root().join("tools/web/chrome_cdp_smoke.js"))
        .arg(format!("file://{}", page.display()))
        .arg(&shot)
        .args([
            "--frames", "20",
            // Asserts the DECODE, not just the bytes: an embedded font has to
            // reach FontFace and an embedded image createImageBitmap, and both
            // live in the fetch chain the inline path bypasses. Counting only
            // __ROCK_WEB_ASSETS keys would pass with that chain skipped.
            "--expect",
            "(function(){var n=0;for(var k in __ROCK_WEB_ASSETS)n++;\
             return [n > 1, Object.keys(__SW.imageBitmaps).length > 0,\
             Object.keys(__SW.fontFamilies || {}).length > 0].join(',');})()",
            "--value", "true,true,true",
        ])
        .output()
        .expect("spawn cdp driver");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("SKIP: no chrome") {
        eprintln!("SKIP single_file_boots: no chrome on PATH");
        return;
    }
    assert!(
        out.status.success() && stdout.contains("PASS"),
        "single-file file:// smoke failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_not_blank(&shot, "the chesslings scene");
}
