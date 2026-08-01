//! stone-web Arc 2 MA gate: `rock web <project> --out <dir>` emits a static
//! browser build (`index.html` + `game.js`), and the emitted `game.js` -- with
//! its browser `print` (console.log) -- runs the program end to end under node
//! (a headless stand-in for the browser JS engine; the WebGL2 leaves land in MB).

use std::process::Command;

fn rock_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn node_available() -> bool {
    Command::new("node").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

#[test]
fn web_build_emits_files_and_runs() {
    let root = std::env::temp_dir().join("rock_web_build_gate");
    let proj = root.join("proj");
    let out = root.join("out");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("stick.toml"), "[package]\nname = \"proj\"\n").unwrap();
    std::fs::write(proj.join("math.rock"), "fn cube(x: int) -> int { return x * x * x }\n").unwrap();
    std::fs::write(
        proj.join("main.rock"),
        "fn main() { print(\"cube3={cube(3)}\") }\n",
    )
    .unwrap();

    let status = Command::new(rock_bin())
        .args(["web", proj.to_str().unwrap(), "--out", out.to_str().unwrap()])
        .status()
        .expect("spawn rock web");
    assert!(status.success(), "rock web failed");

    let index = out.join("index.html");
    let game = out.join("game.js");
    assert!(index.is_file(), "index.html not emitted");
    assert!(game.is_file(), "game.js not emitted");
    let html = std::fs::read_to_string(&index).unwrap();
    assert!(html.contains("game.js") && html.contains("stone-canvas"), "index.html shape wrong:\n{html}");

    if !node_available() {
        eprintln!("SKIP node run: node not found");
        return;
    }
    let out_run = Command::new("node").arg(&game).output().expect("spawn node");
    let stdout = String::from_utf8_lossy(&out_run.stdout);
    assert!(out_run.status.success(), "game.js threw under node:\n{}", String::from_utf8_lossy(&out_run.stderr));
    assert_eq!(stdout.trim(), "cube3=27", "web game.js output drifted:\n{stdout}");
}

/// The web driver's `__rock_shader_register` line must carry the `@textures`
/// slot names as its third argument, in field order. It is a hand-written
/// mirror of skull's Lua `shader_registration_lines`, so a param added on the
/// Lua side does not reach it -- and an arity mismatch is silent on JS (the
/// missing arg reads `undefined`, and the engine's `bind_texture_slots` then
/// dies on `.len`). None of match3/chess/mag declares a `@textures` group, so
/// the web smokes cannot cover the non-empty case.
#[test]
fn web_shader_registration_carries_texture_slots() {
    let root = std::env::temp_dir().join("rock_web_shader_slots");
    let proj = root.join("proj");
    let out = root.join("out");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("stick.toml"), "[package]\nname = \"proj\"\n").unwrap();
    std::fs::write(
        proj.join("main.rock"),
        r#"
value QuadVert { aPos: Vec2, aTexCoord: Vec2 }
value UiVertU  { uProjection: Mat4, uView: Mat4, uTransform: Mat4 }
value UiVary   { position: Vec4, TexCoord: Vec2 }

// Field order is the texture unit.
struct SlotTex { uTexture: Sampler2D, mask_tex: Sampler2D, shadowmap_tex: Sampler2D }

@shader(vertex)
fn ui_vert(@attr a: QuadVert, @uniform u: UiVertU) -> UiVary {
    let clip = u.uProjection * u.uView * u.uTransform * Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }
    return UiVary { position: clip, TexCoord: a.aTexCoord }
}

@shader(fragment)
fn ui_tex_frag(vary: UiVary, @textures t: SlotTex) -> Vec4 {
    return texture(t.uTexture, vary.TexCoord)
        + texture(t.mask_tex, vary.TexCoord)
        + texture(t.shadowmap_tex, vary.TexCoord)
}

global SHADER_KEY: string = rock_shader(ui_vert, ui_tex_frag)

@nomangle inline(js) fn __rock_shader_register(key: string, src: string, texture_slots: [string]) = """
  (globalThis.__ROCK_SHADERS = globalThis.__ROCK_SHADERS || {})[key] = texture_slots;
"""

fn main() { print("{SHADER_KEY}") }
"#,
    )
    .unwrap();

    let status = Command::new(rock_bin())
        .args(["web", proj.to_str().unwrap(), "--out", out.to_str().unwrap()])
        .status()
        .expect("spawn rock web");
    assert!(status.success(), "rock web failed");

    let js = std::fs::read_to_string(out.join("game.js")).unwrap();
    let line = js
        .lines()
        .find(|l| l.starts_with("__rock_shader_register("))
        .expect("a registration line in game.js");
    // The FIRST `{_data:` on the line: texture slots are the third argument,
    // ahead of the uniform-block ones. Located by position rather than by
    // `rfind`, which silently started reading the block's offsets when those
    // arguments were appended.
    let slots = line
        .find("{_data:")
        .map(|i| &line[i..])
        .unwrap_or_else(|| panic!("registration line has no slots argument:\n{line}"));
    assert!(
        slots.starts_with(r#"{_data: ["uTexture", "mask_tex", "shadowmap_tex"], len: 3, __ty: "Vec"}"#),
        "registration slots drifted (field order IS the texture unit):\n{slots}"
    );
    // The `@uniform` blocks travel as parallel FLAT lists: names, sizes,
    // fingerprints, per-block member counts, then members and their std140
    // offsets concatenated. This fixture's only group is on the VERTEX stage,
    // which is loose for one more step (the `@engine` port), so the lists are
    // empty -- still as arguments, never as a short call, which is the
    // silent-on-JS failure this gate exists for.
    assert!(
        line.contains(r#"{_data: [], len: 0, __ty: "Vec"}, {_data: [], len: 0, __ty: "Vec"}, {_data: [], len: 0, __ty: "Vec"}, {_data: [], len: 0, __ty: "Vec"}, {_data: [], len: 0, __ty: "Vec"}, {_data: [], len: 0, __ty: "Vec"});"#),
        "registration uniform-block arguments drifted:\n{line}"
    );
}
