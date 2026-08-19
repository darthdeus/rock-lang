//! Gameplay world content must start at the output origin when its active
//! scene is smaller than the backing canvas.

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

fn run_gameplay(renderer: &str, output: &Path) -> String {
    let mut command = if cfg!(target_os = "macos") {
        let mut command = Command::new("caffeinate");
        command.args(["-du"]);
        command.arg(rock_bin());
        command
    } else {
        Command::new(rock_bin())
    };
    command
        .current_dir(repo_root())
        .args([
            "sim",
            "mag",
            "gameplay_capture_output_resolution",
            "--renderer",
            renderer,
        ])
        .env("ROCK_FRAME_TIMEOUT_SECS", "120")
        .env("MAG_GAMEPLAY_CAPTURE_DIR", output);
    let result = command.output().expect("spawn MAG gameplay capture");
    let mut text = String::from_utf8_lossy(&result.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&result.stderr));
    assert!(
        result.status.success(),
        "{renderer} gameplay capture failed:\n{text}"
    );
    text
}

fn decode(path: &Path) -> RgbaImage {
    image::open(path)
        .unwrap_or_else(|error| panic!("{} did not decode: {error}", path.display()))
        .to_rgba8()
}

fn material(pixel: &[u8; 4]) -> bool {
    pixel[0] > 2 || pixel[1] > 2 || pixel[2] > 2
}

fn first_material_row(image: &RgbaImage, x0: u32, x1: u32) -> Option<u32> {
    (0..image.height()).find(|y| (x0..x1).any(|x| material(&image.get_pixel(x, *y).0)))
}

fn material_population(image: &RgbaImage, x0: u32, y0: u32, x1: u32, y1: u32) -> u32 {
    let mut population = 0;
    for y in y0..y1 {
        for x in x0..x1 {
            if material(&image.get_pixel(x, y).0) {
                population += 1;
            }
        }
    }
    population
}

#[test]
fn mag_gameplay_world_starts_at_the_output_origin_on_gl_and_wgpu() {
    let gl_dir = tempfile::tempdir().expect("GL gameplay capture directory");
    let wgpu_dir = tempfile::tempdir().expect("WGPU gameplay capture directory");
    let gl_text = run_gameplay("gl", gl_dir.path());
    let wgpu_text = run_gameplay("wgpu", wgpu_dir.path());
    assert!(
        gl_text.contains("gameplay_capture_output_resolution: PASS (8/8 expects passed)"),
        "GL fixture denominator changed:\n{gl_text}"
    );
    assert!(
        wgpu_text.contains("gameplay_capture_output_resolution: PASS (10/10 expects passed)"),
        "WGPU fixture denominator changed:\n{wgpu_text}"
    );

    let gl = decode(&gl_dir.path().join("gameplay.png"));
    let wgpu = decode(&wgpu_dir.path().join("gameplay.png"));
    assert_eq!(gl.dimensions(), (1920, 1080));
    assert_eq!(wgpu.dimensions(), gl.dimensions());

    let gl_world_origin = first_material_row(&gl, 300, 700);
    let wgpu_world_origin = first_material_row(&wgpu, 300, 700);
    let gl_world_population = material_population(&gl, 300, 0, 700, 160);
    let wgpu_world_population = material_population(&wgpu, 300, 0, 700, 160);
    let gl_output_ui = material_population(&gl, 755, 0, 1165, 80);
    let wgpu_output_ui = material_population(&wgpu, 755, 0, 1165, 80);
    println!(
        "gameplay origin: GL={gl_world_origin:?} WGPU={wgpu_world_origin:?}; world_top={gl_world_population}/{wgpu_world_population}; output_ui={gl_output_ui}/{wgpu_output_ui}"
    );

    assert_eq!(
        gl_world_origin,
        Some(0),
        "the GL control must expose world content at row zero"
    );
    assert!(
        gl_world_population > 40_000,
        "the GL world-origin control is underpopulated"
    );
    assert!(
        gl_output_ui > 1_000 && wgpu_output_ui > 1_000,
        "both renderers must draw output UI at the top"
    );
    assert_eq!(
        wgpu_world_origin, gl_world_origin,
        "WGPU world content must use the active-scene origin rather than the backing-capacity origin"
    );
    assert!(
        wgpu_world_population > 40_000,
        "WGPU must populate the gameplay world at the output top"
    );
}
