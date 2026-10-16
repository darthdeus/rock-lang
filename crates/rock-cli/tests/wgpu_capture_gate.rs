//! Renderer-neutral presented-frame capture (docs/backends.md
//! S0). The subject is games/wgpu_renderer with WGPU_PROBE_CAPTURE_FILL=1: a
//! red top half and green bottom half drawn at late_draw, so an orientation
//! flip, a stale frame and an all-zero capture are three distinct wrong
//! answers. Each arm asserts in ONE run: the selected renderer booted, the
//! capture saved, the presented frame was populated, and the PNG holds the
//! halves at top-left orientation and the drawable's extent.
//!
//! The wgpu arm's presented-population witness is the present-readback probe
//! line, independent of the PNG -- the run where the surface really presented
//! the halves while the PNG read the hidden GL compatibility buffer is
//! exactly the defect this gate exists to catch.

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Run the fixture for 3 frames with the capture fill probe on. The process
/// chdirs itself to the workspace root, so the capture is directed into
/// `workdir` via STONE_SCREENSHOT_PATH instead of racing other tests at the
/// repo-root default. Returns combined stdout+stderr.
fn run_capture(renderer: Option<&str>, workdir: &std::path::Path) -> String {
    let mut cmd = crate::command::rock_bin();
    cmd.current_dir(repo_root())
        .args(["frame", "games/wgpu_renderer", "-n", "3"])
        .env("ROCK_FRAME_TIMEOUT_SECS", "120")
        .env("STONE_SCREENSHOT_PATH", workdir.join("screenshot.png"))
        .env("WGPU_PROBE_CAPTURE_FILL", "1")
        .env("STONE_WGPU_READBACK", "1");
    if let Some(renderer) = renderer {
        cmd.args(["--renderer", renderer]);
    }
    let out = cmd.output().expect("spawn rock frame");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "bounded capture run failed:\n{text}");
    assert!(
        text.contains("Initialized successfully"),
        "the capture run did not boot:\n{text}"
    );
    assert!(
        text.contains("Screenshot saved to") || text.contains("screenshot saved"),
        "the run never saved a capture:\n{text}"
    );
    text
}

/// Decode the PNG and sample the half centers plus the frame-parity corner:
/// ((w/2, h/4), (w/2, 3h/4), (7w/8, 7h/8)).
fn png_samples(path: &std::path::Path) -> (u32, u32, [u8; 3], [u8; 3], [u8; 3]) {
    let img = image::open(path)
        .unwrap_or_else(|e| panic!("capture PNG at {} did not decode: {e}", path.display()))
        .to_rgba8();
    let (w, h) = img.dimensions();
    assert!(w > 0 && h > 0, "capture PNG is zero-sized");
    let top = img.get_pixel(w / 2, h / 4).0;
    let bottom = img.get_pixel(w / 2, 3 * h / 4).0;
    let corner = img.get_pixel(7 * w / 8, 7 * h / 8).0;
    (
        w,
        h,
        [top[0], top[1], top[2]],
        [bottom[0], bottom[1], bottom[2]],
        [corner[0], corner[1], corner[2]],
    )
}

#[test]
fn wgpu_capture_contains_the_presented_frame() {
    let dir = tempfile::tempdir().expect("tempdir");
    let text = run_capture(None, dir.path());
    assert!(
        text.contains("Selected wgpu renderer"),
        "the fixture did not select wgpu:\n{text}"
    );
    // Independent presented-population witness: the previous present really
    // held the halves, whatever the PNG says.
    assert!(
        text.contains("present top 255 0 0 bottom 0 255 0"),
        "the wgpu surface never presented the probe halves:\n{text}"
    );
    let (w, h, top, bottom, corner) = png_samples(&dir.path().join("screenshot.png"));
    assert_eq!((w, h), (64, 64), "capture must be the drawable's extent");
    assert_eq!(top, [255, 0, 0], "top half of the wgpu capture must be the presented red");
    assert_eq!(bottom, [0, 255, 0], "bottom half of the wgpu capture must be the presented green");
    // Frame 3 is odd, so its corner is blue; a capture of the previous
    // present would read the even frame's white.
    assert_eq!(corner, [0, 0, 255], "the wgpu capture must hold the capture frame's own corner");
}

#[test]
fn gl_capture_contains_the_presented_frame() {
    let dir = tempfile::tempdir().expect("tempdir");
    let text = run_capture(Some("gl"), dir.path());
    assert!(
        !text.contains("Selected wgpu renderer"),
        "the GL control run selected wgpu:\n{text}"
    );
    let (w, h, top, bottom, corner) = png_samples(&dir.path().join("screenshot.png"));
    assert_eq!((w, h), (64, 64), "capture must be the drawable's extent");
    assert_eq!(top, [255, 0, 0], "top half of the GL capture must be the presented red");
    assert_eq!(bottom, [0, 255, 0], "bottom half of the GL capture must be the presented green");
    assert_eq!(corner, [0, 0, 255], "the GL capture must hold the capture frame's own corner");
}
