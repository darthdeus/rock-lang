const char* stone_pad_type_name(void);

const char* stone_scancode_token_at(int index);
const char* stone_scancode_token_for(int scancode);
const char* stone_scancode_label(const char* token);

void stone_imgui_shutdown(void);

// Draw a fullscreen quad in NDC coordinates for post-processing
void stone_draw_fullscreen_quad(void);
void stone_set_fullscreen_uv_scale(float sx, float sy);

// Compact triangle fans use the same vertex layout and return status as stone_draw_mesh.
int stone_draw_mesh_fans(
    const float* vertex_data, int vertex_count, int vertex_stride,
    const int* attr_sizes, int attr_count, int texture_kind, uint64_t texture,
    const int* fan_ranges, int64_t fan_count
);

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
const char* stone_frame_stream_last_error(void);

// Shader system
typedef uint64_t StoneShaderHandle;

const char* stone_uniform_block_binding_state_json(StoneShaderHandle handle);
const char* stone_graphics_capture_finish_json();
const char* stone_mesh_capture_finish_json();

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
const char* stone_native_id_accounting(void);

// Used by the Rock queue_native_scene_pass function.
typedef int32_t (*StoneNativeSceneFn)(void* userdata, uint64_t current_scene, uint64_t partner_scene);
static const int STONE_SCENE_NO_OUTPUT_CHANGE = 0;
static const int STONE_SCENE_OUTPUT_PARTNER = 1;

// Used by the Rock queue_native_silhouette_draw function. Return 1 after drawing, or 0 for no writes.
typedef int32_t (*StoneNativeSilhouetteFn)(void* userdata, uint64_t target);

// Frame hooks registered through `frame_hook_register` (see docs/engine.md).
static const int STONE_FRAME_HOOK_WORLD_BEGIN = 1;
static const int STONE_FRAME_HOOK_WORLD_BATCHES = 2;
static const int STONE_FRAME_HOOK_SCENE_END = 3;
static const int STONE_WORLD_IMAGE_KEEP_POST = 1;
// Reserved; `frame_set_world_image` rejects it.
static const int STONE_WORLD_IMAGE_SKIP_POST = 2;
static const int STONE_STAGE_GL_ERROR = 32;
static const int STONE_IMAGE_IMPORT_REPEAT = 1;
typedef struct StoneFrameInfo {
    uint32_t struct_size;
    uint32_t point;
    uint64_t frame;
    uint64_t context_generation;
    uint64_t scene_target;
    uint64_t scene_partner;
    uint64_t scene_generation;
    int target_width;
    int target_height;
    int active_width;
    int active_height;
    float world_to_scene_offset_x;
    float world_to_scene_offset_y;
    float world_to_scene_scale;
    uint32_t _reserved;
} StoneFrameInfo;
// Return 0 for no writes, 1 after writing the scene target, negative on failure.
typedef int32_t (*StoneFrameHookFn)(void* userdata, const StoneFrameInfo* info);
typedef void (*StoneFrameHookReleaseFn)(void* userdata);

// The table `stone_native_renderer_api(1)` returns. Version 1 only ever
// appends fields: check `abi_version == 1` and `struct_size >= sizeof` before
// use. Every entry must be called on the render thread.
typedef struct StoneNativeRendererApiV1 {
    uint32_t abi_version;
    uint32_t struct_size;
    uint32_t (*stage_begin)(void);
    uint32_t (*stage_end)(uint32_t scope);
    uint8_t (*frame_active)(void);
    uint64_t (*frame_target)(void);
    uint8_t (*frame_size)(int* width, int* height);
    uint64_t (*context_generation)(void);
    uint8_t (*handle_valid)(int kind, uint64_t handle);
    uint64_t (*handle_generation)(int kind, uint64_t handle);
    uint64_t (*find_shader)(const char* name);
    uint64_t (*find_texture)(const char* path);
    uint64_t (*target_info)(uint64_t handle, int* width, int* height, int* format, int* color_attachments, bool* has_depth);
    uint64_t (*shader_info)(uint64_t handle, int* block_count, int* texture_slot_count);
    uint64_t (*texture_info)(uint64_t handle, int* width, int* height, int* format, bool* ready);
    uint64_t (*mesh_info)(uint64_t handle, int* vert_count, int* attr_count, int* inst_count);
    uint64_t (*target_create)(int width, int height, const int* attachment_formats, int attachment_count, bool with_depth);
    uint8_t (*target_resize)(uint64_t handle, int width, int height);
    void (*target_destroy)(uint64_t handle);
    uint64_t (*target_attachment)(uint64_t handle, int slot);
    void (*set_canvas)(uint64_t handle);
    void (*set_canvas_active)(uint64_t handle, int w, int h);
    void (*clear_canvas)(float r, float g, float b, float a);
    void (*clear_canvas_int)(int draw_buffer, int value);
    void (*clear_depth)(float depth);
    int (*shader_uniform_block_count)(uint64_t handle);
    int (*shader_uniform_block_find_by_binding)(uint64_t handle, int binding);
    int (*shader_uniform_block_find_by_name)(uint64_t handle, const char* name);
    uint8_t (*shader_uniform_block_info)(uint64_t handle, int index, int* binding_out, int* size_out, int* member_count_out, uint8_t* name_out, int name_cap);
    uint64_t (*shader_uniform_block_fingerprint)(uint64_t handle, int index);
    uint8_t (*shader_uniform_block_member_info)(uint64_t handle, int block_index, int member_index, int* offset_out, int* kind_out, int* capacity_out, int* stride_out);
    int (*shader_uniform_block_member_name)(uint64_t handle, int block_index, int member_index, uint8_t* name_out, int name_cap);
    int (*shader_uniform_block_member_offset)(uint64_t handle, int block_index, const char* member_name);
    int (*shader_texture_slot_count)(uint64_t handle);
    int (*shader_texture_slot_find)(uint64_t handle, const char* name);
    int (*shader_texture_slot_info)(uint64_t handle, int index, int* unit_out, uint8_t* name_out, int name_cap);
    void (*set_shader)(uint64_t handle);
    uint8_t (*upload_uniform_block)(uint64_t handle, const uint8_t* data, int size, uint64_t fingerprint);
    uint64_t (*get_white_image)(void);
    uint8_t (*bind_image_texture_to_unit)(uint64_t handle, int unit);
    uint8_t (*bind_canvas_texture_to_unit)(uint64_t handle, int unit);
    uint8_t (*bind_canvas_depth_to_unit)(uint64_t handle, int unit);
    void (*bind_sampler)(int unit, int mode, uint64_t image);
    void (*set_depth_mode)(const char* compare, bool write);
    void (*set_blend_mode_i)(int mode);
    void (*set_draw_buffer_mask)(int mask);
    void (*set_color_mask)(bool r, bool g, bool b, bool a);
    void (*set_scissor)(bool enabled, int x, int y, int w, int h);
    void (*set_gl_viewport)(int x, int y, int w, int h);
    int (*draw_mesh)(const float* vertex_data, int vertex_count, int vertex_stride, const int* attr_sizes, int attr_count, int texture_kind, uint64_t texture);
    void (*draw_fullscreen_quad)(void);
    void (*draw_fullscreen_rect)(float u0, float v0, float u1, float v1);
    uint64_t (*mesh_create)(int mode, const float* verts, int vert_count, int stride_floats, const int* attr_sizes, int attr_count);
    int (*mesh_set_instances)(uint64_t handle, const float* data, int count, int stride_floats, const int* attr_sizes, int attr_count);
    int (*mesh_update_instances)(uint64_t handle, const float* data, int count);
    int (*mesh_draw)(uint64_t handle, uint64_t tex, int count);
    void (*mesh_free)(uint64_t handle);
    uint64_t (*target_color_view)(uint64_t source);
    uint64_t (*get_live_mesh_bytes)(void);
    void* (*gl_get_proc_address)(const char* name);
    uint32_t (*image_gl_texture)(uint64_t image);
    uint32_t (*target_gl_texture)(uint64_t target, int slot);
    uint32_t (*target_gl_depth_texture)(uint64_t target);
    uint32_t (*target_gl_framebuffer)(uint64_t target);
    uint64_t (*image_import_gl_texture)(uint32_t name, int width, int height, uint32_t flags);
    uint8_t (*image_import_update)(uint64_t image, int width, int height);
    uint8_t (*image_release_import)(uint64_t image);
    uint64_t (*frame_hook_register)(uint32_t point, int order, StoneFrameHookFn run, StoneFrameHookReleaseFn release, void* userdata, const char* name);
    uint8_t (*frame_hook_unregister)(uint64_t hook);
    uint8_t (*frame_set_world_image)(uint64_t target, uint32_t flags);
    void (*gpu_scope_begin)(const char* name);
    void (*gpu_scope_end)(void);
} StoneNativeRendererApiV1;

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
uint64_t stone_create_shader_from_combined(const char* src);
uint8_t stone_current_shader_bound(void);
float stone_current_target_height(void);
float stone_current_target_width(void);
void stone_destroy_canvas(uint64_t handle);
void stone_destroy_shader(uint64_t handle);
void stone_draw_fullscreen_rect(float u0, float v0, float u1, float v1);
int stone_draw_mesh(const float* vertex_data, int vertex_count, int vertex_stride, const int* attr_sizes, int attr_count, int texture_kind, uint64_t texture);
int stone_font_atlas_instance_count(unsigned int font_id);
int64_t stone_font_data_len(unsigned int font_id);
uint8_t stone_font_release_coverage(unsigned int font_id);
uint8_t stone_frame_stream_begin(const char* path);
uint8_t stone_frame_stream_finish(int* out_frames, int* out_width, int* out_height);
uint8_t stone_frame_stream_record(int w, int h);
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
uint64_t stone_get_live_mesh_bytes(void);
int stone_get_live_mesh_count(void);
uint64_t stone_get_live_mesh_handle(int index);
void stone_get_mouse_position(int* x, int* y);
uint8_t stone_gl_depth_write_enabled(void);
uint32_t stone_gl_pass_state_begin(void);
bool stone_gl_pass_state_end(uint32_t scope);
void stone_graphics_capture_begin(const char* directory, int frame);
void stone_graphics_capture_cancel(void);
void stone_graphics_capture_scene_depth(uint64_t handle);
uint8_t stone_imgui_init(StoneWindow* _window);
uint8_t stone_imgui_initialized(void);
int stone_imgui_last_vtx_count(void);
void stone_imgui_new_frame(void);
void* stone_imgui_prepare_render(void);
void stone_imgui_render(void);
uint8_t stone_is_key_down(const char* key);
uint64_t stone_mesh_bytes(uint64_t handle);
void stone_mesh_capture_arm(bool read_depth);
uint64_t stone_mesh_create(int mode, const float* verts, int vert_count, int stride_floats, const int* attr_sizes, int attr_count);
int stone_mesh_draw(uint64_t handle, uint64_t tex, int count);
void stone_mesh_free(uint64_t handle);
int stone_mesh_set_instances(uint64_t handle, const float* data, int count, int stride_floats, const int* attr_sizes, int attr_count);
int stone_mesh_update_instances(uint64_t handle, const float* data, int count);
uint64_t stone_native_context_generation(void);
uint8_t stone_native_destroy(const char* class, uint64_t handle);
uint64_t stone_native_find_shader(const char* name);
uint64_t stone_native_find_texture(const char* path);
uint8_t stone_native_frame_active(void);
uint64_t stone_native_frame_hook_register(uint32_t point, int order, const void* run, const void* release, void* userdata, const char* name);
uint8_t stone_native_frame_hook_unregister(uint64_t hook);
uint32_t stone_native_frame_hooks_run(uint32_t point, uint64_t scene_target, uint64_t scene_partner, float offset_x, float offset_y, float scale);
uint8_t stone_native_frame_set_world_image(uint64_t target, uint32_t flags);
uint8_t stone_native_frame_size(int* width, int* height);
uint64_t stone_native_frame_target(void);
const void* stone_native_gl_get_proc_address(const char* name);
uint64_t stone_native_handle_generation(int kind, uint64_t handle);
int stone_native_handle_state(const char* class, uint64_t handle);
uint8_t stone_native_handle_valid(int kind, uint64_t handle);
uint64_t stone_native_mesh_info(uint64_t handle, int* vert_count, int* attr_count, int* inst_count);
const void* stone_native_renderer_api(uint32_t abi_version);
uint64_t stone_native_shader_info(uint64_t handle, int* block_count, int* texture_slot_count);
uint32_t stone_native_stage_begin(void);
uint32_t stone_native_stage_end(uint32_t scope);
uint64_t stone_native_target_info(uint64_t handle, int* width, int* height, int* format, int* color_attachments, bool* has_depth);
uint64_t stone_native_texture_info(uint64_t handle, int* width, int* height, int* format, bool* ready);
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
uint8_t stone_save_screenshot(const char* path, int w, int h, int max_size);
uint8_t stone_save_screenshot_sized(const char* path, int w, int h, int max_size, int* out_width, int* out_height);
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
void stone_set_shader(uint64_t handle);
uint64_t stone_set_system_cursor(int cursor_type);
void stone_set_viewport_rect(int x, int y, int w, int h);
uint8_t stone_shader_bind_uniform_block(uint64_t handle, const char* shader_key, const char* block_name, int binding, int size, uint64_t fingerprint);
uint8_t stone_shader_check_uniform_block_layout(uint64_t handle, const char* block_name, const char* names, int names_len, const int* offsets, int count, int plan_size);
uint8_t stone_shader_read_uniform_block(uint64_t handle, const char* block_name, uint8_t* dst, int size);
uint8_t stone_shader_register_block_member(uint64_t handle, int binding, const char* name, int offset, int kind, int capacity, int stride);
uint8_t stone_shader_register_name(uint64_t handle, const char* name);
void stone_shader_set_texture(uint64_t h, const char* n, int unit);
uint8_t stone_shader_snapshot_uniform_block(uint64_t handle, const char* block_name, uint8_t* dst, int size);
int stone_shader_texture_slot_count(uint64_t handle);
int stone_shader_texture_slot_find(uint64_t handle, const char* name);
int stone_shader_texture_slot_info(uint64_t handle, int index, int* unit_out, uint8_t* name_out, int name_cap);
int stone_shader_uniform_block_count(uint64_t handle);
int stone_shader_uniform_block_find_by_binding(uint64_t handle, int binding);
int stone_shader_uniform_block_find_by_name(uint64_t handle, const char* name);
uint64_t stone_shader_uniform_block_fingerprint(uint64_t handle, int index);
uint8_t stone_shader_uniform_block_info(uint64_t handle, int index, int* binding_out, int* size_out, int* member_count_out, uint8_t* name_out, int name_cap);
uint8_t stone_shader_uniform_block_member_info(uint64_t handle, int block_index, int member_index, int* offset_out, int* kind_out, int* capacity_out, int* stride_out);
int stone_shader_uniform_block_member_name(uint64_t handle, int block_index, int member_index, uint8_t* name_out, int name_cap);
int stone_shader_uniform_block_member_offset(uint64_t handle, int block_index, const char* member_name);
uint8_t stone_shader_upload_uniform_block(uint64_t handle, const uint8_t* data, int size, uint64_t fingerprint);
void stone_show_cursor(bool show);
uint8_t stone_sprite_init(void);
uint64_t stone_target_attachment(uint64_t handle, int slot);
uint64_t stone_target_color_view(uint64_t source);
uint64_t stone_target_create(int width, int height, const int* attachment_formats, int attachment_count, bool with_depth);
void stone_target_destroy(uint64_t handle);
uint32_t stone_target_gl_depth_texture(uint64_t handle);
uint32_t stone_target_gl_framebuffer(uint64_t handle);
uint32_t stone_target_gl_texture(uint64_t handle, int slot);
uint8_t stone_target_resize(uint64_t handle, int width, int height);
int stone_test_virtual_pad_attach(void);
uint8_t stone_test_virtual_pad_axis(int index, const char* token, int value);
uint8_t stone_test_virtual_pad_button(int index, const char* token, bool down);
uint8_t stone_test_virtual_pad_detach(int index);
uint8_t stone_text_init(void);
void stone_track_text_draw_call(void);
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
int stone_unfilled_uniform_block_count(void);
