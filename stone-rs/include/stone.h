#ifndef STONE_H
#define STONE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif


#include "stone_ui_submission_probe.h"

/* Copy the reply before issuing another native query on this thread. */
const char* stone_visual_bank_decode(const uint8_t* data, size_t len);
const char* stone_visual_geometry_decode(const uint8_t* data, size_t len,
    uint32_t width, uint32_t height, uint32_t cell_width, uint32_t cell_height,
    uint32_t frames, uint32_t directions);

typedef struct StoneWindow StoneWindow;

typedef enum StoneEventType {
    STONE_EVENT_NONE = 0,
    STONE_EVENT_QUIT = 1,
    STONE_EVENT_KEYDOWN = 2,
    STONE_EVENT_KEYUP = 3,
    STONE_EVENT_MOUSEBUTTONDOWN = 4,
    STONE_EVENT_MOUSEBUTTONUP = 5,
    STONE_EVENT_MOUSEWHEEL = 6,
    STONE_EVENT_PAD_DEVICE_ADDED = 7,
    STONE_EVENT_PAD_DEVICE_REMOVED = 8,
    STONE_EVENT_PAD_ACTIVE_CHANGED = 9,
    STONE_EVENT_PAD_BUTTON_DOWN = 10,
    STONE_EVENT_PAD_BUTTON_UP = 11,
    STONE_EVENT_PAD_AXIS_MOTION = 12,
    STONE_EVENT_TEXTINPUT = 13,
} StoneEventType;

typedef struct StoneEvent {
    StoneEventType event_type;
    int key;
    int scancode;
    int modifiers;
    bool repeat;
    int mouse_button;
    int mouse_x;
    int mouse_y;
    int pad_instance_id;
    const char *pad_button;
    const char *pad_axis;
    int pad_value;
    bool pad_sync;
    char text[32];
} StoneEvent;

typedef struct StoneString {
    const char *data;
    size_t len;
} StoneString;

typedef struct StoneGraphicsStats {
    int draw_calls;
    int text_draw_calls;
    int canvas_switches;
    int shader_switches;
    int stream_upload_calls;
    int stream_storage_replacements;
    int stream_upload_bytes;
    int streamed_vertices;
    int retained_upload_calls;
    int retained_upload_bytes;
    int retained_uploaded_vertices;
    int images;
    int canvases;
    int texture_memory_mib;
    int instanced_draws;
} StoneGraphicsStats;

typedef struct StoneImageLoadStats {
    int load_calls;
    int load_failures;
    int decoded_bytes;
    int load_us;
    int upload_calls;
    int upload_bytes;
    int wait_us;
    int upload_us;
    int active_frames;
    int max_wait_us;
} StoneImageLoadStats;



bool stone_init(void);
void stone_quit(void);
bool stone_teardown(void);
bool stone_imgui_using_mouse(void);
bool stone_imgui_wants_mouse(void);
bool stone_imgui_wants_text_input(void);
bool stone_imgui_begin_window(const char *title, int flags);
bool stone_imgui_button(const char *label);
bool stone_imgui_begin_combo(const char *label, const char *preview);
void stone_imgui_end_combo(void);
void stone_imgui_set_item_default_focus(void);
void stone_imgui_open_popup(const char *id);
bool stone_imgui_begin_popup(const char *id);
void stone_imgui_end_popup(void);
bool stone_imgui_menu_item(const char *label, bool enabled);
void stone_imgui_combo_str(
    const char *label, int current_item, StoneString items,
    bool *changed, int *out_current_item);
bool stone_imgui_input_text(
    const char *label, char *buffer, size_t buffer_size, int flags);
bool stone_imgui_input_text_multiline(
    const char *label, char *buffer, size_t buffer_size,
    float width, float height, int flags);
bool stone_imgui_begin_table(const char *id, int columns, int flags);
void stone_imgui_end_table(void);
void stone_imgui_table_headers_row(void);
void stone_imgui_table_next_row(void);
bool stone_imgui_table_next_column(void);
void stone_imgui_table_setup_column_fixed(const char *label, float width);
void stone_imgui_table_setup_column_stretch(const char *label);
void stone_imgui_begin_disabled(bool disabled);
void stone_imgui_end_disabled(void);
void stone_imgui_indent(float amount);
void stone_imgui_unindent(float amount);
bool stone_imgui_collapsing_header_closed(const char *label);
bool stone_imgui_invisible_button(const char *label, float size_x, float size_y);
bool stone_imgui_selectable(const char *label, bool selected);
void stone_imgui_get_mouse_pos(float *x, float *y);
void stone_imgui_get_content_region_avail(float *x, float *y);
void stone_imgui_get_window_pos(float *x, float *y);
void stone_imgui_get_window_size(float *x, float *y);
bool stone_imgui_is_any_item_active(void);
bool stone_imgui_is_item_visible(void);
bool stone_imgui_is_window_docked(void);
void stone_imgui_same_line(void);
void stone_imgui_get_cursor_screen_pos(float *x, float *y);
void stone_imgui_get_item_rect_min(float *x, float *y);
void stone_imgui_get_item_rect_max(float *x, float *y);
void stone_imgui_dummy(float size_x, float size_y);
void stone_imgui_set_cursor_screen_pos(float pos_x, float pos_y);
void stone_imgui_push_style_color(int idx, uint32_t col_abgr);
void stone_imgui_pop_style_color(int count);
void stone_imgui_set_next_item_width(float width);
void stone_imgui_slider_float(
    const char *label, float value, float min, float max,
    bool *changed, float *out_value);
void stone_imgui_slider_int(
    const char *label, int value, int min, int max,
    bool *changed, int *out_value);
void stone_imgui_checkbox(
    const char *label, bool value, bool *changed, bool *out_value);
void stone_imgui_color_edit4(
    const char *label, float r, float g, float b, float a,
    bool *changed, float *out_r, float *out_g, float *out_b, float *out_a);
void stone_imgui_set_scroll_here_y(float center_y_ratio);
void stone_imgui_window_add_line(
    float p1_x, float p1_y, float p2_x, float p2_y,
    uint32_t col_abgr, float thickness);
void stone_imgui_end_child(void);
void stone_imgui_end(void);
void stone_imgui_text(const char *text);
void stone_imgui_set_tooltip(const char *text);
void stone_imgui_text_colored(
    const char *text, float r, float g, float b, float a);
void stone_imgui_window_add_rect_filled(
    float min_x, float min_y, float max_x, float max_y, uint32_t col_abgr);
void stone_imgui_separator(void);
bool stone_imgui_is_item_hovered(int flags);
bool stone_imgui_is_item_active(void);
bool stone_imgui_is_item_activated(void);
bool stone_imgui_is_item_deactivated(void);
bool stone_imgui_is_item_deactivated_after_edit(void);
StoneWindow *stone_create_window_ex(
    const char *title,
    int x,
    int y,
    int width,
    int height,
    int display,
    bool resizable,
    int fullscreen);
// Borrows the current window or creates a hidden GPU host. NULL after host context loss.
StoneWindow *stone_gpu_host_acquire(void);
void stone_get_window_size(int *width, int *height);
void stone_get_drawable_size(int *width, int *height);
float stone_get_dpi_scale(void);
float stone_get_config_imgui_scale(void);
void stone_clear_canvas(float r, float g, float b, float a);
void stone_set_canvas(uint64_t handle);
void stone_set_canvas_active(uint64_t handle, int w, int h);
void stone_set_gl_viewport(int x, int y, int w, int h);
void stone_set_present_viewport(bool active, int x, int y, int w, int h);
uint64_t stone_create_canvas(int width, int height, int format, bool with_depth);
bool stone_canvas_attach_id_target(uint64_t color_handle, uint64_t id_handle);
bool stone_canvas_attach_color_target(uint64_t color_handle, uint64_t target_handle, int slot);
bool stone_canvas_resize(uint64_t handle, int width, int height);
void stone_clear_canvas_int(int draw_buffer, int value);
void stone_swap_window(StoneWindow *window);
void stone_show_window_if_deferred(void);
void stone_hide_window(void);
void stone_activate_window(StoneWindow *window);
void stone_show_window_mode(int mode);
void stone_set_window_title(StoneWindow *window, const char *title);
void stone_delay(uint32_t ms);
bool stone_poll_event(StoneEvent *event);
double stone_get_time(void);
int stone_proc_reset(void);
int stone_get_display_count(void);
int stone_scancode_count(void);
const char *stone_scancode_token_at(int index);
const char *stone_scancode_token_for(int scancode);
int stone_scancode_value(const char *token);
const char *stone_scancode_label(const char *token);
bool stone_is_key_pressed(int scancode);
bool stone_is_mouse_button_down(int button);
void stone_get_mouse_position(int *x, int *y);
void stone_set_scissor(bool enabled, int x, int y, int w, int h);
void stone_set_color_mask(bool r, bool g, bool b, bool a);
void stone_clear_depth(float depth);
void stone_set_draw_buffer_mask(int mask);
void stone_restore_after_custom_draw(void);
uint8_t stone_gpu_scopes_requested(void);
void stone_gpu_scope_begin(const char *name);
void stone_gpu_pass_scope_begin(const char *shader_name);
void stone_gpu_scope_end(void);
void stone_gpu_label_shader(uint64_t handle, const char *name);
void stone_set_blend_mode_i(int mode);
void stone_set_depth_mode(const char *compare_mode, bool write);
void stone_bind_sampler(int unit, int mode, uint64_t image);
bool stone_bind_canvas_texture_to_unit(uint64_t handle, int unit);
float stone_current_target_width(void);
float stone_current_target_height(void);
void stone_set_fullscreen_uv_scale(float sx, float sy);
void stone_draw_fullscreen_quad(void);
void stone_draw_fullscreen_rect(float u0, float v0, float u1, float v1);
bool stone_current_shader_bound(void);
uint64_t stone_create_shader_from_combined(const char *combined_src);
void stone_destroy_shader(uint64_t handle);
void stone_set_shader(uint64_t handle);
void stone_shader_set_texture(uint64_t handle, const char *name, int texture_unit);
bool stone_shader_upload_uniform_block(
    uint64_t handle,
    const uint8_t *data,
    int size,
    uint64_t fingerprint);
bool stone_bind_image_texture_to_unit(uint64_t handle, int unit);
bool stone_shader_bind_uniform_block(
    uint64_t handle,
    const char *shader_key,
    const char *block_name,
    int binding,
    int size,
    uint64_t fingerprint);
bool stone_shader_check_uniform_block_layout(
    uint64_t handle,
    const char *block_name,
    const char *names,
    int names_len,
    const int *offsets,
    int count,
    int plan_size);
const char *stone_get_working_directory(void);
const char *stone_get_executable_path(void);
const char *stone_get_pref_path(const char *organization, const char *application);
bool stone_file_write(const char *path, const char *data, size_t len);
bool stone_file_append(const char *path, const char *data, size_t len);
int stone_file_write_async(const char *path, const char *data, size_t len, bool append);
int stone_file_write_pair_async(const char *path, const char *data, size_t len,
    const char *second_path, const char *second_data, size_t second_len);
int stone_file_write_poll(int id, bool wait);
int stone_file_transaction_begin(void);
bool stone_file_transaction_write(int transaction, const char *path, const char *data,
    size_t len, bool append);
bool stone_file_transaction_publish(int transaction, const char *path, const char *data,
    size_t len, bool append);
int stone_file_transaction_commit(int transaction);
int stone_file_transaction_poll(int job, bool wait);
bool stone_file_sync(const char *path);
char *stone_file_read(const char *path, size_t *out_len);
void stone_free(void *ptr);
// Filesystem probes return -1 for inspection errors, 0 for absence/non-dir, 1 for found/dir.
int32_t stone_file_exists_status(const char *path);
int32_t stone_is_directory_status(const char *path);
bool stone_create_directory(const char *path);
bool stone_list_directory(const char *path, char ***out_files, int *out_count);
void stone_free_string_array(char **arr, int count);
bool stone_remove_file(const char *path);
bool stone_remove_dir(const char *path);
bool stone_rename(const char *src, const char *dst);
bool stone_telemetry_configure(
    const char *root,
    const char *game,
    const char *build,
    const char *platform,
    const char *channel,
    const char *endpoint,
    const char *token,
    bool explicit_reconsent);
bool stone_telemetry_record(const char *name, int64_t timestamp, const char *data);
void stone_telemetry_close(void);
bool stone_telemetry_clear(const char *root);
uint64_t stone_get_font_instance_image(uint64_t instance_id);
uint32_t stone_load_font_metrics(const char *path, int pixel_size, int face_mask, const char *extra_chars);
int stone_get_font_atlas_instance_pixel_size(uint32_t font_id, int index);
uint64_t stone_get_font_atlas_instance_image(uint32_t font_id, int index);
uint64_t stone_get_font_atlas_instance_image_at(uint32_t font_id, int index);
bool stone_get_font_atlas_size(uint32_t font_id, int *width, int *height);
void stone_draw_text(uint32_t shader, uint32_t font_id, const char *text, float x, float y, float sx, float sy);
void stone_draw_text_ex(uint32_t shader, uint32_t font_id, const char *text, float x, float y, float sx, float sy, float baseline);
float stone_get_text_baseline(uint32_t font_id, const char *text, float scale);
void stone_measure_text(uint32_t font_id, const char *text, float scale, float *width, float *height);
uint32_t stone_load_font(const char *path, int pixel_size, int face_mask, const char *extra_chars);
bool stone_unload_font(uint32_t font_id);
bool stone_font_release_coverage(uint32_t font_id);
bool stone_reload_font(uint32_t font_id, const char *path, int pixel_size);
bool stone_resize_font(uint32_t font_id, int pixel_size);
int stone_get_font_pixel_size(uint32_t font_id);
bool stone_get_glyph_info(uint32_t font_id, int face, int char_code, float *out);
int stone_get_font_slot_count(uint32_t font_id);
int stone_get_font_slot_codepoint(uint32_t font_id, int slot);
uint64_t stone_ensure_font_atlas_instance(uint32_t font_id, int pixel_size);
int stone_font_atlas_instance_count(uint32_t font_id);
bool stone_get_font_instance_size(uint64_t instance_id, int *width, int *height);
int stone_get_font_instance_pixel_size(uint64_t instance_id);
uint64_t stone_get_font_instance_byte_len(uint64_t instance_id);
bool stone_get_font_instance_glyph_info(uint64_t instance_id, int face, int char_code, float *out);
bool stone_get_font_unit_metrics(uint32_t font_id, float *out);
bool stone_get_glyph_unit_metrics(uint32_t font_id, int face, int char_code, float *out);
uint64_t stone_get_font_atlas_image(uint32_t font_id);
uint64_t stone_get_font_atlas_image_cached(uint32_t font_id);
const char *stone_get_hostname(void);
void stone_log_init(void);
void stone_log_set_level(int level);
int stone_log_get_level(void);
int stone_log_record_total(int level);
const char *stone_log_records_text(int level, int max_rows);
void stone_log_bytes(
    int level,
    const unsigned char *category,
    size_t category_len,
    const unsigned char *message,
    size_t message_len,
    int wall_clock);


bool stone_bus_set_gain(uint32_t bus, float gain);
bool stone_bus_set_lowpass(uint32_t bus, float cutoff_hz, float q);
bool stone_bus_duck(uint32_t bus, float depth, float attack_secs, float release_secs);
bool stone_audio_quiet_env(void);
bool stone_voice_active(uint64_t voice);
int64_t stone_voice_stream_frames(uint64_t voice);
bool stone_voice_stop(uint64_t voice);
bool stone_voice_set_gain(uint64_t voice, float gain);
uint64_t stone_load_sound_deferred(const char *path, int64_t frames, int64_t rate, int64_t channels);
uint64_t stone_load_sound_stream(const char *path, int64_t frames, int64_t rate, int64_t channels);
bool stone_sound_admit(uint64_t handle);
int64_t stone_service_sound_admissions(void);
int64_t stone_sound_decode_sync(uint64_t handle);
bool stone_sound_resident(uint64_t handle);
bool stone_sound_playable(uint64_t handle);
int64_t stone_sound_stream_bytes(uint64_t handle);
int64_t stone_sound_pcm_bytes(uint64_t handle);
bool stone_unload_sound(uint64_t handle);
uint64_t stone_load_music(const char *path);
void stone_play_music(uint64_t handle);
uint64_t stone_play_sound(
    uint64_t handle,
    uint32_t bus,
    float volume,
    float speed,
    float decay,
    bool looped,
    bool spatial,
    float pan);
float stone_get_sound_duration(uint64_t handle);
bool stone_reload_sound(uint64_t handle, const char *path);

uint64_t stone_create_image_data(int width, int height);
bool stone_image_data_size(uint64_t handle, int *width, int *height);
void stone_destroy_image_data(uint64_t handle);
bool stone_image_data_save_png(uint64_t handle, const char *path);
void stone_image_data_set_pixel(
    uint64_t handle, int x, int y, float r, float g, float b, float a);
void stone_image_data_get_pixel(
    uint64_t handle, int x, int y, float *r, float *g, float *b, float *a);
bool stone_image_update_from_data(uint64_t image_handle, uint64_t data_handle);
uint64_t stone_new_image_from_data(uint64_t data_handle, int filter_mode);
// flags: bit 0 exact texels (no image cap), bit 1 deferred (a flush does not
// wait for the decode). Positive width/height supply manifest dimensions for
// deferred admission; zero/zero probes the file. The worker validates the size.
uint64_t stone_load_image(const char *path, int filter_mode, int flags, int width, int height);
uint64_t stone_get_white_image(void);
void stone_unload_image(uint64_t handle);
bool stone_reload_image(uint64_t handle, const char *path);
bool stone_image_set_wrap(uint64_t handle, bool repeat);
bool stone_image_exists(uint64_t handle);
int stone_native_handle_state(const char *class_name, uint64_t handle);
void stone_get_image_size(uint64_t handle, int *width, int *height);
void stone_image_storage_size(uint64_t handle, int *width, int *height);
bool stone_probe_image_size(const char *path, int *width, int *height);
void stone_flush_image_uploads(void);
bool stone_image_require(uint64_t handle);
bool stone_image_ready(uint64_t handle);
int64_t stone_image_pending_bytes(void);
int stone_service_image_uploads(void);
void stone_get_graphics_stats(StoneGraphicsStats *stats);
void stone_track_text_draw_call(void);
void stone_get_image_frame_stats(StoneImageLoadStats *stats);
void stone_get_image_profile_stats(StoneImageLoadStats *stats);

// Mesh rendering system. The layout is the CALLER's: float attribs at
// sequential locations from 0, component counts in attr_sizes, offsets
// accumulated from them -- the same contract stone_mesh_create takes.
// stone_draw_mesh returns -1 when skipped, 0 when drawn untextured, 1 when
// the texture resolved and was bound before the draw.
int stone_draw_mesh(
    const float* vertex_data,
    int vertex_count,
    int vertex_stride,
    const int* attr_sizes,
    int attr_count,
    int texture_kind,
    uint64_t texture);

// Compact center/rim fans among ordinary triangle spans. fan_ranges contains
// (first vertex, segment count) pairs in source order. Attribute-only shaders
// may share these records; shaders observing vertex IDs use stone_draw_mesh.
int stone_draw_mesh_fans(
    const float* vertex_data, int vertex_count, int vertex_stride,
    const int* attr_sizes, int attr_count, int texture_kind, uint64_t texture,
    const int* fan_ranges, int fan_count
);

// Retained mesh objects (instancing optional). Float attribs at sequential
// locations: base from 0, instance attribs continue after with divisor 1.
// mode: 0 = triangle_fan, 1 = triangles.
uint64_t stone_mesh_create(
    int mode,
    const float* verts, int vert_count, int stride_floats,
    const int* attr_sizes, int attr_count);
void stone_mesh_set_instances(
    uint64_t handle,
    const float* data, int count, int stride_floats,
    const int* attr_sizes, int attr_count);
int stone_mesh_draw(uint64_t handle, uint64_t texture, int count);
void stone_mesh_free(uint64_t handle);
int stone_mesh_instance_count(uint64_t handle);

void stone_destroy_canvas(uint64_t handle);
bool stone_canvas_exists(uint64_t handle);
uint64_t stone_pixel_readback_new(int is_depth);
uint64_t stone_pixel_readback_area_new(int side);
void stone_pixel_readback_submit(uint64_t id, uint64_t canvas_handle, int x, int y);
float stone_pixel_readback_consume(uint64_t id);
int stone_pixel_readback_poll_rgba(uint64_t id, float* out, int capacity);
float stone_read_canvas_pixel(uint64_t handle, int x, int y);
int stone_read_canvas_nearest_id(uint64_t handle, int x, int y, int side);
int stone_canvas_read_region_i32(uint64_t handle, int x, int y, int w, int h, int32_t *out, int capacity);

bool stone_image_data_fill_rect_rgba8(uint64_t handle, int x, int y, int width, int height, int r, int g, int b, int a);
int64_t stone_relight_diff_submit(int width, int samples, int border, const uint8_t* old, int64_t old_len, const uint8_t* next, int64_t len);
int stone_relight_diff_copy(uint64_t handle, uint8_t* pixels, int64_t len, float* region);
void stone_relight_diff_cancel(uint64_t handle);
int64_t stone_light_table_measure(int size, int cell_size, double epsilon, int count, const float* positions, int* counts);
bool stone_light_table_pack(int size, int cell_size, double epsilon, int count, const float* positions, const float* colors, const int* owners, const int* source_rows, const int* source_slots, const int* counts, int* offsets, float* data, int64_t data_len);

bool stone_actor_query_rect(
    const int32_t *records, int64_t len,
    const int32_t *candidates, int64_t candidate_len,
    int min_x, int min_y, int max_x, int max_y,
    bool include_uninstalled, int32_t *output, int64_t output_len);

uint64_t stone_flow_submit(
    int width, int height, int goal_x, int goal_y, int cap,
    const uint8_t *walkable, int64_t len);
int stone_flow_poll(uint64_t handle);
int stone_flow_wait(uint64_t handle);
int stone_flow_copy(uint64_t handle, float *output, int64_t len);
void stone_flow_cancel(uint64_t handle);
void stone_flow_cancel_all(void);

// Navigation uses two 32-bit words for each Rock int to preserve the full
// exact Rock integer range across the native FFI.
typedef struct NativeNavIntWire {
    int32_t lo;
    int32_t hi;
} NativeNavIntWire;

int64_t stone_nav_create(
    const NativeNavIntWire *layout, int64_t layout_len,
    const uint8_t *blocked, const NativeNavIntWire *corners,
    const NativeNavIntWire *extra, int64_t tiles,
    const NativeNavIntWire *links, int64_t link_len, int64_t cluster_size);
void stone_nav_free(int64_t handle);
int64_t stone_nav_layout_valid(const NativeNavIntWire *data, int64_t len, int64_t cluster);
int64_t stone_nav_edge_cost(int64_t step, int64_t ea, int64_t eb);
double stone_nav_metric(int64_t handle, int64_t field);
int64_t stone_nav_descend_prepare(
    int64_t handle, int64_t slot, int64_t generation,
    int64_t space, int64_t x, int64_t y);
int64_t stone_nav_descend_candidate(
    int64_t handle, int64_t index, int64_t phase, NativeNavIntWire *out);
int64_t stone_nav_descend_accept(int64_t handle, int64_t index, int64_t phase);
int64_t stone_nav_descend_result(int64_t handle, NativeNavIntWire *out);
int64_t stone_nav_route_restore(
    int64_t handle, int64_t slot, int64_t generation,
    const NativeNavIntWire *data, int64_t len,
    int64_t path_pos, int64_t cost, int64_t walked);
int64_t stone_nav_set_links(int64_t handle, const NativeNavIntWire *data, int64_t len);
int64_t stone_nav_edit(
    int64_t handle, int64_t op, int64_t a, int64_t b, int64_t c,
    int64_t d, int64_t e, int64_t f);
int64_t stone_nav_get(int64_t handle, int64_t op, int64_t a, int64_t b, int64_t c);
int64_t stone_nav_request(
    int64_t handle, int64_t space, int64_t x, int64_t y,
    int64_t goal_space, int64_t gx, int64_t gy, int64_t gw, int64_t gh,
    int64_t teleports, int64_t seed, int64_t due_tick);
int64_t stone_nav_plan(
    int64_t handle, int64_t slot, int64_t generation,
    int64_t op, int64_t a, int64_t b, int64_t c);
int64_t stone_nav_step(
    int64_t handle, int64_t slot, int64_t generation,
    int64_t space, int64_t x, int64_t y, int64_t due_tick, NativeNavIntWire *out);
int64_t stone_nav_route_copy(
    int64_t handle, int64_t slot, int64_t generation,
    NativeNavIntWire *out, int64_t len);

// Native-artifact ImGui stubs: the C artifact has no cimgui, so key state is
// "not down", name resolution reports the imgui-absent contract value, and
// injection is a no-op.
bool stone_imgui_is_key_down(int id);
bool stone_imgui_ctrl_down(void);
int stone_imgui_key_id(const char *name);
void stone_imgui_inject_key(int id);
void stone_imgui_release_key(int id);

void stone_show_cursor(bool show);
bool stone_is_cursor_visible(void);
uint64_t stone_create_cursor(const char *path, int hot_x, int hot_y);
uint64_t stone_create_cursor_from_image_data(uint64_t image_data_handle, int hot_x, int hot_y);
uint64_t stone_set_system_cursor(int cursor_type);
void stone_set_cursor(uint64_t cursor_id);
void stone_reset_cursor(void);
void stone_free_cursor(uint64_t cursor_id);
uint64_t stone_active_cursor(void);
void stone_watch_asset(const char *path);
int stone_poll_changed_assets(void);
const char *stone_changed_asset_path(int index);

// Native ImGui transports use GL texture names rather than resource handles.
void stone_imgui_image_raw(uint64_t texture, float size_x, float size_y,
    float uv1_x, float uv1_y, float uv2_x, float uv2_y,
    float tint_r, float tint_g, float tint_b, float tint_a,
    float border_r, float border_g, float border_b, float border_a);
bool stone_imgui_image_button_raw(const char *id, uint64_t texture, float size_x, float size_y,
    float uv1_x, float uv1_y, float uv2_x, float uv2_y,
    float bg_r, float bg_g, float bg_b, float bg_a,
    float tint_r, float tint_g, float tint_b, float tint_a);
void stone_imgui_window_add_image_quad(uint64_t texture,
    float p1_x, float p1_y, float p2_x, float p2_y, float p3_x, float p3_y, float p4_x, float p4_y,
    float uv1_x, float uv1_y, float uv2_x, float uv2_y, float uv3_x, float uv3_y, float uv4_x, float uv4_y,
    uint32_t col);
void stone_imgui_window_add_quad_filled(float p1_x, float p1_y, float p2_x, float p2_y,
    float p3_x, float p3_y, float p4_x, float p4_y, uint32_t col);
bool stone_imgui_is_rect_visible(float x, float y);
bool stone_imgui_is_item_fully_visible(void);
void stone_imgui_set_next_window_pos(float x, float y);
void stone_imgui_set_window_focus(const char *name);
bool stone_imgui_button_sized(const char *label, float x, float y);
bool stone_imgui_collapsing_header(const char *label);
void stone_imgui_input_float(const char *label, float value, float step, float step_fast,
    const char *format, bool *changed, float *result);

#ifdef __cplusplus
}
#endif

#endif
