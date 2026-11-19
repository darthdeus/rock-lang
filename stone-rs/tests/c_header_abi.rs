use std::path::{Path, PathBuf};
use std::process::{Command, Output};

// Every fixture can check a declaration with `ABI(stone_fn, ret (*)(args));`.
const ABI_MACRO: &str = "-DABI(name,signature)=_Static_assert(\
__builtin_types_compatible_p(__typeof__(&name), signature), #name \" ABI\")";

fn clang(dir: &Path, name: &str, source: &str, compile_only: bool, objects: &[PathBuf]) -> PathBuf {
    let source_path = dir.join(format!("{name}.c"));
    std::fs::write(&source_path, source).expect("write C fixture");
    let output = dir.join(name);
    let result = Command::new("clang")
        .args(["-std=c11", "-O0", "-Wall", "-Wextra", "-Werror", ABI_MACRO])
        .arg("-I")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("include"))
        .args(compile_only.then_some("-c"))
        .arg(&source_path)
        .args(objects)
        .arg("-o")
        .arg(&output)
        .output()
        .expect("run clang");
    assert!(
        result.status.success(),
        "clang failed on {name}:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
    output
}

/// Builds `consumer` against stone.h, links each provider compiled on its own,
/// and runs it.
fn run_c(consumer: &str, providers: &[&str]) -> Output {
    let dir = tempfile::tempdir().expect("temporary C fixture directory");
    let objects: Vec<PathBuf> = providers
        .iter()
        .enumerate()
        .map(|(i, source)| clang(dir.path(), &format!("provider{i}"), source, true, &[]))
        .collect();
    let binary = clang(dir.path(), "consumer", consumer, false, &objects);
    Command::new(binary).output().expect("run C fixture")
}

fn assert_c_passes(consumer: &str, providers: &[&str], what: &str) {
    let output = run_c(consumer, providers);
    assert!(
        output.status.success(),
        "{what}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn stone_header_preserves_native_smoke_abi_types() {
    let provider = r#"
#include <stone.h>

static float captured;
static int title_seen;
static int show_seen;
static int activate_seen;

void stone_clear_canvas(float r, float g, float b, float a) {
    captured = r + g + b + a;
}

void stone_set_window_title(StoneWindow *window, const char *title) {
    title_seen = window == 0 && title != 0 && title[0] == 'C';
}

void stone_show_window_if_deferred(void) {
    show_seen = 1;
}

void stone_activate_window(StoneWindow *window) {
    activate_seen = window == 0;
}

double captured_clear_sum(void) {
    return captured;
}

int captured_title_seen(void) {
    return title_seen && show_seen && activate_seen;
}
"#;
    assert_c_passes(
        r#"
#include <stone.h>

extern double captured_clear_sum(void);
extern int captured_title_seen(void);

int main(void) {
    double rock_float = 0.25;
    stone_clear_canvas(rock_float, rock_float, rock_float, rock_float);
    stone_set_window_title((StoneWindow *)0, "C backend");
    stone_show_window_if_deferred();
    stone_activate_window((StoneWindow *)0);
    return captured_clear_sum() == 1.0 && captured_title_seen() ? 0 : 1;
}
"#,
        &[provider],
        "stone.h did not convert Rock's double-width float to Stone's C float ABI",
    );
    assert!(
        !run_c(
            r#"
extern void stone_clear_canvas(double r, double g, double b, double a);
extern double captured_clear_sum(void);

int main(void) {
    double rock_float = 0.25;
    stone_clear_canvas(rock_float, rock_float, rock_float, rock_float);
    return captured_clear_sum() == 1.0 ? 0 : 1;
}
"#,
            &[provider],
        )
        .status
        .success(),
        "the witness no longer distinguishes a guessed Rock-width prototype"
    );
}

#[test]
fn stone_header_exposes_native_game_loop_abi_types() {
    assert_c_passes(
        r#"
#include <stone.h>

bool stone_poll_event(StoneEvent *event) {
    event->event_type = STONE_EVENT_QUIT;
    return true;
}

double stone_get_time(void) { return 1.25; }
int stone_scancode_value(const char *token) { return token[0]; }
bool stone_is_mouse_button_down(int button) { return button == 1; }
void stone_get_mouse_position(int *x, int *y) { *x = 12; *y = 34; }
void stone_set_scissor(bool enabled, int x, int y, int w, int h) {
    (void)enabled; (void)x; (void)y; (void)w; (void)h;
}

int main(void) {
    StoneEvent event = {0};
    int x = 0;
    int y = 0;
    stone_get_mouse_position(&x, &y);
    stone_set_scissor(true, x, y, 8, 9);
    return stone_poll_event(&event)
        && event.event_type == STONE_EVENT_QUIT
        && stone_get_time() == 1.25
        && stone_scancode_value("a") == 'a'
        && stone_is_mouse_button_down(1)
        && x == 12 && y == 34 ? 0 : 1;
}
"#,
        &[],
        "stone.h native game-loop declarations disagree with their consumer",
    );
}

#[test]
fn stone_header_exposes_fullscreen_uv_scale_abi() {
    assert_c_passes(
        r#"
#include <stone.h>

ABI(stone_set_fullscreen_uv_scale, void (*)(float, float));

static float seen_x;
static float seen_y;

void stone_set_fullscreen_uv_scale(float sx, float sy) {
    seen_x = sx;
    seen_y = sy;
}

int main(void) {
    stone_set_fullscreen_uv_scale(0.25f, 0.75f);
    return seen_x == 0.25f && seen_y == 0.75f ? 0 : 1;
}
"#,
        &[],
        "stone.h fullscreen UV scale declaration did not match the provider ABI",
    );
}

#[test]
fn stone_header_exposes_fullscreen_quad_abi() {
    assert_c_passes(
        r#"
#include <stone.h>

ABI(stone_draw_fullscreen_quad, void (*)(void));

static int called;

void stone_draw_fullscreen_quad(void) {
    called = 1;
}

int main(void) {
    stone_draw_fullscreen_quad();
    return called ? 0 : 1;
}
"#,
        &[],
        "stone.h fullscreen quad declaration did not match the provider ABI",
    );
}

#[test]
fn stone_header_exposes_native_rendering_state_abi_types() {
    assert_c_passes(
        r#"
#include <stone.h>

ABI(stone_set_blend_mode_i, void (*)(int));
ABI(stone_bind_sampler, void (*)(int, int, uint64_t));
ABI(stone_set_canvas, void (*)(uint64_t));
ABI(stone_bind_canvas_texture_to_unit, bool (*)(uint64_t, int));
ABI(stone_set_shader, void (*)(uint64_t));
ABI(stone_shader_upload_uniform_block, bool (*)(uint64_t, const uint8_t *, int, uint64_t));
ABI(stone_bind_image_texture_to_unit, bool (*)(uint64_t, int));
ABI(stone_image_exists, bool (*)(uint64_t));
ABI(stone_create_canvas, uint64_t (*)(int, int, int, bool));
ABI(stone_pixel_readback_consume, float (*)(uint64_t));
ABI(stone_set_depth_mode, void (*)(const char *, bool));
ABI(stone_clear_depth, void (*)(float));
ABI(stone_set_color_mask, void (*)(bool, bool, bool, bool));
ABI(stone_set_draw_buffer_mask, void (*)(int));
ABI(stone_current_target_width, float (*)(void));
ABI(stone_current_target_height, float (*)(void));
ABI(stone_set_canvas_active, void (*)(uint64_t, int, int));
ABI(stone_set_gl_viewport, void (*)(int, int, int, int));
ABI(stone_set_present_viewport, void (*)(bool, int, int, int, int));
ABI(stone_draw_fullscreen_rect, void (*)(float, float, float, float));
ABI(stone_current_shader_bound, bool (*)(void));
ABI(stone_create_image_data, uint64_t (*)(int, int));
ABI(stone_image_data_size, bool (*)(uint64_t, int *, int *));
ABI(stone_destroy_image_data, void (*)(uint64_t));
ABI(stone_image_data_set_pixel, void (*)(uint64_t, int, int, float, float, float, float));
ABI(stone_image_update_from_data, bool (*)(uint64_t, uint64_t));
ABI(stone_new_image_from_data, uint64_t (*)(uint64_t, int));
ABI(stone_pixel_readback_new, uint64_t (*)(int));
ABI(stone_pixel_readback_area_new, uint64_t (*)(int));
ABI(stone_load_sound_deferred, uint64_t (*)(const char *, int64_t, int64_t, int64_t));
ABI(stone_load_sound_stream, uint64_t (*)(const char *, int64_t, int64_t, int64_t));
ABI(stone_sound_playable, bool (*)(uint64_t));
ABI(stone_sound_stream_bytes, int64_t (*)(uint64_t));
ABI(stone_unload_sound, bool (*)(uint64_t));
ABI(stone_load_music, uint64_t (*)(const char *));
ABI(stone_play_music, void (*)(uint64_t));

int main(void) { return 0; }
"#,
        &[],
        "stone.h rendering-state declarations did not match the provider ABI",
    );
}

#[test]
fn stone_header_exposes_scancode_lookup_abi_types() {
    assert_c_passes(
        r#"
#include <stone.h>
#include <string.h>

ABI(stone_scancode_count, int (*)(void));
ABI(stone_scancode_token_at, const char *(*)(int));
ABI(stone_scancode_token_for, const char *(*)(int));
ABI(stone_scancode_value, int (*)(const char *));
ABI(stone_scancode_label, const char *(*)(const char *));

int stone_scancode_count(void) { return 1; }
const char *stone_scancode_token_at(int index) { return index == 0 ? "left" : NULL; }
const char *stone_scancode_token_for(int scancode) { return scancode == 1 ? "left" : NULL; }
int stone_scancode_value(const char *token) { return token != NULL && strcmp(token, "left") == 0 ? 1 : -1; }
const char *stone_scancode_label(const char *token) { return token != NULL && strcmp(token, "left") == 0 ? "Left" : NULL; }

int main(void) {
    return stone_scancode_count() == 1
        && strcmp(stone_scancode_token_at(0), "left") == 0
        && strcmp(stone_scancode_token_for(1), "left") == 0
        && stone_scancode_value("left") == 1
        && strcmp(stone_scancode_label("left"), "Left") == 0 ? 0 : 1;
}
"#,
        &[],
        "stone.h scancode declarations did not match the exported provider ABI",
    );
}

#[test]
fn stone_header_exposes_length_aware_file_abi_types() {
    assert_c_passes(
        r#"
#include <stone.h>

ABI(stone_file_write, bool (*)(const char *, const char *, size_t));
ABI(stone_file_append, bool (*)(const char *, const char *, size_t));
ABI(stone_file_read, char *(*)(const char *, size_t *));
ABI(stone_free, void (*)(void *));
ABI(stone_file_exists_status, int32_t (*)(const char *));
ABI(stone_list_directory, bool (*)(const char *, char ***, int *));
ABI(stone_free_string_array, void (*)(char **, int));

int main(void) { return 0; }
"#,
        &[],
        "stone.h file declarations disagree with their exported ABI",
    );
}

#[test]
fn stone_header_declares_file_read_release_and_append_with_the_exported_abi() {
    assert_c_passes(
        r#"
#include <stone.h>
#include <string.h>

int main(void) {
    size_t len = 0;
    char *(*read_fn)(const char *, size_t *) = stone_file_read;
    void (*free_fn)(void *) = stone_free;
    char *data = read_fn("asset.bin", &len);
    int ok = data != NULL && len == 4 && memcmp(data, "DATA", len) == 0;
    free_fn(data);
    bool (*append_fn)(const char *, const char *, size_t) = stone_file_append;
    return ok && append_fn("asset.bin", "DATA", 4) ? 0 : 1;
}
"#,
        &[r#"
#include <stone.h>
#include <stdlib.h>
#include <string.h>

char *stone_file_read(const char *path, size_t *out_len) {
    static const char bytes[] = "DATA";
    if (path == NULL || strcmp(path, "asset.bin") != 0 || out_len == NULL) return NULL;
    char *copy = malloc(sizeof(bytes) - 1);
    if (copy == NULL) return NULL;
    memcpy(copy, bytes, sizeof(bytes) - 1);
    *out_len = sizeof(bytes) - 1;
    return copy;
}

void stone_free(void *ptr) { free(ptr); }

bool stone_file_append(const char *path, const char *data, size_t len) {
    return path != NULL && data != NULL && strcmp(path, "asset.bin") == 0
        && len == 4 && memcmp(data, "DATA", len) == 0;
}
"#],
        "stone.h file-read declarations did not match the exported provider ABI",
    );
}

#[test]
fn stone_event_header_layout_matches_rust_abi() {
    let output = run_c(
        r#"
#include <stddef.h>
#include <stdio.h>
#include <stone.h>

int main(void) {
    printf("%zu %zu %zu %zu %zu %zu %zu %zu %zu\n",
        sizeof(StoneEvent),
        offsetof(StoneEvent, event_type),
        offsetof(StoneEvent, repeat),
        offsetof(StoneEvent, mouse_button),
        offsetof(StoneEvent, pad_button),
        offsetof(StoneEvent, pad_sync),
        offsetof(StoneEvent, text),
        sizeof(((StoneEvent *)0)->text),
        (size_t)STONE_EVENT_TEXTINPUT);
    return 0;
}
"#,
        &[],
    );
    assert!(output.status.success());
    let c_layout = String::from_utf8(output.stdout)
        .expect("layout output must be UTF-8")
        .split_whitespace()
        .map(|value| {
            value
                .parse::<usize>()
                .expect("layout field must be numeric")
        })
        .collect::<Vec<_>>();
    let rust_layout = vec![
        std::mem::size_of::<stone_rs::StoneEvent>(),
        std::mem::offset_of!(stone_rs::StoneEvent, event_type),
        std::mem::offset_of!(stone_rs::StoneEvent, repeat),
        std::mem::offset_of!(stone_rs::StoneEvent, mouse_button),
        std::mem::offset_of!(stone_rs::StoneEvent, pad_button),
        std::mem::offset_of!(stone_rs::StoneEvent, pad_sync),
        std::mem::offset_of!(stone_rs::StoneEvent, text),
        std::mem::size_of_val(&stone_rs::StoneEvent::default().text),
        stone_rs::StoneEventType::TextInput as usize,
    ];
    assert_eq!(c_layout, rust_layout);
}

#[test]
fn stone_header_declares_font_instance_image_with_its_exported_abi() {
    assert_c_passes(
        r#"
#include <stone.h>

int main(void) {
    return stone_get_font_instance_image(UINT64_C(7)) == UINT64_C(12) ? 0 : 1;
}
"#,
        &[r#"
#include <stone.h>

uint64_t stone_get_font_instance_image(uint64_t instance_id) {
    return instance_id + UINT64_C(5);
}
"#],
        "stone.h font declaration did not match the exported provider ABI",
    );
}

#[test]
fn stone_header_declares_engine_ffi_with_exported_abis() {
    assert_c_passes(
        r#"
#include <stone.h>

ABI(stone_bus_duck, bool (*)(uint32_t, float, float, float));
ABI(stone_bus_set_gain, bool (*)(uint32_t, float));
ABI(stone_bus_set_lowpass, bool (*)(uint32_t, float, float));
ABI(stone_canvas_exists, bool (*)(uint64_t));
ABI(stone_get_window_size, void (*)(int *, int *));
ABI(stone_get_drawable_size, void (*)(int *, int *));
ABI(stone_get_dpi_scale, float (*)(void));
ABI(stone_get_config_imgui_scale, float (*)(void));
ABI(stone_canvas_attach_id_target, bool (*)(uint64_t, uint64_t));
ABI(stone_clear_canvas_int, void (*)(int, int));
ABI(stone_get_white_image, uint64_t (*)(void));
ABI(stone_destroy_canvas, void (*)(uint64_t));
ABI(stone_ensure_font_atlas_instance, uint64_t (*)(uint32_t, int));
ABI(stone_file_sync, bool (*)(const char *));
ABI(stone_font_atlas_instance_count, int (*)(uint32_t));
ABI(stone_get_display_count, int (*)(void));
ABI(stone_proc_reset, int (*)(void));
ABI(stone_get_font_instance_byte_len, uint64_t (*)(uint64_t));
ABI(stone_get_font_instance_glyph_info, bool (*)(uint64_t, int, int, float *));
ABI(stone_get_font_instance_pixel_size, int (*)(uint64_t));
ABI(stone_get_font_instance_size, bool (*)(uint64_t, int *, int *));
ABI(stone_get_font_slot_codepoint, int (*)(uint32_t, int));
ABI(stone_get_font_slot_count, int (*)(uint32_t));
ABI(stone_get_font_unit_metrics, bool (*)(uint32_t, float *));
ABI(stone_get_glyph_unit_metrics, bool (*)(uint32_t, int, int, float *));
ABI(stone_get_image_size, void (*)(uint64_t, int *, int *));
ABI(stone_flush_image_uploads, void (*)(void));
ABI(stone_get_graphics_stats, void (*)(StoneGraphicsStats *));
ABI(stone_track_text_draw_call, void (*)(void));
ABI(stone_get_image_frame_stats, void (*)(StoneImageLoadStats *));
ABI(stone_get_image_profile_stats, void (*)(StoneImageLoadStats *));
ABI(stone_get_sound_duration, float (*)(uint64_t));
ABI(stone_reload_sound, bool (*)(uint64_t, const char *));
ABI(stone_image_set_wrap, bool (*)(uint64_t, bool));
ABI(stone_is_directory_status, int32_t (*)(const char *));
ABI(stone_create_directory, bool (*)(const char *));
ABI(stone_load_image, uint64_t (*)(const char *, int, int, int, int));
ABI(stone_reload_image, bool (*)(uint64_t, const char *));
ABI(stone_mesh_set_instances, int (*)(uint64_t, const float *, int, int, const int *, int));
ABI(stone_mesh_update_instances, int (*)(uint64_t, const float *, int));
ABI(stone_mesh_free, void (*)(uint64_t));
ABI(stone_pixel_readback_submit, void (*)(uint64_t, uint64_t, int, int));
ABI(stone_play_sound, uint64_t (*)(uint64_t, uint32_t, float, float, float, bool, bool, float));
ABI(stone_read_canvas_nearest_id, int (*)(uint64_t, int, int, int));
ABI(stone_read_canvas_pixel, float (*)(uint64_t, int, int));
ABI(stone_remove_file, bool (*)(const char *));
ABI(stone_remove_dir, bool (*)(const char *));
ABI(stone_rename, bool (*)(const char *, const char *));
ABI(stone_set_cursor, void (*)(uint64_t));
ABI(stone_show_cursor, void (*)(bool));
ABI(stone_create_cursor, uint64_t (*)(const char *, int, int));
ABI(stone_create_cursor_from_image_data, uint64_t (*)(uint64_t, int, int));
ABI(stone_set_system_cursor, uint64_t (*)(int));
ABI(stone_reset_cursor, void (*)(void));
ABI(stone_free_cursor, void (*)(uint64_t));
ABI(stone_unload_image, void (*)(uint64_t));
ABI(stone_voice_active, bool (*)(uint64_t));
ABI(stone_voice_set_gain, bool (*)(uint64_t, float));
ABI(stone_voice_stop, bool (*)(uint64_t));
ABI(stone_watch_asset, void (*)(const char *));
ABI(stone_poll_changed_assets, int (*)(void));
ABI(stone_create_shader_from_combined, uint64_t (*)(const char *));
ABI(stone_destroy_shader, void (*)(uint64_t));
ABI(stone_shader_bind_uniform_block, bool (*)(uint64_t, const char *, const char *, int, int, uint64_t));
ABI(stone_shader_check_uniform_block_layout, bool (*)(uint64_t, const char *, const char *, int, const int *, int, int));

int main(void) { return 0; }
"#,
        &[],
        "stone.h engine FFI declarations disagree with their exported ABI",
    );
}

#[test]
fn stone_header_declares_font_lifecycle_with_its_exported_abi() {
    assert_c_passes(
        r#"
#include <stone.h>

ABI(stone_load_font, uint32_t (*)(const char *, int, int, const char *));
ABI(stone_unload_font, bool (*)(uint32_t));
ABI(stone_font_release_coverage, bool (*)(uint32_t));
ABI(stone_reload_font, bool (*)(uint32_t, const char *, int));
ABI(stone_resize_font, bool (*)(uint32_t, int));
ABI(stone_get_font_pixel_size, int (*)(uint32_t));

int main(void) {
    uint32_t id = stone_load_font("font.ttf", 16, 3, "x");
    return id == UINT32_C(7)
        && stone_reload_font(id, "font.ttf", 24)
        && stone_resize_font(id, 32)
        && stone_get_font_pixel_size(id) == 24
        && stone_font_release_coverage(id)
        && stone_unload_font(id) ? 0 : 1;
}
"#,
        &[r#"
#include <stone.h>

uint32_t stone_load_font(const char *path, int pixel_size, int face_mask, const char *extra_chars) {
    return path != NULL && pixel_size == 16 && face_mask == 3 && extra_chars != NULL && extra_chars[0] == 'x' ? UINT32_C(7) : 0;
}
bool stone_unload_font(uint32_t font_id) { return font_id == UINT32_C(7); }
bool stone_font_release_coverage(uint32_t font_id) { return font_id == UINT32_C(7); }
bool stone_reload_font(uint32_t font_id, const char *path, int pixel_size) {
    return font_id == UINT32_C(7) && path != NULL && pixel_size == 24;
}
bool stone_resize_font(uint32_t font_id, int pixel_size) {
    return font_id == UINT32_C(7) && pixel_size == 32;
}
int stone_get_font_pixel_size(uint32_t font_id) {
    return font_id == UINT32_C(7) ? 24 : 0;
}
"#],
        "stone.h font lifecycle declarations did not match the exported provider ABI",
    );
}

#[test]
fn stone_header_declares_font_glyph_info_with_its_exported_abi() {
    assert_c_passes(
        r#"
#include <stone.h>

ABI(stone_get_glyph_info, bool (*)(uint32_t, int, int, float *));

bool stone_get_glyph_info(uint32_t font_id, int face, int char_code, float *out) {
    if (font_id != UINT32_C(7) || face != 1 || char_code != 2 || out == NULL) return false;
    out[0] = 1.5f;
    return true;
}

int main(void) {
    float out[9] = {0};
    return stone_get_glyph_info(UINT32_C(7), 1, 2, out)
        && out[0] == 1.5f ? 0 : 1;
}
"#,
        &[],
        "stone.h glyph-info declaration did not match the exported provider ABI",
    );
}

#[test]
fn stone_header_declares_font_atlas_images_with_their_exported_abi() {
    assert_c_passes(
        r#"
#include <stone.h>

ABI(stone_get_font_atlas_image, uint64_t (*)(uint32_t));
ABI(stone_get_font_atlas_image_cached, uint64_t (*)(uint32_t));

uint64_t stone_get_font_atlas_image(uint32_t font_id) {
    return font_id == UINT32_C(7) ? UINT64_C(10) : 0;
}
uint64_t stone_get_font_atlas_image_cached(uint32_t font_id) {
    return font_id == UINT32_C(7) ? UINT64_C(11) : 0;
}

int main(void) {
    return stone_get_font_atlas_image(UINT32_C(7)) == UINT64_C(10)
        && stone_get_font_atlas_image_cached(UINT32_C(7)) == UINT64_C(11) ? 0 : 1;
}
"#,
        &[],
        "stone.h font atlas image declarations did not match the exported provider ABI",
    );
}

#[test]
fn stone_header_declares_hostname_with_its_exported_abi() {
    assert_c_passes(
        r#"
#include <stone.h>

ABI(stone_get_hostname, const char *(*)(void));

const char *stone_get_hostname(void) { return "native-host"; }

int main(void) {
    const char *host = stone_get_hostname();
    return host != NULL && host[0] == 'n' && host[10] == 't' ? 0 : 1;
}
"#,
        &[],
        "stone.h hostname declaration did not match the exported provider ABI",
    );
}

#[test]
fn stone_header_declares_native_path_reads_with_their_exported_abi() {
    assert_c_passes(
        r#"
#include <stone.h>

ABI(stone_get_working_directory, const char *(*)(void));
ABI(stone_get_executable_path, const char *(*)(void));
ABI(stone_changed_asset_path, const char *(*)(int));

const char *stone_get_working_directory(void) { return "native-cwd"; }
const char *stone_get_executable_path(void) { return "native-bin"; }
const char *stone_changed_asset_path(int index) { return index == 2 ? "changed" : NULL; }

int main(void) {
    const char *cwd = stone_get_working_directory();
    const char *exe = stone_get_executable_path();
    const char *changed = stone_changed_asset_path(2);
    return cwd[0] == 'n' && exe[0] == 'n' && changed[0] == 'c' ? 0 : 1;
}
"#,
        &[],
        "stone.h native path declarations did not match the exported provider ABI",
    );
}

#[test]
fn stone_header_declares_pref_path_with_its_exported_abi() {
    assert_c_passes(
        r#"
#include <stone.h>

int main(void) {
    const char *path = stone_get_pref_path("org", "app");
    return path != NULL && path[0] == 'p' && path[5] == '/' ? 0 : 1;
}
"#,
        &[r#"
#include <stone.h>

const char *stone_get_pref_path(const char *organization, const char *application) {
    return organization[0] == 'o' && application[0] == 'a' ? "prefs/" : NULL;
}
"#],
        "stone.h preference path declaration did not match the exported provider ABI",
    );
}

#[test]
fn stone_header_declares_telemetry_close_with_its_exported_abi() {
    assert_c_passes(
        r#"
#include <stone.h>

extern int telemetry_was_closed(void);

int main(void) {
    void (*close_fn)(void) = stone_telemetry_close;
    close_fn();
    return telemetry_was_closed() ? 0 : 1;
}
"#,
        &[r#"
static int closed;
void stone_telemetry_close(void) { closed = 1; }
int telemetry_was_closed(void) { return closed; }
"#],
        "stone.h telemetry close declaration did not match its exported ABI",
    );
}

#[test]
fn stone_header_declares_telemetry_clear_with_its_exported_abi() {
    assert_c_passes(
        r#"
#include <stone.h>

int main(void) {
    bool (*clear_fn)(const char *) = stone_telemetry_clear;
    return clear_fn("root") ? 0 : 1;
}
"#,
        &[r#"
#include <stone.h>

bool stone_telemetry_clear(const char *root) {
    return root != 0 && root[0] == 'r';
}
"#],
        "stone.h telemetry clear declaration did not match its exported ABI",
    );
}

#[test]
fn stone_header_declares_telemetry_configure_with_its_exported_abi() {
    assert_c_passes(
        r#"
#include <stone.h>

int main(void) {
    bool (*configure_fn)(
        const char *, const char *, const char *, const char *,
        const char *, const char *, const char *, bool) = stone_telemetry_configure;
    return configure_fn("root", "game", "build", "platform", "channel", "endpoint", "token", true)
        ? 0 : 1;
}
"#,
        &[r#"
#include <stone.h>

bool stone_telemetry_configure(
    const char *root, const char *game, const char *build, const char *platform,
    const char *channel, const char *endpoint, const char *token, bool explicit_reconsent) {
    (void)root;
    (void)game;
    (void)build;
    (void)platform;
    (void)channel;
    (void)endpoint;
    (void)token;
    return explicit_reconsent;
}
"#],
        "stone.h telemetry configure declaration did not match its exported ABI",
    );
}

#[test]
fn stone_header_imgui_frontier_uses_typed_native_transport() {
    assert_c_passes(
        r#"#include <stone.h>
int main(void) { return 0; }
"#,
        &[],
        "stone.h ImGui frontier declarations disagree with their exported ABI",
    );
}
