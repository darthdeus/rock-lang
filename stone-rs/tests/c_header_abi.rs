use std::path::Path;
use std::process::Command;

fn compile(source: &Path, output: &Path, extra: &[&Path]) {
    let include = Path::new(env!("CARGO_MANIFEST_DIR")).join("include");
    let result = Command::new("clang")
        .args(["-std=c11", "-O0", "-Wall", "-Wextra", "-Werror"])
        .arg("-I")
        .arg(include)
        .arg(source)
        .args(extra)
        .arg("-o")
        .arg(output)
        .output()
        .expect("run clang");
    assert!(
        result.status.success(),
        "clang failed:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn stone_header_preserves_native_smoke_abi_types() {
    let dir = tempfile::tempdir().expect("temporary C fixture directory");
    let provider = dir.path().join("provider.c");
    let provider_object = dir.path().join("provider.o");
    let header_consumer = dir.path().join("header_consumer.c");
    let guessed_consumer = dir.path().join("guessed_consumer.c");
    let header_binary = dir.path().join("header_consumer");
    let guessed_binary = dir.path().join("guessed_consumer");

    std::fs::write(
        &provider,
        r#"
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
"#,
    )
    .expect("write C ABI provider");
    let result = Command::new("clang")
        .args(["-std=c11", "-O0", "-Wall", "-Wextra", "-Werror", "-c"])
        .arg("-I")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("include"))
        .arg(&provider)
        .arg("-o")
        .arg(&provider_object)
        .output()
        .expect("compile C ABI provider");
    assert!(
        result.status.success(),
        "provider compile failed:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );

    std::fs::write(
        &header_consumer,
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
    )
    .expect("write header C consumer");
    compile(&header_consumer, &header_binary, &[&provider_object]);
    assert!(
        Command::new(&header_binary)
            .status()
            .expect("run header C consumer")
            .success(),
        "stone.h did not convert Rock's double-width float to Stone's C float ABI"
    );

    std::fs::write(
        &guessed_consumer,
        r#"
extern void stone_clear_canvas(double r, double g, double b, double a);
extern double captured_clear_sum(void);

int main(void) {
    double rock_float = 0.25;
    stone_clear_canvas(rock_float, rock_float, rock_float, rock_float);
    return captured_clear_sum() == 1.0 ? 0 : 1;
}
"#,
    )
    .expect("write guessed C consumer");
    compile(&guessed_consumer, &guessed_binary, &[&provider_object]);
    assert!(
        !Command::new(&guessed_binary)
            .status()
            .expect("run guessed C consumer")
            .success(),
        "the witness no longer distinguishes a guessed Rock-width prototype"
    );
}

#[test]
fn stone_header_exposes_native_game_loop_abi_types() {
    let dir = tempfile::tempdir().expect("temporary C fixture directory");
    let source = dir.path().join("game_loop_abi.c");
    let binary = dir.path().join("game_loop_abi");

    std::fs::write(
        &source,
        r#"
#include <stone.h>

bool stone_poll_event(StoneEvent *event) {
    event->event_type = STONE_EVENT_QUIT;
    return true;
}

double stone_get_time(void) { return 1.25; }
int stone_scancode_value(const char *token) { return token[0]; }
bool stone_is_key_pressed(int scancode) { return scancode == 'a'; }
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
        && stone_is_key_pressed(stone_scancode_value("a"))
        && stone_is_mouse_button_down(1)
        && x == 12 && y == 34 ? 0 : 1;
}
"#,
    )
    .expect("write native game-loop ABI fixture");

    compile(&source, &binary, &[]);
    assert!(
        Command::new(&binary)
            .status()
            .expect("run native game-loop ABI fixture")
            .success(),
        "stone.h native game-loop declarations disagree with their consumer"
    );
}

#[test]
fn stone_header_exposes_fullscreen_uv_scale_abi() {
    let dir = tempfile::tempdir().expect("temporary fullscreen UV ABI directory");
    let source = dir.path().join("fullscreen_uv_scale_abi.c");
    let binary = dir.path().join("fullscreen_uv_scale_abi");

    std::fs::write(
        &source,
        r#"
#include <stone.h>

_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_set_fullscreen_uv_scale),
    void (*)(float, float)), "stone_set_fullscreen_uv_scale ABI");

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
    )
    .expect("write fullscreen UV scale ABI fixture");

    compile(&source, &binary, &[]);
    assert!(
        Command::new(&binary)
            .status()
            .expect("run fullscreen UV scale ABI fixture")
            .success(),
        "stone.h fullscreen UV scale declaration did not match the provider ABI"
    );
}

#[test]
fn stone_header_exposes_fullscreen_quad_abi() {
    let dir = tempfile::tempdir().expect("temporary fullscreen quad ABI directory");
    let source = dir.path().join("fullscreen_quad_abi.c");
    let binary = dir.path().join("fullscreen_quad_abi");

    std::fs::write(
        &source,
        r#"
#include <stone.h>

_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_draw_fullscreen_quad),
    void (*)(void)), "stone_draw_fullscreen_quad ABI");

static int called;

void stone_draw_fullscreen_quad(void) {
    called = 1;
}

int main(void) {
    stone_draw_fullscreen_quad();
    return called ? 0 : 1;
}
"#,
    )
    .expect("write fullscreen quad ABI fixture");

    compile(&source, &binary, &[]);
    assert!(
        Command::new(&binary)
            .status()
            .expect("run fullscreen quad ABI fixture")
            .success(),
        "stone.h fullscreen quad declaration did not match the provider ABI"
    );
}

#[test]
fn stone_header_exposes_imgui_widget_abi_types() {
    let dir = tempfile::tempdir().expect("temporary C fixture directory");
    let source = dir.path().join("imgui_mouse_abi.c");
    let binary = dir.path().join("imgui_mouse_abi");

    std::fs::write(
        &source,
        r#"
#include <stone.h>
#include <string.h>

_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_using_mouse),
    bool (*)(void)), "stone_imgui_using_mouse ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_wants_mouse),
    bool (*)(void)), "stone_imgui_wants_mouse ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_wants_text_input),
    bool (*)(void)), "stone_imgui_wants_text_input ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_button),
    bool (*)(const char *)), "stone_imgui_button ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_begin_combo),
    bool (*)(const char *, const char *)),
    "stone_imgui_begin_combo ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_end_combo),
    void (*)(void)),
    "stone_imgui_end_combo ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_set_item_default_focus),
    void (*)(void)),
    "stone_imgui_set_item_default_focus ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_open_popup),
    void (*)(const char *)),
    "stone_imgui_open_popup ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_begin_popup),
    bool (*)(const char *)),
    "stone_imgui_begin_popup ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_end_popup),
    void (*)(void)),
    "stone_imgui_end_popup ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_menu_item),
    bool (*)(const char *, bool)),
    "stone_imgui_menu_item ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_combo_str),
    void (*)(const char *, int, StoneString, bool *, int *)),
    "stone_imgui_combo_str ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_input_text),
    bool (*)(const char *, char *, size_t, int)),
    "stone_imgui_input_text ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_input_text_multiline),
    bool (*)(const char *, char *, size_t, float, float, int)),
    "stone_imgui_input_text_multiline ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_begin_table),
    bool (*)(const char *, int, int)), "stone_imgui_begin_table ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_end_table),
    void (*)(void)), "stone_imgui_end_table ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_table_headers_row),
    void (*)(void)), "stone_imgui_table_headers_row ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_table_next_row),
    void (*)(void)), "stone_imgui_table_next_row ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_table_next_column),
    bool (*)(void)), "stone_imgui_table_next_column ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_table_setup_column_fixed),
    void (*)(const char *, float)), "stone_imgui_table_setup_column_fixed ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_table_setup_column_stretch),
    void (*)(const char *)), "stone_imgui_table_setup_column_stretch ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_begin_disabled),
    void (*)(bool)), "stone_imgui_begin_disabled ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_end_disabled),
    void (*)(void)), "stone_imgui_end_disabled ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_indent),
    void (*)(float)), "stone_imgui_indent ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_unindent),
    void (*)(float)), "stone_imgui_unindent ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_collapsing_header_closed),
    bool (*)(const char *)), "stone_imgui_collapsing_header_closed ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_invisible_button),
    bool (*)(const char *, float, float)), "stone_imgui_invisible_button ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_selectable),
    bool (*)(const char *, bool)), "stone_imgui_selectable ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_get_mouse_pos),
    void (*)(float *, float *)), "stone_imgui_get_mouse_pos ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_get_content_region_avail),
    void (*)(float *, float *)), "stone_imgui_get_content_region_avail ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_get_window_pos),
    void (*)(float *, float *)), "stone_imgui_get_window_pos ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_get_window_size),
    void (*)(float *, float *)), "stone_imgui_get_window_size ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_is_any_item_active),
    bool (*)(void)), "stone_imgui_is_any_item_active ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_is_item_active),
    bool (*)(void)), "stone_imgui_is_item_active ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_is_item_hovered),
    bool (*)(int)), "stone_imgui_is_item_hovered ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_is_item_activated),
    bool (*)(void)), "stone_imgui_is_item_activated ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_is_item_deactivated),
    bool (*)(void)), "stone_imgui_is_item_deactivated ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_is_item_deactivated_after_edit),
    bool (*)(void)), "stone_imgui_is_item_deactivated_after_edit ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_is_item_visible),
    bool (*)(void)), "stone_imgui_is_item_visible ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_is_window_docked),
    bool (*)(void)), "stone_imgui_is_window_docked ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_same_line),
    void (*)(void)), "stone_imgui_same_line ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_get_cursor_screen_pos),
    void (*)(float *, float *)), "stone_imgui_get_cursor_screen_pos ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_get_item_rect_min),
    void (*)(float *, float *)), "stone_imgui_get_item_rect_min ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_get_item_rect_max),
    void (*)(float *, float *)), "stone_imgui_get_item_rect_max ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_dummy),
    void (*)(float, float)), "stone_imgui_dummy ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_end_child),
    void (*)(void)), "stone_imgui_end_child ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_set_cursor_screen_pos),
    void (*)(float, float)), "stone_imgui_set_cursor_screen_pos ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_push_style_color),
    void (*)(int, uint32_t)), "stone_imgui_push_style_color ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_pop_style_color),
    void (*)(int)), "stone_imgui_pop_style_color ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_set_next_item_width),
    void (*)(float)), "stone_imgui_set_next_item_width ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_slider_float),
    void (*)(const char *, float, float, float, bool *, float *)),
    "stone_imgui_slider_float ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_slider_int),
    void (*)(const char *, int, int, int, bool *, int *)),
    "stone_imgui_slider_int ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_checkbox),
    void (*)(const char *, bool, bool *, bool *)),
    "stone_imgui_checkbox ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_color_edit4),
    void (*)(const char *, float, float, float, float,
             bool *, float *, float *, float *, float *)),
    "stone_imgui_color_edit4 ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_set_scroll_here_y),
    void (*)(float)), "stone_imgui_set_scroll_here_y ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_window_add_line),
    void (*)(float, float, float, float, uint32_t, float)),
    "stone_imgui_window_add_line ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_end),
    void (*)(void)), "stone_imgui_end ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_text),
    void (*)(const char *)), "stone_imgui_text ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_set_tooltip),
    void (*)(const char *)), "stone_imgui_set_tooltip ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_text_colored),
    void (*)(const char *, float, float, float, float)),
    "stone_imgui_text_colored ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_window_add_rect_filled),
    void (*)(float, float, float, float, uint32_t)),
    "stone_imgui_window_add_rect_filled ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_separator),
    void (*)(void)), "stone_imgui_separator ABI");

bool stone_imgui_using_mouse(void) { return true; }
bool stone_imgui_wants_mouse(void) { return true; }
bool stone_imgui_wants_text_input(void) { return true; }
bool stone_imgui_button(const char *label) { return label != 0; }
bool stone_imgui_begin_combo(const char *label, const char *preview) {
    return label != 0 && preview != 0
        && strcmp(label, "Modes") == 0 && strcmp(preview, "Normal") == 0;
}
void stone_imgui_end_combo(void) {}
void stone_imgui_set_item_default_focus(void) {}
void stone_imgui_open_popup(const char *id) { (void)id; }
bool stone_imgui_begin_popup(const char *id) {
    return id != 0 && strcmp(id, "QuestActions") == 0;
}
void stone_imgui_end_popup(void) {}
bool stone_imgui_menu_item(const char *label, bool enabled) {
    return label != 0 && strcmp(label, "QuestActions") == 0 && enabled;
}
void stone_imgui_combo_str(const char *label, int current_item, StoneString items,
                          bool *changed, int *out_current_item) {
    (void)label;
    (void)items;
    if (changed != 0) *changed = false;
    if (out_current_item != 0) *out_current_item = current_item;
}
bool stone_imgui_input_text(const char *label, char *buffer, size_t buffer_size, int flags) {
    return label != 0 && buffer != 0 && buffer_size == 16 && flags == 7;
}
bool stone_imgui_input_text_multiline(const char *label, char *buffer, size_t buffer_size,
                                      float width, float height, int flags) {
    return label != 0 && buffer != 0 && buffer_size == 16
        && width == 32.0f && height == 48.0f && flags == 7;
}
bool stone_imgui_begin_table(const char *id, int columns, int flags) {
    return id != 0 && columns == 2 && flags == 7;
}
void stone_imgui_end_table(void) {}
void stone_imgui_table_headers_row(void) {}
void stone_imgui_table_next_row(void) {}
bool stone_imgui_table_next_column(void) { return true; }
void stone_imgui_table_setup_column_fixed(const char *label, float width) {
    (void)label;
    (void)width;
}
void stone_imgui_table_setup_column_stretch(const char *label) { (void)label; }
void stone_imgui_begin_disabled(bool disabled) { (void)disabled; }
void stone_imgui_end_disabled(void) {}
void stone_imgui_indent(float amount) { (void)amount; }
void stone_imgui_unindent(float amount) { (void)amount; }
bool stone_imgui_collapsing_header_closed(const char *label) { return label != 0; }
bool stone_imgui_invisible_button(const char *label, float size_x, float size_y) {
    return label != 0 && size_x == 3.0f && size_y == 4.0f;
}
bool stone_imgui_selectable(const char *label, bool selected) {
    return label != 0 && selected;
}
void stone_imgui_get_mouse_pos(float *x, float *y) {
    if (x != 0) *x = 12.5f;
    if (y != 0) *y = -4.25f;
}
void stone_imgui_get_content_region_avail(float *x, float *y) {
    if (x != 0) *x = 100.0f;
    if (y != 0) *y = 80.0f;
}
bool stone_imgui_is_any_item_active(void) { return true; }
bool stone_imgui_is_item_active(void) { return true; }
bool stone_imgui_is_item_activated(void) { return true; }
bool stone_imgui_is_item_deactivated(void) { return true; }
bool stone_imgui_is_item_deactivated_after_edit(void) { return true; }
bool stone_imgui_is_item_visible(void) { return true; }
bool stone_imgui_is_window_docked(void) { return true; }
void stone_imgui_same_line(void) {}
void stone_imgui_get_cursor_screen_pos(float *x, float *y) {
    if (x != 0) *x = 7.0f;
    if (y != 0) *y = 11.0f;
}
void stone_imgui_get_item_rect_min(float *x, float *y) {
    if (x != 0) *x = 13.0f;
    if (y != 0) *y = 17.0f;
}
void stone_imgui_get_item_rect_max(float *x, float *y) {
    if (x != 0) *x = 23.0f;
    if (y != 0) *y = 29.0f;
}
void stone_imgui_dummy(float size_x, float size_y) {
    (void)size_x;
    (void)size_y;
}
void stone_imgui_end_child(void) {}
void stone_imgui_set_cursor_screen_pos(float pos_x, float pos_y) {
    (void)pos_x;
    (void)pos_y;
}
void stone_imgui_push_style_color(int idx, uint32_t col_abgr) {
    (void)idx;
    (void)col_abgr;
}
void stone_imgui_pop_style_color(int count) { (void)count; }
void stone_imgui_set_next_item_width(float width) { (void)width; }
void stone_imgui_slider_float(
    const char *label, float value, float min, float max,
    bool *changed, float *out_value) {
    (void)label;
    (void)value;
    (void)min;
    (void)max;
    if (changed != 0) *changed = false;
    if (out_value != 0) *out_value = value;
}
void stone_imgui_slider_int(
    const char *label, int value, int min, int max,
    bool *changed, int *out_value) {
    (void)label;
    (void)min;
    (void)max;
    if (changed != 0) *changed = false;
    if (out_value != 0) *out_value = value;
}
void stone_imgui_checkbox(
    const char *label, bool value, bool *changed, bool *out_value) {
    (void)label;
    if (changed != 0) *changed = false;
    if (out_value != 0) *out_value = value;
}
void stone_imgui_color_edit4(
    const char *label, float r, float g, float b, float a,
    bool *changed, float *out_r, float *out_g, float *out_b, float *out_a) {
    (void)label;
    if (changed != 0) *changed = false;
    if (out_r != 0) *out_r = r;
    if (out_g != 0) *out_g = g;
    if (out_b != 0) *out_b = b;
    if (out_a != 0) *out_a = a;
}
void stone_imgui_set_scroll_here_y(float center_y_ratio) { (void)center_y_ratio; }
void stone_imgui_window_add_line(
    float p1_x, float p1_y, float p2_x, float p2_y,
    uint32_t col_abgr, float thickness) {
    (void)p1_x;
    (void)p1_y;
    (void)p2_x;
    (void)p2_y;
    (void)col_abgr;
    (void)thickness;
}
void stone_imgui_end(void) {}
void stone_imgui_text(const char *text) { (void)text; }
void stone_imgui_set_tooltip(const char *text) { (void)text; }
void stone_imgui_text_colored(
    const char *text, float r, float g, float b, float a) {
    (void)text;
    (void)r;
    (void)g;
    (void)b;
    (void)a;
}
void stone_imgui_window_add_rect_filled(
    float min_x, float min_y, float max_x, float max_y, uint32_t col_abgr) {
    (void)min_x;
    (void)min_y;
    (void)max_x;
    (void)max_y;
    (void)col_abgr;
}
void stone_imgui_separator(void) {}

int main(void) {
    float cursor_x = 0.0f;
    float cursor_y = 0.0f;
    float mouse_x = 0.0f;
    float mouse_y = 0.0f;
    float content_x = 0.0f;
    float content_y = 0.0f;
    float item_min_x = 0.0f;
    float item_min_y = 0.0f;
    float item_max_x = 0.0f;
    float item_max_y = 0.0f;
    stone_imgui_get_cursor_screen_pos(&cursor_x, &cursor_y);
    stone_imgui_get_mouse_pos(&mouse_x, &mouse_y);
    stone_imgui_get_content_region_avail(&content_x, &content_y);
    (void)stone_imgui_is_any_item_active();
    stone_imgui_get_item_rect_min(&item_min_x, &item_min_y);
    stone_imgui_get_item_rect_max(&item_max_x, &item_max_y);
    stone_imgui_dummy(cursor_x, cursor_y);
    stone_imgui_end_child();
    stone_imgui_set_cursor_screen_pos(cursor_x, cursor_y);
    stone_imgui_push_style_color(0, 0xff808080u);
    stone_imgui_pop_style_color(1);
    stone_imgui_set_next_item_width(220.0f);
    bool changed = true;
    float slider_float = 0.0f;
    int slider_int = 0;
    bool checkbox = true;
    float color_r = 0.0f;
    float color_g = 0.0f;
    float color_b = 0.0f;
    float color_a = 0.0f;
    stone_imgui_slider_float("float", 0.25f, 0.0f, 1.0f, &changed, &slider_float);
    stone_imgui_slider_int("int", 2, 0, 4, &changed, &slider_int);
    stone_imgui_checkbox("checkbox", false, &changed, &checkbox);
    stone_imgui_color_edit4(
        "color", 0.1f, 0.2f, 0.3f, 0.4f, &changed,
        &color_r, &color_g, &color_b, &color_a);
    int combo_index = 0;
    stone_imgui_combo_str("combo", 0, (StoneString){"a\0", 2}, &changed, &combo_index);
    char input_text[17] = "before";
    (void)stone_imgui_input_text("field", input_text, sizeof(input_text) - 1, 7);
    char input_text_multiline[17] = "before";
    (void)stone_imgui_input_text_multiline(
        "field", input_text_multiline, sizeof(input_text_multiline) - 1, 32.0f, 48.0f, 7);
    stone_imgui_begin_table("table", 2, 7);
    stone_imgui_table_setup_column_stretch("Name");
    stone_imgui_table_setup_column_fixed("Value", 96.0f);
    stone_imgui_table_headers_row();
    stone_imgui_table_next_row();
    (void)stone_imgui_table_next_column();
    stone_imgui_end_table();
    stone_imgui_begin_disabled(true);
    stone_imgui_end_disabled();
    stone_imgui_indent(22.0f);
    stone_imgui_unindent(0.0f);
    stone_imgui_set_scroll_here_y(0.5f);
    stone_imgui_window_add_line(1.0f, 2.0f, 3.0f, 4.0f, 0xff112233u, 2.5f);
    stone_imgui_same_line();
    stone_imgui_set_tooltip("tooltip");
    stone_imgui_window_add_rect_filled(1.0f, 2.0f, 3.0f, 4.0f, 0xff112233u);
    stone_imgui_text_colored("colored", 0.1f, 0.2f, 0.3f, 0.4f);
    return stone_imgui_using_mouse() && stone_imgui_wants_mouse()
        && stone_imgui_wants_text_input() && stone_imgui_button("button")
        && stone_imgui_begin_combo("Modes", "Normal")
        && (stone_imgui_end_combo(), true)
        && (stone_imgui_set_item_default_focus(), true)
        && (stone_imgui_open_popup("popup"), true)
        && stone_imgui_begin_popup("QuestActions")
        && (stone_imgui_end_popup(), true)
        && stone_imgui_menu_item("QuestActions", true)
        && stone_imgui_collapsing_header_closed("closed")
        && stone_imgui_invisible_button("surface", 3.0f, 4.0f)
        && stone_imgui_selectable("selected", true)
        && stone_imgui_is_item_active()
        && stone_imgui_is_item_activated()
        && stone_imgui_is_item_deactivated()
        && stone_imgui_is_item_deactivated_after_edit()
        && stone_imgui_is_item_visible()
        && stone_imgui_is_window_docked()
        && mouse_x == 12.5f && mouse_y == -4.25f
        && content_x == 100.0f && content_y == 80.0f
        && cursor_x == 7.0f && cursor_y == 11.0f ? 0 : 1;
}
"#,
    )
    .expect("write ImGui mouse ABI fixture");

    compile(&source, &binary, &[]);
    assert!(
        Command::new(&binary)
            .status()
            .expect("run ImGui mouse ABI fixture")
            .success(),
        "stone.h ImGui mouse declaration did not match the provider ABI"
    );
}

#[test]
fn stone_header_exposes_native_rendering_state_abi_types() {
    let dir = tempfile::tempdir().expect("temporary C fixture directory");
    let source = dir.path().join("rendering_state_abi.c");
    let binary = dir.path().join("rendering_state_abi");

    std::fs::write(
        &source,
        r#"
#include <stone.h>

_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_set_blend_mode_i),
    void (*)(int)), "stone_set_blend_mode_i ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_bind_sampler),
    void (*)(int, int, uint64_t)), "stone_bind_sampler ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_set_canvas),
    void (*)(uint64_t)), "stone_set_canvas ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_bind_canvas_texture_to_unit),
    bool (*)(uint64_t, int)), "stone_bind_canvas_texture_to_unit ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_set_shader),
    void (*)(uint64_t)), "stone_set_shader ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_shader_upload_uniform_block),
    bool (*)(uint64_t, const uint8_t *, int, uint64_t)),
    "stone_shader_upload_uniform_block ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_bind_image_texture_to_unit),
    bool (*)(uint64_t, int)), "stone_bind_image_texture_to_unit ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_image_exists),
    bool (*)(uint64_t)), "stone_image_exists ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_create_canvas),
    uint64_t (*)(int, int, int, bool)), "stone_create_canvas ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_pixel_readback_consume),
    float (*)(uint64_t)), "stone_pixel_readback_consume ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_set_depth_mode),
    void (*)(const char *, bool)), "stone_set_depth_mode ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_clear_depth),
    void (*)(float)), "stone_clear_depth ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_set_color_mask),
    void (*)(bool, bool, bool, bool)), "stone_set_color_mask ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_set_draw_buffer_mask),
    void (*)(int)), "stone_set_draw_buffer_mask ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_current_target_width),
    float (*)(void)), "stone_current_target_width ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_current_target_height),
    float (*)(void)), "stone_current_target_height ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_set_canvas_active),
    void (*)(uint64_t, int, int)), "stone_set_canvas_active ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_set_gl_viewport),
    void (*)(int, int, int, int)), "stone_set_gl_viewport ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_set_present_viewport),
    void (*)(bool, int, int, int, int)), "stone_set_present_viewport ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_draw_fullscreen_rect),
    void (*)(float, float, float, float)), "stone_draw_fullscreen_rect ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_current_shader_bound),
    bool (*)(void)), "stone_current_shader_bound ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_create_image_data),
    uint64_t (*)(int, int)), "stone_create_image_data ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_image_data_size),
    bool (*)(uint64_t, int *, int *)), "stone_image_data_size ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_destroy_image_data),
    void (*)(uint64_t)), "stone_destroy_image_data ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_image_data_set_pixel),
    void (*)(uint64_t, int, int, float, float, float, float)),
    "stone_image_data_set_pixel ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_image_update_from_data),
    bool (*)(uint64_t, uint64_t)), "stone_image_update_from_data ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_new_image_from_data),
    uint64_t (*)(uint64_t, int)), "stone_new_image_from_data ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_pixel_readback_new),
    uint64_t (*)(int)), "stone_pixel_readback_new ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_pixel_readback_area_new),
    uint64_t (*)(int)), "stone_pixel_readback_area_new ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_load_sound_deferred),
    uint64_t (*)(const char *, int64_t, int64_t, int64_t)), "stone_load_sound_deferred ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_load_sound_stream),
    uint64_t (*)(const char *, int64_t, int64_t, int64_t)), "stone_load_sound_stream ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_sound_playable),
    bool (*)(uint64_t)), "stone_sound_playable ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_sound_stream_bytes),
    int64_t (*)(uint64_t)), "stone_sound_stream_bytes ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_unload_sound),
    bool (*)(uint64_t)), "stone_unload_sound ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_load_music),
    uint64_t (*)(const char *)), "stone_load_music ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_play_music),
    void (*)(uint64_t)), "stone_play_music ABI");

int main(void) { return 0; }
"#,
    )
    .expect("write native rendering-state ABI fixture");

    compile(&source, &binary, &[]);
    assert!(
        Command::new(&binary)
            .status()
            .expect("run native rendering-state ABI fixture")
            .success(),
        "stone.h rendering-state declarations did not match the provider ABI"
    );
}

#[test]
fn stone_header_exposes_scancode_lookup_abi_types() {
    let dir = tempfile::tempdir().expect("temporary C fixture directory");
    let source = dir.path().join("scancode_abi.c");
    let binary = dir.path().join("scancode_abi");

    std::fs::write(
        &source,
        r#"
#include <stone.h>
#include <string.h>

_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_scancode_count),
    int (*)(void)), "stone_scancode_count ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_scancode_token_at),
    const char *(*)(int)), "stone_scancode_token_at ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_scancode_token_for),
    const char *(*)(int)), "stone_scancode_token_for ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_scancode_value),
    int (*)(const char *)), "stone_scancode_value ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_scancode_label),
    const char *(*)(const char *)), "stone_scancode_label ABI");

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
    )
    .expect("write scancode ABI fixture");

    compile(&source, &binary, &[]);
    assert!(
        Command::new(&binary)
            .status()
            .expect("run scancode ABI fixture")
            .success(),
        "stone.h scancode declarations did not match the exported provider ABI"
    );
}

#[test]
fn stone_header_exposes_length_aware_file_abi_types() {
    let dir = tempfile::tempdir().expect("temporary C fixture directory");
    let source = dir.path().join("file_abi.c");
    let binary = dir.path().join("file_abi");

    std::fs::write(
        &source,
        r#"
#include <stone.h>

_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_file_write),
    bool (*)(const char *, const char *, size_t)), "stone_file_write ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_file_append),
    bool (*)(const char *, const char *, size_t)), "stone_file_append ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_file_read),
    char *(*)(const char *, size_t *)), "stone_file_read ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_free),
    void (*)(void *)), "stone_free ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_file_exists_status),
    int32_t (*)(const char *)), "stone_file_exists_status ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_list_directory),
    bool (*)(const char *, char ***, int *)), "stone_list_directory ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_free_string_array),
    void (*)(char **, int)), "stone_free_string_array ABI");

int main(void) { return 0; }
"#,
    )
    .expect("write file ABI fixture");

    compile(&source, &binary, &[]);
}

#[test]
fn stone_header_declares_file_read_release_and_append_with_the_exported_abi() {
    let dir = tempfile::tempdir().expect("temporary C fixture directory");
    let provider = dir.path().join("provider.c");
    let provider_object = dir.path().join("provider.o");
    let consumer = dir.path().join("consumer.c");
    let binary = dir.path().join("consumer");

    std::fs::write(
        &provider,
        r#"
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
"#,
    )
    .expect("write file-read ABI provider");
    let result = Command::new("clang")
        .args(["-std=c11", "-O0", "-Wall", "-Wextra", "-Werror", "-c"])
        .arg("-I")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("include"))
        .arg(&provider)
        .arg("-o")
        .arg(&provider_object)
        .output()
        .expect("compile file-read ABI provider");
    assert!(
        result.status.success(),
        "provider compile failed:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );

    std::fs::write(
        &consumer,
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
    )
    .expect("write file-read ABI consumer");
    compile(&consumer, &binary, &[&provider_object]);
    assert!(
        Command::new(&binary)
            .status()
            .expect("run file-read ABI consumer")
            .success(),
        "stone.h file-read declarations did not match the exported provider ABI"
    );
}

#[test]
fn stone_event_header_layout_matches_rust_abi() {
    let dir = tempfile::tempdir().expect("temporary C fixture directory");
    let source = dir.path().join("event_layout.c");
    let binary = dir.path().join("event_layout");

    std::fs::write(
        &source,
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
    )
    .expect("write StoneEvent layout fixture");
    compile(&source, &binary, &[]);

    let output = Command::new(&binary)
        .output()
        .expect("run StoneEvent layout fixture");
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
    let dir = tempfile::tempdir().expect("temporary C fixture directory");
    let provider = dir.path().join("provider.c");
    let provider_object = dir.path().join("provider.o");
    let consumer = dir.path().join("consumer.c");
    let binary = dir.path().join("consumer");

    std::fs::write(
        &provider,
        r#"
#include <stone.h>

uint64_t stone_get_font_instance_image(uint64_t instance_id) {
    return instance_id + UINT64_C(5);
}
"#,
    )
    .expect("write font ABI provider");
    let result = Command::new("clang")
        .args(["-std=c11", "-O0", "-Wall", "-Wextra", "-Werror", "-c"])
        .arg("-I")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("include"))
        .arg(&provider)
        .arg("-o")
        .arg(&provider_object)
        .output()
        .expect("compile font ABI provider");
    assert!(
        result.status.success(),
        "provider compile failed:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );

    std::fs::write(
        &consumer,
        r#"
#include <stone.h>

int main(void) {
    return stone_get_font_instance_image(UINT64_C(7)) == UINT64_C(12) ? 0 : 1;
}
"#,
    )
    .expect("write font ABI consumer");
    compile(&consumer, &binary, &[&provider_object]);
    assert!(
        Command::new(&binary)
            .status()
            .expect("run font ABI consumer")
            .success(),
        "stone.h font declaration did not match the exported provider ABI"
    );
}

#[test]
fn stone_header_declares_native_mag_ffi_with_exported_abis() {
    let dir = tempfile::tempdir().expect("temporary C fixture directory");
    let source = dir.path().join("mag_ffi_abi.c");
    let binary = dir.path().join("mag_ffi_abi");

    std::fs::write(
        &source,
        r#"
#include <stone.h>

_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_bus_duck),
    bool (*)(uint32_t, float, float, float)), "stone_bus_duck ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_bus_set_gain),
    bool (*)(uint32_t, float)), "stone_bus_set_gain ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_bus_set_lowpass),
    bool (*)(uint32_t, float, float)), "stone_bus_set_lowpass ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_canvas_exists),
    bool (*)(uint64_t)), "stone_canvas_exists ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_window_size),
    void (*)(int *, int *)), "stone_get_window_size ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_drawable_size),
    void (*)(int *, int *)), "stone_get_drawable_size ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_dpi_scale),
    float (*)(void)), "stone_get_dpi_scale ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_config_imgui_scale),
    float (*)(void)), "stone_get_config_imgui_scale ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_canvas_attach_id_target),
    bool (*)(uint64_t, uint64_t)), "stone_canvas_attach_id_target ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_clear_canvas_int),
    void (*)(int, int)), "stone_clear_canvas_int ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_white_image),
    uint64_t (*)(void)), "stone_get_white_image ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_destroy_canvas),
    void (*)(uint64_t)), "stone_destroy_canvas ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_ensure_font_atlas_instance),
    uint64_t (*)(uint32_t, int)), "stone_ensure_font_atlas_instance ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_file_sync),
    bool (*)(const char *)), "stone_file_sync ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_font_atlas_instance_count),
    int (*)(uint32_t)), "stone_font_atlas_instance_count ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_display_count),
    int (*)(void)), "stone_get_display_count ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_proc_reset),
    int (*)(void)), "stone_proc_reset ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_font_instance_byte_len),
    uint64_t (*)(uint64_t)), "stone_get_font_instance_byte_len ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_font_instance_glyph_info),
    bool (*)(uint64_t, int, int, float *)), "stone_get_font_instance_glyph_info ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_font_instance_pixel_size),
    int (*)(uint64_t)), "stone_get_font_instance_pixel_size ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_font_instance_size),
    bool (*)(uint64_t, int *, int *)), "stone_get_font_instance_size ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_font_slot_codepoint),
    int (*)(uint32_t, int)), "stone_get_font_slot_codepoint ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_font_slot_count),
    int (*)(uint32_t)), "stone_get_font_slot_count ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_font_unit_metrics),
    bool (*)(uint32_t, float *)), "stone_get_font_unit_metrics ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_glyph_unit_metrics),
    bool (*)(uint32_t, int, int, float *)), "stone_get_glyph_unit_metrics ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_image_size),
    void (*)(uint64_t, int *, int *)), "stone_get_image_size ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_flush_image_uploads),
    void (*)(void)), "stone_flush_image_uploads ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_graphics_stats),
    void (*)(StoneGraphicsStats *)), "stone_get_graphics_stats ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_track_text_draw_call),
    void (*)(void)), "stone_track_text_draw_call ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_image_frame_stats),
    void (*)(StoneImageLoadStats *)), "stone_get_image_frame_stats ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_image_profile_stats),
    void (*)(StoneImageLoadStats *)), "stone_get_image_profile_stats ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_sound_duration),
    float (*)(uint64_t)), "stone_get_sound_duration ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_reload_sound),
    bool (*)(uint64_t, const char *)), "stone_reload_sound ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_image_set_wrap),
    bool (*)(uint64_t, bool)), "stone_image_set_wrap ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_is_directory_status),
    int32_t (*)(const char *)), "stone_is_directory_status ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_create_directory),
    bool (*)(const char *)), "stone_create_directory ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_load_image),
    uint64_t (*)(const char *, int, int, int, int)), "stone_load_image ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_reload_image),
    bool (*)(uint64_t, const char *)), "stone_reload_image ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_mesh_free),
    void (*)(uint64_t)), "stone_mesh_free ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_pixel_readback_submit),
    void (*)(uint64_t, uint64_t, int, int)), "stone_pixel_readback_submit ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_play_sound),
    uint64_t (*)(uint64_t, uint32_t, float, float, float, bool, bool, float)), "stone_play_sound ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_read_canvas_nearest_id),
    int (*)(uint64_t, int, int, int)), "stone_read_canvas_nearest_id ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_read_canvas_pixel),
    float (*)(uint64_t, int, int)), "stone_read_canvas_pixel ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_remove_file),
    bool (*)(const char *)), "stone_remove_file ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_remove_dir),
    bool (*)(const char *)), "stone_remove_dir ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_rename),
    bool (*)(const char *, const char *)), "stone_rename ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_set_cursor),
    void (*)(uint64_t)), "stone_set_cursor ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_show_cursor),
    void (*)(bool)), "stone_show_cursor ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_is_cursor_visible),
    bool (*)(void)), "stone_is_cursor_visible ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_create_cursor),
    uint64_t (*)(const char *, int, int)), "stone_create_cursor ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_create_cursor_from_image_data),
    uint64_t (*)(uint64_t, int, int)), "stone_create_cursor_from_image_data ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_set_system_cursor),
    uint64_t (*)(int)), "stone_set_system_cursor ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_reset_cursor),
    void (*)(void)), "stone_reset_cursor ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_free_cursor),
    void (*)(uint64_t)), "stone_free_cursor ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_active_cursor),
    uint64_t (*)(void)), "stone_active_cursor ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_unload_image),
    void (*)(uint64_t)), "stone_unload_image ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_voice_active),
    bool (*)(uint64_t)), "stone_voice_active ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_voice_stream_frames),
    int64_t (*)(uint64_t)), "stone_voice_stream_frames ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_voice_set_gain),
    bool (*)(uint64_t, float)), "stone_voice_set_gain ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_voice_stop),
    bool (*)(uint64_t)), "stone_voice_stop ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_watch_asset),
    void (*)(const char *)), "stone_watch_asset ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_poll_changed_assets),
    int (*)(void)), "stone_poll_changed_assets ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_create_shader_from_combined),
    uint64_t (*)(const char *)), "stone_create_shader_from_combined ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_destroy_shader),
    void (*)(uint64_t)), "stone_destroy_shader ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_shader_bind_uniform_block),
    bool (*)(uint64_t, const char *, const char *, int, int, uint64_t)),
    "stone_shader_bind_uniform_block ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_shader_check_uniform_block_layout),
    bool (*)(uint64_t, const char *, const char *, int, const int *, int, int)),
    "stone_shader_check_uniform_block_layout ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_imgui_begin_window),
    bool (*)(const char *, int)),
    "stone_imgui_begin_window ABI");

int main(void) { return 0; }
"#,
    )
    .expect("write native MAG FFI ABI fixture");
    compile(&source, &binary, &[]);
}

#[test]
fn stone_header_declares_font_lifecycle_with_its_exported_abi() {
    let dir = tempfile::tempdir().expect("temporary C fixture directory");
    let provider = dir.path().join("provider.c");
    let provider_object = dir.path().join("provider.o");
    let consumer = dir.path().join("consumer.c");
    let binary = dir.path().join("consumer");

    std::fs::write(
        &provider,
        r#"
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
"#,
    )
    .expect("write font lifecycle ABI provider");
    let result = Command::new("clang")
        .args(["-std=c11", "-O0", "-Wall", "-Wextra", "-Werror", "-c"])
        .arg("-I")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("include"))
        .arg(&provider)
        .arg("-o")
        .arg(&provider_object)
        .output()
        .expect("compile font lifecycle ABI provider");
    assert!(
        result.status.success(),
        "provider compile failed:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );

    std::fs::write(
        &consumer,
        r#"
#include <stone.h>

_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_load_font),
    uint32_t (*)(const char *, int, int, const char *)), "stone_load_font ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_unload_font),
    bool (*)(uint32_t)), "stone_unload_font ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_font_release_coverage),
    bool (*)(uint32_t)), "stone_font_release_coverage ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_reload_font),
    bool (*)(uint32_t, const char *, int)), "stone_reload_font ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_resize_font),
    bool (*)(uint32_t, int)), "stone_resize_font ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_font_pixel_size),
    int (*)(uint32_t)), "stone_get_font_pixel_size ABI");

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
    )
    .expect("write font lifecycle ABI consumer");
    compile(&consumer, &binary, &[&provider_object]);
    assert!(
        Command::new(&binary)
            .status()
            .expect("run font lifecycle ABI consumer")
            .success(),
        "stone.h font lifecycle declarations did not match the exported provider ABI"
    );
}

#[test]
fn stone_header_declares_font_glyph_info_with_its_exported_abi() {
    let dir = tempfile::tempdir().expect("temporary C fixture directory");
    let source = dir.path().join("font_glyph_info_abi.c");
    let binary = dir.path().join("font_glyph_info_abi");

    std::fs::write(
        &source,
        r#"
#include <stone.h>

_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_glyph_info),
    bool (*)(uint32_t, int, int, float *)), "stone_get_glyph_info ABI");

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
    )
    .expect("write font glyph-info ABI fixture");
    compile(&source, &binary, &[]);
    assert!(
        Command::new(&binary)
            .status()
            .expect("run font glyph-info ABI fixture")
            .success(),
        "stone.h glyph-info declaration did not match the exported provider ABI"
    );
}

#[test]
fn stone_header_declares_font_atlas_images_with_their_exported_abi() {
    let dir = tempfile::tempdir().expect("temporary C fixture directory");
    let source = dir.path().join("font_atlas_images_abi.c");
    let binary = dir.path().join("font_atlas_images_abi");

    std::fs::write(
        &source,
        r#"
#include <stone.h>

_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_font_atlas_image),
    uint64_t (*)(uint32_t)), "stone_get_font_atlas_image ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_font_atlas_image_cached),
    uint64_t (*)(uint32_t)), "stone_get_font_atlas_image_cached ABI");

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
    )
    .expect("write font atlas image ABI fixture");
    compile(&source, &binary, &[]);
    assert!(
        Command::new(&binary)
            .status()
            .expect("run font atlas image ABI fixture")
            .success(),
        "stone.h font atlas image declarations did not match the exported provider ABI"
    );
}

#[test]
fn stone_header_declares_hostname_with_its_exported_abi() {
    let dir = tempfile::tempdir().expect("temporary C fixture directory");
    let source = dir.path().join("hostname_abi.c");
    let binary = dir.path().join("hostname_abi");

    std::fs::write(
        &source,
        r#"
#include <stone.h>

_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_hostname),
    const char *(*)(void)), "stone_get_hostname ABI");

const char *stone_get_hostname(void) { return "native-host"; }

int main(void) {
    const char *host = stone_get_hostname();
    return host != NULL && host[0] == 'n' && host[10] == 't' ? 0 : 1;
}
"#,
    )
    .expect("write hostname ABI fixture");
    compile(&source, &binary, &[]);
    assert!(
        Command::new(&binary)
            .status()
            .expect("run hostname ABI fixture")
            .success(),
        "stone.h hostname declaration did not match the exported provider ABI"
    );
}

#[test]
fn stone_header_declares_native_path_reads_with_their_exported_abi() {
    let dir = tempfile::tempdir().expect("temporary C fixture directory");
    let source = dir.path().join("path_abi.c");
    let binary = dir.path().join("path_abi");

    std::fs::write(
        &source,
        r#"
#include <stone.h>

_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_working_directory),
    const char *(*)(void)), "stone_get_working_directory ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_get_executable_path),
    const char *(*)(void)), "stone_get_executable_path ABI");
_Static_assert(__builtin_types_compatible_p(
    __typeof__(&stone_changed_asset_path),
    const char *(*)(int)), "stone_changed_asset_path ABI");

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
    )
    .expect("write native path ABI fixture");
    compile(&source, &binary, &[]);
    assert!(
        Command::new(&binary)
            .status()
            .expect("run native path ABI fixture")
            .success(),
        "stone.h native path declarations did not match the exported provider ABI"
    );
}

#[test]
fn stone_header_declares_pref_path_with_its_exported_abi() {
    let dir = tempfile::tempdir().expect("temporary C fixture directory");
    let provider = dir.path().join("provider.c");
    let provider_object = dir.path().join("provider.o");
    let consumer = dir.path().join("consumer.c");
    let binary = dir.path().join("consumer");

    std::fs::write(
        &provider,
        r#"
#include <stone.h>

const char *stone_get_pref_path(const char *organization, const char *application) {
    return organization[0] == 'o' && application[0] == 'a' ? "prefs/" : NULL;
}
"#,
    )
    .expect("write preference path ABI provider");
    let result = Command::new("clang")
        .args(["-std=c11", "-O0", "-Wall", "-Wextra", "-Werror", "-c"])
        .arg("-I")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("include"))
        .arg(&provider)
        .arg("-o")
        .arg(&provider_object)
        .output()
        .expect("compile preference path ABI provider");
    assert!(
        result.status.success(),
        "provider compile failed:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );

    std::fs::write(
        &consumer,
        r#"
#include <stone.h>

int main(void) {
    const char *path = stone_get_pref_path("org", "app");
    return path != NULL && path[0] == 'p' && path[5] == '/' ? 0 : 1;
}
"#,
    )
    .expect("write preference path ABI consumer");
    compile(&consumer, &binary, &[&provider_object]);
    assert!(
        Command::new(&binary)
            .status()
            .expect("run preference path ABI consumer")
            .success(),
        "stone.h preference path declaration did not match the exported provider ABI"
    );
}

#[test]
fn stone_header_declares_telemetry_close_with_its_exported_abi() {
    let dir = tempfile::tempdir().expect("temporary C fixture directory");
    let provider = dir.path().join("provider.c");
    let provider_object = dir.path().join("provider.o");
    let consumer = dir.path().join("consumer.c");
    let binary = dir.path().join("consumer");

    std::fs::write(
        &provider,
        r#"
static int closed;
void stone_telemetry_close(void) { closed = 1; }
int telemetry_was_closed(void) { return closed; }
"#,
    )
    .expect("write telemetry ABI provider");
    let result = Command::new("clang")
        .args(["-std=c11", "-O0", "-Wall", "-Wextra", "-Werror", "-c"])
        .arg(&provider)
        .arg("-o")
        .arg(&provider_object)
        .output()
        .expect("compile telemetry ABI provider");
    assert!(
        result.status.success(),
        "provider compile failed:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );

    std::fs::write(
        &consumer,
        r#"
#include <stone.h>

extern int telemetry_was_closed(void);

int main(void) {
    void (*close_fn)(void) = stone_telemetry_close;
    close_fn();
    return telemetry_was_closed() ? 0 : 1;
}
"#,
    )
    .expect("write telemetry ABI consumer");
    compile(&consumer, &binary, &[&provider_object]);
    assert!(
        Command::new(&binary)
            .status()
            .expect("run telemetry ABI consumer")
            .success(),
        "stone.h telemetry close declaration did not match its exported ABI"
    );
}

#[test]
fn stone_header_declares_telemetry_clear_with_its_exported_abi() {
    let dir = tempfile::tempdir().expect("temporary C directory");
    let provider = dir.path().join("provider.c");
    let provider_object = dir.path().join("provider.o");
    let consumer = dir.path().join("consumer.c");
    let binary = dir.path().join("consumer");

    std::fs::write(
        &provider,
        r#"
#include <stone.h>

bool stone_telemetry_clear(const char *root) {
    return root != 0 && root[0] == 'r';
}
"#,
    )
    .expect("write telemetry clear ABI provider");
    let result = Command::new("clang")
        .args(["-std=c11", "-O0", "-Wall", "-Wextra", "-Werror", "-c"])
        .arg("-I")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("include"))
        .arg(&provider)
        .arg("-o")
        .arg(&provider_object)
        .output()
        .expect("compile telemetry clear ABI provider");
    assert!(
        result.status.success(),
        "provider compile failed:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );

    std::fs::write(
        &consumer,
        r#"
#include <stone.h>

int main(void) {
    bool (*clear_fn)(const char *) = stone_telemetry_clear;
    return clear_fn("root") ? 0 : 1;
}
"#,
    )
    .expect("write telemetry clear ABI consumer");
    compile(&consumer, &binary, &[&provider_object]);
    assert!(
        Command::new(&binary)
            .status()
            .expect("run telemetry clear ABI consumer")
            .success(),
        "stone.h telemetry clear declaration did not match its exported ABI"
    );
}

#[test]
fn stone_header_declares_telemetry_configure_with_its_exported_abi() {
    let dir = tempfile::tempdir().expect("temporary C directory");
    let provider = dir.path().join("provider.c");
    let provider_object = dir.path().join("provider.o");
    let consumer = dir.path().join("consumer.c");
    let binary = dir.path().join("consumer");

    std::fs::write(
        &provider,
        r#"
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
"#,
    )
    .expect("write telemetry configure ABI provider");
    let result = Command::new("clang")
        .args(["-std=c11", "-O0", "-Wall", "-Wextra", "-Werror", "-c"])
        .arg("-I")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("include"))
        .arg(&provider)
        .arg("-o")
        .arg(&provider_object)
        .output()
        .expect("compile telemetry configure ABI provider");
    assert!(
        result.status.success(),
        "provider compile failed:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );

    std::fs::write(
        &consumer,
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
    )
    .expect("write telemetry configure ABI consumer");
    compile(&consumer, &binary, &[&provider_object]);
    assert!(
        Command::new(&binary)
            .status()
            .expect("run telemetry configure ABI consumer")
            .success(),
        "stone.h telemetry configure declaration did not match its exported ABI"
    );
}

#[test]
fn stone_header_declares_length_aware_logging_with_its_exported_abi() {
    let dir = tempfile::tempdir().expect("temporary C fixture directory");
    let source = dir.path().join("logging_abi.c");
    let binary = dir.path().join("logging_abi");

    std::fs::write(
        &source,
        r#"
#include <stone.h>

static int active_level = 2;
void stone_log_init(void) {}
void stone_log_set_level(int level) { active_level = level; }
int stone_log_get_level(void) { return active_level; }

void stone_log_bytes(
    int level,
    const unsigned char *category,
    size_t category_len,
    const unsigned char *message,
    size_t message_len,
    int wall_clock) {
    (void)level;
    (void)category;
    (void)category_len;
    (void)message;
    (void)message_len;
    (void)wall_clock;
}

int main(void) {
    void (*init_fn)(void) = stone_log_init;
    void (*set_level_fn)(int) = stone_log_set_level;
    int (*get_level_fn)(void) = stone_log_get_level;
    void (*log_fn)(int, const unsigned char *, size_t, const unsigned char *, size_t, int)
        = stone_log_bytes;
    init_fn();
    set_level_fn(3);
    if (get_level_fn() != 3) return 1;
    log_fn(2, (const unsigned char *)"C", 1, (const unsigned char *)"native", 6, 0);
    return 0;
}
"#,
    )
    .expect("write logging ABI consumer");
    compile(&source, &binary, &[]);
}

#[test]
fn stone_header_imgui_frontier_uses_typed_native_transport() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("imgui_frontier.c");
    let binary = dir.path().join("imgui_frontier");
    std::fs::write(&source, r#"#include <stone.h>
#define ABI(name, signature) _Static_assert(__builtin_types_compatible_p(__typeof__(&name), signature), #name " ABI")
ABI(stone_imgui_is_rect_visible, bool (*)(float, float));
ABI(stone_imgui_is_item_fully_visible, bool (*)(void));
ABI(stone_imgui_set_next_window_pos, void (*)(float, float));
ABI(stone_imgui_set_window_focus, void (*)(const char *));
ABI(stone_imgui_button_sized, bool (*)(const char *, float, float));
ABI(stone_imgui_collapsing_header, bool (*)(const char *));
ABI(stone_imgui_input_float, void (*)(const char *, float, float, float, const char *, bool *, float *));
ABI(stone_imgui_window_add_quad_filled, void (*)(float, float, float, float, float, float, float, float, uint32_t));
ABI(stone_imgui_window_add_image_quad, void (*)(uint64_t, float, float, float, float, float, float, float, float, float, float, float, float, float, float, float, float, uint32_t));
ABI(stone_imgui_image_raw, void (*)(uint64_t, float, float, float, float, float, float, float, float, float, float, float, float, float, float));
ABI(stone_imgui_image_button_raw, bool (*)(const char *, uint64_t, float, float, float, float, float, float, float, float, float, float, float, float, float, float));
int main(void) { return 0; }
"#).unwrap();
    compile(&source, &binary, &[]);
    assert!(Command::new(binary).status().unwrap().success());
}
