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
StoneWindow* stone_create_window_ex(const char* title, int x, int y, int width, int height, int display, bool resizable, int fullscreen);
StoneWindow* stone_gpu_host_acquire(void);
unsigned long long stone_gl_swap_count(void);
uint8_t stone_poll_event(StoneEvent* event);
void stone_start_text_input(void);
void stone_stop_text_input(void);

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

double stone_get_time(void);
double stone_wall_time(void);

void stone_reset_image_profile_stats(void);
void stone_reset_image_profile_stats_to_frame(void);
uint64_t stone_get_live_texture_bytes(void);
uint64_t stone_get_live_image_bytes(void);
uint64_t stone_get_live_mip_bytes(void);
void stone_reset_frame_stats(void);

// Bind a texture to a specific texture unit
uint8_t stone_bind_image_texture_to_unit(StoneImageHandle handle, int unit);

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

// Generated by stone-rs/build.rs.
void stone_get_graphics_stats(GraphicsStats* stats);
void stone_get_image_frame_stats(ImageLoadStats* out);
void stone_get_image_profile_stats(ImageLoadStats* out);
