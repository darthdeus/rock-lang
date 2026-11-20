// Opt-in UI trace ABI. Payload pointers are copied before returning.
void stone_ui_probe_begin(bool windowless);
void stone_ui_probe_stop(void);
void stone_ui_probe_context(const char *phase, const char *pass, const char *surface, const char *space);
int stone_ui_probe_accept(const char *kind, const char *key, const char *detail);
uint8_t stone_ui_probe_expect_vertices(int id, const void *vertices, int count, int stride);
void stone_ui_probe_end_operation(void);
void stone_ui_probe_custom_consumed(void);
uint8_t stone_ui_probe_observe(const char *kind, const char *detail);
uint8_t stone_ui_probe_resource(const char *kind, uint64_t handle, const char *source, int generation);
uint8_t stone_ui_probe_check(int expected);
int stone_ui_probe_accepted(void);
uint8_t stone_ui_probe_refuse(const char *kind, const char *detail);
uint8_t stone_ui_probe_check_population(int accepted, int refused, int observed);
uint8_t stone_ui_probe_retire(const char *kind, uint64_t handle);
int stone_ui_probe_consumed(void);
int stone_ui_probe_z(int index);
double stone_ui_probe_vertex(int index, int lane);

typedef struct StoneWindow StoneWindow;
typedef struct StoneGLContext StoneGLContext;
typedef enum {
    STONE_EVENT_NONE = 0,
    STONE_EVENT_QUIT,
    STONE_EVENT_KEYDOWN,
    STONE_EVENT_KEYUP,
    STONE_EVENT_MOUSEBUTTONDOWN,
    STONE_EVENT_MOUSEBUTTONUP,
    STONE_EVENT_MOUSEWHEEL,
    STONE_EVENT_PAD_DEVICE_ADDED,
    STONE_EVENT_PAD_DEVICE_REMOVED,
    STONE_EVENT_PAD_ACTIVE_CHANGED,
    STONE_EVENT_PAD_BUTTONDOWN,
    STONE_EVENT_PAD_BUTTONUP,
    STONE_EVENT_PAD_AXISMOTION,
    STONE_EVENT_TEXTINPUT,
} StoneEventType;

typedef enum {
    STONE_KEY_ESCAPE = 27,
    STONE_KEY_SPACE = 32,
} StoneKeyCode;

typedef struct {
    StoneEventType type;
    StoneKeyCode key;
    int scancode;
    int modifiers;
    bool repeat;
    int mouse_button;
    int mouse_x;
    int mouse_y;
    int pad_instance_id;
    const char* pad_button;
    const char* pad_axis;
    int pad_value;
    bool pad_sync;
    char text[32];
} StoneEvent;

uint8_t stone_init(void);
void stone_quit(void);
void stone_hide_window(void);
void stone_show_window(void);
void stone_show_window_mode(int mode);
uint8_t stone_is_window_hidden(void);
uint8_t stone_quit_requested(void);
uint8_t stone_teardown(void);
uint8_t stone_is_initialized(void);
int stone_test_virtual_pad_attach(void);
uint8_t stone_test_virtual_pad_button(int index, const char* token, bool down);
uint8_t stone_test_virtual_pad_axis(int index, const char* token, int value);
uint8_t stone_test_virtual_pad_detach(int index);
uint8_t stone_pad_connected(void);
const char* stone_pad_type_name(void);
StoneWindow* stone_create_window_ex(const char* title, int x, int y, int width, int height, int display, bool resizable, int fullscreen);
StoneWindow* stone_gpu_host_acquire(void);
const char* stone_sdl_video_driver(void);
unsigned long long stone_gl_swap_count(void);
uint8_t stone_poll_event(StoneEvent* event);
void stone_start_text_input(void);
void stone_stop_text_input(void);

const char* stone_scancode_token_at(int index);
const char* stone_scancode_token_for(int scancode);
int stone_scancode_value(const char* token);
const char* stone_scancode_label(const char* token);

// Input query functions
uint8_t stone_is_key_down(const char* key);
void stone_get_mouse_position(int* x, int* y);
void stone_get_global_mouse_position(int* x, int* y);

// Cursor functions
void stone_show_cursor(bool show);
uint64_t stone_create_cursor(const char* path, int hot_x, int hot_y);
uint64_t stone_set_system_cursor(int cursor_type);
void stone_set_cursor(uint64_t cursor_id);
void stone_reset_cursor(void);
void stone_free_cursor(uint64_t cursor_id);

// Window functions
void stone_set_window_title(StoneWindow* window, const char* title);
void stone_get_window_size(int* width, int* height);
void stone_get_drawable_size(int* width, int* height);
float stone_get_dpi_scale(void);
float stone_get_config_imgui_scale(void);
void stone_get_window_position(int* x, int* y);
void stone_get_window_borders(int* top, int* left, int* bottom, int* right);
void stone_save_window_geometry(const char* title);
void stone_set_window_size(int width, int height);
void stone_set_window_display(int index);
void stone_set_display_mode(int width, int height);
uint8_t stone_take_window_resized(void);
uint8_t stone_take_window_moved(void);
uint8_t stone_take_window_display_changed(void);
int stone_get_display_count(void);
int stone_get_window_refresh_rate(void);

// Filesystem functions
const char* stone_get_working_directory(void);
const char* stone_get_executable_path(void);
const char* stone_get_pref_path(const char* organization, const char* application);
double stone_file_modtime(const char* path);
bool stone_file_write(const char* path, const char* data, size_t len);
bool stone_file_append(const char* path, const char* data, size_t len);
int stone_file_write_async(const char* path, const char* data, size_t len, bool append);
int stone_file_write_pair_async(const char* path, const char* data, size_t len, const char* second_path, const char* second_data, size_t second_len);
int stone_file_write_poll(int id, bool wait);
int stone_file_rename_async(const char* source, const char* target);
int stone_file_transaction_begin(void);
bool stone_file_transaction_write(int transaction, const char* path, const char* data, size_t len, bool append);
bool stone_file_transaction_publish(int transaction, const char* path, const char* data, size_t len, bool append);
uint8_t stone_file_transaction_write_png(int transaction, const char* path, const uint8_t* pixels, int width, int height);
uint8_t stone_file_transaction_cleanup(int transaction, const char* path, bool on_success);
uint8_t stone_file_transaction_cleanup_directory(int transaction, const char* directory);
uint8_t stone_file_transaction_keep(int transaction, const char* directory, const char* filename);
int stone_file_transaction_commit(int transaction);
uint8_t stone_file_transaction_abort(int transaction);
int stone_file_transaction_cancel(int job);
int stone_file_transaction_poll(int job, bool wait);
uint8_t stone_file_jobs_test_pause(void);
uint8_t stone_file_jobs_test_release(void);
uint8_t stone_file_jobs_test_entered(void);
uint8_t stone_file_jobs_test_pause_publication(void);
uint8_t stone_file_jobs_test_publication_entered(void);
uint8_t stone_remove_file(const char* path);
uint8_t stone_remove_dir(const char* path);
uint8_t stone_file_sync(const char* path);
char* stone_file_read(const char* path, size_t* out_len);
const char* stone_get_hostname(void);
int stone_file_exists_status(const char* path);
int stone_is_directory_status(const char* path);
int64_t stone_file_size(const char* path);
uint8_t stone_create_directory(const char* path);
uint8_t stone_copy_file(const char* src, const char* dst);
uint8_t stone_rename(const char* src, const char* dst);

bool stone_telemetry_configure(const char* root, const char* game, const char* build, const char* platform, const char* channel, const char* endpoint, const char* token, bool explicit_reconsent);
bool stone_telemetry_record(const char* name, int64_t timestamp, const char* data);
bool stone_telemetry_validate(const char* name, int64_t timestamp, const char* data);
void stone_telemetry_close(void);
bool stone_telemetry_clear(const char* root);
bool stone_telemetry_withdrawn(const char* root);

// Unified logging bridge (rock-log). __rock_log / log_once /
// log_throttled route here so the whole game process shares
// one logger/level/format.
void stone_log(int level, const char* category, const char* msg, int wall_clock);
int stone_log_get_level(void);
void stone_log_set_level(int level);

bool stone_list_directory(const char* path, char*** out_files, int* out_count);
void stone_free_string_array(char** arr, int count);

void stone_swap_window(StoneWindow* window);
void stone_show_window_if_deferred(void);
void stone_activate_window(StoneWindow* window);
void stone_gl_finish(void);
void stone_gl_flush(void);
uint8_t stone_vsync_active(void);
uint32_t stone_gl_error_count(void);
uint32_t stone_shader_error_count(void);
void stone_gpu_timer_begin(void);
void stone_gpu_timer_end(void);
float stone_gpu_timer_poll_ms(void);
void stone_gpu_timed_pass_begin(void);
void stone_gpu_timed_pass_end(uint32_t tag);
float stone_gpu_timed_pass_poll_ms(void);
uint32_t stone_gpu_timed_pass_tag(void);
uint8_t stone_gpu_scopes_requested(void);
uint8_t stone_gpu_scopes_finish(void);
void stone_gpu_scope_begin(const char* name);
void stone_gpu_pass_scope_begin(const char* shader_name);
void stone_gpu_scope_end(void);
void stone_gpu_label_shader(uint64_t handle, const char* name);
void stone_gpu_scopes_reset_stats(void);
const char* stone_gpu_scopes_json(void);
void stone_set_vsync(int enabled);
void stone_force_vsync_off(int force);
void stone_set_fullscreen(int mode);
int stone_get_fullscreen(void);
void stone_delay(uint32_t ms);

uint8_t stone_imgui_init(StoneWindow* window);
void stone_imgui_shutdown(void);
void stone_imgui_new_frame(void);
void stone_imgui_render(void);
void* stone_imgui_prepare_render(void);
int stone_imgui_last_vtx_count(void);
uint8_t stone_imgui_initialized(void);

typedef uint64_t StoneImageHandle;
typedef uint64_t StoneImageDataHandle;
StoneImageHandle stone_load_image(const char* path, int filter_mode, int flags, int width, int height);
StoneImageHandle stone_get_white_image(void);
void stone_get_image_size(StoneImageHandle handle, int* width, int* height);
void stone_image_storage_size(StoneImageHandle handle, int* width, int* height);
uint8_t stone_image_is_stub(StoneImageHandle handle);
uint8_t stone_probe_image_size(const char* path, int* width, int* height);
uint64_t stone_get_image_byte_len(StoneImageHandle handle);
bool stone_supports_bc1(void);
int stone_get_image_format(StoneImageHandle handle);
uint8_t stone_image_exists(StoneImageHandle handle);
uint32_t stone_get_image_texture(StoneImageHandle handle);
uint8_t stone_image_set_wrap(StoneImageHandle handle, bool repeat);
int stone_log_record_total(int level);
const char* stone_log_records_text(int level, int max_rows);
void stone_log_collect_errors(void);
const char* stone_log_collected_errors(size_t* len);
int stone_log_take_collected_error(const uint8_t* record, size_t len);
int64_t stone_image_read_texel(StoneImageHandle handle, int x, int y);
void stone_bind_sampler(int unit, int mode, StoneImageHandle image);
uint8_t stone_sampler_bound(int unit);
void stone_unload_image(StoneImageHandle handle);
uint8_t stone_reload_image(StoneImageHandle handle, const char* path);
void stone_flush_image_uploads(void);
void stone_set_frame_image_upload_budget(int bytes);
void stone_set_frame_image_upload_time_budget(int microseconds);
void stone_begin_image_upload_frame(void);
int stone_image_uploads_pending(void);
uint8_t stone_image_require(StoneImageHandle handle);
uint8_t stone_image_ready(StoneImageHandle handle);
int64_t stone_image_pending_bytes(void);
uint8_t stone_image_decode_failed(StoneImageHandle handle);
uint64_t stone_image_decode_failed_count(void);
int stone_service_image_uploads(void);

// Decode-only handoff: the selected renderer's image registry lives in
// a separate cdylib the native one cannot reach, so a selected load
// asks for RGBA8 bytes here and uploads them there.

// Asset hot reload: a single cross-platform filesystem watcher.
void stone_watch_asset(const char* path);
int stone_poll_changed_assets(void);
const char* stone_changed_asset_path(int index);

// CPU-side image data manipulation
StoneImageDataHandle stone_create_image_data(int width, int height);
uint8_t stone_image_data_size(StoneImageDataHandle handle, int* width, int* height);
int stone_get_image_data_count(void);
void stone_destroy_image_data(StoneImageDataHandle handle);
void stone_image_data_set_pixel(StoneImageDataHandle handle, int x, int y, float r, float g, float b, float a);
void stone_image_data_get_pixel(StoneImageDataHandle handle, int x, int y, float* r, float* g, float* b, float* a);
void stone_image_data_fill_rgba(StoneImageDataHandle handle, const uint8_t* rgba, int pixel_count);
bool stone_image_data_read_rgba(StoneImageDataHandle handle, uint8_t* rgba, int pixel_count);
StoneImageDataHandle stone_image_data_load_png(const char* path);
StoneImageDataHandle stone_image_data_load_png_bytes(const uint8_t* bytes, int len);
uint8_t stone_set_clipboard_text(const char* text);
const char* stone_system_report(void);
uint8_t stone_image_data_save_png(StoneImageDataHandle handle, const char* path);
uint8_t stone_image_update_from_data(StoneImageHandle image_handle, StoneImageDataHandle data_handle);
StoneImageHandle stone_new_image_from_data(StoneImageDataHandle data_handle, int filter_mode);

typedef uint64_t StoneSoundHandle;
typedef uint64_t StoneVoiceHandle;
uint8_t stone_audio_init(void);
void stone_audio_quit(void);
uint8_t stone_audio_stub_active(void);
uint8_t stone_audio_quiet_env(void);
StoneSoundHandle stone_load_sound_deferred(const char* path, int64_t frames, int64_t rate, int64_t channels);
StoneSoundHandle stone_load_sound_stream(const char* path, int64_t frames, int64_t rate, int64_t channels);
uint8_t stone_sound_admit(StoneSoundHandle handle);
int64_t stone_service_sound_admissions(void);
int64_t stone_sound_decode_sync(StoneSoundHandle handle);
uint8_t stone_sound_resident(StoneSoundHandle handle);
uint8_t stone_sound_playable(StoneSoundHandle handle);
int64_t stone_sound_stream_bytes(StoneSoundHandle handle);
uint8_t stone_unload_sound(StoneSoundHandle handle);
uint8_t stone_reload_sound(StoneSoundHandle handle, const char* path);
StoneSoundHandle stone_load_music(const char* path);
uint8_t stone_unload_music(StoneSoundHandle handle);
uint64_t stone_music_playing_handle(void);
int64_t stone_music_position_ms(void);
StoneVoiceHandle stone_play_sound(StoneSoundHandle handle, uint32_t bus, float volume, float speed, float decay, bool looped, bool spatial, float pan);
float stone_get_sound_duration(StoneSoundHandle handle);
int stone_get_sound_peaks(StoneSoundHandle handle, float* out, int buckets);
int64_t stone_sound_sample_count(StoneSoundHandle handle);
int64_t stone_music_byte_len(StoneSoundHandle handle);
void stone_stop_sound(StoneSoundHandle handle);
uint8_t stone_bus_set_gain(uint32_t bus, float gain);
uint64_t stone_bus_samples_pulled(uint32_t bus);
float stone_bus_peak(uint32_t bus);
uint8_t stone_bus_set_lowpass(uint32_t bus, float cutoff_hz, float q);
uint8_t stone_bus_set_distortion(uint32_t bus, float drive, float mix);
uint8_t stone_bus_set_reverb(uint32_t bus, bool enabled, float room_size, float damping, float wet, float dry, float width);
uint8_t stone_bus_duck(uint32_t bus, float depth, float attack_secs, float release_secs);
uint8_t stone_voice_active(StoneVoiceHandle voice);
uint8_t stone_voice_stop(StoneVoiceHandle voice);
uint8_t stone_voice_set_gain(StoneVoiceHandle voice, float gain);
uint8_t stone_voice_set_pan(StoneVoiceHandle voice, float pan);
uint8_t stone_voice_set_speed(StoneVoiceHandle voice, float speed);
void stone_play_music(StoneSoundHandle handle);
void stone_set_reverb(bool enabled, float room_size, float damping, float wet, float dry, float width);

// Input system. Mouse and window queries are declared once, in the
// "Input query functions" and "Window functions" groups above.
double stone_get_time(void);
double stone_wall_time(void);

// Sprite rendering system
uint8_t stone_sprite_init(void);
uint8_t stone_current_shader_bound(void);
float stone_current_target_width(void);
float stone_current_target_height(void);

// Draw a fullscreen quad in NDC coordinates for post-processing
void stone_draw_fullscreen_quad(void);
void stone_set_fullscreen_uv_scale(float sx, float sy);
// The semantic submission's geometry verb: one fullscreen rectangle
// with an EXPLICIT corner UV mapping, (u0,v0) at NDC (-1,-1) and
// (u1,v1) at NDC (+1,+1). Infers nothing from the bound target, and
// guarantees no culling.
void stone_draw_fullscreen_rect(float u0, float v0, float u1, float v1);

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
    const int* fan_ranges, int fan_count
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

StoneCanvasHandle stone_create_canvas(int width, int height, int format, bool with_depth);
void stone_destroy_canvas(StoneCanvasHandle handle);
uint8_t stone_canvas_bound_as_target(StoneCanvasHandle handle);
void stone_set_canvas(StoneCanvasHandle handle);
void stone_set_canvas_active(StoneCanvasHandle handle, int w, int h);
uint8_t stone_bind_canvas_texture_to_unit(StoneCanvasHandle handle, int unit);
uint8_t stone_bind_canvas_depth_to_unit(StoneCanvasHandle handle, int unit);
uint8_t stone_canvas_exists(StoneCanvasHandle handle);
uint8_t stone_canvas_has_depth(StoneCanvasHandle handle);
int stone_get_live_canvas_count(void);
uint64_t stone_get_live_canvas_handle(int index);
int stone_canvas_report(StoneCanvasHandle handle, int* width, int* height);
uint32_t stone_get_canvas_texture(StoneCanvasHandle handle);
void stone_set_viewport_rect(int x, int y, int w, int h);
// The submission's GpuRect: bottom-left, the bound target's own
// pixels, no density scaling and no Y flip.
void stone_set_gl_viewport(int x, int y, int w, int h);
uint8_t stone_canvas_attach_id_target(StoneCanvasHandle color_handle, StoneCanvasHandle id_handle);
uint8_t stone_canvas_attach_color_target(StoneCanvasHandle color_handle, StoneCanvasHandle target_handle, int slot);
void stone_clear_canvas(float r, float g, float b, float a);
void stone_clear_canvas_int(int draw_buffer, int value);
void stone_set_present_viewport(bool active, int x, int y, int w, int h);
uint8_t stone_canvas_save_png(StoneCanvasHandle handle, const char* path);
int  stone_canvas_read_rgba8(StoneCanvasHandle handle, uint8_t* out, int capacity);
uint64_t stone_canvas_capture_begin(uint64_t handle);
int stone_canvas_capture_poll(uint64_t id, uint8_t* out, int capacity);
uint8_t stone_canvas_capture_release(uint64_t id);
uint8_t stone_canvas_capture_cancel(uint64_t id);
uint8_t stone_canvas_upload_rgba32f(StoneCanvasHandle handle, const float* pixels, int count);
int  stone_canvas_read_region_rgba32f(StoneCanvasHandle handle, int x, int y, int w, int h, float* out, int capacity);
int stone_canvas_read_region_i32(StoneCanvasHandle handle, int x, int y, int w, int h, int32_t* out, int capacity);
uint8_t stone_canvas_upload_rgba8(StoneCanvasHandle handle, const uint8_t* pixels, int byte_len);
float stone_read_canvas_pixel(StoneCanvasHandle handle, int x, int y);
float stone_read_canvas_pixel_channel(StoneCanvasHandle handle, int x, int y, int channel);
int stone_read_canvas_nearest_id(StoneCanvasHandle handle, int x, int y, int side);
float stone_read_depth_pixel(StoneCanvasHandle handle, int x, int y);
void stone_read_window_rect(int x, int y, int w, int h, uint8_t* out);

// Async pixel readback via double-buffered PBOs. Each slot
// returns a value 1 frame stale. submit() issues glReadPixels into
// one PBO; consume() maps the other (filled last frame). Call submit
// when the canvas is quiescent (no pending writes) to get real async
// behavior — otherwise the driver serializes on pending writes and
// the submit stalls. read() is a convenience = consume + submit for
// callers whose canvas is quiescent at the single call site.
uint64_t stone_pixel_readback_new(int is_depth);
uint64_t stone_pixel_readback_area_new(int side);
void     stone_pixel_readback_destroy(uint64_t id);
void     stone_pixel_readback_submit(uint64_t id, StoneCanvasHandle canvas_handle, int x, int y);
void     stone_pixel_readback_submit_area(uint64_t id, StoneCanvasHandle canvas_handle, int x, int y, int side);
float    stone_pixel_readback_consume(uint64_t id);
int      stone_pixel_readback_poll_id(uint64_t id);
int      stone_pixel_readback_poll_rgba(uint64_t id, float* out, int capacity);
int      stone_pixel_readback_in_flight(uint64_t id);
int      stone_pixel_readback_answer_x(uint64_t id);
int      stone_pixel_readback_answer_y(uint64_t id);
uint8_t     stone_pixel_readback_reset_contents(uint64_t id);

// OpenGL state management
uint32_t stone_gl_pass_state_begin(void);
bool stone_gl_pass_state_end(uint32_t scope);
void stone_set_blend_mode_i(int mode);
void stone_set_depth_mode(const char* compare_mode, bool write);
void stone_clear_depth(float depth);
void stone_set_color_mask(bool r, bool g, bool b, bool a);
void stone_set_clear_color(float r, float g, float b, float a);
void stone_set_scissor(bool enabled, int x, int y, int w, int h);
uint8_t stone_gl_depth_write_enabled(void);
const char* stone_gl_blend_state(void);
const char* stone_gl_write_state(void);
uint8_t stone_canvas_active_is_creation(StoneCanvasHandle handle);
int stone_canvas_active_w(StoneCanvasHandle handle);
int stone_canvas_active_h(StoneCanvasHandle handle);
int stone_canvas_capacity_w(StoneCanvasHandle handle);
int stone_canvas_capacity_h(StoneCanvasHandle handle);
uint8_t stone_canvas_resize(StoneCanvasHandle handle, int width, int height);
void stone_set_draw_buffer_mask(int mask);
void stone_restore_after_custom_draw(void);

// Shape drawing (rectangle/circle/line retired -- immediate shapes
// build verts engine-side + submit via stone_draw_mesh; draw.rock)

// Screenshot system
uint8_t stone_save_screenshot(const char* filepath, int width, int height, int max_size);
uint8_t stone_save_screenshot_sized(const char* filepath, int width, int height, int max_size, int* out_width, int* out_height);
uint8_t stone_frame_stream_begin(const char* path);
uint8_t stone_frame_stream_record(int width, int height);
uint8_t stone_frame_stream_finish(int* out_frames, int* out_width, int* out_height);
const char* stone_frame_stream_last_error(void);

// Graphics statistics (GraphicsStats typedef auto-generated from the `value` struct)
void stone_get_graphics_stats(GraphicsStats* stats);
void stone_get_image_frame_stats(ImageLoadStats* stats);
void stone_get_image_profile_stats(ImageLoadStats* stats);
void stone_reset_image_profile_stats(void);
void stone_reset_image_profile_stats_to_frame(void);
uint64_t stone_get_live_texture_bytes(void);
uint64_t stone_get_live_image_bytes(void);
uint64_t stone_get_live_mip_bytes(void);
uint64_t stone_get_live_canvas_bytes(void);
void stone_reset_frame_stats(void);
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

// Bind a texture to a specific texture unit
uint8_t stone_bind_image_texture_to_unit(StoneImageHandle handle, int unit);

// Text rendering system. stone_load_font returns a 1-based font_id
// (0 = failure). All draw/measure/glyph functions take a font_id to
// pick which loaded font to operate on. Engine creates one default
// font at startup; games can load additional fonts via load_font().
uint8_t stone_text_init(void);
uint32_t stone_load_font(const char* path, int pixel_size, int face_mask, const char* extra_chars);
// Metrics-only load: same file, same charset, same CPU raster and the
// same font-unit metrics, but no GL texture. The only font load that
// succeeds without a window, which is what lets the `rock test <game>`
// luajit process measure and lay out text. Drawing with the id it
// returns is refused by name in stone-rs, never silently.
uint32_t stone_load_font_metrics(const char* path, int pixel_size, int face_mask, const char* extra_chars);
int stone_get_font_slot_count(uint32_t font_id);
int stone_get_font_slot_codepoint(uint32_t font_id, int slot);
bool stone_unload_font(uint32_t font_id);
bool stone_reload_font(uint32_t font_id, const char* path, int pixel_size);
bool stone_resize_font(uint32_t font_id, int pixel_size);
void stone_stream_text(uint32_t shader, uint32_t font_id, const char* text, float x, float y, float scale_x, float scale_y, float baseline, bool multiline);
float stone_get_text_baseline(uint32_t font_id, const char* text, float scale);
void stone_measure_text(uint32_t font_id, const char* text, float scale, float* out_width, float* out_height);

// Font atlas exposure for unified text batching
uint64_t stone_get_font_atlas_image(uint32_t font_id);
uint64_t stone_get_font_atlas_image_cached(uint32_t font_id);
uint8_t stone_get_font_atlas_size(uint32_t font_id, int* width, int* height);
int stone_get_font_pixel_size(uint32_t font_id);
int64_t stone_font_data_len(uint32_t font_id);
uint8_t stone_font_release_coverage(uint32_t font_id);
bool stone_get_glyph_info(uint32_t font_id, int face, int char_code, float* out);
uint64_t stone_ensure_font_atlas_instance(uint32_t font_id, int pixel_size);
uint64_t stone_get_font_instance_image(uint64_t instance_id);
int stone_get_font_atlas_instance_pixel_size(uint32_t font_id, uint32_t index);
uint64_t stone_get_font_atlas_instance_image_at(uint32_t font_id, int index);
uint8_t stone_get_font_instance_size(uint64_t instance_id, int* width, int* height);
int stone_get_font_instance_pixel_size(uint64_t instance_id);
int stone_font_atlas_instance_count(uint32_t font_id);
int stone_get_font_instance_byte_len(uint64_t instance_id);
uint8_t stone_get_font_instance_glyph_info(uint64_t instance_id, int face, int char_code, float* out);
uint8_t stone_get_font_unit_metrics(uint32_t font_id, float* out);
uint8_t stone_get_glyph_unit_metrics(uint32_t font_id, int face, int char_code, float* out);

void stone_free(void* ptr);

int stone_lua_jit_health_sample(void);
const char* stone_lua_jit_health_json(void);
int stone_lua_jit_health_frame_delta(float* out, size_t len);
double stone_lua_alloc_gross_request_kb(void);
double stone_lua_alloc_positive_growth_kb(void);
double stone_lua_alloc_explicit_free_kb(void);
double stone_lua_alloc_reallocation_shrink_kb(void);
uint64_t stone_lua_alloc_call_count(void);
uint64_t stone_lua_alloc_allocation_call_count(void);
uint64_t stone_lua_alloc_reallocation_call_count(void);
uint64_t stone_lua_alloc_free_call_count(void);
int stone_lua_alloc_counter_begin(void);
int stone_lua_alloc_counter_end(void);

const char* stone_native_live_handles(const char* class);
uint8_t stone_native_destroy(const char* class, uint64_t handle);
int stone_native_handle_state(const char* class, uint64_t handle);
const char* stone_native_id_accounting(void);
