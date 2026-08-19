//! Numerical GL/WGPU parity for the deterministic MAG splash and menu.

use std::path::{Path, PathBuf};
use std::process::Command;

use image::RgbaImage;

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn rock_command() -> Command {
    if cfg!(target_os = "macos") {
        let mut command = Command::new("caffeinate");
        command.args(["-du"]);
        command.arg(rock_bin());
        command
    } else {
        Command::new(rock_bin())
    }
}

fn run_menu(renderer: &str, output: &Path) -> String {
    let mut command = rock_command();
    command
        .current_dir(repo_root())
        .args(["sim", "mag", "menu_capture", "--renderer", renderer])
        .env("ROCK_FRAME_TIMEOUT_SECS", "120")
        .env("MAG_MENU_CAPTURE_DIR", output);
    let result = command.output().expect("spawn MAG menu capture");
    let mut text = String::from_utf8_lossy(&result.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&result.stderr));
    assert!(
        result.status.success(),
        "{renderer} menu capture failed:\n{text}"
    );
    text
}

fn run_output_resolution_menu(renderer: &str, output: &Path) -> String {
    let mut command = rock_command();
    command
        .current_dir(repo_root())
        .args([
            "sim",
            "mag",
            "menu_capture_output_resolution",
            "--renderer",
            renderer,
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "120")
        .env("MAG_MENU_CAPTURE_DIR", output);
    let result = command.output().expect("spawn MAG output-resolution capture");
    let mut text = String::from_utf8_lossy(&result.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&result.stderr));
    assert!(
        result.status.success(),
        "{renderer} output-resolution menu capture failed:\n{text}"
    );
    text
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Bounds {
    min_x: u32,
    min_y: u32,
    max_x: u32,
    max_y: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImageStats {
    width: u32,
    height: u32,
    nonzero_pixels: u32,
    content_bounds: Option<Bounds>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct DiffStats {
    differing_pixels: u32,
    max_delta: [u8; 4],
    mean_delta: [f64; 4],
}

#[derive(Debug, Clone, Copy)]
struct Region {
    name: &'static str,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

fn decode(path: &Path) -> RgbaImage {
    image::open(path)
        .unwrap_or_else(|error| panic!("{} did not decode: {error}", path.display()))
        .to_rgba8()
}

fn image_stats(image: &RgbaImage) -> ImageStats {
    let (width, height) = image.dimensions();
    let mut nonzero_pixels = 0;
    let mut content_bounds: Option<Bounds> = None;
    for y in 0..height {
        for x in 0..width {
            let pixel = image.get_pixel(x, y).0;
            if pixel[..3] == [0, 0, 0] {
                continue;
            }
            nonzero_pixels += 1;
            content_bounds = Some(match content_bounds {
                Some(bounds) => Bounds {
                    min_x: bounds.min_x.min(x),
                    min_y: bounds.min_y.min(y),
                    max_x: bounds.max_x.max(x),
                    max_y: bounds.max_y.max(y),
                },
                None => Bounds {
                    min_x: x,
                    min_y: y,
                    max_x: x,
                    max_y: y,
                },
            });
        }
    }
    ImageStats {
        width,
        height,
        nonzero_pixels,
        content_bounds,
    }
}

fn region_population(image: &RgbaImage, region: Region) -> u32 {
    let mut population = 0;
    for y in region.y0..region.y1 {
        for x in region.x0..region.x1 {
            if image.get_pixel(x, y).0[..3] != [0, 0, 0] {
                population += 1;
            }
        }
    }
    population
}

fn region_bounds(image: &RgbaImage, region: Region) -> Option<Bounds> {
    let mut bounds: Option<Bounds> = None;
    for y in region.y0..region.y1 {
        for x in region.x0..region.x1 {
            if image.get_pixel(x, y).0[..3] == [0, 0, 0] {
                continue;
            }
            bounds = Some(match bounds {
                Some(current) => Bounds {
                    min_x: current.min_x.min(x),
                    min_y: current.min_y.min(y),
                    max_x: current.max_x.max(x),
                    max_y: current.max_y.max(y),
                },
                None => Bounds {
                    min_x: x,
                    min_y: y,
                    max_x: x,
                    max_y: y,
                },
            });
        }
    }
    bounds
}

fn diff_stats(left: &RgbaImage, right: &RgbaImage) -> DiffStats {
    assert_eq!(
        left.dimensions(),
        right.dimensions(),
        "parity images must have equal dimensions"
    );
    let mut differing_pixels = 0;
    let mut max_delta = [0; 4];
    let mut sums = [0_u64; 4];
    for (left, right) in left.pixels().zip(right.pixels()) {
        let mut differs = false;
        for channel in 0..4 {
            let delta = left.0[channel].abs_diff(right.0[channel]);
            max_delta[channel] = max_delta[channel].max(delta);
            sums[channel] += u64::from(delta);
            differs |= delta != 0;
        }
        if differs {
            differing_pixels += 1;
        }
    }
    let pixels = f64::from(left.width() * left.height());
    DiffStats {
        differing_pixels,
        max_delta,
        mean_delta: sums.map(|sum| sum as f64 / pixels),
    }
}

fn assert_fixture_image(path: &Path, regions: &[Region]) -> (RgbaImage, ImageStats) {
    let image = decode(path);
    let stats = image_stats(&image);
    assert_eq!(
        (stats.width, stats.height),
        (1280, 720),
        "{} must stay at the fixture's exact drawable extent; stats={stats:?}",
        path.display()
    );
    assert!(
        stats.nonzero_pixels > 0 && stats.content_bounds.is_some(),
        "{} must have nonzero content and bounds; stats={stats:?}",
        path.display()
    );
    let observations: Vec<_> = regions
        .iter()
        .map(|region| {
            (
                region.name,
                region_population(&image, *region),
                region_bounds(&image, *region),
            )
        })
        .collect();
    for (name, population, bounds) in &observations {
        assert!(
            *population > 0 && bounds.is_some(),
            "{} semantic region {name} is empty; stats={stats:?} observations={observations:?}",
            path.display(),
        );
    }
    println!(
        "{} image stats: {stats:?} semantic_regions={observations:?}",
        path.display()
    );
    (image, stats)
}

fn assert_same_renderer_controls(
    name: &str,
    first: &RgbaImage,
    first_stats: ImageStats,
    second: &RgbaImage,
    second_stats: ImageStats,
) {
    let controls = diff_stats(first, second);
    println!("{name} stats: first={first_stats:?} second={second_stats:?} diff={controls:?}");
    assert_eq!(
        first_stats, second_stats,
        "{name} same-renderer controls changed their dimensions/content bounds: {first_stats:?} vs {second_stats:?}; diff={controls:?}"
    );
    assert_eq!(
        controls,
        DiffStats {
            differing_pixels: 0,
            max_delta: [0; 4],
            mean_delta: [0.0; 4],
        },
        "{name} same-renderer controls must be byte-identical: {controls:?}"
    );
}

fn assert_cross_renderer_parity(name: &str, gl: &RgbaImage, wgpu: &RgbaImage) {
    let stats = diff_stats(gl, wgpu);
    let total_pixels = gl.width() * gl.height();
    let max_rgb_delta = if cfg!(target_os = "linux") { 25 } else { 2 };
    println!("{name} GL/WGPU diff: total_pixels={total_pixels} stats={stats:?}");
    assert!(
        stats.differing_pixels <= total_pixels / 100,
        "{name} differs on too many pixels: {stats:?}; total_pixels={total_pixels}"
    );
    assert!(
        stats
            .max_delta
            .iter()
            .take(3)
            .all(|delta| *delta <= max_rgb_delta),
        "{name} RGB maximum delta exceeds {max_rgb_delta}: {stats:?}"
    );
    assert!(
        stats.mean_delta[..3].iter().all(|delta| *delta <= 0.01),
        "{name} RGB mean delta exceeds 0.01: {stats:?}"
    );
    assert!(
        stats.max_delta[3] <= 100 && stats.mean_delta[3] <= 0.25,
        "{name} alpha delta exceeds the measured opaque-surface tolerance: {stats:?}"
    );
}

fn assert_cross_renderer_geometry(name: &str, gl: &RgbaImage, wgpu: &RgbaImage, regions: &[Region]) {
    for region in regions {
        let gl_bounds = region_bounds(gl, *region);
        let wgpu_bounds = region_bounds(wgpu, *region);
        assert_eq!(
            gl_bounds, wgpu_bounds,
            "{name} region {} geometry changed between GL and WGPU",
            region.name
        );
        assert!(
            gl_bounds.is_some(),
            "{name} region {} has no presented pixels",
            region.name
        );
    }
}

fn assert_cross_renderer_pixel_witnesses(
    name: &str,
    gl: &RgbaImage,
    wgpu: &RgbaImage,
    witnesses: &[(&str, u32, u32)],
) {
    for (label, x, y) in witnesses {
        let gl_pixel = gl.get_pixel(*x, *y).0;
        let wgpu_pixel = wgpu.get_pixel(*x, *y).0;
        assert_eq!(
            gl_pixel, wgpu_pixel,
            "{name} {label} presented pixel changed between GL and WGPU at ({x},{y})"
        );
        assert_ne!(
            gl_pixel[..3], [0, 0, 0],
            "{name} {label} presented pixel is empty at ({x},{y})"
        );
        assert_eq!(gl_pixel[3], 255, "{name} {label} presented pixel is not opaque");
    }
}

fn output_resolution_regions() -> [Region; 6] {
    [
        Region {
            name: "title",
            x0: 700,
            y0: 240,
            x1: 1220,
            y1: 380,
        },
        Region {
            name: "play",
            x0: 780,
            y0: 390,
            x1: 1140,
            y1: 470,
        },
        Region {
            name: "settings",
            x0: 780,
            y0: 465,
            x1: 1140,
            y1: 545,
        },
        Region {
            name: "credits",
            x0: 780,
            y0: 535,
            x1: 1140,
            y1: 615,
        },
        Region {
            name: "quit",
            x0: 780,
            y0: 610,
            x1: 1140,
            y1: 690,
        },
        Region {
            name: "version",
            x0: 1660,
            y0: 1010,
            x1: 1920,
            y1: 1080,
        },
    ]
}

fn assert_output_resolution_image(path: &Path) -> (RgbaImage, ImageStats, Vec<(String, u32, Option<Bounds>)>) {
    let image = decode(path);
    let stats = image_stats(&image);
    assert_eq!(
        (stats.width, stats.height),
        (1920, 1080),
        "{} must use the non-degenerate drawable extent; stats={stats:?}",
        path.display()
    );
    assert!(
        stats.nonzero_pixels > 0 && stats.content_bounds.is_some(),
        "{} must contain presented pixels; stats={stats:?}",
        path.display()
    );
    let regions = output_resolution_regions();
    let observations: Vec<_> = regions
        .iter()
        .map(|region| {
            (
                region.name.to_owned(),
                region_population(&image, *region),
                region_bounds(&image, *region),
            )
        })
        .collect();
    for (name, population, bounds) in &observations {
        assert!(
            *population > 0 && bounds.is_some(),
            "{} semantic region {name} is empty; stats={stats:?} observations={observations:?}",
            path.display(),
        );
    }
    println!(
        "{} output-resolution image stats: {stats:?} regions={observations:?}",
        path.display()
    );
    (image, stats, observations)
}

fn assert_output_resolution_pixel_witnesses(name: &str, image: &RgbaImage) {
    for (label, x, y) in [
        ("title", 960, 320),
        ("play", 960, 430),
        ("settings background", 900, 500),
        ("credits background", 900, 575),
        ("quit background", 900, 647),
        ("version", 1860, 1068),
    ] {
        let pixel = image.get_pixel(x, y).0;
        assert_eq!(pixel[3], 255, "{name} {label} pixel must be opaque");
        assert_ne!(
            pixel[..3], [0, 0, 0],
            "{name} {label} pixel must come from the presented menu"
        );
        println!("{name} {label} pixel=({}, {}, {}, {})", pixel[0], pixel[1], pixel[2], pixel[3]);
    }
}

#[test]
fn mag_splash_and_menu_have_numerical_gl_wgpu_parity() {
    let gl_first_dir = tempfile::tempdir().expect("first GL capture directory");
    let gl_second_dir = tempfile::tempdir().expect("second GL capture directory");
    let wgpu_first_dir = tempfile::tempdir().expect("first WGPU capture directory");
    let wgpu_second_dir = tempfile::tempdir().expect("second WGPU capture directory");
    let gl_first_text = run_menu("gl", gl_first_dir.path());
    let gl_second_text = run_menu("gl", gl_second_dir.path());
    let wgpu_first_text = run_menu("wgpu", wgpu_first_dir.path());
    let wgpu_second_text = run_menu("wgpu", wgpu_second_dir.path());
    assert!(
        gl_first_text.contains("menu_capture: PASS (6/6 expects passed)")
            && gl_second_text.contains("menu_capture: PASS (6/6 expects passed)"),
        "GL fixture denominator changed:\nfirst:\n{gl_first_text}\nsecond:\n{gl_second_text}"
    );
    assert!(
        wgpu_first_text.contains("menu_capture: PASS (9/9 expects passed)")
            && wgpu_second_text.contains("menu_capture: PASS (9/9 expects passed)"),
        "WGPU fixture denominator changed:\nfirst:\n{wgpu_first_text}\nsecond:\n{wgpu_second_text}"
    );

    let splash_regions = [
        Region {
            name: "splash center",
            x0: 240,
            y0: 80,
            x1: 1040,
            y1: 640,
        },
        Region {
            name: "splash title",
            x0: 450,
            y0: 250,
            x1: 830,
            y1: 350,
        },
        Region {
            name: "splash logos",
            x0: 450,
            y0: 390,
            x1: 830,
            y1: 470,
        },
        Region {
            name: "splash version",
            x0: 1100,
            y0: 650,
            x1: 1280,
            y1: 720,
        },
    ];
    let menu_regions = [
        Region {
            name: "title",
            x0: 500,
            y0: 150,
            x1: 780,
            y1: 260,
        },
        Region {
            name: "button column",
            x0: 500,
            y0: 260,
            x1: 780,
            y1: 500,
        },
        Region {
            name: "version",
            x0: 0,
            y0: 650,
            x1: 300,
            y1: 720,
        },
    ];

    let (gl_splash, gl_splash_stats) = assert_fixture_image(
        &gl_first_dir.path().join("splash.png"),
        &splash_regions,
    );
    let (gl_menu, gl_menu_stats) =
        assert_fixture_image(&gl_first_dir.path().join("menu.png"), &menu_regions);
    let (gl_splash_control, gl_splash_control_stats) = assert_fixture_image(
        &gl_second_dir.path().join("splash.png"),
        &splash_regions,
    );
    let (gl_menu_control, gl_menu_control_stats) =
        assert_fixture_image(&gl_second_dir.path().join("menu.png"), &menu_regions);
    let (wgpu_splash, wgpu_splash_stats) = assert_fixture_image(
        &wgpu_first_dir.path().join("splash.png"),
        &splash_regions,
    );
    let (wgpu_menu, wgpu_menu_stats) =
        assert_fixture_image(&wgpu_first_dir.path().join("menu.png"), &menu_regions);
    let (wgpu_splash_control, wgpu_splash_control_stats) = assert_fixture_image(
        &wgpu_second_dir.path().join("splash.png"),
        &splash_regions,
    );
    let (wgpu_menu_control, wgpu_menu_control_stats) = assert_fixture_image(
        &wgpu_second_dir.path().join("menu.png"),
        &menu_regions,
    );

    assert_same_renderer_controls(
        "splash GL",
        &gl_splash,
        gl_splash_stats,
        &gl_splash_control,
        gl_splash_control_stats,
    );
    assert_same_renderer_controls(
        "menu GL",
        &gl_menu,
        gl_menu_stats,
        &gl_menu_control,
        gl_menu_control_stats,
    );
    assert_same_renderer_controls(
        "splash WGPU",
        &wgpu_splash,
        wgpu_splash_stats,
        &wgpu_splash_control,
        wgpu_splash_control_stats,
    );
    assert_same_renderer_controls(
        "menu WGPU",
        &wgpu_menu,
        wgpu_menu_stats,
        &wgpu_menu_control,
        wgpu_menu_control_stats,
    );
    assert_cross_renderer_parity("splash", &gl_splash, &wgpu_splash);
    assert_cross_renderer_parity("menu", &gl_menu, &wgpu_menu);
    assert_cross_renderer_geometry("splash", &gl_splash, &wgpu_splash, &splash_regions);
    assert_cross_renderer_geometry("menu", &gl_menu, &wgpu_menu, &menu_regions);
    assert_cross_renderer_pixel_witnesses(
        "splash",
        &gl_splash,
        &wgpu_splash,
        &[
            ("title", 600, 275),
            ("presents", 556, 315),
            ("made with", 629, 377),
            ("rock logo", 555, 398),
            ("SDL logo", 620, 403),
        ],
    );
    assert_cross_renderer_pixel_witnesses(
        "menu",
        &gl_menu,
        &wgpu_menu,
        &[
            ("title", 640, 216),
            ("Play button", 640, 300),
            ("Settings button", 640, 350),
            ("Credits text", 640, 400),
            ("Quit text", 640, 447),
            ("version", 40, 710),
        ],
    );
}

#[test]
fn mag_output_resolution_ui_has_cross_renderer_parity() {
    let gl_first_dir = tempfile::tempdir().expect("first GL output-resolution capture directory");
    let gl_second_dir = tempfile::tempdir().expect("second GL output-resolution capture directory");
    let wgpu_first_dir = tempfile::tempdir().expect("first WGPU output-resolution capture directory");
    let wgpu_second_dir = tempfile::tempdir().expect("second WGPU output-resolution capture directory");
    let gl_first_text = run_output_resolution_menu("gl", gl_first_dir.path());
    let gl_second_text = run_output_resolution_menu("gl", gl_second_dir.path());
    let wgpu_first_text = run_output_resolution_menu("wgpu", wgpu_first_dir.path());
    let wgpu_second_text = run_output_resolution_menu("wgpu", wgpu_second_dir.path());
    assert!(
        gl_first_text.contains("menu_capture_output_resolution: PASS (12/12 expects passed)")
            && gl_second_text.contains("menu_capture_output_resolution: PASS (12/12 expects passed)"),
        "GL fixture denominator changed:\nfirst:\n{gl_first_text}\nsecond:\n{gl_second_text}"
    );
    assert!(
        wgpu_first_text.contains("menu_capture_output_resolution: PASS (14/14 expects passed)")
            && wgpu_second_text.contains("menu_capture_output_resolution: PASS (14/14 expects passed)"),
        "WGPU fixture denominator changed:\nfirst:\n{wgpu_first_text}\nsecond:\n{wgpu_second_text}"
    );
    let (gl, gl_stats, gl_regions) =
        assert_output_resolution_image(&gl_first_dir.path().join("menu.png"));
    let (gl_control, gl_control_stats, _gl_control_regions) =
        assert_output_resolution_image(&gl_second_dir.path().join("menu.png"));
    let (wgpu, wgpu_stats, wgpu_regions) =
        assert_output_resolution_image(&wgpu_first_dir.path().join("menu.png"));
    let (wgpu_control, wgpu_control_stats, _wgpu_control_regions) =
        assert_output_resolution_image(&wgpu_second_dir.path().join("menu.png"));
    assert_same_renderer_controls(
        "output-resolution GL",
        &gl,
        gl_stats,
        &gl_control,
        gl_control_stats,
    );
    assert_same_renderer_controls(
        "output-resolution WGPU",
        &wgpu,
        wgpu_stats,
        &wgpu_control,
        wgpu_control_stats,
    );
    assert_output_resolution_pixel_witnesses("GL", &gl);
    assert_output_resolution_pixel_witnesses("WGPU", &wgpu);
    let gl_geometry: Vec<_> = gl_regions
        .iter()
        .map(|(name, _, bounds)| (name, bounds))
        .collect();
    let wgpu_geometry: Vec<_> = wgpu_regions
        .iter()
        .map(|(name, _, bounds)| (name, bounds))
        .collect();
    assert_eq!(
        gl_geometry, wgpu_geometry,
        "output-resolution menu geometry changed between GL and WGPU"
    );
}
