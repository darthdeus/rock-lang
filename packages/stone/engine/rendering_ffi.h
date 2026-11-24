const char* stone_pad_type_name(void);

const char* stone_scancode_token_at(int index);
const char* stone_scancode_token_for(int scancode);
const char* stone_scancode_label(const char* token);

void stone_imgui_shutdown(void);

// Draw a fullscreen quad in NDC coordinates for post-processing
void stone_draw_fullscreen_quad(void);
void stone_set_fullscreen_uv_scale(float sx, float sy);

// Mesh rendering system. The layout is the CALLER's: float attribs at
// sequential locations from 0, component counts in attr_sizes, offsets
// accumulated from them -- the same contract stone_mesh_create takes.
// Returns -1 when skipped, 0 when drawn untextured, 1 when the texture
// resolved and was bound before the draw.
int stone_draw_mesh(
    const float* vertex_data,
    int vertex_count,
    int vertex_stride,
    const int* attr_sizes,
    int attr_count,
    int texture_kind,
    uint64_t texture
);

int stone_draw_mesh_fans(
    const float* vertex_data, int vertex_count, int vertex_stride,
    const int* attr_sizes, int attr_count, int texture_kind, uint64_t texture,
    const int* fan_ranges, int64_t fan_count
);

// Retained mesh objects (instancing optional). Float attribs at
// sequential locations: base from 0, instance attribs continue
// after with divisor 1. mode: 0 = triangle_fan, 1 = triangles.
// Draws under the currently bound shader (with_shader).
uint64_t stone_mesh_create(
    int mode,
    const float* verts, int vert_count, int stride_floats,
    const int* attr_sizes, int attr_count);
int stone_mesh_set_instances(
    uint64_t handle,
    const float* data, int count, int stride_floats,
    const int* attr_sizes, int attr_count);
int stone_mesh_update_instances(uint64_t handle, const float* data, int count);
int stone_mesh_draw(uint64_t handle, StoneImageHandle texture, int count);
void stone_mesh_free(uint64_t handle);
int stone_get_live_mesh_count(void);
uint64_t stone_get_live_mesh_handle(int index);
uint64_t stone_mesh_bytes(uint64_t handle);

// Canvas/FBO system
typedef uint64_t StoneCanvasHandle;

// Canvas format constants
static const int STONE_CANVAS_RGBA8 = 0;
static const int STONE_CANVAS_RGBA16F = 1;
static const int STONE_CANVAS_RGBA32F = 2;
static const int STONE_CANVAS_R32I = 3;

const char* stone_gl_blend_state(void);
const char* stone_gl_write_state(void);

// Screenshot system
uint8_t stone_save_screenshot(const char* filepath, int width, int height, int max_size);
uint8_t stone_save_screenshot_sized(const char* filepath, int width, int height, int max_size, int* out_width, int* out_height);
uint8_t stone_frame_stream_begin(const char* path);
uint8_t stone_frame_stream_record(int width, int height);
uint8_t stone_frame_stream_finish(int* out_frames, int* out_width, int* out_height);
const char* stone_frame_stream_last_error(void);
void stone_track_text_draw_call(void);

// Shader system
typedef uint64_t StoneShaderHandle;

StoneShaderHandle stone_create_shader_from_combined(const char* combined_src);
void stone_destroy_shader(StoneShaderHandle handle);
void stone_set_shader(StoneShaderHandle handle);

uint8_t stone_shader_bind_uniform_block(StoneShaderHandle handle, const char* shader_key, const char* block_name, int binding, int size, uint64_t fingerprint);
uint8_t stone_shader_upload_uniform_block(StoneShaderHandle handle, const uint8_t* data, int size, uint64_t fingerprint);
int stone_unfilled_uniform_block_count();
uint8_t stone_shader_read_uniform_block(StoneShaderHandle handle, const char* block_name, uint8_t* dst, int size);
const char* stone_uniform_block_binding_state_json(StoneShaderHandle handle);
void stone_graphics_capture_begin(const char* directory, int frame);
void stone_graphics_capture_cancel();
void stone_graphics_capture_scene_depth(StoneCanvasHandle handle);
const char* stone_graphics_capture_finish_json();
void stone_mesh_capture_arm(bool read_depth);
const char* stone_mesh_capture_finish_json();
uint8_t stone_shader_snapshot_uniform_block(StoneShaderHandle handle, const char* block_name, uint8_t* dst, int size);
uint8_t stone_shader_check_uniform_block_layout(StoneShaderHandle handle, const char* block_name, const char* names, int names_len, const int* offsets, int count, int plan_size);

void stone_shader_set_texture(StoneShaderHandle handle, const char* name, int texture_unit);

// Text rendering system. stone_load_font returns a 1-based font_id
// (0 = failure). All draw/measure/glyph functions take a font_id to
// pick which loaded font to operate on. Engine creates one default
// font at startup; games can load additional fonts via load_font().
uint32_t stone_load_font(const char* path, int pixel_size, int face_mask, const char* extra_chars);
// Metrics-only load: same file, same charset, same CPU raster and the
// same font-unit metrics, but no GL texture. The only font load that
// succeeds without a window, which is what lets the `rock test <game>`
// luajit process measure and lay out text. Drawing with the id it
// returns is refused by name in stone-rs, never silently.
uint32_t stone_load_font_metrics(const char* path, int pixel_size, int face_mask, const char* extra_chars);
bool stone_unload_font(uint32_t font_id);
bool stone_reload_font(uint32_t font_id, const char* path, int pixel_size);
bool stone_resize_font(uint32_t font_id, int pixel_size);
void stone_stream_text(uint32_t shader, uint32_t font_id, const char* text, float x, float y, float scale_x, float scale_y, float baseline, bool multiline);
float stone_get_text_baseline(uint32_t font_id, const char* text, float scale);
void stone_measure_text(uint32_t font_id, const char* text, float scale, float* out_width, float* out_height);

// Font atlas exposure for unified text batching
uint64_t stone_get_font_atlas_image(uint32_t font_id);
uint64_t stone_get_font_atlas_image_cached(uint32_t font_id);
int stone_get_font_pixel_size(uint32_t font_id);
bool stone_get_glyph_info(uint32_t font_id, int face, int char_code, float* out);
uint64_t stone_ensure_font_atlas_instance(uint32_t font_id, int pixel_size);
double stone_ensure_font_atlas_instance_number(uint32_t font_id, int pixel_size);

const char* stone_native_live_handles(const char* class);
uint8_t stone_native_destroy(const char* class, uint64_t handle);
int stone_native_handle_state(const char* class, uint64_t handle);
const char* stone_native_id_accounting(void);

// Generated by stone-rs/build.rs.
uint8_t stone_bind_canvas_depth_to_unit(uint64_t handle, int unit);
uint8_t stone_bind_canvas_texture_to_unit(uint64_t handle, int unit);
void stone_bind_sampler(int unit, int mode, uint64_t image);
int stone_canvas_active_h(uint64_t handle);
uint8_t stone_canvas_active_is_creation(uint64_t handle);
int stone_canvas_active_w(uint64_t handle);
uint8_t stone_canvas_attach_color_target(uint64_t color_handle, uint64_t id_handle, int slot);
uint8_t stone_canvas_attach_id_target(uint64_t color_handle, uint64_t id_handle);
uint8_t stone_canvas_bound_as_target(uint64_t handle);
int stone_canvas_capacity_h(uint64_t handle);
int stone_canvas_capacity_w(uint64_t handle);
uint64_t stone_canvas_capture_begin(uint64_t canvas_handle);
uint8_t stone_canvas_capture_cancel(uint64_t id);
int stone_canvas_capture_poll(uint64_t id, uint8_t* out, int capacity);
uint8_t stone_canvas_capture_release(uint64_t id);
uint8_t stone_canvas_exists(uint64_t handle);
uint8_t stone_canvas_has_depth(uint64_t handle);
int stone_canvas_read_region_i32(uint64_t handle, int x, int y, int w, int h, int* out, int capacity);
int stone_canvas_read_region_rgba32f(uint64_t handle, int x, int y, int w, int h, float* out, int capacity);
int stone_canvas_read_rgba8(uint64_t handle, uint8_t* out, int capacity);
int stone_canvas_report(uint64_t handle, int* width, int* height);
uint8_t stone_canvas_resize(uint64_t handle, int width, int height);
uint8_t stone_canvas_save_png(uint64_t handle, const char* path);
uint8_t stone_canvas_upload_rgba32f(uint64_t handle, const float* pixels, int count);
uint8_t stone_canvas_upload_rgba8(uint64_t handle, const uint8_t* pixels, int byte_len);
void stone_clear_canvas(float r, float g, float b, float a);
void stone_clear_canvas_int(int draw_buffer, int value);
void stone_clear_depth(float depth);
uint64_t stone_create_canvas(int width, int height, int format, bool with_depth);
uint64_t stone_create_cursor(const char* path, int hot_x, int hot_y);
uint8_t stone_current_shader_bound(void);
float stone_current_target_height(void);
float stone_current_target_width(void);
void stone_destroy_canvas(uint64_t handle);
void stone_draw_fullscreen_rect(float u0, float v0, float u1, float v1);
int stone_font_atlas_instance_count(unsigned int font_id);
int64_t stone_font_data_len(unsigned int font_id);
uint8_t stone_font_release_coverage(unsigned int font_id);
void stone_free_cursor(uint64_t cursor_id);
uint32_t stone_get_canvas_texture(uint64_t handle);
uint64_t stone_get_font_atlas_instance_image_at(unsigned int font_id, int index);
int stone_get_font_atlas_instance_pixel_size(unsigned int font_id, unsigned int index);
uint8_t stone_get_font_atlas_size(unsigned int font_id, int* width, int* height);
int stone_get_font_instance_byte_len(uint64_t instance_id);
uint8_t stone_get_font_instance_glyph_info(uint64_t instance_id, int face, int char_code, float* out);
uint64_t stone_get_font_instance_image(uint64_t instance_id);
int stone_get_font_instance_pixel_size(uint64_t instance_id);
uint8_t stone_get_font_instance_size(uint64_t instance_id, int* width, int* height);
int stone_get_font_slot_codepoint(unsigned int font_id, int slot);
int stone_get_font_slot_count(unsigned int font_id);
uint8_t stone_get_font_unit_metrics(unsigned int font_id, float* out);
void stone_get_global_mouse_position(int* x, int* y);
uint8_t stone_get_glyph_unit_metrics(unsigned int font_id, int face, int char_code, float* out);
uint64_t stone_get_live_canvas_bytes(void);
int stone_get_live_canvas_count(void);
uint64_t stone_get_live_canvas_handle(int index);
void stone_get_mouse_position(int* x, int* y);
uint8_t stone_gl_depth_write_enabled(void);
uint32_t stone_gl_pass_state_begin(void);
bool stone_gl_pass_state_end(uint32_t scope);
uint8_t stone_imgui_init(StoneWindow* _window);
uint8_t stone_imgui_initialized(void);
int stone_imgui_last_vtx_count(void);
void stone_imgui_new_frame(void);
void* stone_imgui_prepare_render(void);
void stone_imgui_render(void);
uint8_t stone_is_key_down(const char* key);
uint8_t stone_pad_connected(void);
int stone_pixel_readback_answer_x(uint64_t id);
int stone_pixel_readback_answer_y(uint64_t id);
uint64_t stone_pixel_readback_area_new(int max_side);
float stone_pixel_readback_consume(uint64_t id);
void stone_pixel_readback_destroy(uint64_t id);
int stone_pixel_readback_in_flight(uint64_t id);
uint64_t stone_pixel_readback_new(int is_depth);
int stone_pixel_readback_poll_id(uint64_t id);
int stone_pixel_readback_poll_rgba(uint64_t id, float* out, int capacity);
uint8_t stone_pixel_readback_reset_contents(uint64_t id);
void stone_pixel_readback_submit(uint64_t id, uint64_t canvas_handle, int x, int y);
void stone_pixel_readback_submit_area(uint64_t id, uint64_t canvas_handle, int x, int y, int side);
int stone_read_canvas_nearest_id(uint64_t canvas_handle, int x, int y, int side);
float stone_read_canvas_pixel(uint64_t handle, int x, int y);
float stone_read_canvas_pixel_channel(uint64_t handle, int x, int y, int channel);
float stone_read_depth_pixel(uint64_t handle, int x, int y);
void stone_read_window_rect(int x, int y, int w, int h, uint8_t* out);
void stone_reset_cursor(void);
void stone_restore_after_custom_draw(void);
uint8_t stone_sampler_bound(int unit);
int stone_scancode_value(const char* token);
void stone_set_blend_mode_i(int mode);
void stone_set_canvas(uint64_t handle);
void stone_set_canvas_active(uint64_t handle, int w, int h);
void stone_set_clear_color(float r, float g, float b, float a);
void stone_set_color_mask(bool r, bool g, bool b, bool a);
void stone_set_cursor(uint64_t cursor_id);
void stone_set_depth_mode(const char* compare, bool write);
void stone_set_draw_buffer_mask(int mask);
void stone_set_gl_viewport(int x, int y, int w, int h);
void stone_set_present_viewport(bool active, int x, int y, int w, int h);
void stone_set_scissor(bool enabled, int x, int y, int w, int h);
uint64_t stone_set_system_cursor(int cursor_type);
void stone_set_viewport_rect(int x, int y, int w, int h);
void stone_show_cursor(bool show);
uint8_t stone_sprite_init(void);
int stone_test_virtual_pad_attach(void);
uint8_t stone_test_virtual_pad_axis(int index, const char* token, int value);
uint8_t stone_test_virtual_pad_button(int index, const char* token, bool down);
uint8_t stone_test_virtual_pad_detach(int index);
uint8_t stone_text_init(void);
int stone_ui_probe_accept(const char* kind, const char* key, const char* detail);
int stone_ui_probe_accepted(void);
void stone_ui_probe_begin(bool windowless);
uint8_t stone_ui_probe_check(int expected);
uint8_t stone_ui_probe_check_population(int accepted, int refused, int observed);
int stone_ui_probe_consumed(void);
void stone_ui_probe_context(const char* phase, const char* pass, const char* surface, const char* space);
void stone_ui_probe_custom_consumed(void);
void stone_ui_probe_end_operation(void);
uint8_t stone_ui_probe_expect_vertices(int id, const void* data, int count, int stride);
uint8_t stone_ui_probe_observe(const char* kind, const char* detail);
uint8_t stone_ui_probe_refuse(const char* kind, const char* detail);
uint8_t stone_ui_probe_resource(const char* kind, uint64_t handle, const char* source, int generation);
uint8_t stone_ui_probe_retire(const char* kind, uint64_t handle);
void stone_ui_probe_stop(void);
double stone_ui_probe_vertex(int index, int lane);
int stone_ui_probe_z(int index);
