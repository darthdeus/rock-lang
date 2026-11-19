use std::borrow::Cow;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

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

pub fn context_request(host: GlHost) -> GlContextRequest {
    let (major, minor) = match host {
        GlHost::AppleDesktop => (4, 1),
        GlHost::Desktop => (3, 3),
    };
    GlContextRequest {
        profile: GlProfile::Core,
        major,
        minor,
    }
}

pub static GL_ERROR_COUNT: AtomicU64 = AtomicU64::new(0);

// A failed compile or link sets no GL error, so GL_ERROR_COUNT alone would stay 0.
pub static SHADER_ERROR_COUNT: AtomicU64 = AtomicU64::new(0);

fn force_shader_failure() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("STONE_SHADER_FORCE_FAIL").as_deref() == Ok("1"))
}

#[cfg(debug_assertions)]
fn panic_on_gl_error() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("STONE_GL_PANIC").as_deref() == Ok("1"))
}

pub fn debug_groups_supported(major: u32, minor: u32, has_khr_debug: bool) -> bool {
    has_khr_debug || (major, minor) >= (4, 3)
}

/// Wrapper around glow::Context that checks required buffer storage allocations
/// in every build and other GL calls in debug builds.
///
/// # Safety
///
/// Unsafe calls require the native context to be alive and current on this thread,
/// with the called operation supported. Object handles and uniform locations must
/// belong to this context (or its share group where GL permits) and be valid for the operation.
/// Bindings, formats and buffer ranges must satisfy the selected GL operation;
/// error checks do not validate host-memory access or extend resource lifetimes.
pub struct DebugGl {
    gl: glow::Context,
}

pub type DrawCaptureHook = unsafe fn(&DebugGl, &str, &[i64]);

thread_local! {
    // Native capture state belongs to the rendering thread, as do its GL contexts.
    static DRAW_CAPTURE: std::cell::Cell<Option<DrawCaptureHook>> = const { std::cell::Cell::new(None) };
}

impl DebugGl {
    pub fn new(gl: glow::Context) -> Self {
        Self { gl }
    }

    pub fn supports_bc1(&self) -> bool {
        let extensions = self.gl.supported_extensions();
        extensions.contains("GL_EXT_texture_compression_s3tc")
            || extensions.contains("GL_EXT_texture_compression_dxt1")
    }

    /// # Safety
    /// Requires the active texture binding, a valid extent and complete blocks of the selected format.
    pub unsafe fn upload_compressed(&self, internal: u32, width: i32, height: i32, blocks: &[u8]) {
        self.gl.compressed_tex_image_2d(
            glow::TEXTURE_2D,
            0,
            internal as i32,
            width,
            height,
            0,
            blocks.len() as i32,
            blocks,
        );
        self.check_error("compressed_tex_image_2d");
    }

    pub fn max_texture_anisotropy(&self) -> Option<f32> {
        let version = self.gl.version();
        let extensions = self.gl.supported_extensions();
        let supported = (!version.is_embedded && (version.major, version.minor) >= (4, 6))
            || extensions.contains("GL_ARB_texture_filter_anisotropic")
            || extensions.contains("GL_EXT_texture_filter_anisotropic");
        if !supported {
            return None;
        }
        let limit = unsafe { self.gl.get_parameter_f32(glow::MAX_TEXTURE_MAX_ANISOTROPY) };
        self.check_error("max_texture_anisotropy");
        Some(limit)
    }

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    pub unsafe fn get_sampler_parameter_i32(&self, sampler: glow::Sampler, name: u32) -> i32 {
        self.gl.get_sampler_parameter_i32(sampler, name)
    }

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    pub unsafe fn get_framebuffer_attachment_parameter_i32(
        &self,
        target: u32,
        attachment: u32,
        name: u32,
    ) -> i32 {
        self.gl
            .get_framebuffer_attachment_parameter_i32(target, attachment, name)
    }

    pub fn set_capture(&self, hook: Option<DrawCaptureHook>) {
        DRAW_CAPTURE.with(|capture| capture.set(hook));
    }

    unsafe fn capture(&self, operation: &str, args: &[i64]) {
        if let Some(hook) = DRAW_CAPTURE.with(std::cell::Cell::get) {
            hook(self, operation, args);
        }
    }

    pub fn supports_timer_query(&self) -> bool {
        // Apple's desktop GL driver exposes timer queries but returns
        // unreliable values, and the GL stack is deprecated there: a wrong
        // number is worse than `n/a`.
        !cfg!(target_os = "macos")
    }

    pub fn supports_debug_groups(&self) -> bool {
        let v = self.gl.version();
        debug_groups_supported(
            v.major,
            v.minor,
            self.gl.supported_extensions().contains("GL_KHR_debug"),
        )
    }

    pub fn describe(&self) -> String {
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

    #[inline]
    fn checked_error(&self, operation: &str) -> Result<(), String> {
        let err = unsafe { self.gl.get_error() };
        if err == glow::NO_ERROR {
            return Ok(());
        }
        GL_ERROR_COUNT.fetch_add(1, Ordering::Relaxed);
        let error = format!("gl error after {operation}: 0x{err:X}");
        log::error!(target: "GL", "{error}");
        Err(error)
    }
}

macro_rules! checked_gl_methods {
    ($(fn $name:ident($($arg:ident: $ty:ty),* $(,)?) $(-> $ret:ty)?;)*) => {
        $(
            /// # Safety
            /// Requires the [OpenGL call contract](Self#safety).
            pub unsafe fn $name(&self, $($arg: $ty),*) $(-> $ret)? {
                let result = unsafe { self.gl.$name($($arg),*) };
                self.check_error(stringify!($name));
                result
            }
        )*
    };
}

impl DebugGl {
    checked_gl_methods! {
        fn clear_color(r: f32, g: f32, b: f32, a: f32);
        fn viewport(x: i32, y: i32, width: i32, height: i32);
        fn use_program(program: Option<glow::Program>);
        fn bind_vertex_array(vao: Option<glow::VertexArray>);
        fn bind_buffer(target: u32, buffer: Option<glow::Buffer>);
        fn bind_texture(target: u32, texture: Option<glow::Texture>);
        fn bind_framebuffer(target: u32, framebuffer: Option<glow::Framebuffer>);
        fn bind_renderbuffer(target: u32, renderbuffer: Option<glow::Renderbuffer>);
        fn active_texture(texture: u32);
        fn vertex_attrib_divisor(index: u32, divisor: u32);
        fn enable(cap: u32);
        fn disable(cap: u32);
        fn is_enabled(cap: u32) -> bool;
        fn scissor(x: i32, y: i32, width: i32, height: i32);
        fn blend_func(sfactor: u32, dfactor: u32);
        fn blend_func_separate(src_rgb: u32, dst_rgb: u32, src_alpha: u32, dst_alpha: u32);
        fn blend_equation(mode: u32);
        fn blend_equation_separate(rgb: u32, alpha: u32);
        fn depth_func(func: u32);
        fn depth_mask(flag: bool);
        fn cull_face(mode: u32);
        fn front_face(mode: u32);
        fn enable_vertex_attrib_array(index: u32);
        fn disable_vertex_attrib_array(index: u32);
        fn clear_depth_f32(depth: f32);
        fn color_mask(red: bool, green: bool, blue: bool, alpha: bool);

        fn get_active_uniforms(program: glow::Program) -> u32;
        fn get_active_uniform(program: glow::Program, index: u32) -> Option<glow::ActiveUniform>;
        fn get_uniform_block_index(program: glow::Program, name: &str) -> Option<u32>;
        fn uniform_block_binding(program: glow::Program, index: u32, binding: u32);
        fn get_active_uniform_block_parameter_i32(
            program: glow::Program,
            index: u32,
            pname: u32,
        ) -> i32;
        fn get_uniform_indices(program: glow::Program, names: &[&str]) -> Vec<Option<u32>>;
        fn get_active_uniforms_parameter(
            program: glow::Program,
            uniforms: &[u32],
            pname: u32,
        ) -> Vec<i32>;
        fn get_uniform_location(
            program: glow::Program,
            name: &str,
        ) -> Option<glow::UniformLocation>;
        fn uniform_1_i32(location: Option<&glow::UniformLocation>, x: i32);
        fn uniform_1_i32_slice(location: Option<&glow::UniformLocation>, data: &[i32]);
        fn uniform_2_i32_slice(location: Option<&glow::UniformLocation>, data: &[i32]);
        fn uniform_3_i32_slice(location: Option<&glow::UniformLocation>, data: &[i32]);
        fn uniform_4_i32_slice(location: Option<&glow::UniformLocation>, data: &[i32]);
        fn uniform_1_f32(location: Option<&glow::UniformLocation>, x: f32);
        fn uniform_2_f32(location: Option<&glow::UniformLocation>, x: f32, y: f32);
        fn uniform_3_f32(location: Option<&glow::UniformLocation>, x: f32, y: f32, z: f32);
        fn uniform_4_f32(location: Option<&glow::UniformLocation>, x: f32, y: f32, z: f32, w: f32);
        fn uniform_1_f32_slice(location: Option<&glow::UniformLocation>, data: &[f32]);
        fn uniform_2_f32_slice(location: Option<&glow::UniformLocation>, data: &[f32]);
        fn uniform_3_f32_slice(location: Option<&glow::UniformLocation>, data: &[f32]);
        fn uniform_4_f32_slice(location: Option<&glow::UniformLocation>, data: &[f32]);
        fn uniform_matrix_2_f32_slice(
            location: Option<&glow::UniformLocation>,
            transpose: bool,
            data: &[f32],
        );
        fn uniform_matrix_3_f32_slice(
            location: Option<&glow::UniformLocation>,
            transpose: bool,
            data: &[f32],
        );
        fn uniform_matrix_4_f32_slice(
            location: Option<&glow::UniformLocation>,
            transpose: bool,
            data: &[f32],
        );

        fn bind_buffer_base(target: u32, index: u32, buffer: Option<glow::Buffer>);
        fn bind_buffer_range(
            target: u32,
            index: u32,
            buffer: Option<glow::Buffer>,
            offset: i32,
            size: i32,
        );
        fn transform_feedback_varyings(program: glow::Program, varyings: &[&str], buffer_mode: u32);
        fn begin_transform_feedback(primitive_mode: u32);
        fn end_transform_feedback();
        fn get_buffer_parameter_i32(target: u32, parameter: u32) -> i32;
        fn fence_sync(condition: u32, flags: u32) -> Result<glow::Fence, String>;
        fn client_wait_sync(fence: glow::Fence, flags: u32, timeout: i32) -> u32;
        fn delete_sync(fence: glow::Fence);

        fn get_parameter_string(parameter: u32) -> String;
        fn get_parameter_indexed_i32(parameter: u32, index: u32) -> i32;
        fn get_tex_parameter_i32(target: u32, parameter: u32) -> i32;
        fn get_tex_level_parameter_i32(target: u32, level: i32, parameter: u32) -> i32;

        fn delete_texture(texture: glow::Texture);
        fn delete_framebuffer(framebuffer: glow::Framebuffer);
        fn delete_renderbuffer(renderbuffer: glow::Renderbuffer);
        fn delete_vertex_array(vao: glow::VertexArray);
        fn delete_buffer(buffer: glow::Buffer);
        fn delete_shader(shader: glow::Shader);
        fn delete_program(program: glow::Program);
        fn create_sampler() -> Result<glow::Sampler, String>;
        fn sampler_parameter_i32(sampler: glow::Sampler, parameter: u32, value: i32);
        fn sampler_parameter_f32(sampler: glow::Sampler, parameter: u32, value: f32);
        fn bind_sampler(unit: u32, sampler: Option<glow::Sampler>);
        fn tex_parameter_i32(target: u32, parameter: u32, value: i32);
        fn framebuffer_texture_2d(
            target: u32,
            attachment: u32,
            textarget: u32,
            texture: Option<glow::Texture>,
            level: i32,
        );
        fn framebuffer_renderbuffer(
            target: u32,
            attachment: u32,
            renderbuffertarget: u32,
            renderbuffer: Option<glow::Renderbuffer>,
        );
        fn renderbuffer_storage(target: u32, internal_format: u32, width: i32, height: i32);
        fn renderbuffer_storage_multisample(
            target: u32,
            samples: i32,
            internal_format: u32,
            width: i32,
            height: i32,
        );
        fn check_framebuffer_status(target: u32) -> u32;
        fn pixel_store_i32(storage: u32, value: i32);
        fn finish();
        fn flush();
        fn generate_mipmap(target: u32);
        fn draw_buffers(buffers: &[u32]);

        fn shader_source(shader: glow::Shader, source: &str);
        fn compile_shader(shader: glow::Shader);
        fn get_shader_compile_status(shader: glow::Shader) -> bool;
        fn get_shader_info_log(shader: glow::Shader) -> String;
        fn attach_shader(program: glow::Program, shader: glow::Shader);
        fn link_program(program: glow::Program);
        fn get_program_link_status(program: glow::Program) -> bool;
        fn get_program_info_log(program: glow::Program) -> String;

        fn create_query() -> Result<glow::Query, String>;
        fn delete_query(query: glow::Query);
        fn begin_query(target: u32, query: glow::Query);
        fn end_query(target: u32);
        fn get_query_parameter_u32(query: glow::Query, parameter: u32) -> u32;
        fn get_query_parameter_u64(query: glow::Query, parameter: u32) -> u64;
        fn query_counter(query: glow::Query, target: u32);
        fn pop_debug_group();
    }

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    pub unsafe fn clear(&self, mask: u32) {
        self.capture("clear", &[mask as i64]);
        self.gl.clear(mask);
        self.check_error("clear");
    }

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    /// `values` must contain every component read for the selected buffer.
    // The only defined way to clear an integer colour buffer (glClear on one is undefined).
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

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    /// Enabled vertex attributes must cover the requested vertex range.
    pub unsafe fn draw_arrays(&self, mode: u32, first: i32, count: i32) {
        self.capture("draw_arrays", &[mode as i64, first as i64, count as i64]);
        self.gl.draw_arrays(mode, first, count);
        self.check_error("draw_arrays");
    }

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    /// Enabled vertex and instance attributes must cover the requested ranges.
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

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    /// `parameter` must select a single boolean result.
    pub unsafe fn get_parameter_bool(&self, parameter: u32) -> bool {
        let v = self.gl.get_parameter_bool(parameter);
        self.check_error("get_parameter_bool");
        v
    }

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    /// `N` must cover every component written by `parameter`.
    pub unsafe fn get_parameter_bool_array<const N: usize>(&self, parameter: u32) -> [bool; N] {
        let v = self.gl.get_parameter_bool_array::<N>(parameter);
        self.check_error("get_parameter_bool_array");
        v
    }

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    /// `out` must cover every component written by `parameter`.
    pub unsafe fn get_parameter_f32_slice(&self, parameter: u32, out: &mut [f32]) {
        self.gl.get_parameter_f32_slice(parameter, out);
        self.check_error("get_parameter_f32_slice");
    }

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    /// The bound index buffer must cover the offset/count/type, and all referenced vertices must
    /// be valid.
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

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    pub unsafe fn get_error(&self) -> u32 {
        self.gl.get_error()
    }

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    pub unsafe fn tex_buffer(&self, target: u32, internal_format: u32, buffer: glow::Buffer) {
        self.gl.tex_buffer(target, internal_format, Some(buffer));
        self.check_error("tex_buffer");
    }

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    /// The output slice must cover every component of the selected uniform.
    pub unsafe fn get_uniform_f32(
        &self,
        program: glow::Program,
        location: &glow::UniformLocation,
        out: &mut [f32],
    ) {
        self.gl.get_uniform_f32(program, location, out);
        self.check_error("get_uniform_f32");
    }

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    /// `parameter` must select a single integer result.
    pub unsafe fn get_parameter_i32(&self, parameter: u32) -> i32 {
        let result = self.gl.get_parameter_i32(parameter);
        self.check_error("get_parameter_i32");
        result
    }

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    /// `out` must cover every component written by `parameter`.
    pub unsafe fn get_parameter_i32_slice(&self, parameter: u32, out: &mut [i32]) {
        self.gl.get_parameter_i32_slice(parameter, out);
        self.check_error("get_parameter_i32_slice");
    }

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    /// The output slice must cover every component of the selected uniform.
    pub unsafe fn get_uniform_i32(
        &self,
        program: glow::Program,
        location: &glow::UniformLocation,
        out: &mut [i32],
    ) {
        self.gl.get_uniform_i32(program, location, out);
        self.check_error("get_uniform_i32");
    }

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    /// A vertex buffer must be bound; offset, stride and format must describe valid storage for
    /// subsequent draws.
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

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    pub unsafe fn buffer_data_u8_slice(
        &self,
        target: u32,
        data: &[u8],
        usage: u32,
    ) -> Result<(), String> {
        self.gl.buffer_data_u8_slice(target, data, usage);
        self.checked_error("buffer_data_u8_slice")
    }

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    pub unsafe fn buffer_data_size(
        &self,
        target: u32,
        size: i32,
        usage: u32,
    ) -> Result<(), String> {
        self.gl.buffer_data_size(target, size, usage);
        self.checked_error("buffer_data_size")
    }

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    /// The range and access flags must be valid for the bound buffer. Check the returned pointer
    /// before use; obey its access permissions, synchronization requirements and mapping
    /// lifetime.
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

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    /// The bound buffer must be mapped; no access through its mapped pointer may follow this
    /// call.
    pub unsafe fn unmap_buffer(&self, target: u32) {
        self.gl.unmap_buffer(target);
        self.check_error("unmap_buffer");
    }

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    pub unsafe fn buffer_sub_data_u8_slice(
        &self,
        target: u32,
        offset: i32,
        data: &[u8],
    ) -> Result<(), String> {
        self.gl.buffer_sub_data_u8_slice(target, offset, data);
        self.checked_error("buffer_sub_data_u8_slice")
    }

    // Desktop GL only: glGetBufferSubData does not exist in GLES.
    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
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

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    /// Supplied pixel data must cover the transfer under the current unpack alignment, row length and skip
    /// state. Slice data requires no unpack buffer bound; a buffer offset requires a sufficiently
    /// sized unpack buffer.
    #[expect(
        clippy::too_many_arguments,
        reason = "Preserve the OpenGL operation signature"
    )]
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

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    /// Supplied pixel data must cover the transfer under the current unpack alignment, row length and skip
    /// state. Slice data requires no unpack buffer bound; a buffer offset requires a sufficiently
    /// sized unpack buffer.
    #[expect(
        clippy::too_many_arguments,
        reason = "Preserve the OpenGL operation signature"
    )]
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

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    #[expect(
        clippy::too_many_arguments,
        reason = "Preserve the OpenGL operation signature"
    )]
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

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    /// The destination must cover the transfer under the current pack alignment, row length and
    /// skip state. Slice data requires no pack buffer bound; a buffer offset requires a
    /// sufficiently sized pack buffer.
    #[expect(
        clippy::too_many_arguments,
        reason = "Preserve the OpenGL operation signature"
    )]
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

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    /// The destination must cover the full texture level under the current pack state. Slice data
    /// requires no pack buffer bound; a buffer offset requires a sufficiently sized pack buffer.
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

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    pub unsafe fn push_debug_group(&self, message: &str) {
        self.gl
            .push_debug_group(glow::DEBUG_SOURCE_APPLICATION, 0, message);
        self.check_error("push_debug_group");
    }

    /// # Safety
    /// Requires the [OpenGL call contract](Self#safety).
    pub unsafe fn object_label(&self, identifier: u32, name: u32, label: &str) {
        self.gl.object_label(identifier, name, Some(label));
        self.check_error("object_label");
    }
}

fn report_shader_failure(message: fmt::Arguments<'_>) {
    log::error!(target: "SHADER", "{message}");
    SHADER_ERROR_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Compile a single GL shader stage. Context-free helper: caller passes the
/// `DebugGl`.
pub fn compile_shader(gl: &DebugGl, shader_type: u32, source: &str) -> Option<glow::Shader> {
    unsafe {
        // Fault injection for the shader-failure gate: corrupting the source runs
        // the REAL compile-status failure path rather than a hand-faked one.
        let source: Cow<'_, str> = if force_shader_failure() {
            format!("{source}\n#error STONE_SHADER_FORCE_FAIL\n").into()
        } else {
            source.into()
        };
        let shader = gl.create_shader(shader_type).ok()?;
        gl.shader_source(shader, &source);
        gl.compile_shader(shader);

        if !gl.get_shader_compile_status(shader) {
            let log = gl.get_shader_info_log(shader);
            let type_name = if shader_type == glow::VERTEX_SHADER {
                "vertex"
            } else {
                "fragment"
            };
            report_shader_failure(format_args!(
                "{type_name} shader compilation failed:\n{log}"
            ));
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
            report_shader_failure(format_args!("shader program linking failed:\n{log}"));
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
    fn buffer_upload_reports_driver_failure() {
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
        unsafe extern "system" fn buffer_sub_data(_: u32, _: isize, size: isize, _: *const c_void) {
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
                "glBufferSubData" => buffer_sub_data as *const c_void,
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
            assert!(gl
                .buffer_sub_data_u8_slice(glow::ARRAY_BUFFER, 0, &[])
                .is_ok());
            let result = gl.buffer_sub_data_u8_slice(glow::ARRAY_BUFFER, 0, &[0; 16]);
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
