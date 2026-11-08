use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{c_char, c_int, CStr};
use std::sync::Arc;

use crate::gl_state;
use crate::handles::Class;
use crate::{asset_registry, image_decode, log_once, with_gl};

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ImageLoadStats {
    pub load_calls: c_int,
    pub load_failures: c_int,
    pub decoded_bytes: c_int,
    pub load_us: c_int,
    pub upload_calls: c_int,
    pub upload_bytes: c_int,
    pub wait_us: c_int,
    pub upload_us: c_int,
    pub active_frames: c_int,
    pub max_wait_us: c_int,
}

thread_local! {
    static FRAME_IMAGE_LOAD_STATS: RefCell<ImageLoadStats> = RefCell::new(ImageLoadStats::default());
    static PROFILE_IMAGE_LOAD_STATS: RefCell<ImageLoadStats> = RefCell::new(ImageLoadStats::default());
}

pub(crate) fn decoded_upload_bytes(decoded: &image_decode::Decoded) -> usize {
    decoded.pixels.as_ref().map_or(0, Vec::len)
}

pub(crate) fn take_decoded_upload_budget(
    pending: &mut VecDeque<Arc<image_decode::Decoded>>,
    budget_bytes: usize,
    used_bytes: &mut usize,
) -> Vec<Arc<image_decode::Decoded>> {
    if budget_bytes == 0 || pending.is_empty() {
        return Vec::new();
    }
    let mut selected = Vec::new();
    while let Some(next) = pending.front() {
        let bytes = decoded_upload_bytes(next);
        if *used_bytes > 0 && used_bytes.saturating_add(bytes) > budget_bytes {
            break;
        }
        *used_bytes = used_bytes.saturating_add(bytes);
        selected.push(pending.pop_front().expect("front existed"));
    }
    selected
}

fn elapsed_us(start: std::time::Instant) -> c_int {
    c_int::try_from(start.elapsed().as_micros()).unwrap_or(c_int::MAX)
}

fn note_profile_activity(profile: &mut ImageLoadStats, frame_was_inactive: bool) {
    if frame_was_inactive {
        profile.active_frames = profile.active_frames.saturating_add(1);
    }
}

fn image_stats_active(stats: &ImageLoadStats) -> bool {
    stats.load_calls > 0
        || stats.load_failures > 0
        || stats.upload_calls > 0
        || stats.wait_us > 0
        || stats.upload_us > 0
}

fn record_image_load(success: bool, bytes: usize, us: c_int) {
    FRAME_IMAGE_LOAD_STATS.with(|frame| {
        let mut frame = frame.borrow_mut();
        let inactive = !image_stats_active(&frame);
        if success {
            frame.load_calls = frame.load_calls.saturating_add(1);
            frame.decoded_bytes = frame
                .decoded_bytes
                .saturating_add(c_int::try_from(bytes).unwrap_or(c_int::MAX));
        } else {
            frame.load_failures = frame.load_failures.saturating_add(1);
        }
        frame.load_us = frame.load_us.saturating_add(us);
        frame.active_frames = 1;

        PROFILE_IMAGE_LOAD_STATS.with(|profile| {
            let mut profile = profile.borrow_mut();
            note_profile_activity(&mut profile, inactive);
            if success {
                profile.load_calls = profile.load_calls.saturating_add(1);
                profile.decoded_bytes = profile
                    .decoded_bytes
                    .saturating_add(c_int::try_from(bytes).unwrap_or(c_int::MAX));
            } else {
                profile.load_failures = profile.load_failures.saturating_add(1);
            }
            profile.load_us = profile.load_us.saturating_add(us);
        });
    });
}

fn record_image_upload(calls: usize, bytes: usize, wait_us: c_int, upload_us: c_int) {
    if calls == 0 && bytes == 0 && wait_us == 0 && upload_us == 0 {
        return;
    }
    FRAME_IMAGE_LOAD_STATS.with(|frame| {
        let mut frame = frame.borrow_mut();
        let inactive = !image_stats_active(&frame);
        frame.upload_calls = frame
            .upload_calls
            .saturating_add(c_int::try_from(calls).unwrap_or(c_int::MAX));
        frame.upload_bytes = frame
            .upload_bytes
            .saturating_add(c_int::try_from(bytes).unwrap_or(c_int::MAX));
        frame.wait_us = frame.wait_us.saturating_add(wait_us);
        frame.upload_us = frame.upload_us.saturating_add(upload_us);
        frame.max_wait_us = frame.max_wait_us.max(wait_us);
        frame.active_frames = 1;

        PROFILE_IMAGE_LOAD_STATS.with(|profile| {
            let mut profile = profile.borrow_mut();
            note_profile_activity(&mut profile, inactive);
            profile.upload_calls = profile
                .upload_calls
                .saturating_add(c_int::try_from(calls).unwrap_or(c_int::MAX));
            profile.upload_bytes = profile
                .upload_bytes
                .saturating_add(c_int::try_from(bytes).unwrap_or(c_int::MAX));
            profile.wait_us = profile.wait_us.saturating_add(wait_us);
            profile.upload_us = profile.upload_us.saturating_add(upload_us);
            profile.max_wait_us = profile.max_wait_us.max(wait_us);
        });
    });
}

pub(crate) fn reset_image_frame_stats() {
    FRAME_IMAGE_LOAD_STATS.with(|stats| *stats.borrow_mut() = ImageLoadStats::default());
}

#[no_mangle]
pub extern "C" fn stone_reset_image_profile_stats() {
    PROFILE_IMAGE_LOAD_STATS.with(|stats| *stats.borrow_mut() = ImageLoadStats::default());
}

#[no_mangle]
pub extern "C" fn stone_reset_image_profile_stats_to_frame() {
    let frame = FRAME_IMAGE_LOAD_STATS.with(|stats| *stats.borrow());
    PROFILE_IMAGE_LOAD_STATS.with(|stats| *stats.borrow_mut() = frame);
}

#[no_mangle]
pub extern "C" fn stone_image_frame_activity() -> c_int {
    FRAME_IMAGE_LOAD_STATS.with(|stats| {
        let stats = stats.borrow();
        if image_stats_active(&stats) {
            1
        } else {
            0
        }
    })
}

#[no_mangle]
pub extern "C" fn stone_get_image_frame_stats(out: *mut ImageLoadStats) {
    if out.is_null() {
        return;
    }
    FRAME_IMAGE_LOAD_STATS.with(|stats| unsafe { *out = *stats.borrow() });
}

#[no_mangle]
pub extern "C" fn stone_get_image_profile_stats(out: *mut ImageLoadStats) {
    if out.is_null() {
        return;
    }
    PROFILE_IMAGE_LOAD_STATS.with(|stats| unsafe { *out = *stats.borrow() });
}

#[cfg(test)]
mod image_load_stats_tests {
    use super::*;

    fn decoded(id: u64, bytes: usize) -> Arc<image_decode::Decoded> {
        Arc::new(image_decode::Decoded {
            id,
            pixels: Some(vec![0; bytes]),
            width: bytes as u32,
            height: 1,
            stored: (bytes as u32, 1),
            format: ImageFormat::Rgba8,
        })
    }

    #[test]
    fn upload_budget_keeps_whole_images_and_always_makes_progress() {
        let mut pending =
            std::collections::VecDeque::from([decoded(1, 6), decoded(2, 10), decoded(3, 4)]);

        let first = take_decoded_upload_budget(&mut pending, 9, &mut 0);
        assert_eq!(first.iter().map(|d| d.id).collect::<Vec<_>>(), [1]);
        assert_eq!(pending.iter().map(|d| d.id).collect::<Vec<_>>(), [2, 3]);

        let oversized = take_decoded_upload_budget(&mut pending, 9, &mut 0);
        assert_eq!(oversized.iter().map(|d| d.id).collect::<Vec<_>>(), [2]);
        assert_eq!(pending.iter().map(|d| d.id).collect::<Vec<_>>(), [3]);

        let last = take_decoded_upload_budget(&mut pending, 9, &mut 0);
        assert_eq!(last.iter().map(|d| d.id).collect::<Vec<_>>(), [3]);
        assert!(pending.is_empty());
    }

    #[test]
    fn frame_and_profile_stats_retain_load_upload_and_wait_activity() {
        reset_image_frame_stats();
        stone_reset_image_profile_stats();
        record_image_load(true, 400, 7);
        record_image_upload(1, 400, 11, 13);

        let mut frame = ImageLoadStats::default();
        stone_get_image_frame_stats(&mut frame);
        assert_eq!(frame.load_calls, 1);
        assert_eq!(frame.decoded_bytes, 400);
        assert_eq!(frame.upload_calls, 1);
        assert_eq!(frame.wait_us, 11);
        assert_eq!(frame.upload_us, 13);
        assert_eq!(frame.active_frames, 1);

        let mut profile = ImageLoadStats::default();
        stone_get_image_profile_stats(&mut profile);
        assert_eq!(profile.active_frames, 1);

        reset_image_frame_stats();
        record_image_upload(0, 0, 17, 0);
        assert_eq!(stone_image_frame_activity(), 1);
        stone_reset_image_profile_stats_to_frame();
        stone_get_image_profile_stats(&mut profile);
        assert_eq!(profile.wait_us, 17);
        assert_eq!(profile.active_frames, 1);
    }
}

// =============================================================================
// Image Handle
// =============================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImageFormat {
    R8,
    Rgba8,
    DepthRg8,
    MaterialRg8,
}

impl ImageFormat {
    fn report_code(self) -> c_int {
        match self {
            Self::Rgba8 => 0,
            Self::DepthRg8 => 1,
            Self::R8 => 2,
            Self::MaterialRg8 => 3,
        }
    }

    pub(crate) fn bytes_per_texel(self) -> u32 {
        match self {
            Self::R8 => 1,
            Self::Rgba8 => 4,
            Self::DepthRg8 | Self::MaterialRg8 => 2,
        }
    }

    pub(crate) fn gl_internal(self) -> u32 {
        match self {
            Self::R8 => glow::R8,
            Self::Rgba8 => glow::RGBA8,
            Self::DepthRg8 | Self::MaterialRg8 => glow::RG8,
        }
    }

    pub(crate) fn gl_external(self) -> u32 {
        match self {
            Self::R8 => glow::RED,
            Self::Rgba8 => glow::RGBA,
            Self::DepthRg8 | Self::MaterialRg8 => glow::RG,
        }
    }

    pub(crate) fn pack_rgba8(self, rgba: &[u8]) -> Option<Cow<'_, [u8]>> {
        if !rgba.len().is_multiple_of(4) {
            return None;
        }
        let pixels = rgba.as_chunks::<4>().0;
        match self {
            Self::Rgba8 => Some(Cow::Borrowed(rgba)),
            Self::R8 => Some(Cow::Owned(pixels.iter().map(|p| p[0]).collect())),
            Self::DepthRg8 => Some(Cow::Owned(
                pixels.iter().flat_map(|p| [p[0], p[1]]).collect(),
            )),
            Self::MaterialRg8 => Some(Cow::Owned(
                pixels.iter().flat_map(|p| [p[1], p[2]]).collect(),
            )),
        }
    }

    pub(crate) fn pack_rgba8_owned(self, rgba: Vec<u8>) -> Option<Vec<u8>> {
        if self == Self::Rgba8 {
            if rgba.len().is_multiple_of(4) {
                Some(rgba)
            } else {
                None
            }
        } else {
            self.pack_rgba8(&rgba).map(|packed| packed.into_owned())
        }
    }
}

pub(crate) struct LoadedImage {
    pub(crate) texture: glow::Texture,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) format: ImageFormat,
    /// Wrap mode last set by `stone_image_set_wrap`. A bound sampler object
    /// overrides the texture's own wrap state, so the sampler chosen for a draw
    /// has to agree with it or a REPEAT texture (trails) silently clamps.
    pub(crate) repeat: bool,
    /// Loaded headers-only under `STONE_ASSET_STUB`: correct size, no asset
    /// pixels. The texture holds one white texel (`STUB_TEXEL`), so a draw that
    /// samples it gets white rather than undefined contents -- that is the
    /// contract, not a bug. Cleared the moment a writer defines real storage.
    pub(crate) stub: bool,
}

thread_local! {
    pub(crate) static IMAGES: RefCell<HashMap<u64, LoadedImage>> = RefCell::new(HashMap::new());
    pub(crate) static WHITE_IMAGE: RefCell<Option<u64>> = const { RefCell::new(None) };
    /// Images whose GL storage is SMALLER than their logical size, which only
    /// `STONE_IMAGE_MAX_SIDE` produces. Empty in every production process.
    ///
    /// A side table rather than a field on `LoadedImage` because the cap reaches
    /// asset uploads and nothing else: `text.rs` registers font atlases in
    /// `IMAGES` and `image_data.rs` builds images from CPU buffers, both at full
    /// size and neither able to answer for a reduction that never applies to it.
    /// Asking GL per write was the alternative and was measured: a
    /// `glGetTexLevelParameteriv` on this driver's GL-on-Metal path flushes, and
    /// one per image write put recurring 55 ms frames into every rendering sim.
    static CAPPED_STORAGE: RefCell<HashMap<u64, (u32, u32)>> = RefCell::new(HashMap::new());
    /// Images whose load site declared their texels exact, so no cap may
    /// resample them. A side table for the same reason `CAPPED_STORAGE` is one:
    /// only an asset load can make this declaration, while `text.rs` and
    /// `image_data.rs` also put entries in `IMAGES` and have nothing to say
    /// about it. Empty in every process that publishes no cap.
    static EXACT_TEXELS: RefCell<HashSet<u64>> = RefCell::new(HashSet::new());
    static MIPMAPPED_IMAGES: RefCell<HashSet<u64>> = RefCell::new(HashSet::new());
    /// Handles whose deferred decode failed to produce pixels
    /// (IMAGE_DECODE_FAILURE_PRESENTATION). Readiness stays false for them,
    /// so they never read Resident; their texture draws the placeholder fill.
    static FAILED_DECODE: RefCell<HashSet<u64>> = RefCell::new(HashSet::new());
    /// Cumulative failed deferred decodes, for the residency counters.
    static FAILED_DECODE_COUNT: RefCell<u64> = const { RefCell::new(0) };
}

pub(crate) fn register_mipmapped_image(handle: u64) {
    MIPMAPPED_IMAGES.with(|m| {
        m.borrow_mut().insert(handle);
    });
}

/// Whether `handle` declared its texels exact at load. A hot reload and a
/// deferred upload both have to answer this the same way the first load did.
fn exact_texels(handle: u64) -> bool {
    EXACT_TEXELS.with(|e| e.borrow().contains(&handle))
}

/// Whether `handle`'s deferred decode failed. False for unknown handles.
pub(crate) fn image_decode_failed(handle: u64) -> bool {
    FAILED_DECODE.with(|f| f.borrow().contains(&handle))
}

/// Mark a live handle's decode failed, once. A result for an unloaded handle
/// is a dropped unload race, not a failure.
fn mark_decode_failed(id: u64) {
    let live = IMAGES.with(|images| images.borrow().contains_key(&id));
    if !live {
        return;
    }
    let fresh = FAILED_DECODE.with(|f| f.borrow_mut().insert(id));
    if fresh {
        FAILED_DECODE_COUNT.with(|c| *c.borrow_mut() += 1);
        upload_decode_placeholder(id);
    }
}

/// The failure placeholder's cells: the development missing-image checker's
/// bright magenta on near-black, the same bytes `development_missing_image`
/// (`assets.rock`) paints (`image_data_set_pixel` truncates, so 0.1 is 25).
const DECODE_FAILED_BRIGHT: [u8; 4] = [255, 0, 204, 255];
const DECODE_FAILED_DARK: [u8; 4] = [25, 0, 25, 255];

/// Define a failed handle's texture as the placeholder checker at its storage
/// size, through the same storage path a real upload takes. Every draw
/// through the handle -- present and future -- samples the placeholder; the
/// logical size is untouched, so sheet frames and UVs keep working.
fn upload_decode_placeholder(id: u64) {
    let Some((w, h, format)) =
        IMAGES.with(|i| i.borrow().get(&id).map(|i| (i.width, i.height, i.format)))
    else {
        return;
    };
    let (sw, sh) = stored_size(id, (w, h));
    let len = (sw as usize).saturating_mul(sh as usize).saturating_mul(4);
    if len == 0 {
        return;
    }
    let mut rgba = vec![0u8; len];
    let row = sw as usize;
    for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let bright = ((i % row) / 2 + (i / row) / 2).is_multiple_of(2);
        px.copy_from_slice(if bright {
            &DECODE_FAILED_BRIGHT
        } else {
            &DECODE_FAILED_DARK
        });
    }
    let Some(pixels) = format.pack_rgba8(&rgba) else {
        log_once!(
            "IMAGE",
            "image {}: cannot pack decode-failure placeholder as {:?}",
            id,
            format
        );
        return;
    };
    publish_image_pixels(id, (w, h), (sw, sh), format, &pixels);
}

/// What GL stores for `handle`: its logical size unless the cap reduced it.
fn stored_size(handle: u64, logical: (u32, u32)) -> (u32, u32) {
    CAPPED_STORAGE
        .with(|c| c.borrow().get(&handle).copied())
        .unwrap_or(logical)
}

/// Record what a write just defined, so the next writer and the texel probe read
/// one answer. Dropping the entry is how an image returns to full storage, which
/// is why this takes both sizes rather than only the one it stores.
fn record_storage(handle: u64, logical: (u32, u32), stored: (u32, u32)) {
    CAPPED_STORAGE.with(|c| {
        let mut capped = c.borrow_mut();
        if stored == logical {
            capped.remove(&handle);
        } else {
            capped.insert(handle, stored);
        }
    });
}

/// What GL stores for a headers-only image: one texel.
const STUB_STORAGE: (u32, u32) = (1, 1);

/// The texel itself. White rather than `stone_get_white_image`'s own texture,
/// even though the colour is the same: a handle OWNS the object
/// `stone_unload_image` deletes and every image writer redefines, so one shared
/// object would be freed out from under every other stubbed image the first time
/// one was unloaded or reloaded.
const STUB_RGBA8_TEXEL: [u8; 4] = [255, 255, 255, 255];
const STUB_RG8_TEXEL: [u8; 2] = [255, 255];

fn stub_texel(format: ImageFormat) -> &'static [u8] {
    match format {
        ImageFormat::Rgba8 => &STUB_RGBA8_TEXEL,
        ImageFormat::DepthRg8 | ImageFormat::MaterialRg8 => &STUB_RG8_TEXEL,
        ImageFormat::R8 => &[255],
    }
}

pub(crate) unsafe fn with_rg_unpack_alignment<R>(
    gl: &crate::DebugGl,
    format: ImageFormat,
    upload: impl FnOnce() -> R,
) -> R {
    if !matches!(format, ImageFormat::DepthRg8 | ImageFormat::MaterialRg8) {
        return upload();
    }
    let old_alignment = gl.get_parameter_i32(glow::UNPACK_ALIGNMENT);
    if old_alignment != 1 {
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
    }
    let result = upload();
    if old_alignment != 1 {
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, old_alignment);
    }
    result
}

unsafe fn tex_image_for_format(
    gl: &crate::DebugGl,
    texture: glow::Texture,
    width: u32,
    height: u32,
    format: ImageFormat,
    pixels: Option<&[u8]>,
) {
    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
    with_rg_unpack_alignment(gl, format, || {
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            format.gl_internal() as i32,
            width as i32,
            height as i32,
            0,
            format.gl_external(),
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(pixels),
        );
    });
}

/// GL storage dimensions for a load: one white texel when headers-only,
/// otherwise what `STONE_IMAGE_MAX_SIDE` leaves of the file's own size.
///
/// `exact_texels` is a declaration about the FILE's texels, so it exempts an
/// image from the cap but says nothing to a stub, which holds none of them.
fn load_storage_dims(stubbed: bool, exact_texels: bool, width: u32, height: u32) -> (u32, u32) {
    if stubbed {
        STUB_STORAGE
    } else {
        image_decode::storage_dims(exact_texels, width, height)
    }
}

fn stored_texel_bytes(
    mut width: u32,
    mut height: u32,
    bytes_per_texel: u32,
    mipmapped: bool,
) -> u64 {
    let mut bytes = 0u64;
    loop {
        bytes += u64::from(width) * u64::from(height) * u64::from(bytes_per_texel);
        if !mipmapped || (width == 1 && height == 1) {
            break;
        }
        width = (width / 2).max(1);
        height = (height / 2).max(1);
    }
    bytes
}

fn image_byte_len(handle: u64, image: &LoadedImage) -> u64 {
    let stored = stored_size(handle, (image.width, image.height));
    let mipmapped = MIPMAPPED_IMAGES.with(|m| m.borrow().contains(&handle));
    stored_texel_bytes(
        stored.0,
        stored.1,
        image.format.bytes_per_texel(),
        mipmapped,
    )
}

/// Publish a finished load: the image, its exactness declaration, what GL now
/// holds for it, and the path a stub reports itself by.
///
/// One place, so the stub arm and the real arm cannot record different things
/// about the same load. `stored` is what was actually defined -- `STUB_STORAGE`
/// for a stub, the capped size otherwise -- while `logical` stays the file's
/// own and is what `stone_get_image_size` answers.
fn register_loaded_image(
    id: u64,
    texture: glow::Texture,
    logical: (u32, u32),
    stored: (u32, u32),
    format: ImageFormat,
    exact_texels: bool,
    stubbed: bool,
    path: &str,
) {
    IMAGES.with(|images| {
        images.borrow_mut().insert(
            id,
            LoadedImage {
                texture,
                repeat: false,
                width: logical.0,
                height: logical.1,
                format,
                stub: stubbed,
            },
        );
    });
    // Before any deferred work: the pool and every later write read it.
    EXACT_TEXELS.with(|e| {
        if exact_texels {
            e.borrow_mut().insert(id);
        } else {
            e.borrow_mut().remove(&id);
        }
    });
    record_storage(id, logical, stored);
    if stubbed {
        STUB_PATHS.with(|p| p.borrow_mut().insert(id, path.to_string()));
    }
}

#[cfg(test)]
mod stub_storage_tests {
    use super::*;

    fn fake_texture(name: u32) -> glow::Texture {
        glow::NativeTexture(std::num::NonZeroU32::new(name).expect("non-zero texture name"))
    }

    #[test]
    fn stored_bytes_use_the_uploaded_extent_and_texel_width() {
        assert_eq!(stored_texel_bytes(3, 2, 4, false), 24);
        assert_eq!(stored_texel_bytes(3, 2, 2, false), 12);
        assert_eq!(stored_texel_bytes(3, 2, 1, true), 7);
    }

    #[test]
    fn image_formats_use_sized_gl_storage_and_matching_upload_lanes() {
        assert_eq!(ImageFormat::Rgba8.gl_internal(), glow::RGBA8);
        assert_eq!(ImageFormat::Rgba8.gl_external(), glow::RGBA);
        assert_eq!(ImageFormat::Rgba8.bytes_per_texel(), 4);
        assert_eq!(ImageFormat::DepthRg8.gl_internal(), glow::RG8);
        assert_eq!(ImageFormat::DepthRg8.gl_external(), glow::RG);
        assert_eq!(ImageFormat::DepthRg8.bytes_per_texel(), 2);
        assert_eq!(ImageFormat::MaterialRg8.gl_internal(), glow::RG8);
        assert_eq!(ImageFormat::MaterialRg8.gl_external(), glow::RG);
        assert_eq!(ImageFormat::MaterialRg8.bytes_per_texel(), 2);
    }

    /// A headers-only load defines one white texel, so what GL holds is 1x1
    /// while every caller still reads the file's own dimensions.
    #[test]
    fn a_stub_stores_one_texel_and_keeps_its_logical_size() {
        assert_eq!(load_storage_dims(true, false, 1024, 512), STUB_STORAGE);
        // `exact_texels` is a declaration about the FILE's texels. A stub holds
        // none of them, so it cannot exempt one from the white texel.
        assert_eq!(load_storage_dims(true, true, 768, 768), STUB_STORAGE);
        assert_eq!(load_storage_dims(false, true, 768, 768), (768, 768));

        // Through the registration the load itself performs, so the record and
        // the answer cannot come apart.
        let handle = 0x5747_4231u64;
        let stored = load_storage_dims(true, false, 1024, 512);
        register_loaded_image(
            handle,
            fake_texture(1),
            (1024, 512),
            stored,
            ImageFormat::Rgba8,
            false,
            true,
            "sheet.png",
        );

        let (mut w, mut h) = (0 as c_int, 0 as c_int);
        stone_image_storage_size(handle, &mut w, &mut h);
        assert_eq!(
            (w, h),
            (1, 1),
            "a stub holds one texel, not the file's size"
        );

        let (mut lw, mut lh) = (0 as c_int, 0 as c_int);
        stone_get_image_size(handle, &mut lw, &mut lh);
        assert_eq!(
            (lw, lh),
            (1024, 512),
            "the logical size stays the file's own"
        );
        assert_eq!(stub_path(handle).as_deref(), Some("sheet.png"));

        IMAGES.with(|images| images.borrow_mut().remove(&handle));
        CAPPED_STORAGE.with(|c| c.borrow_mut().remove(&handle));
        STUB_PATHS.with(|p| p.borrow_mut().remove(&handle));
    }
}

// =============================================================================
// Image Loading (using `image` crate)
// =============================================================================

/// Decode an image to RGBA8 from the embedded asset registry first, falling
/// back to disk. Returns `(rgba_bytes, width, height)`; logs once + None on
/// failure. Shared by texture, ImageData, and cursor loaders so their sources
/// and accepted formats cannot drift.
pub(crate) fn decode_rgba_image(path_str: &str) -> Option<(Vec<u8>, u32, u32)> {
    let data = resolve_image_bytes(path_str)?;
    image_decode::decode_rgba_bytes(&data, path_str)
}

// Most baked PNGs reach their first IDAT within 64 bytes. Keep the larger
// metadata window for other containers before the existing full-read fallback.
const IMAGE_HEADER_PREFIX: usize = 64 * 1024;
const IMAGE_HEADER_INITIAL: usize = 64;

struct ImageHeader {
    bytes: Vec<u8>,
    dimensions: Option<(u32, u32)>,
    complete: bool,
}

fn resolve_image_header(path: &str) -> Option<ImageHeader> {
    let mut limit = IMAGE_HEADER_INITIAL;
    let mut bytes = asset_registry::resolve_asset_prefix(path, limit)?;
    let mut dimensions = image_decode::probe_dimensions(&bytes);
    if dimensions.is_none() && bytes.len() == limit {
        limit = IMAGE_HEADER_PREFIX;
        bytes = asset_registry::resolve_asset_prefix(path, limit)?;
        dimensions = image_decode::probe_dimensions(&bytes);
    }
    Some(ImageHeader {
        complete: bytes.len() < limit,
        bytes,
        dimensions,
    })
}

#[cfg(test)]
mod image_header_tests {
    #[test]
    fn metadata_heavy_jpeg_admits_dimensions_beyond_the_initial_prefix() {
        let pixels = image::RgbImage::from_pixel(11, 7, image::Rgb([12, 90, 231]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode_image(&pixels)
            .unwrap();

        // A legal JPEG comment before the frame header pushes dimensions past
        // the small admission read without changing the encoded image.
        let comment = vec![b'x'; 4096];
        assert_eq!(&jpeg[..2], &[0xff, 0xd8]);
        let mut bytes = jpeg[..2].to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe]);
        bytes.extend_from_slice(&((comment.len() + 2) as u16).to_be_bytes());
        bytes.extend_from_slice(&comment);
        bytes.extend_from_slice(&jpeg[2..]);
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), pixels.dimensions());
        assert_eq!(super::image_decode::probe_dimensions(&bytes[..64]), None);

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("metadata-heavy.jpg");
        std::fs::write(&path, &bytes).unwrap();
        let header = super::resolve_image_header(path.to_str().unwrap()).unwrap();
        assert_eq!(header.dimensions, Some(pixels.dimensions()));
        assert!(header.complete);
        assert_eq!(header.bytes, bytes);
    }
}

/// The compressed bytes behind an asset path, embedded registry first then
/// disk. Split out so the deferred path in `stone_load_image` can header-probe
/// and hand the same bytes to the decode pool without reading twice.
fn resolve_image_bytes(path_str: &str) -> Option<Vec<u8>> {
    match asset_registry::resolve_asset(path_str) {
        Some(d) => Some(d),
        None => {
            log_once!("IMAGE", "Failed to load image {}: not found", path_str);
            None
        }
    }
}

/// `stone_load_image` flag: the file's texels are read individually, so the
/// image cap does not apply.
const LOAD_EXACT_TEXELS: i32 = 1;
/// `stone_load_image` flag: a flush need not wait for this decode; a budgeted
/// pump publishes it, or `stone_image_require` promotes it.
const LOAD_DEFERRED: i32 = 2;
const LOAD_DEPTH_RG8: i32 = 4;
const LOAD_MATERIAL_RG8: i32 = 8;

#[no_mangle]
pub extern "C" fn stone_load_image(
    path: *const c_char,
    filter_mode: i32,
    flags: i32,
    width: i32,
    height: i32,
) -> u64 {
    let declared_size = match (width, height) {
        (0, 0) => None,
        (w, h) if w > 0 && h > 0 => Some((w as u32, h as u32)),
        _ => {
            log_once!(
                "IMAGE",
                "stone_load_image: invalid manifest size {}x{}",
                width,
                height
            );
            record_image_load(false, 0, 0);
            return 0;
        }
    };
    let format = match flags & (LOAD_DEPTH_RG8 | LOAD_MATERIAL_RG8) {
        0 => ImageFormat::Rgba8,
        LOAD_DEPTH_RG8 => ImageFormat::DepthRg8,
        LOAD_MATERIAL_RG8 => ImageFormat::MaterialRg8,
        _ => {
            log_once!(
                "IMAGE",
                "stone_load_image: conflicting packed image format flags for {:?}",
                path
            );
            record_image_load(false, 0, 0);
            return 0;
        }
    };
    let class = if flags & LOAD_DEFERRED != 0 {
        crate::image_publication::RequestClass::Deferred
    } else {
        crate::image_publication::RequestClass::Required
    };
    let exact_texels = flags & LOAD_EXACT_TEXELS != 0 || format != ImageFormat::Rgba8;

    load_image(
        path,
        filter_mode,
        exact_texels,
        format,
        class,
        declared_size,
    )
}

fn load_image(
    path: *const c_char,
    filter_mode: i32,
    exact_texels: bool,
    format: ImageFormat,
    class: crate::image_publication::RequestClass,
    declared_size: Option<(u32, u32)>,
) -> u64 {
    let load_started = std::time::Instant::now();
    let gl_filter = match gl_state::gl_filter_for(filter_mode) {
        Some(f) => f,
        None => {
            log_once!(
                "IMAGE",
                "stone_load_image: unknown filter_mode {} for {:?}",
                filter_mode,
                path
            );
            record_image_load(false, 0, elapsed_us(load_started));
            return 0;
        }
    };
    if path.is_null() {
        record_image_load(false, 0, elapsed_us(load_started));
        return 0;
    }
    let path_str = unsafe { CStr::from_ptr(path).to_str().unwrap_or("") };

    // Collect completed workers into the publication state. The load site
    // publishes no pixels; only a pump or flush can make those images ready.
    let uploads = publish_uploads("load");
    if uploads.selected != 0 {
        record_image_upload(
            uploads.calls,
            uploads.bytes,
            crate::image_publication::selection_wait_us(),
            uploads.upload_us(),
        );
    }

    // A Prefetch handle needs dimensions now, but its full payload belongs on
    // the decode worker. Stubs never need the payload at all.
    let mut read_in_worker = class == crate::image_publication::RequestClass::Deferred
        && !image_decode::stub_images()
        // Without manifest dimensions, registry prefix resolution already
        // copies the payload; keep that copy for the worker.
        && (declared_size.is_some() || !asset_registry::contains_asset(path_str));
    let prefix_only = image_decode::stub_images() || read_in_worker;
    let (data, header_dimensions, complete) = if read_in_worker && declared_size.is_some() {
        // Manifest-sized prefetch does no file I/O on the GL thread. The
        // worker validates these dimensions before pixels can be published.
        (Vec::new(), declared_size, false)
    } else if prefix_only {
        match resolve_image_header(path_str) {
            Some(header) => (header.bytes, header.dimensions, header.complete),
            None => {
                log_once!("IMAGE", "Failed to load image {}: not found", path_str);
                record_image_load(false, 0, elapsed_us(load_started));
                return 0;
            }
        }
    } else {
        match resolve_image_bytes(path_str) {
            Some(d) => (d, None, true),
            None => {
                record_image_load(false, 0, elapsed_us(load_started));
                return 0;
            }
        }
    };

    // A short prefix is already the entire file; keep those bytes for decode.
    if read_in_worker && complete {
        read_in_worker = false;
    }

    // Allocate the texture at the header's dimensions and let the pool decode
    // the pixels; `stone_flush_image_uploads` fills them in before anything
    // draws. The handle and `stone_get_image_size` are correct immediately, so
    // callers cannot observe the deferral. A header the probe can't read (or a
    // build with no pool) decodes inline, exactly as before. Headers-only takes
    // the same probe but queues nothing; either way a header the probe cannot
    // read decodes rather than inventing a size, because a wrong `image_dims`
    // would corrupt sheet math silently.
    let mut data = data;
    let mut probed = if prefix_only {
        header_dimensions
    } else {
        image_decode::probe_dimensions(&data)
    };
    if probed.is_none() && prefix_only {
        // The prefix was not enough to find the dimensions, so this image has
        // to be decoded after all -- and the truncated bytes cannot be what
        // decodes. Re-read in full.
        data = match resolve_image_bytes(path_str) {
            Some(d) => d,
            None => {
                record_image_load(false, 0, elapsed_us(load_started));
                return 0;
            }
        };
        if read_in_worker {
            // A long header (for example JPEG metadata) keeps the old full-read
            // path; if it probes now, decoding still runs on the worker.
            probed = image_decode::probe_dimensions(&data);
            read_in_worker = false;
        }
    }
    let stubbed = image_decode::stub_images() && probed.is_some();
    let deferred = if stubbed { None } else { probed };
    let (pixels, width, height) = match probed {
        Some((w, h)) => (None, w, h),
        None => match image_decode::decode_rgba_bytes(&data, path_str) {
            // The decode pool caps on its own thread; this is the inline arm,
            // so the reduction happens here instead.
            Some((rgba, w, h)) => match image_decode::store_at_storage(exact_texels, rgba, w, h)
                .and_then(|stored| format.pack_rgba8_owned(stored))
            {
                Some(stored) => (Some(stored), w, h),
                None => {
                    log_once!(
                        "IMAGE",
                        "stone_load_image: could not pack {} as {:?}",
                        path_str,
                        format
                    );
                    record_image_load(false, 0, elapsed_us(load_started));
                    return 0;
                }
            },
            None => {
                record_image_load(false, 0, elapsed_us(load_started));
                return 0;
            }
        },
    };
    // What GL stores: one white texel for a stub, otherwise what
    // `STONE_IMAGE_MAX_SIDE` may have reduced the file's size to. The LOGICAL
    // size recorded below and answered by `stone_get_image_size` stays `width` x
    // `height` either way, so no frame rect, sheet index or UV can see the
    // difference -- which also makes it invisible to every caller, so the one
    // place that knows says so.
    let (storage_width, storage_height) = load_storage_dims(stubbed, exact_texels, width, height);
    if !stubbed && (storage_width, storage_height) != (width, height) {
        ::log::debug!(target: "IMAGE", "{} stored {}x{} of {}x{} under the image cap",
            path_str, storage_width, storage_height, width, height);
    }

    // Create OpenGL texture. A stub defines its one white texel here; `pixels`
    // is None for it, and the texel is not counted as an upload below because
    // it is not asset data -- doing so would report every stubbed load as one.
    let upload: Option<&[u8]> = if stubbed {
        Some(stub_texel(format))
    } else {
        pixels.as_deref()
    };
    let inline_bytes = pixels.as_ref().map_or(0, Vec::len);
    let texture_started = std::time::Instant::now();
    let texture = with_gl(|gl| unsafe {
        let texture = match gl.create_texture() {
            Ok(t) => t,
            Err(e) => {
                log_once!(
                    "IMAGE",
                    "stone_load_image: gl.create_texture() failed for {}: {}",
                    path_str,
                    e
                );
                return None;
            }
        };
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            gl_state::gl_min_filter_for(filter_mode).unwrap() as i32,
        );
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, gl_filter as i32);
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );

        // Async loads define storage at publication instead. Allocating here with
        // a null image and filling it via tex_sub_image_2d later costs a second
        // full pass over the pixels -- 427ms rather than 99ms for mag's 1.5GB.
        if let Some(data) = upload {
            tex_image_for_format(
                gl,
                texture,
                storage_width,
                storage_height,
                format,
                Some(data),
            );
            if filter_mode == 2 {
                gl.generate_mipmap(glow::TEXTURE_2D);
            }
        }

        Some(texture)
    });

    match texture {
        Some(Some(tex)) => {
            let id = crate::handles::mint(Class::Image);
            // A deferred load has no storage yet, but the pool reduces to
            // exactly this size and `upload_decoded` defines exactly it, so one
            // record covers both arms; a stub's record is the white texel just
            // defined, which is what a later write has to replace.
            register_loaded_image(
                id,
                tex,
                (width, height),
                (storage_width, storage_height),
                format,
                exact_texels,
                stubbed,
                path_str,
            );
            if filter_mode == 2 {
                register_mipmapped_image(id);
            }
            if deferred.is_some() {
                crate::image_publication::submit(id, path_str, width, height, class);
                let source = if read_in_worker {
                    image_decode::DecodeSource::Asset {
                        dimensions: (width, height),
                    }
                } else {
                    image_decode::DecodeSource::Bytes(data)
                };
                image_decode::submit(id, path_str.to_string(), source, exact_texels, format);
            }
            if inline_bytes > 0 {
                record_image_upload(1, inline_bytes, 0, elapsed_us(texture_started));
            }
            let decoded_bytes = (width as usize)
                .saturating_mul(height as usize)
                .saturating_mul(4);
            record_image_load(true, decoded_bytes, elapsed_us(load_started));
            id
        }
        _ => {
            record_image_load(false, 0, elapsed_us(load_started));
            0
        }
    }
}

pub(crate) fn refresh_image_mipmaps(handle: u64, texture: glow::Texture) {
    if !MIPMAPPED_IMAGES.with(|m| m.borrow().contains(&handle)) {
        return;
    }
    with_gl(|gl| unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.generate_mipmap(glow::TEXTURE_2D);
    });
}

/// Publish every Required image, waiting for its decode if necessary.
/// Deferred results stay pending until a pump publishes them or the request
/// is promoted to Required. Readiness must precede drawing or pixel readback;
/// an admitted handle alone does not expose sampleable pixels.
#[no_mangle]
pub extern "C" fn stone_flush_image_uploads() {
    let uploads = publish_uploads("flush");
    if uploads.selected == 0 {
        return;
    }
    let wait_us = crate::image_publication::selection_wait_us();
    let (n, bytes, upload_us) = (uploads.calls, uploads.bytes, uploads.upload_us());
    record_image_upload(n, bytes, wait_us, upload_us);
    ::log::debug!(target: "IMAGE", "flush {} images {:.0}MB wait={:.1}ms upload={:.1}ms",
        n, bytes as f64 / 1048576.0, wait_us as f64 / 1000.0, upload_us as f64 / 1000.0);
}

/// One site's publications, uploaded as each is handed over so a draining
/// flush holds one decoded buffer at a time. Only the uploads are timed.
#[derive(Default)]
struct PublishedUploads {
    selected: usize,
    calls: usize,
    bytes: usize,
    upload: std::time::Duration,
}

impl PublishedUploads {
    fn upload_us(&self) -> c_int {
        c_int::try_from(self.upload.as_micros()).unwrap_or(c_int::MAX)
    }
}

fn publish_uploads(site: &str) -> PublishedUploads {
    let mut uploads = PublishedUploads::default();
    crate::image_publication::publish(site, |d| {
        uploads.selected += 1;
        let started = std::time::Instant::now();
        let (calls, bytes) = upload_decoded(std::iter::once(d));
        uploads.upload += started.elapsed();
        uploads.calls += calls;
        uploads.bytes += bytes;
    });
    uploads
}

#[no_mangle]
pub extern "C" fn stone_set_frame_image_upload_budget(bytes: c_int) {
    crate::image_publication::set_budget(bytes.max(0) as usize);
}

#[no_mangle]
pub extern "C" fn stone_image_uploads_pending() -> c_int {
    c_int::try_from(crate::image_publication::pending()).unwrap_or(c_int::MAX)
}

#[no_mangle]
pub extern "C" fn stone_service_image_uploads() -> c_int {
    let uploads = publish_uploads("pump");
    let wait_us = crate::image_publication::selection_wait_us();
    if uploads.selected != 0 {
        let (calls, bytes, upload_us) = (uploads.calls, uploads.bytes, uploads.upload_us());
        record_image_upload(calls, bytes, wait_us, upload_us);
        ::log::debug!(target: "IMAGE", "pump {} images {:.0}MB upload={:.1}ms pending={}",
            calls, bytes as f64 / 1048576.0, upload_us as f64 / 1000.0,
            crate::image_publication::pending());
    }
    stone_image_uploads_pending()
}

/// Define texture storage from finished decodes. Returns (images, bytes).
fn upload_decoded(decoded: impl IntoIterator<Item = Arc<image_decode::Decoded>>) -> (usize, usize) {
    let (mut n, mut bytes) = (0usize, 0usize);
    for d in decoded {
        let Some(pixels) = d.pixels.as_ref() else {
            mark_decode_failed(d.id);
            continue;
        };
        // The handle can be gone if the image was unloaded while its decode was
        // in flight; the result is simply dropped.
        let Some((w, h, format)) = IMAGES.with(|i| {
            i.borrow()
                .get(&d.id)
                .map(|image| (image.width, image.height, image.format))
        }) else {
            continue;
        };
        if format != d.format {
            log_once!(
                "IMAGE",
                "deferred image {} format mismatch {:?} != {:?}; skipped",
                d.id,
                d.format,
                format
            );
            continue;
        }
        // `stone_get_image_size` already answered from the header probe, so a
        // decode that disagrees would leave every caller's dimensions wrong for
        // the texture this is about to define. Reject rather than upload.
        if (w, h) != (d.width, d.height) {
            log_once!(
                "IMAGE",
                "deferred decode size {}x{} != probed {}x{} for image {}; skipped",
                d.width,
                d.height,
                w,
                h,
                d.id
            );
            mark_decode_failed(d.id);
            continue;
        }
        // The pool already reduced these bytes to the storage size. A length
        // that disagrees would have `tex_image_2d` read past the buffer, so it
        // is refused rather than uploaded.
        let (sw, sh) = d.stored;
        let expected = (sw as usize)
            .saturating_mul(sh as usize)
            .saturating_mul(format.bytes_per_texel() as usize);
        if pixels.len() != expected {
            log_once!(
                "IMAGE",
                "image {} decoded {} bytes, not the {} of its {}x{} {:?} storage; skipped",
                d.id,
                pixels.len(),
                expected,
                sw,
                sh,
                format
            );
            mark_decode_failed(d.id);
            continue;
        }
        if !publish_image_pixels(d.id, (d.width, d.height), (sw, sh), format, pixels) {
            continue;
        }
        n += 1;
        bytes += pixels.len();
    }
    (n, bytes)
}

/// Successful decodes and failure fills publish packed texels through the
/// same format-aware storage path.
fn publish_image_pixels(
    id: u64,
    logical: (u32, u32),
    stored: (u32, u32),
    format: ImageFormat,
    pixels: &[u8],
) -> bool {
    let Some((target, fresh)) = image_write_target_stored(id, logical, stored, pixels) else {
        return false;
    };
    if !fresh
        && with_gl(|gl| unsafe {
            tex_image_for_format(gl, target, stored.0, stored.1, format, Some(pixels));
        })
        .is_none()
    {
        ::log::error!(target: "IMAGE", "image {id}: no GL context for pixel publication");
        return false;
    }
    refresh_image_mipmaps(id, target);
    true
}

/// Whether this live image holds the headers-only white texel rather than
/// decoded pixels. Query the load outcome, not the process's decoder policy:
/// a header probe can fail and require a real decode even in a stubbed run.
/// False for an unknown handle.
#[no_mangle]
pub extern "C" fn stone_image_is_stub(handle: u64) -> bool {
    IMAGES.with(|images| images.borrow().get(&handle).is_some_and(|image| image.stub))
}

/// What GL actually holds for `handle`: one texel for a stub, otherwise the
/// extent after `STONE_IMAGE_MAX_SIDE`. An exact-texel declaration exempts a
/// real image from the cap. Zeros for an unknown handle.
///
/// A DIAGNOSTIC. No draw may branch on it: the whole point of the cap is that
/// sheet frames, pixel rects and UVs are derived from the logical size and
/// cannot see a reduction. It exists so a fixture can ask whether a reduction
/// happened at all.
#[no_mangle]
pub extern "C" fn stone_image_storage_size(handle: u64, width: *mut c_int, height: *mut c_int) {
    let stored = IMAGES.with(|images| {
        images
            .borrow()
            .get(&handle)
            .map(|img| stored_size(handle, (img.width, img.height)))
    });
    let (w, h) = stored.unwrap_or((0, 0));
    unsafe {
        if !width.is_null() {
            *width = w as c_int;
        }
        if !height.is_null() {
            *height = h as c_int;
        }
    }
}

/// The logical size an asset would load at, read from its container header
/// without decoding, allocating a texture or submitting work. Residency
/// accounting prices an absent image with it. False (and 0x0) when the asset
/// is missing or the header cannot be read.
#[no_mangle]
pub extern "C" fn stone_probe_image_size(
    path: *const c_char,
    width: *mut c_int,
    height: *mut c_int,
) -> bool {
    let size = if path.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(path) }
            .to_str()
            .ok()
            .and_then(resolve_image_header)
            .and_then(|header| header.dimensions)
    };
    let (w, h) = size.unwrap_or((0, 0));
    unsafe {
        if !width.is_null() {
            *width = w as c_int;
        }
        if !height.is_null() {
            *height = h as c_int;
        }
    }
    size.is_some()
}

#[no_mangle]
pub extern "C" fn stone_get_image_size(handle: u64, width: *mut c_int, height: *mut c_int) {
    IMAGES.with(|images| {
        if let Some(img) = images.borrow().get(&handle) {
            unsafe {
                if !width.is_null() {
                    *width = img.width as c_int;
                }
                if !height.is_null() {
                    *height = img.height as c_int;
                }
            }
        }
    });
}

/// Switch an image's sampler wrap mode. Textures are created
/// CLAMP_TO_EDGE; scrolling UVs (trails) need GL_REPEAT. Only meaningful
/// for standalone images -- an atlas region cannot repeat.
#[no_mangle]
pub extern "C" fn stone_image_set_wrap(handle: u64, repeat: bool) -> bool {
    let texture = IMAGES.with(|images| images.borrow().get(&handle).map(|img| img.texture));
    let Some(texture) = texture else {
        crate::log_once!("IMAGE", "stone_image_set_wrap: invalid handle {}", handle);
        return false;
    };
    IMAGES.with(|images| {
        if let Some(img) = images.borrow_mut().get_mut(&handle) {
            img.repeat = repeat;
        }
    });
    with_gl(|gl| unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        let mode = if repeat {
            glow::REPEAT
        } else {
            glow::CLAMP_TO_EDGE
        } as i32;
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, mode);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, mode);
    })
    .is_some()
}

/// The texture a write to `handle` must target, and whether it already holds
/// `data`.
///
/// `true` in the second slot means the returned texture already carries `data`
/// as its complete contents, so the caller's own upload is redundant. `None`
/// for an unknown image.
///
/// `data` is the image's full logical size. A writer whose bytes are already at
/// the image's GL storage size (the asset uploads, which `STONE_IMAGE_MAX_SIDE`
/// may have reduced) calls `image_write_target_stored` instead.
pub(crate) fn image_write_target(
    handle: u64,
    width: u32,
    height: u32,
    data: &[u8],
) -> Option<(glow::Texture, bool)> {
    image_write_target_stored(handle, (width, height), (width, height), data)
}

/// `image_write_target` for a writer whose `data` is `stored` texels rather than
/// the image's `logical` size. The two differ only under `STONE_IMAGE_MAX_SIDE`.
pub(crate) fn image_write_target_stored(
    handle: u64,
    logical: (u32, u32),
    stored: (u32, u32),
    data: &[u8],
) -> Option<(glow::Texture, bool)> {
    let current = IMAGES.with(|images| images.borrow().get(&handle).map(|img| img.texture))?;
    // A writer's own upload can be a `tex_sub_image_2d` wider than the storage
    // the cap left, which GL refuses and drops. So a write that does not match
    // what GL holds is completed here, at the size this one names, and the
    // caller skips its own upload.
    if stored_size(handle, logical) != stored {
        return define_image_storage(handle, current, logical, stored, data)
            .map(|texture| (texture, true));
    }
    Some((current, false))
}

/// Define level 0 of `texture` as `stored` texels of `data`, and record both the
/// image's `logical` size and what GL now holds. `None` with no GL context.
///
/// The two sizes are the whole point: `logical` is what every Rock caller reads
/// back through `stone_get_image_size`, `stored` is only what GL holds.
fn define_image_storage(
    handle: u64,
    texture: glow::Texture,
    logical: (u32, u32),
    stored: (u32, u32),
    data: &[u8],
) -> Option<glow::Texture> {
    let format = IMAGES.with(|images| images.borrow().get(&handle).map(|img| img.format))?;
    with_gl(|gl| unsafe {
        tex_image_for_format(gl, texture, stored.0, stored.1, format, Some(data));
    })?;
    IMAGES.with(|images| {
        if let Some(img) = images.borrow_mut().get_mut(&handle) {
            img.width = logical.0;
            img.height = logical.1;
            // Real pixels landed, so an image loaded headers-only stops being one.
            img.stub = false;
        }
    });
    record_storage(handle, logical, stored);
    Some(texture)
}

/// Read one texel straight out of an image's texture, as 0xRRGGBBAA.
///
/// A PROBE, and desktop-GL only: `glGetTexImage` does not exist on GLES.
/// Nothing in the engine calls this; it is how a fixture
/// asks what is actually in the texture a handle names. -1 when the handle is
/// unknown or the coordinate is outside it.
#[no_mangle]
pub extern "C" fn stone_image_read_texel(handle: u64, x: c_int, y: c_int) -> i64 {
    let Some((texture, logical)) = IMAGES.with(|images| {
        images
            .borrow()
            .get(&handle)
            .map(|img| (img.texture, (img.width, img.height)))
    }) else {
        return -1;
    };
    // `glGetTexImage` writes the whole level, so the buffer and the stride come
    // from what GL stores, never from the image's logical size: under
    // `STONE_IMAGE_MAX_SIDE` the two differ and a logical-sized stride would
    // read the wrong texel out of a short buffer.
    let (width, height) = stored_size(handle, logical);
    if x < 0 || y < 0 || x as u32 >= width || y as u32 >= height {
        return -1;
    }
    let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
    let read = with_gl(|gl| unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.get_tex_image(
            glow::TEXTURE_2D,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(&mut pixels)),
        );
    });
    if read.is_none() {
        return -1;
    }
    let i = ((y as usize) * (width as usize) + x as usize) * 4;
    i64::from(pixels[i]) << 24
        | i64::from(pixels[i + 1]) << 16
        | i64::from(pixels[i + 2]) << 8
        | i64::from(pixels[i + 3])
}

/// Whether an image samples with GL_REPEAT, as the CPU mirror holds it: 1
/// repeat, 0 clamp, -1 no such handle.
#[no_mangle]
pub extern "C" fn stone_image_wrap_state(handle: u64) -> c_int {
    IMAGES.with(|images| match images.borrow().get(&handle) {
        Some(img) => c_int::from(img.repeat),
        None => -1,
    })
}

#[no_mangle]
pub extern "C" fn stone_image_exists(handle: u64) -> bool {
    if handle == 0 {
        return false;
    }
    IMAGES.with(|images| images.borrow().contains_key(&handle))
}

/// Whether the handle's deferred decode failed to produce pixels. False for
/// unknown handles and for every synchronously loaded image.
#[no_mangle]
pub extern "C" fn stone_image_decode_failed(handle: u64) -> bool {
    image_decode_failed(handle)
}

/// Cumulative failed deferred decodes. Residency snapshots this at its frame
/// and profile boundaries and reports the deltas.
#[no_mangle]
pub extern "C" fn stone_image_decode_failed_count() -> u64 {
    FAILED_DECODE_COUNT.with(|c| *c.borrow())
}

/// The wrap mode GL itself holds for an image's texture: 1 repeat, 0 clamp, -1
/// unknown handle or unreadable. The second, independent view of the row above
/// -- a restore that put the mirror back and left the texture parameters is a
/// state the CPU-side reading cannot see.
#[no_mangle]
pub extern "C" fn stone_image_gl_wrap_state(handle: u64) -> c_int {
    let Some(texture) = IMAGES.with(|images| images.borrow().get(&handle).map(|i| i.texture))
    else {
        return -1;
    };
    with_gl(|gl| unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        let s = gl.get_tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S);
        let t = gl.get_tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T);
        // The two axes are read separately and disagreement refuses, for the
        // reason the blend row reads its RGB and ALPHA halves apart: the setter
        // can only express one mode, so it could not describe what it read.
        if s != t {
            return -1;
        }
        c_int::from(s == glow::REPEAT as i32)
    })
    .unwrap_or(-1)
}

// Immutable sampler objects, one per (filter, wrap) pair, created once and
// never mutated -- explicit per-draw sampling, replacing the filter baked onto
// the texture at load time. Indexed [filter][repeat], filter 0 = NEAREST.

/// Bind an image's texture to a unit, resolved here from the image handle. The
/// rendering path binds through this rather than fetching the name and handing
/// it back; the raw getter below stays for cimgui, whose consumers are GL by
/// construction.
#[no_mangle]
pub extern "C" fn stone_bind_image_texture_to_unit(handle: u64, unit: c_int) -> bool {
    let tex = sample_image_texture_for_bind(handle);
    match tex {
        Some(t) => crate::shader::stone_bind_texture_to_unit(t, unit) != 0,
        None => false,
    }
}

// Uploads bind on the current unit. Preserve that unit's binding so publishing
// one input cannot replace an earlier input of the draw being assembled.
pub(crate) fn sample_image_texture_for_bind(handle: u64) -> Option<u32> {
    if let Some(texture) = sample_image_texture(handle) {
        return Some(texture);
    }
    if handle == 0 {
        return None;
    }
    crate::image_publication::stone_image_require(handle);
    with_gl(|gl| unsafe {
        let active = gl.get_parameter_i32(glow::ACTIVE_TEXTURE) as u32;
        let binding = gl.get_parameter_i32(glow::TEXTURE_BINDING_2D) as u32;
        stone_flush_image_uploads();
        gl.active_texture(active);
        gl.bind_texture(
            glow::TEXTURE_2D,
            std::num::NonZeroU32::new(binding).map(glow::NativeTexture),
        );
    });
    sample_image_texture(handle)
}

#[no_mangle]
pub extern "C" fn stone_get_image_texture(handle: u64) -> u32 {
    sample_image_texture(handle).unwrap_or(0)
}

/// Sampling never exposes the storage allocated for an unpublished decode.
/// Failed decodes have a complete diagnostic checker and remain sampleable.
pub(crate) fn sample_image_texture(handle: u64) -> Option<u32> {
    if !crate::image_publication::stone_image_ready(handle) && !image_decode_failed(handle) {
        return None;
    }
    IMAGES.with(|images| {
        let borrowed = images.borrow();
        let img = borrowed.get(&handle)?;
        // Asking for the GL name is what a draw does just before binding, so a
        // stub reaching here is a draw sampling white. That is the contract
        // under `STONE_ASSET_STUB`, not a fault, so it is a debug line: a sim
        // whose assertion needs the file's own texels declares
        // `@sim_env("STONE_ASSET_STUB", "0")`. Reported per IMAGE, not per call
        // site: one `log_once` keyed on the site would name the first image and
        // swallow the rest.
        if img.stub
            && crate::rock_log_reexport::once_should_emit_for(
                concat!(file!(), ":", line!()),
                handle,
            )
        {
            ::log::debug!(target: "IMAGE",
                "image {} is headers-only (STONE_ASSET_STUB=1); sampling it reads white",
                stub_path(handle).unwrap_or_else(|| format!("handle {handle}")));
        }
        Some(img.texture.0.get())
    })
}

thread_local! {
    /// Paths of headers-only images, so the report above can name the asset.
    /// Only populated under `STONE_ASSET_STUB`.
    static STUB_PATHS: RefCell<HashMap<u64, String>> = RefCell::new(HashMap::new());
}

fn stub_path(handle: u64) -> Option<String> {
    STUB_PATHS.with(|p| p.borrow().get(&handle).cloned())
}

/// Free a previously stone_load_image'd handle and its GL texture.
#[no_mangle]
pub extern "C" fn stone_unload_image(handle: u64) {
    // A request destroyed before publication must not keep its bytes
    // counted, its readiness false, or a flush waiting for its decode.
    crate::image_publication::retire(handle);
    let removed = IMAGES.with(|images| images.borrow_mut().remove(&handle));
    FAILED_DECODE.with(|f| f.borrow_mut().remove(&handle));
    CAPPED_STORAGE.with(|c| c.borrow_mut().remove(&handle));
    EXACT_TEXELS.with(|e| e.borrow_mut().remove(&handle));
    MIPMAPPED_IMAGES.with(|m| m.borrow_mut().remove(&handle));
    if let Some(img) = removed {
        crate::handles::retire(Class::Image, handle);
        with_gl(|gl| unsafe {
            gl.delete_texture(img.texture);
        });
    }
}

pub(crate) fn unload_all() {
    let handles: Vec<u64> = IMAGES.with(|i| i.borrow().keys().copied().collect());
    for handle in handles {
        stone_unload_image(handle);
    }
    WHITE_IMAGE.with(|white| *white.borrow_mut() = None);
}

/// Re-decode `path` and re-upload it into the texture already bound to
/// `handle`, keeping the SAME handle and GL texture object. Existing
/// `Image(handle)` values scattered through game state stay valid -- this is
/// what makes asset hot reload reference-free. Filter/wrap state on the
/// texture is preserved (we only replace the pixel data + dimensions).
/// Returns false if the handle is unknown or the file can't be decoded.
#[no_mangle]
pub extern "C" fn stone_reload_image(handle: u64, path: *const c_char) -> bool {
    if handle == 0 || path.is_null() {
        return false;
    }
    let path_str = unsafe { CStr::from_ptr(path).to_str().unwrap_or("") };

    // A pending deferred upload for this handle would land after the reload and
    // put the pre-edit pixels back; drain it first.
    stone_flush_image_uploads();

    let (data, width, height) = match decode_rgba_image(path_str) {
        Some(t) => t,
        None => return false,
    };

    // A hot reload is an asset load like any other, so it stores at the cap --
    // and honours the exactness this handle declared when it was first loaded,
    // which is why that declaration outlives the call that made it.
    let Some(format) = IMAGES.with(|images| images.borrow().get(&handle).map(|img| img.format))
    else {
        return false;
    };
    let exact = exact_texels(handle) || format != ImageFormat::Rgba8;
    let (storage_width, storage_height) = image_decode::storage_dims(exact, width, height);
    let Some(data) = image_decode::store_at_storage(exact, data, width, height)
        .and_then(|stored| format.pack_rgba8_owned(stored))
    else {
        log_once!(
            "IMAGE",
            "stone_reload_image: cannot store {} at {}x{}",
            path_str,
            storage_width,
            storage_height
        );
        return false;
    };

    let Some((texture, fresh)) = image_write_target_stored(
        handle,
        (width, height),
        (storage_width, storage_height),
        &data,
    ) else {
        log_once!(
            "IMAGE",
            "stone_reload_image: no writable texture for handle {} ({})",
            handle,
            path_str
        );
        return false;
    };
    if fresh {
        refresh_image_mipmaps(handle, texture);
        FAILED_DECODE.with(|f| f.borrow_mut().remove(&handle));
        return true;
    }

    let uploaded = with_gl(|gl| unsafe {
        tex_image_for_format(
            gl,
            texture,
            storage_width,
            storage_height,
            format,
            Some(&data),
        );
    });
    if uploaded.is_none() {
        return false;
    }
    refresh_image_mipmaps(handle, texture);

    IMAGES.with(|images| {
        if let Some(img) = images.borrow_mut().get_mut(&handle) {
            img.width = width;
            img.height = height;
        }
    });
    record_storage(handle, (width, height), (storage_width, storage_height));
    FAILED_DECODE.with(|f| f.borrow_mut().remove(&handle));
    true
}

#[no_mangle]
pub extern "C" fn stone_get_white_image() -> u64 {
    WHITE_IMAGE.with(|wi| {
        if let Some(id) = *wi.borrow() {
            return id;
        }

        // Create 1x1 white texture
        let texture = with_gl(|gl| unsafe {
            let texture = match gl.create_texture() {
                Ok(t) => t,
                Err(e) => {
                    log_once!(
                        "IMAGE",
                        "stone_get_white_image: gl.create_texture() failed: {}",
                        e
                    );
                    return None;
                }
            };
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));

            let data: [u8; 4] = [255, 255, 255, 255];
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                1,
                1,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&data)),
            );

            Some(texture)
        });

        match texture {
            Some(Some(tex)) => {
                let id = crate::handles::mint(Class::Image);
                IMAGES.with(|images| {
                    images.borrow_mut().insert(
                        id,
                        LoadedImage {
                            texture: tex,
                            width: 1,
                            height: 1,
                            repeat: false,
                            format: ImageFormat::Rgba8,
                            stub: false,
                        },
                    );
                });
                *wi.borrow_mut() = Some(id);
                id
            }
            _ => 0,
        }
    })
}

/// GL bytes in live image level-zero storage, derived from the owning image
/// registry and the actual stored extents. Mip levels and canvas attachments
/// have separate counters so callers can choose an image-only or broad total.
pub(crate) fn live_image_bytes() -> u64 {
    IMAGES.with(|images| {
        images
            .borrow()
            .iter()
            .map(|(handle, img)| {
                let (width, height) = stored_size(*handle, (img.width, img.height));
                u64::from(width) * u64::from(height) * u64::from(img.format.bytes_per_texel())
            })
            .sum()
    })
}

pub(crate) fn live_mip_bytes() -> u64 {
    IMAGES.with(|images| {
        images
            .borrow()
            .iter()
            .map(|(handle, image)| mip_chain_bytes(*handle, image))
            .sum()
    })
}

fn mip_chain_bytes(handle: u64, image: &LoadedImage) -> u64 {
    let mipmapped = MIPMAPPED_IMAGES.with(|m| m.borrow().contains(&handle));
    if !mipmapped || image.stub {
        return 0;
    }
    let (mut width, mut height) = stored_size(handle, (image.width, image.height));
    let mut bytes = 0u64;
    while width > 1 || height > 1 {
        width = (width / 2).max(1);
        height = (height / 2).max(1);
        bytes += u64::from(width) * u64::from(height) * u64::from(image.format.bytes_per_texel());
    }
    bytes
}

pub(crate) fn live_texture_bytes() -> u64 {
    live_image_bytes() + live_mip_bytes() + crate::canvas::live_canvas_bytes()
}

/// Honest GL bytes for one live image, including its mip chain when present.
/// The census reads this per classified handle instead of multiplying
/// `stone_get_image_size` by 4, which overcounts R8 and RG8 textures. Zero for
/// an unknown handle.
#[no_mangle]
pub extern "C" fn stone_get_image_byte_len(handle: u64) -> u64 {
    IMAGES.with(|images| {
        images
            .borrow()
            .get(&handle)
            .map(|img| image_byte_len(handle, img))
            .unwrap_or(0)
    })
}

#[no_mangle]
pub extern "C" fn stone_get_image_format(handle: u64) -> c_int {
    IMAGES.with(|images| {
        images
            .borrow()
            .get(&handle)
            .map_or(-1, |image| image.format.report_code())
    })
}

#[cfg(all(test, target_os = "linux"))]
mod relight_rg8_gl_tests {
    use super::*;
    use std::ffi::CString;
    use std::io::Cursor;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::process::Command;

    const LOAD_EXACT_TEXELS: i32 = 1;
    const LOAD_DEPTH_RG8: i32 = 4;
    const LOAD_FLAGS: i32 = LOAD_EXACT_TEXELS | LOAD_DEPTH_RG8;
    const LOAD_RGBA_FLAGS: i32 = LOAD_EXACT_TEXELS;
    const LOAD_MATERIAL_RG8: i32 = 8;
    const MATERIAL_FLAGS: i32 = LOAD_EXACT_TEXELS | LOAD_MATERIAL_RG8;
    const CHILD_ENV: &str = "STONE_RELIGHT_RG8_GL_CHILD";
    const CHILD_TEST: &str = "image_texture::relight_rg8_gl_tests::depth_rg8_preserves_every_depth_code_for_sync_and_deferred";
    const MATERIAL_CHILD_TEST: &str = "image_texture::relight_rg8_gl_tests::material_rg8_preserves_asymmetric_lanes_for_sync_and_deferred";

    struct GlSession;

    impl GlSession {
        fn start() -> Self {
            assert!(crate::stone_init(), "Stone SDL initialization failed");
            let title = CString::new("relight RG8 upload test").unwrap();
            let window = crate::stone_create_window_ex(title.as_ptr(), 0, 0, 320, 240, -1, true, 0);
            if window.is_null() {
                crate::stone_teardown();
                panic!("Stone could not create the GL test window");
            }
            Self
        }
    }

    impl Drop for GlSession {
        fn drop(&mut self) {
            let _ = crate::stone_teardown();
        }
    }

    #[derive(Default)]
    struct ImageScope(Vec<u64>);

    impl ImageScope {
        fn load(&mut self, path: &CString) -> u64 {
            self.load_with_flags(path, LOAD_FLAGS)
        }

        fn load_with_flags(&mut self, path: &CString, flags: i32) -> u64 {
            let handle = stone_load_image(path.as_ptr(), 0, flags, 0, 0);
            assert_ne!(handle, 0, "depth fixture did not load");
            self.0.push(handle);
            stone_flush_image_uploads();
            handle
        }

        fn unload(&mut self, handle: u64) {
            let Some(index) = self.0.iter().position(|held| *held == handle) else {
                panic!("test tried to release an image outside its scope");
            };
            self.0.swap_remove(index);
            stone_unload_image(handle);
        }
    }

    impl Drop for ImageScope {
        fn drop(&mut self) {
            for handle in self.0.drain(..) {
                stone_unload_image(handle);
            }
        }
    }

    fn register_png(name: &str, width: u32, height: u32, pixels: &[u8]) -> CString {
        assert_eq!(pixels.len(), width as usize * height as usize * 4);
        let image = image::RgbaImage::from_raw(width, height, pixels.to_vec()).unwrap();
        let mut cursor = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut cursor, image::ImageFormat::Png)
            .expect("PNG fixture encoding failed");
        let encoded = cursor.into_inner();
        let path = CString::new(name).unwrap();
        crate::asset_registry::stone_register_asset(path.as_ptr(), encoded.as_ptr(), encoded.len());
        path
    }

    fn read_rg(handle: u64, width: u32, height: u32) -> (i32, Vec<u8>) {
        let texture = IMAGES.with(|images| images.borrow().get(&handle).unwrap().texture);
        with_gl(|gl| unsafe {
            let old_binding = gl.get_parameter_i32(glow::TEXTURE_BINDING_2D);
            let old_pack_buffer = gl.get_parameter_i32(glow::PIXEL_PACK_BUFFER_BINDING);
            let old_alignment = gl.get_parameter_i32(glow::PACK_ALIGNMENT);
            let mut pixels = vec![0; width as usize * height as usize * 2];
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            let internal =
                gl.get_tex_level_parameter_i32(glow::TEXTURE_2D, 0, glow::TEXTURE_INTERNAL_FORMAT);
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            gl.get_tex_image(
                glow::TEXTURE_2D,
                0,
                glow::RG,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixels)),
            );
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, old_alignment);
            gl.bind_texture(
                glow::TEXTURE_2D,
                (old_binding != 0).then(|| {
                    glow::NativeTexture(std::num::NonZeroU32::new(old_binding as u32).unwrap())
                }),
            );
            gl.bind_buffer(
                glow::PIXEL_PACK_BUFFER,
                (old_pack_buffer != 0).then(|| {
                    glow::NativeBuffer(std::num::NonZeroU32::new(old_pack_buffer as u32).unwrap())
                }),
            );
            (internal, pixels)
        })
        .expect("GL context unavailable while reading the fixture")
    }

    fn read_rgba(handle: u64, width: u32, height: u32) -> Vec<u8> {
        let texture = IMAGES.with(|images| images.borrow().get(&handle).unwrap().texture);
        with_gl(|gl| unsafe {
            let old_binding = gl.get_parameter_i32(glow::TEXTURE_BINDING_2D);
            let old_pack_buffer = gl.get_parameter_i32(glow::PIXEL_PACK_BUFFER_BINDING);
            let old_alignment = gl.get_parameter_i32(glow::PACK_ALIGNMENT);
            let mut pixels = vec![0; width as usize * height as usize * 4];
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            gl.get_tex_image(
                glow::TEXTURE_2D,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixels)),
            );
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, old_alignment);
            gl.bind_texture(
                glow::TEXTURE_2D,
                (old_binding != 0).then(|| {
                    glow::NativeTexture(std::num::NonZeroU32::new(old_binding as u32).unwrap())
                }),
            );
            gl.bind_buffer(
                glow::PIXEL_PACK_BUFFER,
                (old_pack_buffer != 0).then(|| {
                    glow::NativeBuffer(std::num::NonZeroU32::new(old_pack_buffer as u32).unwrap())
                }),
            );
            pixels
        })
        .expect("GL context unavailable while reading the RGBA control")
    }

    struct UnpackAlignmentGuard(i32);

    impl UnpackAlignmentGuard {
        fn set(alignment: i32) -> Self {
            let previous = unpack_alignment();
            set_unpack_alignment(alignment);
            Self(previous)
        }
    }

    impl Drop for UnpackAlignmentGuard {
        fn drop(&mut self) {
            set_unpack_alignment(self.0);
        }
    }

    fn set_unpack_alignment(alignment: i32) {
        with_gl(|gl| unsafe { gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, alignment) })
            .expect("GL context unavailable while setting unpack alignment");
    }

    fn unpack_alignment() -> i32 {
        with_gl(|gl| unsafe { gl.get_parameter_i32(glow::UNPACK_ALIGNMENT) })
            .expect("GL context unavailable while reading unpack alignment")
    }

    fn pack_state() -> (i32, i32, i32) {
        with_gl(|gl| unsafe {
            (
                gl.get_parameter_i32(glow::PACK_ALIGNMENT),
                gl.get_parameter_i32(glow::PIXEL_PACK_BUFFER_BINDING),
                gl.get_parameter_i32(glow::TEXTURE_BINDING_2D),
            )
        })
        .expect("GL context unavailable while reading pack state")
    }

    fn run_upload_case() {
        let _session = GlSession::start();
        let mut expected = Vec::with_capacity(256 * 256 * 4);
        for code in 0u32..=u16::MAX as u32 {
            expected.extend_from_slice(&[
                (code >> 8) as u8,
                code as u8,
                (code.wrapping_mul(17) & 0xff) as u8,
                code as u8,
            ]);
        }
        let full_path = register_png(
            &format!("tests/relight-depth-{}.png", std::process::id()),
            256,
            256,
            &expected,
        );
        let baseline = live_texture_bytes();
        let mut images = ImageScope::default();
        let first = images.load(&full_path);
        let rgba_control = images.load_with_flags(&full_path, LOAD_RGBA_FLAGS);
        assert_eq!(
            stone_get_image_byte_len(first),
            256 * 256 * 2,
            "depth RG8 accounts the two uploaded lanes for all 65,536 codes",
        );
        assert_eq!(
            stone_get_image_format(first),
            1,
            "the live handle reports depth RG8"
        );
        let full_readback_state = pack_state();
        let (internal, packed) = read_rg(first, 256, 256);
        assert_eq!(
            pack_state(),
            full_readback_state,
            "full-image readback restores its GL pack state"
        );
        let control_readback_state = pack_state();
        let control = read_rgba(rgba_control, 256, 256);
        assert_eq!(
            pack_state(),
            control_readback_state,
            "RGBA control readback restores its GL pack state"
        );
        assert_eq!(
            internal,
            glow::RG8 as i32,
            "depth uses sized GL_RG8 storage"
        );
        for (index, actual) in packed.as_chunks::<2>().0.iter().enumerate() {
            assert_eq!(
                actual[0],
                control[index * 4],
                "depth high byte at code {index} matches the RGBA control"
            );
            assert_eq!(
                actual[1],
                control[index * 4 + 1],
                "depth low byte at code {index} matches the RGBA control"
            );
        }
        assert_eq!(
            &packed[510..514],
            &[control[255 * 4], control[255 * 4 + 1], control[256 * 4], control[256 * 4 + 1]],
            "the RG8 bytes on either side of the low-byte rollover match the independent RGBA control",
        );

        let odd_expected: Vec<u8> = (0u32..15)
            .flat_map(|i| [(i * 11) as u8, (i * 17) as u8, (i * 23) as u8, 255])
            .collect();
        let odd_path = register_png(
            &format!("tests/relight-depth-odd-{}.png", std::process::id()),
            5,
            3,
            &odd_expected,
        );
        let original_unpack = unpack_alignment();
        let odd_rgba_control = images.load_with_flags(&odd_path, LOAD_RGBA_FLAGS);
        let odd = {
            let _unpack = UnpackAlignmentGuard::set(4);
            let unpack_before = unpack_alignment();
            assert_eq!(
                unpack_before, 4,
                "the odd-row fixture starts with four-byte unpack alignment"
            );
            let odd = images.load(&odd_path);
            assert_eq!(
                unpack_alignment(),
                unpack_before,
                "odd-width RG8 upload restores GL unpack state"
            );
            odd
        };
        assert_eq!(
            unpack_alignment(),
            original_unpack,
            "the caller's odd-row unpack alignment is restored after the test scope"
        );
        assert_eq!(stone_get_image_byte_len(odd), 5 * 3 * 2);
        let readback_before = pack_state();
        let (_, odd_packed) = read_rg(odd, 5, 3);
        assert_eq!(
            pack_state(),
            readback_before,
            "odd-width readback restores its GL pack state"
        );
        let odd_control = read_rgba(odd_rgba_control, 5, 3);
        for (index, actual) in odd_packed.as_chunks::<2>().0.iter().enumerate() {
            assert_eq!(
                actual[0],
                odd_control[index * 4],
                "odd row high byte at texel {index} matches the RGBA control"
            );
            assert_eq!(
                actual[1],
                odd_control[index * 4 + 1],
                "odd row low byte at texel {index} matches the RGBA control"
            );
        }
        assert_eq!(
            &odd_packed[8..12],
            &[
                odd_control[16],
                odd_control[17],
                odd_control[20],
                odd_control[21]
            ],
            "odd-width row rollover matches the independent RGBA control",
        );
        let alignment_before_failure = unpack_alignment();
        let alignment_cleanup = catch_unwind(AssertUnwindSafe(|| {
            let _alignment =
                UnpackAlignmentGuard::set(if alignment_before_failure == 1 { 4 } else { 1 });
            panic!("intentional unpack-alignment cleanup probe");
        }));
        assert!(
            alignment_cleanup.is_err(),
            "the unpack-alignment cleanup probe must unwind"
        );
        assert_eq!(
            unpack_alignment(),
            alignment_before_failure,
            "unwind cleanup restores the caller's unpack alignment"
        );

        images.unload(first);
        assert_eq!(stone_get_image_byte_len(first), 0);
        let reacquired = images.load(&full_path);
        assert_eq!(stone_get_image_byte_len(reacquired), 256 * 256 * 2);
        let reacquired_readback_state = pack_state();
        assert_eq!(read_rg(reacquired, 256, 256).1, packed);
        assert_eq!(
            pack_state(),
            reacquired_readback_state,
            "reacquired image readback restores its GL pack state"
        );

        let before_failure = live_texture_bytes();
        let cleanup_result = catch_unwind(AssertUnwindSafe(|| {
            let mut temporary = ImageScope::default();
            temporary.load(&full_path);
            panic!("intentional image-scope cleanup probe");
        }));
        assert!(
            cleanup_result.is_err(),
            "the intentional cleanup probe must unwind"
        );
        assert_eq!(
            live_texture_bytes(),
            before_failure,
            "unwind cleanup releases loaded textures"
        );
        images.unload(reacquired);
        images.unload(odd);
        images.unload(rgba_control);
        images.unload(odd_rgba_control);
        assert_eq!(
            live_texture_bytes(),
            baseline,
            "release and reacquire return to the original byte count"
        );
    }

    fn run_material_upload_case() {
        let _session = GlSession::start();
        let mut expected = Vec::with_capacity(256 * 4);
        for value in 0u32..=255 {
            expected.extend_from_slice(&[
                (value.wrapping_mul(37).wrapping_add(11) & 0xff) as u8,
                value as u8,
                (255 - value) as u8,
                (value.wrapping_mul(19).wrapping_add(3) & 0xff) as u8,
            ]);
        }
        let path = register_png(
            &format!("tests/relight-material-{}.png", std::process::id()),
            256,
            1,
            &expected,
        );
        let baseline = live_texture_bytes();
        let mut images = ImageScope::default();
        let rgba_control = images.load_with_flags(&path, LOAD_RGBA_FLAGS);
        let image = images.load_with_flags(&path, MATERIAL_FLAGS);
        assert_eq!(
            stone_get_image_byte_len(image),
            256 * 2,
            "material RG8 stores two lanes for every byte value"
        );
        assert_eq!(
            stone_get_image_format(image),
            3,
            "the live handle reports material RG8"
        );
        let material_readback_state = pack_state();
        let (internal, packed) = read_rg(image, 256, 1);
        assert_eq!(
            pack_state(),
            material_readback_state,
            "material readback restores its GL pack state"
        );
        let control = read_rgba(rgba_control, 256, 1);
        assert_eq!(
            internal,
            glow::RG8 as i32,
            "material uses sized GL_RG8 storage"
        );
        for value in 0..256usize {
            assert_eq!(
                packed[value * 2],
                control[value * 4 + 1],
                "roughness lane for value {value} matches the RGBA control"
            );
            assert_eq!(
                packed[value * 2 + 1],
                control[value * 4 + 2],
                "metallic lane for value {value} matches the RGBA control"
            );
        }
        images.unload(image);
        images.unload(rgba_control);
        assert_eq!(
            live_texture_bytes(),
            baseline,
            "material release restores live texture bytes"
        );
    }

    fn run_isolated(child_test: &str) {
        let output = Command::new(std::env::current_exe().expect("test binary path"))
            .arg("--exact")
            .arg(child_test)
            .arg("--nocapture")
            .env(CHILD_ENV, "1")
            .env("STONE_HEADLESS", "1")
            .env("SDL_VIDEODRIVER", "offscreen")
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
            .env("STONE_ASSET_STUB", "0")
            .env("STONE_IMAGE_MAX_SIDE", "0")
            .output()
            .expect("could not launch isolated GL upload test");
        assert!(
            output.status.success(),
            "isolated GL upload test failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn isolated_failure_case(name: &str) -> bool {
        const CHILD: &str = "STONE_DECODE_FAILURE_STORAGE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            return true;
        }
        for cap in ["0", "3"] {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    &format!("image_texture::relight_rg8_gl_tests::{name}"),
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("STONE_HEADLESS", "1")
                .env("SDL_VIDEODRIVER", "offscreen")
                .env("LIBGL_ALWAYS_SOFTWARE", "1")
                .env("STONE_ASSET_STUB", "0")
                .env("STONE_IMAGE_MAX_SIDE", cap)
                .output()
                .expect("could not launch decode-failure GL test");
            assert!(
                output.status.success(),
                "cap={cap}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        false
    }

    fn corrupt_png(good: &CString) -> CString {
        let bad = CString::new("tests/decode-storage-bad.png").unwrap();
        let mut encoded = crate::asset_registry::resolve_asset(good.to_str().unwrap()).unwrap();
        let mut off = 8;
        loop {
            let len = u32::from_be_bytes(encoded[off..off + 4].try_into().unwrap()) as usize;
            if &encoded[off + 4..off + 8] == b"IDAT" {
                assert!(len > 0);
                let at = off + 8 + len / 2;
                encoded[at] = encoded[at].wrapping_add(1);
                break;
            }
            off += 12 + len;
            assert!(off + 8 <= encoded.len(), "no IDAT in encoded PNG");
        }
        crate::asset_registry::stone_register_asset(bad.as_ptr(), encoded.as_ptr(), encoded.len());
        bad
    }

    fn driver_levels(handle: u64) -> Vec<(u32, u32, u32)> {
        let texture = IMAGES.with(|i| i.borrow().get(&handle).unwrap().texture);
        with_gl(|gl| unsafe {
            let old = gl.get_parameter_i32(glow::TEXTURE_BINDING_2D);
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            let mut levels = Vec::new();
            for level in 0..32 {
                let get = |p| gl.get_tex_level_parameter_i32(glow::TEXTURE_2D, level, p) as u32;
                let (w, h) = (get(glow::TEXTURE_WIDTH), get(glow::TEXTURE_HEIGHT));
                if w == 0 || h == 0 {
                    break;
                }
                let bits = [
                    glow::TEXTURE_RED_SIZE,
                    glow::TEXTURE_GREEN_SIZE,
                    glow::TEXTURE_BLUE_SIZE,
                    glow::TEXTURE_ALPHA_SIZE,
                ]
                .into_iter()
                .map(get)
                .sum();
                levels.push((w, h, bits));
                if w == 1 && h == 1 {
                    break;
                }
            }
            gl.bind_texture(
                glow::TEXTURE_2D,
                (old != 0)
                    .then(|| glow::NativeTexture(std::num::NonZeroU32::new(old as u32).unwrap())),
            );
            levels
        })
        .unwrap()
    }

    fn assert_failure_storage(handle: u64, control: u64, format: ImageFormat) {
        let levels = driver_levels(handle);
        assert_eq!(
            levels,
            driver_levels(control),
            "failure storage and mip levels match a valid production decode"
        );
        let driver_bytes: u64 = levels
            .iter()
            .map(|&(w, h, bits)| u64::from(w) * u64::from(h) * u64::from(bits) / 8)
            .sum();
        assert_eq!(
            stone_get_image_byte_len(handle),
            driver_bytes,
            "residency accounting matches the driver's actual channel sizes and mip extents"
        );
        let (w, h, _) = levels[0];
        assert_eq!(
            read_rg(handle, w, h).0,
            format.gl_internal() as i32,
            "a decode-failure placeholder must preserve declared GL storage"
        );
        let pixels = read_rgba(handle, w, h);
        // Explicit cell layout, independent of the upload's index arithmetic.
        let bright = [
            [true, true, false, false, true],
            [true, true, false, false, true],
            [false, false, true, true, false],
        ];
        for (y, row) in pixels.chunks_exact(w as usize * 4).enumerate() {
            for (x, actual) in row.as_chunks::<4>().0.iter().enumerate() {
                let source = if bright[y][x] {
                    DECODE_FAILED_BRIGHT
                } else {
                    DECODE_FAILED_DARK
                };
                let expected = match format {
                    ImageFormat::DepthRg8 => [source[0], source[1], 0, 255],
                    ImageFormat::MaterialRg8 => [source[1], source[2], 0, 255],
                    ImageFormat::Rgba8 => source,
                    ImageFormat::R8 => [source[0], 0, 0, 255],
                };
                assert_eq!(
                    *actual, expected,
                    "failure shader lanes at ({x},{y}) for {format:?}"
                );
            }
        }
        assert!(image_decode_failed(handle));
        assert!(
            !crate::image_publication::stone_image_ready(handle),
            "failed pixels never admit as Resident"
        );
    }

    #[test]
    fn native_teardown_retires_unpublished_requests() {
        if !isolated_failure_case("native_teardown_retires_unpublished_requests") {
            return;
        }
        let _session = GlSession::start();
        let path = register_png(
            "tests/teardown-pending.png",
            4,
            2,
            &[0, 255, 0, 255].repeat(8),
        );
        let handle = stone_load_image(path.as_ptr(), 2, LOAD_DEFERRED, 0, 0);
        assert_ne!(handle, 0);
        assert!(
            crate::image_publication::stone_image_pending_bytes() > 0,
            "the fixture owns an unpublished request"
        );
        assert!(crate::stone_teardown());
        assert_eq!(
            crate::image_publication::stone_image_pending_bytes(),
            0,
            "native teardown retires requests with their image handles"
        );
    }

    fn run_failure_storage_case() {
        let _session = GlSession::start();
        let good = register_png(
            "tests/decode-storage-good.png",
            5,
            3,
            &[0, 255, 0, 255].repeat(15),
        );
        let bad = corrupt_png(&good);
        let mut images = ImageScope::default();
        for (format, flags) in [
            (ImageFormat::DepthRg8, LOAD_FLAGS),
            (ImageFormat::MaterialRg8, MATERIAL_FLAGS),
            (ImageFormat::Rgba8, 0),
        ] {
            let control = stone_load_image(good.as_ptr(), 2, flags, 0, 0);
            images.0.push(control);
            stone_flush_image_uploads();
            assert!(!image_decode_failed(control));
            let failed = stone_load_image(bad.as_ptr(), 2, flags, 0, 0);
            images.0.push(failed);
            assert_ne!(failed, 0);
            let alignment = unpack_alignment();
            stone_flush_image_uploads();
            assert_eq!(
                unpack_alignment(),
                alignment,
                "publication restores unpack alignment"
            );
            assert_failure_storage(failed, control, format);
            images.unload(failed);
            images.unload(control);
        }
    }

    #[test]
    fn decode_failure_preserves_declared_storage() {
        if isolated_failure_case("decode_failure_preserves_declared_storage") {
            run_failure_storage_case();
        }
    }

    #[test]
    fn depth_rg8_preserves_every_depth_code_for_sync_and_deferred() {
        if std::env::var_os(CHILD_ENV).is_some() {
            run_upload_case();
            return;
        }

        run_isolated(CHILD_TEST);
    }

    #[test]
    fn material_rg8_preserves_asymmetric_lanes_for_sync_and_deferred() {
        if std::env::var_os(CHILD_ENV).is_some() {
            run_material_upload_case();
            return;
        }
        run_isolated(MATERIAL_CHILD_TEST);
    }
}

pub(crate) fn capture_resources() -> serde_json::Value {
    IMAGES.with(|images| {
        images.borrow().iter().map(|(handle, image)| serde_json::json!({
        "handle": handle, "texture": image.texture.0.get(), "width": image.width,
        "height": image.height, "format": image.format.report_code(), "stub": image.stub,
    })).collect()
    })
}
