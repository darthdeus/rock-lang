//! stone-web Arc 2 MB gates, headless-Chrome tier (game-integration tier,
//! excluded from the default nextest profile like game_snapshot_tests -- runs
//! under `-P full`):
//!
//! - `web_chrome_screenshot_nonblank`: headless Chrome renders the build and
//!   the screenshot must not be blank (>= 8 distinct colors: board tiles +
//!   frame + background). Skips with a log when chrome is absent.
//!
//! The fast node/fake-GL half lives in `web_fake_gl_tests` and runs in the
//! DEFAULT profile -- see that file's header for why the split exists.

use std::path::PathBuf;
use std::process::Command;

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn build_web(game: &str, out: &std::path::Path) {
    let status = Command::new(rock_bin())
        .current_dir(repo_root())
        .args(["web", game, "--out", out.to_str().unwrap()])
        .status()
        .expect("spawn rock web");
    assert!(status.success(), "rock web {game} failed");
    assert!(out.join("game.js").is_file(), "game.js not emitted");
    assert!(out.join("index.html").is_file(), "index.html not emitted");
}

// One headless-Chrome screenshot smoke: build the game's web dir, serve it over
// HTTP (`fetch` of preloaded assets is CORS-blocked on file:// -- "serve with
// any static file server" is the deliverable's contract), screenshot after
// `budget_ms` of virtual time, and assert (a) the canvas2d glyph atlas baked
// (real TTF fetched + baked) and (b) the frame clears the >=8-quantized-color
// blank bar. Budget note: software-WebGL (swiftshader) boot can outlast a small
// budget (blank pre-first-frame capture), while a much larger one over-runs
// swiftshader's staging pool after the frame is already captured -- 10s works
// for match3/chess; mag's larger preload gets 30s.
fn run_chrome_screenshot(game: &str, budget_ms: u32, subject: &str) {
    let chrome = ["google-chrome", "google-chrome-stable", "chromium"]
        .iter()
        .find(|c| Command::new(c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false));
    let Some(chrome) = chrome else {
        eprintln!("SKIP chrome screenshot ({game}): no chrome on PATH");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    build_web(game, dir.path());

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut server = Command::new("python3")
        .args(["-m", "http.server", &port.to_string(), "-d"])
        .arg(dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn http.server");
    std::thread::sleep(std::time::Duration::from_millis(300));

    let shot = dir.path().join("shot.png");
    let out = Command::new(chrome)
        .args([
            "--headless=new",
            "--no-sandbox",
            "--enable-unsafe-swiftshader",
            "--enable-logging=stderr",
            "--v=0",
            "--window-size=1280,720",
        ])
        .arg(format!("--virtual-time-budget={budget_ms}"))
        .arg(format!("--screenshot={}", shot.display()))
        .arg(format!("http://127.0.0.1:{port}/index.html"))
        .output()
        .expect("spawn chrome");
    let _ = server.kill();
    let _ = server.wait();
    assert!(out.status.success(), "chrome exited non-zero ({game})");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("font atlas baked"),
        "canvas2d glyph atlas did not bake ({game}).\nchrome stderr tail:\n{}",
        &stderr[stderr.len().saturating_sub(2000)..]
    );

    let img = image::open(&shot).expect("read screenshot").to_rgba8();
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

#[test]
fn web_chrome_screenshot_nonblank_with_text() {
    run_chrome_screenshot("games/match3", 10000, "match3 tiles");
}

// Chess in real headless Chrome: 14 MB of book/opening/sound assets + font are
// fetched before main; the rendered board (light/dark tiles + pieces + side
// panels) must clear the color bar.
#[test]
fn web_chrome_screenshot_nonblank_chess() {
    run_chrome_screenshot("games/chess", 10000, "the chess board");
}

// Build `game`, serve it, and drive it with tools/web/chrome_cdp_smoke.js.
// Returns the tempdir (keep it alive -- the screenshot lives in it) and the
// screenshot path, or None when node/chrome is absent.
fn run_cdp(game: &str, test: &str, args: &[&str]) -> Option<(tempfile::TempDir, PathBuf)> {
    if Command::new("node").arg("--version").output().map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("SKIP {test}: node not on PATH");
        return None;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    build_web(game, dir.path());

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut server = Command::new("python3")
        .args(["-m", "http.server", &port.to_string(), "-d"])
        .arg(dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn http.server");
    std::thread::sleep(std::time::Duration::from_millis(300));

    let shot = dir.path().join("shot.png");
    let out = Command::new("node")
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

// mag in real headless Chrome, CDP-driven: the virtual-time screenshot path
// deterministically races mag's first present (~175 MB preload; frame 1 does
// every shader compile + texture upload under swiftshader), so the driver
// (tools/web/chrome_cdp_smoke.js) waits for 30 REAL frames instead, then
// verifies PICKING end-to-end -- an injected click on the player must flow
// MRT FragId write -> R32I attachment -> async PBO readback -> SELECTION ->
// `last_click_target == ClickTarget::Player` -- and captures the screenshot
// for the non-blank color bar.
// Arc C box 4, WebGL2 leg. One canvas bound twice in a single pass -- unit 1
// nearest, unit 2 linear -- with the verdict computed in Rock so both backends
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

#[test]
fn web_chrome_screenshot_and_click_pick_mag() {
    let Some((_dir, shot)) = run_cdp(
        "mag",
        "web_chrome_screenshot_and_click_pick_mag",
        &[
            "--frames",
            "30",
            // Derive the click from the player's LIVE projection, never a
            // literal pixel: a hard-coded point silently becomes a test of the
            // current framing and goes stale whenever camera framing or present
            // scaling moves (it did -- Arc A's FrameView work invalidated the
            // old 950,465). `render_pos + 1.5` is the sprite body rather than
            // the feet anchor, matching mag's own `player_body_probe_world`
            // test hook, and logical output units map to CSS through the canvas
            // element's rect so the expression holds at any devicePixelRatio.
            "--click-expr",
            "(function(){               var rp=render_pos_Movable_ret_Vec2(__G.player.movable);               var s=world_to_screen_Vec2_ret_Vec2(rp[0],rp[1]+1.5);               var c=document.querySelector('canvas');var r=c.getBoundingClientRect();               return [r.left+s[0]*(r.width/__SW.logicalW),                       r.top+s[1]*(r.height/__SW.logicalH)];})()",
            // Report whether anything was picked AT ALL, so the two failures are
            // told apart from the message alone:
            //   "nohover:None" -> the click landed off the sprite (a projection
            //                     or framing fault, since the point is derived).
            //   "hover:None"   -> something was picked and the click path
            //                     dropped it; that is a real pipeline defect.
            "--expect",
            "(__G.HOVER_ENTITY_ID ? \"hover\" : \"nohover\") + \":\" + \
             (__G.last_click_target && __G.last_click_target.__variant)",
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
    if Command::new("node")
        .arg("--version")
        .output()
        .map(|output| !output.status.success())
        .unwrap_or(true)
    {
        eprintln!("SKIP web_chrome_hot_reload_preserves_state: node not on PATH");
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
    let mut server = Command::new(rock_bin())
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
    let mut smoke = Command::new("node")
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
