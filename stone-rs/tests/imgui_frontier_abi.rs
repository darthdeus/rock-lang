#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::process::Command;

#[path = "support/stone_cdylib.rs"]
mod stone_cdylib;

#[cfg(target_os = "linux")]
#[path = "support/private_display.rs"]
mod private_display;

fn compile(source: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let c = dir.path().join("frontier.c");
    let binary = dir.path().join("frontier");
    std::fs::write(&c, source).unwrap();
    let artifact = stone_cdylib::artifact();
    let lib = artifact.parent().unwrap();
    let gamelibs = gamelibs::host_dir().join("include");
    let output = Command::new("clang")
        .args(["-std=c11", "-O0", "-Wall", "-Wextra", "-Werror"])
        .arg("-I")
        .arg(gamelibs)
        .arg("-L")
        .arg(lib)
        // rpath: Cargo's LD_LIBRARY_PATH can contain an older top-level cdylib.
        .arg(format!("-Wl,-rpath,{}", lib.display()))
        .arg(c)
        .arg(artifact)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (dir, binary)
}

fn run(source: &str, headless: bool) {
    let (_dir, binary) = compile(source);
    let mut command = Command::new(binary);
    command.current_dir(_dir.path());
    if headless {
        command.env("STONE_HEADLESS", "1");
    } else {
        command.env_remove("STONE_HEADLESS");
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
}

fn headless_probe() -> (tempfile::TempDir, std::path::PathBuf) {
    compile(
        r#"
#include <stdbool.h>
typedef struct StoneWindow StoneWindow;
extern bool stone_init(void);
extern bool stone_teardown(void);
extern StoneWindow *stone_create_window_ex(const char *title, int x, int y, int width, int height,
    int display, bool resizable, int fullscreen);
#include <assert.h>
#include <dlfcn.h>
#define CIMGUI_DEFINE_ENUMS_AND_STRUCTS
#include <cimgui/cimgui.h>
#define IG(name, type) ((type)dlsym(RTLD_DEFAULT, #name))
extern bool stone_imgui_init(StoneWindow *window);
extern void stone_imgui_shutdown(void);
extern void stone_imgui_new_frame(void);
extern void *stone_imgui_prepare_render(void);
int main(void) {
    assert(stone_init());
    StoneWindow *window = stone_create_window_ex("Headless ini", 0, 0, 320, 240, -1, false, 0);
    assert(window != NULL);
    assert(stone_imgui_init(window));
    ImGuiIO *(*get_io)(void) = (ImGuiIO *(*)(void))dlsym(RTLD_DEFAULT, "igGetIO_Nil");
    assert(get_io != NULL);
    get_io()->ConfigFlags |= ImGuiConfigFlags_DockingEnable;
    stone_imgui_new_frame();
    IG(igBegin, bool (*)(const char *, bool *, int))("Headless ini", NULL, 0);
    IG(igTextUnformatted, void (*)(const char *, const char *))("Concurrent settings probe", NULL);
    IG(igEnd, void (*)(void))();
    assert(stone_imgui_prepare_render() != NULL);
    stone_imgui_shutdown();
    assert(stone_teardown());
    return 0;
}
"#,
    )
}

fn concurrent_probes(dir: &std::path::Path, binary: &std::path::Path) {
    let outputs = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    Command::new(binary)
                        .current_dir(dir)
                        .env("STONE_HEADLESS", "1")
                        .output()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    for output in outputs {
        let output = output.unwrap();
        assert!(output.status.success(), "{output:?}");
    }
}

#[test]
fn concurrent_headless_imgui_does_not_persist_shared_settings() {
    let (dir, binary) = headless_probe();
    let shared_ini = dir.path().join("imgui.ini");
    concurrent_probes(dir.path(), &binary);
    assert!(
        !shared_ini.exists(),
        "headless ImGui wrote shared {}",
        shared_ini.display()
    );
}

#[test]
fn concurrent_headless_imgui_ignores_shared_dock_settings() {
    let (dir, binary) = headless_probe();
    let shared_ini = dir.path().join("imgui.ini");
    let settings = "[Window][Headless ini]\nPos=60,60\nSize=320,240\nCollapsed=0\nDockId=0x00000001\n\n[Docking][Data]\nDockSpace ID=0x00000001 Window=0x1BBC0F80 Pos=0,0 Size=320,240 Split=X\n  DockNode ID=0x00000001 Parent=0x00000001 SizeRef=320,240 CentralNode=1\n\n";
    std::fs::write(&shared_ini, settings).unwrap();
    concurrent_probes(dir.path(), &binary);
    assert_eq!(std::fs::read_to_string(shared_ini).unwrap(), settings);
}

#[test]
fn desktop_imgui_frame_produces_draw_data() {
    // The probe needs a real window; SDL's headless `offscreen` driver aborts
    // on `window != NULL`. macOS always has a GUI session.
    #[cfg(target_os = "linux")]
    if !private_display::available() {
        return;
    }
    run(
        r#"
#define _GNU_SOURCE
#include <stdbool.h>
typedef struct StoneWindow StoneWindow;
extern bool stone_init(void);
extern bool stone_teardown(void);
extern StoneWindow *stone_create_window_ex(const char *title, int x, int y, int width, int height,
    int display, bool resizable, int fullscreen);
#include <assert.h>
#include <dlfcn.h>
#define CIMGUI_DEFINE_ENUMS_AND_STRUCTS
#include <cimgui/cimgui.h>
#define IG(name, type) ((type)dlsym(RTLD_DEFAULT, #name))
extern bool stone_imgui_init(StoneWindow *window);
extern void stone_imgui_shutdown(void);
extern void stone_imgui_new_frame(void);
extern bool stone_imgui_initialized(void);
extern void *stone_imgui_prepare_render(void);
extern int stone_imgui_last_vtx_count(void);
int main(void) {
    assert(stone_init());
    StoneWindow *window = stone_create_window_ex("ImGui frontier", 0, 0, 640, 480, -1, false, 0);
    assert(window != NULL);
    assert(stone_imgui_init(window));
    assert(stone_imgui_initialized());
    stone_imgui_new_frame();
    IG(igSetNextWindowPos, void (*)(ImVec2, int, ImVec2))((ImVec2){20, 20}, 0, (ImVec2){0, 0});
    IG(igSetNextWindowSize, void (*)(ImVec2, int))((ImVec2){400, 400}, 0);
    assert(IG(igBegin, bool (*)(const char *, bool *, int))("frontier", NULL, 0));
    assert(!IG(igButton, bool (*)(const char *, ImVec2))("button", (ImVec2){30, 20}));
    IG(igEnd, void (*)(void))();
    assert(stone_imgui_prepare_render() != NULL);
    assert(stone_imgui_last_vtx_count() > 0);
    stone_imgui_shutdown();
    assert(stone_teardown());
    return 0;
}
"#,
        false,
    );
}

/// Hover must land on the widget under the pointer after Stone follows the WM exit.
#[cfg(target_os = "linux")]
#[test]
fn imgui_pointer_follows_a_window_the_wm_moved_out_of_fullscreen() {
    if !private_display::available() {
        return;
    }
    run(
        r#"
#define _GNU_SOURCE
#include <stdbool.h>
typedef struct StoneWindow StoneWindow;
extern bool stone_init(void);
extern bool stone_teardown(void);
extern StoneWindow *stone_create_window_ex(const char *title, int x, int y, int width, int height,
    int display, bool resizable, int fullscreen);
extern bool stone_poll_event(void *event);
extern void stone_swap_window(StoneWindow *window);
extern void stone_show_window_if_deferred(void);
#include <assert.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#define CIMGUI_DEFINE_ENUMS_AND_STRUCTS
#include <cimgui/cimgui.h>
#define IG(name, type) ((type)dlsym(RTLD_DEFAULT, #name))
extern bool stone_imgui_init(StoneWindow *window);
extern void stone_imgui_shutdown(void);
extern void stone_imgui_new_frame(void);
extern void stone_imgui_render(void);
extern void stone_set_fullscreen(int mode);

static bool hovered;
static void frame(StoneWindow *window) {
    _Alignas(8) unsigned char event[128];
    while (stone_poll_event(event)) {}
    stone_imgui_new_frame();
    IG(igSetNextWindowPos, void (*)(ImVec2, int, ImVec2))((ImVec2){20, 20}, 0, (ImVec2){0, 0});
    IG(igSetNextWindowSize, void (*)(ImVec2, int))((ImVec2){200, 200}, 0);
    IG(igBegin, bool (*)(const char *, bool *, int))("moved", NULL, 0);
    IG(igButton, bool (*)(const char *, ImVec2))("target", (ImVec2){150, 100});
    hovered = IG(igIsItemHovered, bool (*)(int))(0);
    IG(igEnd, void (*)(void))();
    stone_imgui_render();
    stone_swap_window(window);
    stone_show_window_if_deferred();
    usleep(16000);
}

int main(void) {
    assert(stone_init());
    StoneWindow *window = stone_create_window_ex("ImGui moved", 0, 0, 640, 480, -1, true, 0);
    assert(window != NULL);
    assert(stone_imgui_init(window));
    void *(*current_window)(void) = (void *(*)(void))dlsym(RTLD_DEFAULT, "SDL_GL_GetCurrentWindow");
    unsigned (*window_flags)(void *) = (unsigned (*)(void *))dlsym(RTLD_DEFAULT, "SDL_GetWindowFlags");
    void *(*keyboard_focus)(void) = (void *(*)(void))dlsym(RTLD_DEFAULT, "SDL_GetKeyboardFocus");
    void (*warp)(void *, int, int) = (void (*)(void *, int, int))dlsym(RTLD_DEFAULT, "SDL_WarpMouseInWindow");
    unsigned (*global_mouse)(int *, int *) = (unsigned (*)(int *, int *))dlsym(RTLD_DEFAULT, "SDL_GetGlobalMouseState");
    ImGuiIO *(*get_io)(void) = (ImGuiIO *(*)(void))dlsym(RTLD_DEFAULT, "igGetIO_Nil");
    assert(current_window && window_flags && keyboard_focus && warp && global_mouse && get_io);
    for (int i = 0; i < 20; i++) frame(window);
    stone_set_fullscreen(2);
    for (int i = 0; i < 20; i++) frame(window);
    void *sdl_window = current_window();
    assert(window_flags(sdl_window) & 0x1); // SDL_WINDOW_FULLSCREEN

    assert(system("i3-msg -q '[title=\"ImGui moved\"] fullscreen disable, floating enable, "
                  "resize set 640 480, move position 100 100, focus'") == 0);
    for (int i = 0; i < 20; i++) frame(window);
    warp(sdl_window, 95, 95);
    for (int i = 0; i < 10; i++) frame(window);

    int gx = 0, gy = 0;
    global_mouse(&gx, &gy);
    ImGuiIO *io = get_io();
    printf("flags=%x focused=%d global=(%d,%d) imgui=(%.0f,%.0f) hovered=%d\n",
        window_flags(sdl_window), keyboard_focus() == sdl_window, gx, gy,
        io->MousePos.x, io->MousePos.y, hovered);
    fflush(stdout);
    // The window has left fullscreen, sits at (100,100), and owns keyboard focus.
    assert(!(window_flags(sdl_window) & 0x1));
    assert(keyboard_focus() == sdl_window);
    assert(gx == 195 && gy == 195);
    assert(io->MousePos.x == 95 && io->MousePos.y == 95);
    assert(hovered);

    stone_imgui_shutdown();
    assert(stone_teardown());
    return 0;
}
"#,
        false,
    );
}
