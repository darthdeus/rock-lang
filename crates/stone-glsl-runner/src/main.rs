//! Headless GL executor for `rock test --backend glsl`.
//!
//! A dumb GL harness: it knows nothing about Rock, bone, or the test
//! discovery. rock-cli emits the vertex+fragment GLSL for each
//! `@test(returns = ...)` value test (via `skull-codegen-glsl::emit_value_test`)
//! and feeds this process a JSON job array on stdin; the runner compiles each
//! pair, draws a fullscreen triangle into a 1x1 offscreen target (R32I for
//! int/bool, R32F for float), reads the single pixel back, and writes one
//! `__SKULL__` line per test to stdout. rock-cli parses those lines through the
//! same shared parser + `compare_value` the luajit and node executors use --
//! this is a transport for `__SKULL__` lines, nothing more (the seam contract).
//!
//! Protocol:
//!   argv:  --profile 330core|300es
//!   stdin: [{ "name": "...", "vertex": "...", "fragment": "...",
//!            "ret": "int"|"float"|"bool"|"compile" }, ...]
//!   stdout (one per job):
//!     __SKULL__\tVALUE\t<name>\t<serialized>   -- the read-back scalar
//!     __SKULL__\tFAIL\t<name>: <detail>         -- a GL compile/link/render error
//!
//! `ret: "compile"` is the compile-only kind: compile + link under the
//! profile's REAL driver, no draw or readback
//! (render shaders have arbitrary attr/uniform interfaces the 1x1 harness
//! cannot feed). Success is `VALUE\t<name>\tcompiled`; a compile/link error is
//! the usual FAIL line carrying the driver log.
//!
//! Exit codes: 0 = jobs ran (individual FAILs are on stdout, still 0); 2 = no
//! GL context could be created; the caller treats this as an error unless
//! `--skip-gpu` was explicitly requested; 3 = malformed invocation/stdin. A GL
//! error mid-run is a loud failure surfaced on the failing job's line -- never
//! a silent skip.

use glow::HasContext;
use std::io::Read;

fn main() {
    let mut profile: Option<&str> = None;
    let argv: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "--profile" => {
                i += 1;
                profile = argv.get(i).map(|s| s.as_str());
            }
            other => {
                eprintln!("stone-glsl-runner: unexpected argument `{other}`");
                std::process::exit(3);
            }
        }
        i += 1;
    }
    let profile = match profile {
        Some(p @ ("330core" | "300es")) => p,
        _ => {
            eprintln!("stone-glsl-runner: --profile must be 330core or 300es");
            std::process::exit(3);
        }
    };

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("stone-glsl-runner: failed to read stdin");
        std::process::exit(3);
    }
    let jobs = match parse_jobs(&input) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("stone-glsl-runner: {e}");
            std::process::exit(3);
        }
    };

    let gl_ctx = match GlCtx::create(profile) {
        Ok(c) => c,
        Err(e) => {
            // The caller treats exit 2 as an error unless `--skip-gpu` was
            // explicitly requested.
            eprintln!("stone-glsl-runner: no GL context: {e}");
            std::process::exit(2);
        }
    };

    let mut out = String::new();
    for job in &jobs {
        match gl_ctx.run_one(job) {
            Ok(serialized) => {
                out.push_str(&format!("__SKULL__\tVALUE\t{}\t{}\n", job.name, serialized));
            }
            Err(detail) => {
                out.push_str(&format!("__SKULL__\tFAIL\t{}: {}\n", job.name, escape(&detail)));
            }
        }
    }
    print!("{out}");
}

struct Job {
    name: String,
    vertex: String,
    fragment: String,
    ret: String,
}

fn parse_jobs(input: &str) -> Result<Vec<Job>, String> {
    let v: serde_json::Value =
        serde_json::from_str(input).map_err(|e| format!("bad job JSON: {e}"))?;
    let arr = v.as_array().ok_or("job payload must be a JSON array")?;
    let mut jobs = Vec::with_capacity(arr.len());
    for item in arr {
        let get = |k: &str| -> Result<String, String> {
            item.get(k)
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| format!("job missing string field `{k}`"))
        };
        jobs.push(Job {
            name: get("name")?,
            vertex: get("vertex")?,
            fragment: get("fragment")?,
            ret: get("ret")?,
        });
    }
    Ok(jobs)
}

/// A `\t`/`\n` escaper so a driver's multi-line compile log stays on one
/// `__SKULL__` line (the parser splits on tabs past the marker).
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n").replace('\t', " ")
}

struct GlCtx {
    // Order matters for Drop: gl before context before window before subsystems.
    gl: glow::Context,
    int_fbo: FboTarget,
    float_fbo: FboTarget,
    vao: glow::VertexArray,
    _gl_context: sdl2::video::GLContext,
    _window: sdl2::video::Window,
    _video: sdl2::VideoSubsystem,
    _sdl: sdl2::Sdl,
}

struct FboTarget {
    fbo: glow::Framebuffer,
    _tex: glow::Texture,
}

impl GlCtx {
    fn create(profile: &str) -> Result<GlCtx, String> {
        #[cfg(target_os = "macos")]
        sdl2::hint::set("SDL_MAC_BACKGROUND_APP", "1");

        let sdl = sdl2::init().map_err(|e| format!("sdl init: {e}"))?;
        let video = sdl.video().map_err(|e| format!("sdl video: {e}"))?;
        let gl_attr = video.gl_attr();
        match profile {
            "330core" => {
                gl_attr.set_context_profile(sdl2::video::GLProfile::Core);
                gl_attr.set_context_version(3, 3);
            }
            "300es" => {
                gl_attr.set_context_profile(sdl2::video::GLProfile::GLES);
                gl_attr.set_context_version(3, 0);
            }
            _ => return Err(format!("unknown profile `{profile}`")),
        }
        let mut wb = video.window("stone-glsl-runner", 1, 1);
        wb.opengl().hidden();
        let window = wb.build().map_err(|e| format!("window: {e}"))?;
        let gl_context = window
            .gl_create_context()
            .map_err(|e| format!("gl context: {e}"))?;
        window
            .gl_make_current(&gl_context)
            .map_err(|e| format!("make current: {e}"))?;
        let gl = unsafe {
            glow::Context::from_loader_function(|s| video.gl_get_proc_address(s) as *const _)
        };

        unsafe {
            let vao = gl.create_vertex_array().map_err(|e| format!("vao: {e}"))?;
            gl.bind_vertex_array(Some(vao));
            let int_fbo = make_target(&gl, glow::R32I, glow::RED_INTEGER, glow::INT)?;
            let float_fbo = make_target(&gl, glow::R32F, glow::RED, glow::FLOAT)?;
            Ok(GlCtx {
                gl,
                int_fbo,
                float_fbo,
                vao,
                _gl_context: gl_context,
                _window: window,
                _video: video,
                _sdl: sdl,
            })
        }
    }

    fn run_one(&self, job: &Job) -> Result<String, String> {
        let gl = &self.gl;
        unsafe {
            let program = compile_program(gl, &job.vertex, &job.fragment)?;
            if job.ret == "compile" {
                gl.delete_program(program);
                return Ok("compiled".to_string());
            }
            let target = match job.ret.as_str() {
                "float" => &self.float_fbo,
                "int" | "bool" => &self.int_fbo,
                other => {
                    gl.delete_program(program);
                    return Err(format!("unknown ret kind `{other}`"));
                }
            };
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(target.fbo));
            gl.viewport(0, 0, 1, 1);
            gl.use_program(Some(program));
            gl.bind_vertex_array(Some(self.vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.finish();

            let mut buf = [0u8; 4];
            gl.read_pixels(
                0,
                0,
                1,
                1,
                match job.ret.as_str() {
                    "float" => glow::RED,
                    _ => glow::RED_INTEGER,
                },
                match job.ret.as_str() {
                    "float" => glow::FLOAT,
                    _ => glow::INT,
                },
                glow::PixelPackData::Slice(Some(&mut buf)),
            );
            let err = gl.get_error();
            gl.delete_program(program);
            if err != glow::NO_ERROR {
                return Err(format!("gl error during draw/readback: 0x{err:X}"));
            }
            Ok(match job.ret.as_str() {
                "float" => {
                    let f = f32::from_ne_bytes(buf);
                    format!("{}", f as f64)
                }
                "bool" => {
                    if i32::from_ne_bytes(buf) != 0 {
                        "true".to_string()
                    } else {
                        "false".to_string()
                    }
                }
                _ => format!("{}", i32::from_ne_bytes(buf)),
            })
        }
    }
}

/// A 1x1 offscreen color target with the given internal/read format.
unsafe fn make_target(
    gl: &glow::Context,
    internal: u32,
    format: u32,
    ty: u32,
) -> Result<FboTarget, String> {
    let tex = gl.create_texture().map_err(|e| format!("texture: {e}"))?;
    gl.bind_texture(glow::TEXTURE_2D, Some(tex));
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_MIN_FILTER,
        glow::NEAREST as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_MAG_FILTER,
        glow::NEAREST as i32,
    );
    gl.tex_image_2d(
        glow::TEXTURE_2D,
        0,
        internal as i32,
        1,
        1,
        0,
        format,
        ty,
        glow::PixelUnpackData::Slice(None),
    );
    let fbo = gl.create_framebuffer().map_err(|e| format!("fbo: {e}"))?;
    gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
    gl.framebuffer_texture_2d(
        glow::FRAMEBUFFER,
        glow::COLOR_ATTACHMENT0,
        glow::TEXTURE_2D,
        Some(tex),
        0,
    );
    let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
    if status != glow::FRAMEBUFFER_COMPLETE {
        return Err(format!("framebuffer incomplete: 0x{status:X}"));
    }
    Ok(FboTarget { fbo, _tex: tex })
}

unsafe fn compile_program(
    gl: &glow::Context,
    vertex: &str,
    fragment: &str,
) -> Result<glow::Program, String> {
    let vs = compile_stage(gl, glow::VERTEX_SHADER, vertex)?;
    let fs = match compile_stage(gl, glow::FRAGMENT_SHADER, fragment) {
        Ok(fs) => fs,
        Err(e) => {
            gl.delete_shader(vs);
            return Err(e);
        }
    };
    let program = gl.create_program().map_err(|e| format!("program: {e}"))?;
    gl.attach_shader(program, vs);
    gl.attach_shader(program, fs);
    gl.link_program(program);
    let ok = gl.get_program_link_status(program);
    gl.delete_shader(vs);
    gl.delete_shader(fs);
    if !ok {
        let log = gl.get_program_info_log(program);
        gl.delete_program(program);
        return Err(format!("glsl link: {log}"));
    }
    Ok(program)
}

unsafe fn compile_stage(
    gl: &glow::Context,
    kind: u32,
    src: &str,
) -> Result<glow::Shader, String> {
    let shader = gl.create_shader(kind).map_err(|e| format!("shader: {e}"))?;
    gl.shader_source(shader, src);
    gl.compile_shader(shader);
    if !gl.get_shader_compile_status(shader) {
        let log = gl.get_shader_info_log(shader);
        gl.delete_shader(shader);
        let stage = if kind == glow::VERTEX_SHADER { "vertex" } else { "fragment" };
        return Err(format!("glsl {stage} compile: {log}"));
    }
    Ok(shader)
}
