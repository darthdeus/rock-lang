use glow::HasContext;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GlHost {
    Desktop,
    AppleDesktop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GlProfile {
    Core,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GlContextRequest {
    pub profile: GlProfile,
    pub major: u8,
    pub minor: u8,
}

/// The single context version each host asks for. Nothing in the engine needs
/// more than GL 3.3 core; Apple's driver tops out at 4.1.
pub fn context_request(host: GlHost) -> GlContextRequest {
    match host {
        GlHost::AppleDesktop => GlContextRequest {
            profile: GlProfile::Core,
            major: 4,
            minor: 1,
        },
        GlHost::Desktop => GlContextRequest {
            profile: GlProfile::Core,
            major: 3,
            minor: 3,
        },
    }
}

// =============================================================================
// Debug GL Wrapper
// =============================================================================

/// Total GL errors seen by DebugGl since process start. Buffer storage
/// allocations are checked in every build; other calls only in debug builds.
/// Exposed over FFI as `stone_gl_error_count` so sims/tests can assert
/// a run stayed GL-clean instead of relying on someone reading the log.
pub static GL_ERROR_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Shader compile + program link failures since process start. A failed compile
/// sets no GL error, so `GL_ERROR_COUNT` stays 0 and the frame loop draws on
/// without the program -- the log line was the only signal, which is how
/// `just test-games` reported `ok` over a shader that never compiled. Exposed
/// as `stone_shader_error_count`; bounded runs exit nonzero on it.
pub static SHADER_ERROR_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Seeded from `STONE_SHADER_FORCE_FAIL` on first read and settable after that
/// (`stone_shader_force_fail`). A process-wide `OnceLock` read could not prove
/// per-ENTRY attribution: both entries of a differential pair would see the same
/// value, so no arm could diverge.
static FORCE_SHADER_FAIL: std::sync::atomic::AtomicI8 = std::sync::atomic::AtomicI8::new(-1);

fn force_shader_failure() -> bool {
    let current = FORCE_SHADER_FAIL.load(std::sync::atomic::Ordering::Relaxed);
    if current >= 0 {
        return current == 1;
    }
    let from_env = i8::from(std::env::var("STONE_SHADER_FORCE_FAIL").as_deref() == Ok("1"));
    FORCE_SHADER_FAIL.store(from_env, std::sync::atomic::Ordering::Relaxed);
    from_env == 1
}

/// Turn the injection above on or off from inside a running entry.
pub fn set_force_shader_failure(on: bool) {
    FORCE_SHADER_FAIL.store(i8::from(on), std::sync::atomic::Ordering::Relaxed);
}

#[cfg(debug_assertions)]
fn panic_on_gl_error() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("STONE_GL_PANIC").as_deref() == Ok("1"))
}

/// Whether a context offers `KHR_debug` groups and object labels: core in
/// GL 4.3, otherwise only through the extension. Apple's desktop GL stops at
/// 4.1 without it.
pub fn debug_groups_supported(major: u32, minor: u32, has_khr_debug: bool) -> bool {
    has_khr_debug || (major, minor) >= (4, 3)
}

/// Wrapper around glow::Context that checks required buffer storage allocations
/// in every build and other GL calls in debug builds.
pub struct DebugGl {
    gl: glow::Context,
}

thread_local! {
    // Native capture state belongs to the rendering thread, as do its GL contexts.
    static DRAW_CAPTURE: std::cell::Cell<Option<unsafe fn(&DebugGl, &str, &[i64])>> = const { std::cell::Cell::new(None) };
}

impl DebugGl {
    pub fn new(gl: glow::Context) -> Self {
        Self { gl }
    }

    pub unsafe fn get_sampler_parameter_i32(&self, sampler: glow::Sampler, name: u32) -> i32 {
        self.gl.get_sampler_parameter_i32(sampler, name)
    }

    pub unsafe fn get_framebuffer_attachment_parameter_i32(
        &self,
        target: u32,
        attachment: u32,
        name: u32,
    ) -> i32 {
        self.gl
            .get_framebuffer_attachment_parameter_i32(target, attachment, name)
    }

    pub fn set_capture(&self, hook: Option<unsafe fn(&DebugGl, &str, &[i64])>) {
        DRAW_CAPTURE.with(|capture| capture.set(hook));
    }

    unsafe fn capture(&self, operation: &str, args: &[i64]) {
        if let Some(hook) = DRAW_CAPTURE.with(std::cell::Cell::get) {
            hook(self, operation, args);
        }
    }

    /// Whether `GL_TIME_ELAPSED` timer queries yield a number worth reporting.
    /// The single answer to "can we time the GPU here" -- callers must not add
    /// their own platform gate on top, or the capability and the log describing
    /// it drift apart. Core in GL 3.3, the oldest context Stone requests.
    pub fn supports_timer_query(&self) -> bool {
        // Apple's desktop GL driver exposes timer queries but returns
        // unreliable values, and the GL stack is deprecated there: a wrong
        // number is worse than `n/a`.
        !cfg!(target_os = "macos")
    }

    /// Whether `glPushDebugGroup`/`glObjectLabel` are available, so capture
    /// tools see named passes. See [`debug_groups_supported`].
    pub fn supports_debug_groups(&self) -> bool {
        let v = self.gl.version();
        debug_groups_supported(
            v.major,
            v.minor,
            self.gl.supported_extensions().contains("GL_KHR_debug"),
        )
    }

    /// One-line description of the live context: version, renderer, vendor and
    /// the capabilities that silently change rendering/measurement behaviour.
    /// Logged once at init -- on a device you cannot attach a debugger to, this
    /// is the only record of what the driver actually offered.
    pub fn describe(&self) -> String {
        use glow::HasContext as _;
        let v = self.gl.version();
        unsafe {
            format!(
                "GL {}.{} | renderer={:?} vendor={:?} | timer_query={} debug_groups={}",
                v.major,
                v.minor,
                self.gl.get_parameter_string(glow::RENDERER),
                self.gl.get_parameter_string(glow::VENDOR),
                self.supports_timer_query(),
                self.supports_debug_groups(),
            )
        }
    }

    #[inline]
    fn check_error(
        &self,
        #[cfg_attr(not(debug_assertions), allow(unused_variables))] operation: &str,
    ) {
        #[cfg(debug_assertions)]
        if let Err(error) = self.checked_error(operation) {
            if panic_on_gl_error() {
                panic!("{error}");
            }
        }
    }

    // Storage allocation is required in release builds too. Consume and report
    // the error once, before a caller can publish or draw from invalid storage.
    #[inline]
    fn checked_error(&self, operation: &str) -> Result<(), String> {
        let err = unsafe { self.gl.get_error() };
        if err == glow::NO_ERROR {
            return Ok(());
        }
        GL_ERROR_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let error = format!("gl error after {operation}: 0x{err:X}");
        log::error!(target: "GL", "{error}");
        Err(error)
    }
}

// Implement GL methods with automatic error checking
impl DebugGl {
    pub unsafe fn clear_color(&self, r: f32, g: f32, b: f32, a: f32) {
        self.gl.clear_color(r, g, b, a);
        self.check_error("clear_color");
    }

    pub unsafe fn clear(&self, mask: u32) {
        self.capture("clear", &[mask as i64]);
        self.gl.clear(mask);
        self.check_error("clear");
    }

    /// glClearBufferiv -- the only defined way to clear an integer color
    /// buffer (glClear on one is undefined per spec).
    pub unsafe fn clear_buffer_i32_slice(&self, target: u32, draw_buffer: u32, values: &[i32]) {
        self.capture(
            "clear_buffer_i32_slice",
            &[
                target as i64,
                draw_buffer as i64,
                values.len() as i64,
                values.first().copied().unwrap_or(0) as i64,
                values.get(1).copied().unwrap_or(0) as i64,
                values.get(2).copied().unwrap_or(0) as i64,
                values.get(3).copied().unwrap_or(0) as i64,
            ],
        );
        self.gl.clear_buffer_i32_slice(target, draw_buffer, values);
        self.check_error("clear_buffer_i32_slice");
    }

    pub unsafe fn viewport(&self, x: i32, y: i32, width: i32, height: i32) {
        self.gl.viewport(x, y, width, height);
        self.check_error("viewport");
    }

    pub unsafe fn use_program(&self, program: Option<glow::Program>) {
        self.gl.use_program(program);
        self.check_error("use_program");
    }

    pub unsafe fn bind_vertex_array(&self, vao: Option<glow::VertexArray>) {
        self.gl.bind_vertex_array(vao);
        self.check_error("bind_vertex_array");
    }

    pub unsafe fn bind_buffer(&self, target: u32, buffer: Option<glow::Buffer>) {
        self.gl.bind_buffer(target, buffer);
        self.check_error("bind_buffer");
    }

    pub unsafe fn bind_texture(&self, target: u32, texture: Option<glow::Texture>) {
        self.gl.bind_texture(target, texture);
        self.check_error("bind_texture");
    }

    pub unsafe fn bind_framebuffer(&self, target: u32, framebuffer: Option<glow::Framebuffer>) {
        self.gl.bind_framebuffer(target, framebuffer);
        self.check_error("bind_framebuffer");
    }

    pub unsafe fn bind_renderbuffer(&self, target: u32, renderbuffer: Option<glow::Renderbuffer>) {
        self.gl.bind_renderbuffer(target, renderbuffer);
        self.check_error("bind_renderbuffer");
    }

    pub unsafe fn active_texture(&self, texture: u32) {
        self.gl.active_texture(texture);
        self.check_error("active_texture");
    }

    pub unsafe fn draw_arrays(&self, mode: u32, first: i32, count: i32) {
        self.capture("draw_arrays", &[mode as i64, first as i64, count as i64]);
        self.gl.draw_arrays(mode, first, count);
        self.check_error("draw_arrays");
    }

    pub unsafe fn draw_arrays_instanced(
        &self,
        mode: u32,
        first: i32,
        count: i32,
        instance_count: i32,
    ) {
        self.capture(
            "draw_arrays_instanced",
            &[
                mode as i64,
                first as i64,
                count as i64,
                instance_count as i64,
            ],
        );
        self.gl
            .draw_arrays_instanced(mode, first, count, instance_count);
        self.check_error("draw_arrays_instanced");
    }

    pub unsafe fn vertex_attrib_divisor(&self, index: u32, divisor: u32) {
        self.gl.vertex_attrib_divisor(index, divisor);
        self.check_error("vertex_attrib_divisor");
    }

    pub unsafe fn enable(&self, cap: u32) {
        self.gl.enable(cap);
        self.check_error("enable");
    }

    pub unsafe fn scissor(&self, x: i32, y: i32, width: i32, height: i32) {
        self.gl.scissor(x, y, width, height);
        self.check_error("scissor");
    }

    pub unsafe fn disable(&self, cap: u32) {
        self.gl.disable(cap);
        self.check_error("disable");
    }

    pub unsafe fn blend_func(&self, sfactor: u32, dfactor: u32) {
        self.gl.blend_func(sfactor, dfactor);
        self.check_error("blend_func");
    }

    pub unsafe fn blend_func_separate(
        &self,
        src_rgb: u32,
        dst_rgb: u32,
        src_alpha: u32,
        dst_alpha: u32,
    ) {
        self.gl
            .blend_func_separate(src_rgb, dst_rgb, src_alpha, dst_alpha);
        self.check_error("blend_func_separate");
    }

    pub unsafe fn blend_equation(&self, mode: u32) {
        self.gl.blend_equation(mode);
        self.check_error("blend_equation");
    }

    pub unsafe fn blend_equation_separate(&self, rgb: u32, alpha: u32) {
        self.gl.blend_equation_separate(rgb, alpha);
        self.check_error("blend_equation_separate");
    }

    pub unsafe fn depth_func(&self, func: u32) {
        self.gl.depth_func(func);
        self.check_error("depth_func");
    }

    pub unsafe fn depth_mask(&self, flag: bool) {
        self.gl.depth_mask(flag);
        self.check_error("depth_mask");
    }

    /// Read back the states a `glClear` is gated by, so a caller that has to
    /// open them can put them back. Used by canvas creation, which clears the
    /// new target and may run mid-render.
    pub unsafe fn is_enabled(&self, cap: u32) -> bool {
        let v = self.gl.is_enabled(cap);
        self.check_error("is_enabled");
        v
    }

    pub unsafe fn get_parameter_bool(&self, parameter: u32) -> bool {
        let v = self.gl.get_parameter_bool(parameter);
        self.check_error("get_parameter_bool");
        v
    }

    pub unsafe fn get_parameter_bool_array<const N: usize>(&self, parameter: u32) -> [bool; N] {
        let v = self.gl.get_parameter_bool_array::<N>(parameter);
        self.check_error("get_parameter_bool_array");
        v
    }

    pub unsafe fn get_parameter_f32_slice(&self, parameter: u32, out: &mut [f32]) {
        self.gl.get_parameter_f32_slice(parameter, out);
        self.check_error("get_parameter_f32_slice");
    }

    pub unsafe fn cull_face(&self, mode: u32) {
        self.gl.cull_face(mode);
        self.check_error("cull_face");
    }

    pub unsafe fn front_face(&self, mode: u32) {
        self.gl.front_face(mode);
        self.check_error("front_face");
    }

    pub unsafe fn draw_elements(&self, mode: u32, count: i32, element_type: u32, offset: i32) {
        self.capture(
            "draw_elements",
            &[
                mode as i64,
                count as i64,
                element_type as i64,
                offset as i64,
            ],
        );
        self.gl.draw_elements(mode, count, element_type, offset);
        self.check_error("draw_elements");
    }

    pub unsafe fn uniform_matrix_3_f32_slice(
        &self,
        location: Option<&glow::UniformLocation>,
        transpose: bool,
        data: &[f32],
    ) {
        self.gl
            .uniform_matrix_3_f32_slice(location, transpose, data);
        self.check_error("uniform_matrix_3_f32_slice");
    }

    #[allow(dead_code)]
    pub unsafe fn get_error(&self) -> u32 {
        self.gl.get_error()
    }

    pub unsafe fn get_active_uniforms(&self, program: glow::Program) -> u32 {
        let result = self.gl.get_active_uniforms(program);
        self.check_error("get_active_uniforms");
        result
    }

    pub unsafe fn get_active_uniform(
        &self,
        program: glow::Program,
        index: u32,
    ) -> Option<glow::ActiveUniform> {
        let result = self.gl.get_active_uniform(program, index);
        self.check_error("get_active_uniform");
        result
    }

    pub unsafe fn get_uniform_block_index(
        &self,
        program: glow::Program,
        name: &str,
    ) -> Option<u32> {
        let result = self.gl.get_uniform_block_index(program, name);
        self.check_error("get_uniform_block_index");
        result
    }

    pub unsafe fn uniform_block_binding(&self, program: glow::Program, index: u32, binding: u32) {
        self.gl.uniform_block_binding(program, index, binding);
        self.check_error("uniform_block_binding");
    }

    pub unsafe fn get_active_uniform_block_parameter_i32(
        &self,
        program: glow::Program,
        index: u32,
        pname: u32,
    ) -> i32 {
        let result = self
            .gl
            .get_active_uniform_block_parameter_i32(program, index, pname);
        self.check_error("get_active_uniform_block_parameter_i32");
        result
    }

    pub unsafe fn get_uniform_indices(
        &self,
        program: glow::Program,
        names: &[&str],
    ) -> Vec<Option<u32>> {
        let result = self.gl.get_uniform_indices(program, names);
        self.check_error("get_uniform_indices");
        result
    }

    pub unsafe fn get_active_uniforms_parameter(
        &self,
        program: glow::Program,
        uniforms: &[u32],
        pname: u32,
    ) -> Vec<i32> {
        let result = self
            .gl
            .get_active_uniforms_parameter(program, uniforms, pname);
        self.check_error("get_active_uniforms_parameter");
        result
    }

    pub unsafe fn bind_buffer_base(&self, target: u32, index: u32, buffer: Option<glow::Buffer>) {
        self.gl.bind_buffer_base(target, index, buffer);
        self.check_error("bind_buffer_base");
    }

    pub unsafe fn bind_buffer_range(
        &self,
        target: u32,
        index: u32,
        buffer: Option<glow::Buffer>,
        offset: i32,
        size: i32,
    ) {
        self.gl
            .bind_buffer_range(target, index, buffer, offset, size);
        self.check_error("bind_buffer_range");
    }

    pub unsafe fn tex_buffer(&self, target: u32, internal_format: u32, buffer: glow::Buffer) {
        self.gl.tex_buffer(target, internal_format, Some(buffer));
        self.check_error("tex_buffer");
    }

    pub unsafe fn transform_feedback_varyings(
        &self,
        program: glow::Program,
        varyings: &[&str],
        buffer_mode: u32,
    ) {
        self.gl
            .transform_feedback_varyings(program, varyings, buffer_mode);
        self.check_error("transform_feedback_varyings");
    }

    pub unsafe fn begin_transform_feedback(&self, primitive_mode: u32) {
        self.gl.begin_transform_feedback(primitive_mode);
        self.check_error("begin_transform_feedback");
    }

    pub unsafe fn end_transform_feedback(&self) {
        self.gl.end_transform_feedback();
        self.check_error("end_transform_feedback");
    }

    pub unsafe fn get_parameter_string(&self, parameter: u32) -> String {
        let result = self.gl.get_parameter_string(parameter);
        self.check_error("get_parameter_string");
        result
    }

    pub unsafe fn get_uniform_location(
        &self,
        program: glow::Program,
        name: &str,
    ) -> Option<glow::UniformLocation> {
        let result = self.gl.get_uniform_location(program, name);
        self.check_error("get_uniform_location");
        result
    }

    pub unsafe fn get_uniform_f32(
        &self,
        program: glow::Program,
        location: &glow::UniformLocation,
        out: &mut [f32],
    ) {
        self.gl.get_uniform_f32(program, location, out);
        self.check_error("get_uniform_f32");
    }

    #[allow(dead_code)]
    pub unsafe fn get_parameter_i32(&self, parameter: u32) -> i32 {
        let result = self.gl.get_parameter_i32(parameter);
        self.check_error("get_parameter_i32");
        result
    }

    pub unsafe fn get_buffer_parameter_i32(&self, target: u32, parameter: u32) -> i32 {
        let result = self.gl.get_buffer_parameter_i32(target, parameter);
        self.check_error("get_buffer_parameter_i32");
        result
    }

    pub unsafe fn get_parameter_indexed_i32(&self, parameter: u32, index: u32) -> i32 {
        let result = self.gl.get_parameter_indexed_i32(parameter, index);
        self.check_error("get_parameter_indexed_i32");
        result
    }

    #[allow(dead_code)]
    pub unsafe fn get_tex_parameter_i32(&self, target: u32, parameter: u32) -> i32 {
        let result = self.gl.get_tex_parameter_i32(target, parameter);
        self.check_error("get_tex_parameter_i32");
        result
    }

    #[allow(dead_code)]
    pub unsafe fn get_tex_level_parameter_i32(
        &self,
        target: u32,
        level: i32,
        parameter: u32,
    ) -> i32 {
        let result = self
            .gl
            .get_tex_level_parameter_i32(target, level, parameter);
        self.check_error("get_tex_level_parameter_i32");
        result
    }

    #[allow(dead_code)]
    pub unsafe fn get_parameter_i32_slice(&self, parameter: u32, out: &mut [i32]) {
        self.gl.get_parameter_i32_slice(parameter, out);
        self.check_error("get_parameter_i32_slice");
    }

    pub unsafe fn uniform_1_i32(&self, location: Option<&glow::UniformLocation>, x: i32) {
        self.gl.uniform_1_i32(location, x);
        self.check_error("uniform_1_i32");
    }

    pub unsafe fn uniform_1_f32(&self, location: Option<&glow::UniformLocation>, x: f32) {
        self.gl.uniform_1_f32(location, x);
        self.check_error("uniform_1_f32");
    }

    pub unsafe fn uniform_2_f32(&self, location: Option<&glow::UniformLocation>, x: f32, y: f32) {
        self.gl.uniform_2_f32(location, x, y);
        self.check_error("uniform_2_f32");
    }

    pub unsafe fn uniform_3_f32_slice(
        &self,
        location: Option<&glow::UniformLocation>,
        data: &[f32],
    ) {
        self.gl.uniform_3_f32_slice(location, data);
        self.check_error("uniform_3_f32_slice");
    }

    pub unsafe fn uniform_3_f32(
        &self,
        location: Option<&glow::UniformLocation>,
        x: f32,
        y: f32,
        z: f32,
    ) {
        self.gl.uniform_3_f32(location, x, y, z);
        self.check_error("uniform_3_f32");
    }

    pub unsafe fn uniform_4_f32(
        &self,
        location: Option<&glow::UniformLocation>,
        x: f32,
        y: f32,
        z: f32,
        w: f32,
    ) {
        self.gl.uniform_4_f32(location, x, y, z, w);
        self.check_error("uniform_4_f32");
    }

    pub unsafe fn get_uniform_i32(
        &self,
        program: glow::Program,
        location: &glow::UniformLocation,
        out: &mut [i32],
    ) {
        self.gl.get_uniform_i32(program, location, out);
        self.check_error("get_uniform_i32");
    }

    pub unsafe fn uniform_1_i32_slice(
        &self,
        location: Option<&glow::UniformLocation>,
        data: &[i32],
    ) {
        self.gl.uniform_1_i32_slice(location, data);
        self.check_error("uniform_1_i32_slice");
    }

    pub unsafe fn uniform_2_i32_slice(
        &self,
        location: Option<&glow::UniformLocation>,
        data: &[i32],
    ) {
        self.gl.uniform_2_i32_slice(location, data);
        self.check_error("uniform_2_i32_slice");
    }

    pub unsafe fn uniform_3_i32_slice(
        &self,
        location: Option<&glow::UniformLocation>,
        data: &[i32],
    ) {
        self.gl.uniform_3_i32_slice(location, data);
        self.check_error("uniform_3_i32_slice");
    }

    pub unsafe fn uniform_4_i32_slice(
        &self,
        location: Option<&glow::UniformLocation>,
        data: &[i32],
    ) {
        self.gl.uniform_4_i32_slice(location, data);
        self.check_error("uniform_4_i32_slice");
    }

    pub unsafe fn uniform_matrix_2_f32_slice(
        &self,
        location: Option<&glow::UniformLocation>,
        transpose: bool,
        data: &[f32],
    ) {
        self.gl
            .uniform_matrix_2_f32_slice(location, transpose, data);
        self.check_error("uniform_matrix_2_f32_slice");
    }

    pub unsafe fn uniform_1_f32_slice(
        &self,
        location: Option<&glow::UniformLocation>,
        data: &[f32],
    ) {
        self.gl.uniform_1_f32_slice(location, data);
        self.check_error("uniform_1_f32_slice");
    }

    pub unsafe fn uniform_2_f32_slice(
        &self,
        location: Option<&glow::UniformLocation>,
        data: &[f32],
    ) {
        self.gl.uniform_2_f32_slice(location, data);
        self.check_error("uniform_2_f32_slice");
    }

    pub unsafe fn uniform_4_f32_slice(
        &self,
        location: Option<&glow::UniformLocation>,
        data: &[f32],
    ) {
        self.gl.uniform_4_f32_slice(location, data);
        self.check_error("uniform_4_f32_slice");
    }

    pub unsafe fn uniform_matrix_4_f32_slice(
        &self,
        location: Option<&glow::UniformLocation>,
        transpose: bool,
        data: &[f32],
    ) {
        self.gl
            .uniform_matrix_4_f32_slice(location, transpose, data);
        self.check_error("uniform_matrix_4_f32_slice");
    }

    pub unsafe fn vertex_attrib_pointer_f32(
        &self,
        index: u32,
        size: i32,
        data_type: u32,
        normalized: bool,
        stride: i32,
        offset: i32,
    ) {
        self.gl
            .vertex_attrib_pointer_f32(index, size, data_type, normalized, stride, offset);
        self.check_error("vertex_attrib_pointer_f32");
    }

    pub unsafe fn enable_vertex_attrib_array(&self, index: u32) {
        self.gl.enable_vertex_attrib_array(index);
        self.check_error("enable_vertex_attrib_array");
    }

    pub unsafe fn disable_vertex_attrib_array(&self, index: u32) {
        self.gl.disable_vertex_attrib_array(index);
        self.check_error("disable_vertex_attrib_array");
    }

    pub unsafe fn buffer_data_u8_slice(
        &self,
        target: u32,
        data: &[u8],
        usage: u32,
    ) -> Result<(), String> {
        self.gl.buffer_data_u8_slice(target, data, usage);
        self.checked_error("buffer_data_u8_slice")
    }

    pub unsafe fn buffer_data_size(
        &self,
        target: u32,
        size: i32,
        usage: u32,
    ) -> Result<(), String> {
        self.gl.buffer_data_size(target, size, usage);
        self.checked_error("buffer_data_size")
    }

    pub unsafe fn map_buffer_range(
        &self,
        target: u32,
        offset: i32,
        length: i32,
        access: u32,
    ) -> *mut u8 {
        let ptr = self.gl.map_buffer_range(target, offset, length, access);
        self.check_error("map_buffer_range");
        ptr
    }

    pub unsafe fn fence_sync(&self, condition: u32, flags: u32) -> Result<glow::Fence, String> {
        let result = self.gl.fence_sync(condition, flags);
        self.check_error("fence_sync");
        result
    }

    pub unsafe fn client_wait_sync(&self, fence: glow::Fence, flags: u32, timeout: i32) -> u32 {
        let result = self.gl.client_wait_sync(fence, flags, timeout);
        self.check_error("client_wait_sync");
        result
    }

    pub unsafe fn delete_sync(&self, fence: glow::Fence) {
        self.gl.delete_sync(fence);
        self.check_error("delete_sync");
    }

    pub unsafe fn unmap_buffer(&self, target: u32) {
        self.gl.unmap_buffer(target);
        self.check_error("unmap_buffer");
    }

    pub unsafe fn buffer_sub_data_u8_slice(&self, target: u32, offset: i32, data: &[u8]) {
        self.gl.buffer_sub_data_u8_slice(target, offset, data);
        self.check_error("buffer_sub_data_u8_slice");
    }

    /// Desktop GL only: `glGetBufferSubData` does not exist in GLES, which is
    /// why nothing on the render path uses it.
    pub unsafe fn get_buffer_sub_data(&self, target: u32, offset: i32, dst: &mut [u8]) {
        self.gl.get_buffer_sub_data(target, offset, dst);
        self.check_error("get_buffer_sub_data");
    }

    pub fn create_texture(&self) -> Result<glow::Texture, String> {
        let result = unsafe { self.gl.create_texture() };
        self.check_error("create_texture");
        result
    }

    pub fn create_framebuffer(&self) -> Result<glow::Framebuffer, String> {
        let result = unsafe { self.gl.create_framebuffer() };
        self.check_error("create_framebuffer");
        result
    }

    pub fn create_renderbuffer(&self) -> Result<glow::Renderbuffer, String> {
        let result = unsafe { self.gl.create_renderbuffer() };
        self.check_error("create_renderbuffer");
        result
    }

    pub fn create_vertex_array(&self) -> Result<glow::VertexArray, String> {
        let result = unsafe { self.gl.create_vertex_array() };
        self.check_error("create_vertex_array");
        result
    }

    pub fn create_buffer(&self) -> Result<glow::Buffer, String> {
        let result = unsafe { self.gl.create_buffer() };
        self.check_error("create_buffer");
        result
    }

    pub fn create_shader(&self, shader_type: u32) -> Result<glow::Shader, String> {
        let result = unsafe { self.gl.create_shader(shader_type) };
        self.check_error("create_shader");
        result
    }

    pub fn create_program(&self) -> Result<glow::Program, String> {
        let result = unsafe { self.gl.create_program() };
        self.check_error("create_program");
        result
    }

    pub unsafe fn delete_texture(&self, texture: glow::Texture) {
        self.gl.delete_texture(texture);
        self.check_error("delete_texture");
    }

    pub unsafe fn delete_framebuffer(&self, framebuffer: glow::Framebuffer) {
        self.gl.delete_framebuffer(framebuffer);
        self.check_error("delete_framebuffer");
    }

    pub unsafe fn delete_renderbuffer(&self, renderbuffer: glow::Renderbuffer) {
        self.gl.delete_renderbuffer(renderbuffer);
        self.check_error("delete_renderbuffer");
    }

    pub unsafe fn delete_vertex_array(&self, vao: glow::VertexArray) {
        self.gl.delete_vertex_array(vao);
        self.check_error("delete_vertex_array");
    }

    pub unsafe fn delete_buffer(&self, buffer: glow::Buffer) {
        self.gl.delete_buffer(buffer);
        self.check_error("delete_buffer");
    }

    pub unsafe fn delete_shader(&self, shader: glow::Shader) {
        self.gl.delete_shader(shader);
        self.check_error("delete_shader");
    }

    pub unsafe fn delete_program(&self, program: glow::Program) {
        self.gl.delete_program(program);
        self.check_error("delete_program");
    }

    pub unsafe fn tex_parameter_i32(&self, target: u32, parameter: u32, value: i32) {
        self.gl.tex_parameter_i32(target, parameter, value);
        self.check_error("tex_parameter_i32");
    }

    pub unsafe fn create_sampler(&self) -> Result<glow::Sampler, String> {
        let r = self.gl.create_sampler();
        self.check_error("create_sampler");
        r
    }

    pub unsafe fn sampler_parameter_i32(&self, sampler: glow::Sampler, parameter: u32, value: i32) {
        self.gl.sampler_parameter_i32(sampler, parameter, value);
        self.check_error("sampler_parameter_i32");
    }

    pub unsafe fn sampler_parameter_f32(&self, sampler: glow::Sampler, parameter: u32, value: f32) {
        self.gl.sampler_parameter_f32(sampler, parameter, value);
        self.check_error("sampler_parameter_f32");
    }

    pub unsafe fn bind_sampler(&self, unit: u32, sampler: Option<glow::Sampler>) {
        self.gl.bind_sampler(unit, sampler);
        self.check_error("bind_sampler");
    }

    #[allow(dead_code)]
    pub unsafe fn tex_image_2d(
        &self,
        target: u32,
        level: i32,
        internal_format: i32,
        width: i32,
        height: i32,
        border: i32,
        format: u32,
        ty: u32,
        pixels: glow::PixelUnpackData,
    ) {
        self.gl.tex_image_2d(
            target,
            level,
            internal_format,
            width,
            height,
            border,
            format,
            ty,
            pixels,
        );
        self.check_error("tex_image_2d");
    }

    #[allow(dead_code)]
    pub unsafe fn tex_sub_image_2d(
        &self,
        target: u32,
        level: i32,
        x_offset: i32,
        y_offset: i32,
        width: i32,
        height: i32,
        format: u32,
        ty: u32,
        pixels: glow::PixelUnpackData,
    ) {
        self.gl.tex_sub_image_2d(
            target, level, x_offset, y_offset, width, height, format, ty, pixels,
        );
        self.check_error("tex_sub_image_2d");
    }

    pub unsafe fn framebuffer_texture_2d(
        &self,
        target: u32,
        attachment: u32,
        textarget: u32,
        texture: Option<glow::Texture>,
        level: i32,
    ) {
        self.gl
            .framebuffer_texture_2d(target, attachment, textarget, texture, level);
        self.check_error("framebuffer_texture_2d");
    }

    pub unsafe fn framebuffer_renderbuffer(
        &self,
        target: u32,
        attachment: u32,
        renderbuffertarget: u32,
        renderbuffer: Option<glow::Renderbuffer>,
    ) {
        self.gl
            .framebuffer_renderbuffer(target, attachment, renderbuffertarget, renderbuffer);
        self.check_error("framebuffer_renderbuffer");
    }

    pub unsafe fn renderbuffer_storage(
        &self,
        target: u32,
        internal_format: u32,
        width: i32,
        height: i32,
    ) {
        self.gl
            .renderbuffer_storage(target, internal_format, width, height);
        self.check_error("renderbuffer_storage");
    }

    pub unsafe fn renderbuffer_storage_multisample(
        &self,
        target: u32,
        samples: i32,
        internal_format: u32,
        width: i32,
        height: i32,
    ) {
        self.gl
            .renderbuffer_storage_multisample(target, samples, internal_format, width, height);
        self.check_error("renderbuffer_storage_multisample");
    }

    pub unsafe fn blit_framebuffer(
        &self,
        src_x0: i32,
        src_y0: i32,
        src_x1: i32,
        src_y1: i32,
        dst_x0: i32,
        dst_y0: i32,
        dst_x1: i32,
        dst_y1: i32,
        mask: u32,
        filter: u32,
    ) {
        self.capture(
            "blit_framebuffer",
            &[
                src_x0 as i64,
                src_y0 as i64,
                src_x1 as i64,
                src_y1 as i64,
                dst_x0 as i64,
                dst_y0 as i64,
                dst_x1 as i64,
                dst_y1 as i64,
                mask as i64,
                filter as i64,
            ],
        );
        self.gl.blit_framebuffer(
            src_x0, src_y0, src_x1, src_y1, dst_x0, dst_y0, dst_x1, dst_y1, mask, filter,
        );
        self.check_error("blit_framebuffer");
    }

    pub unsafe fn check_framebuffer_status(&self, target: u32) -> u32 {
        let result = self.gl.check_framebuffer_status(target);
        self.check_error("check_framebuffer_status");
        result
    }

    pub unsafe fn shader_source(&self, shader: glow::Shader, source: &str) {
        self.gl.shader_source(shader, source);
        self.check_error("shader_source");
    }

    pub unsafe fn compile_shader(&self, shader: glow::Shader) {
        self.gl.compile_shader(shader);
        self.check_error("compile_shader");
    }

    pub unsafe fn get_shader_compile_status(&self, shader: glow::Shader) -> bool {
        let result = self.gl.get_shader_compile_status(shader);
        self.check_error("get_shader_compile_status");
        result
    }

    pub unsafe fn get_shader_info_log(&self, shader: glow::Shader) -> String {
        let result = self.gl.get_shader_info_log(shader);
        self.check_error("get_shader_info_log");
        result
    }

    pub unsafe fn attach_shader(&self, program: glow::Program, shader: glow::Shader) {
        self.gl.attach_shader(program, shader);
        self.check_error("attach_shader");
    }

    pub unsafe fn link_program(&self, program: glow::Program) {
        self.gl.link_program(program);
        self.check_error("link_program");
    }

    pub unsafe fn get_program_link_status(&self, program: glow::Program) -> bool {
        let result = self.gl.get_program_link_status(program);
        self.check_error("get_program_link_status");
        result
    }

    pub unsafe fn get_program_info_log(&self, program: glow::Program) -> String {
        let result = self.gl.get_program_info_log(program);
        self.check_error("get_program_info_log");
        result
    }

    pub unsafe fn pixel_store_i32(&self, storage: u32, value: i32) {
        self.gl.pixel_store_i32(storage, value);
        self.check_error("pixel_store_i32");
    }

    #[allow(dead_code)]
    pub unsafe fn read_pixels(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        format: u32,
        ty: u32,
        pixels: glow::PixelPackData,
    ) {
        self.gl.read_pixels(x, y, width, height, format, ty, pixels);
        self.check_error("read_pixels");
    }

    #[allow(dead_code)]
    pub unsafe fn get_tex_image(
        &self,
        target: u32,
        level: i32,
        format: u32,
        ty: u32,
        pixels: glow::PixelPackData,
    ) {
        self.gl.get_tex_image(target, level, format, ty, pixels);
        self.check_error("get_tex_image");
    }

    #[allow(dead_code)]
    pub unsafe fn finish(&self) {
        self.gl.finish();
        self.check_error("finish");
    }

    #[allow(dead_code)]
    pub unsafe fn flush(&self) {
        self.gl.flush();
        self.check_error("flush");
    }

    #[allow(dead_code)]
    pub unsafe fn generate_mipmap(&self, target: u32) {
        self.gl.generate_mipmap(target);
        self.check_error("generate_mipmap");
    }

    pub unsafe fn clear_depth_f32(&self, depth: f32) {
        self.gl.clear_depth_f32(depth);
        self.check_error("clear_depth_f32");
    }

    pub unsafe fn color_mask(&self, red: bool, green: bool, blue: bool, alpha: bool) {
        self.gl.color_mask(red, green, blue, alpha);
        self.check_error("color_mask");
    }

    // --- MRT (multiple render targets) --------------------------------------

    pub unsafe fn draw_buffers(&self, buffers: &[u32]) {
        self.gl.draw_buffers(buffers);
        self.check_error("draw_buffers");
    }

    // --- Query objects (GPU timer; GL_TIME_ELAPSED) -------------------------

    pub unsafe fn create_query(&self) -> Result<glow::Query, String> {
        let result = self.gl.create_query();
        self.check_error("create_query");
        result
    }

    pub unsafe fn delete_query(&self, query: glow::Query) {
        self.gl.delete_query(query);
        self.check_error("delete_query");
    }

    pub unsafe fn begin_query(&self, target: u32, query: glow::Query) {
        self.gl.begin_query(target, query);
        self.check_error("begin_query");
    }

    pub unsafe fn end_query(&self, target: u32) {
        self.gl.end_query(target);
        self.check_error("end_query");
    }

    pub unsafe fn get_query_parameter_u32(&self, query: glow::Query, parameter: u32) -> u32 {
        let result = self.gl.get_query_parameter_u32(query, parameter);
        self.check_error("get_query_parameter_u32");
        result
    }

    pub unsafe fn get_query_parameter_u64(&self, query: glow::Query, parameter: u32) -> u64 {
        let result = self.gl.get_query_parameter_u64(query, parameter);
        self.check_error("get_query_parameter_u64");
        result
    }

    pub unsafe fn query_counter(&self, query: glow::Query, target: u32) {
        self.gl.query_counter(query, target);
        self.check_error("query_counter");
    }

    // --- KHR_debug groups and labels (gated by supports_debug_groups) -------

    pub unsafe fn push_debug_group(&self, message: &str) {
        self.gl
            .push_debug_group(glow::DEBUG_SOURCE_APPLICATION, 0, message);
        self.check_error("push_debug_group");
    }

    pub unsafe fn pop_debug_group(&self) {
        self.gl.pop_debug_group();
        self.check_error("pop_debug_group");
    }

    pub unsafe fn object_label(&self, identifier: u32, name: u32, label: &str) {
        self.gl.object_label(identifier, name, Some(label));
        self.check_error("object_label");
    }

    pub unsafe fn get_object_label(&self, identifier: u32, name: u32) -> String {
        let label = self.gl.get_object_label(identifier, name);
        self.check_error("get_object_label");
        label
    }
}

/// Compile a single GL shader stage. Context-free helper: caller passes the
/// `DebugGl`.
pub fn compile_shader(gl: &DebugGl, shader_type: u32, source: &str) -> Option<glow::Shader> {
    unsafe {
        // Fault injection for the shader-failure gate: corrupting the source runs
        // the REAL compile-status failure path rather than a hand-faked one.
        let injected: std::borrow::Cow<str> = if force_shader_failure() {
            format!("{source}\n#error STONE_SHADER_FORCE_FAIL\n").into()
        } else {
            source.into()
        };
        let source = injected.as_ref();
        let shader = gl.create_shader(shader_type).ok()?;
        gl.shader_source(shader, source);
        gl.compile_shader(shader);

        if !gl.get_shader_compile_status(shader) {
            let log = gl.get_shader_info_log(shader);
            let type_name = if shader_type == glow::VERTEX_SHADER {
                "vertex"
            } else {
                "fragment"
            };
            ::log::error!(target: "SHADER", "{} shader compilation failed:\n{}", type_name, log);
            SHADER_ERROR_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            gl.delete_shader(shader);
            return None;
        }

        Some(shader)
    }
}

/// Link a vertex+fragment shader pair into a program. Context-free helper.
pub fn link_program(
    gl: &DebugGl,
    vertex_shader: glow::Shader,
    fragment_shader: glow::Shader,
) -> Option<glow::Program> {
    unsafe {
        let program = gl.create_program().ok()?;
        gl.attach_shader(program, vertex_shader);
        gl.attach_shader(program, fragment_shader);
        gl.link_program(program);

        if !gl.get_program_link_status(program) {
            let log = gl.get_program_info_log(program);
            ::log::error!(target: "SHADER", "shader program linking failed:\n{}", log);
            SHADER_ERROR_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            gl.delete_program(program);
            return None;
        }

        Some(program)
    }
}

#[cfg(test)]
mod tests {
    use super::debug_groups_supported;

    #[test]
    fn buffer_allocation_reports_driver_failure() {
        use std::{cell::Cell, ffi::c_void};
        thread_local! { static ERROR: Cell<u32> = const { Cell::new(glow::NO_ERROR) }; }
        unsafe extern "system" fn get_string(_: u32) -> *const u8 {
            c"3.3".as_ptr().cast()
        }
        unsafe extern "system" fn get_integer(_: u32, value: *mut i32) {
            *value = 0;
        }
        unsafe extern "system" fn get_error() -> u32 {
            ERROR.with(|error| error.replace(glow::NO_ERROR))
        }
        unsafe extern "system" fn buffer_data(_: u32, size: isize, _: *const c_void, _: u32) {
            if size > 0 {
                ERROR.with(|error| error.set(glow::OUT_OF_MEMORY));
            }
        }
        let gl = super::DebugGl::new(unsafe {
            glow::Context::from_loader_function(|name| match name {
                "glGetString" => get_string as *const c_void,
                "glGetIntegerv" => get_integer as *const c_void,
                "glGetError" => get_error as *const c_void,
                "glBufferData" => buffer_data as *const c_void,
                _ => std::ptr::null(),
            })
        });
        unsafe {
            assert!(gl
                .buffer_data_u8_slice(glow::ARRAY_BUFFER, &[], glow::STATIC_DRAW)
                .is_ok());
            assert!(gl
                .buffer_data_size(glow::ARRAY_BUFFER, 0, glow::STREAM_DRAW)
                .is_ok());
            let result = gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &[0; 16], glow::STATIC_DRAW);
            assert!(result.unwrap_err().contains("0x505"));
            let result = gl.buffer_data_size(glow::ARRAY_BUFFER, 16, glow::STREAM_DRAW);
            assert!(result.unwrap_err().contains("0x505"));
        }
    }

    #[test]
    fn debug_groups_follow_core_versions_and_the_extension() {
        assert!(debug_groups_supported(4, 3, false));
        assert!(debug_groups_supported(4, 6, false));
        assert!(!debug_groups_supported(4, 1, false));
        assert!(!debug_groups_supported(3, 3, false));
        assert!(debug_groups_supported(3, 3, true));
    }
}
