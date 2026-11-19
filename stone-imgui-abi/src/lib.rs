//! The consumed cimgui draw-data wire.
//!
//! cimgui's generated cdef is the external authority. These are only the
//! fields Stone and the selected renderer consume; the layout test keeps that
//! narrow wire tied to the shipped declaration.

use std::ffi::c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ImVec2 {
    pub x: f32,
    pub y: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ImVec4 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ImTextureRef {
    pub tex_data: *mut ImTextureData,
    pub tex_id: u64,
}

pub type ImDrawIdx = u16;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ImDrawVert {
    pub pos: ImVec2,
    pub uv: ImVec2,
    pub col: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ImVector<T> {
    pub size: i32,
    pub capacity: i32,
    pub data: *mut T,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ImDrawCmd {
    pub clip_rect: ImVec4,
    pub tex_ref: ImTextureRef,
    pub vtx_offset: u32,
    pub idx_offset: u32,
    pub elem_count: u32,
    pub user_callback: *mut c_void,
    pub user_callback_data: *mut c_void,
    pub user_callback_data_size: i32,
    pub user_callback_data_offset: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ImDrawList {
    pub cmd_buffer: ImVector<ImDrawCmd>,
    pub idx_buffer: ImVector<ImDrawIdx>,
    pub vtx_buffer: ImVector<ImDrawVert>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ImDrawData {
    pub valid: u8,
    pub cmd_lists_count: i32,
    pub total_idx_count: i32,
    pub total_vtx_count: i32,
    pub cmd_lists: ImVector<*mut ImDrawList>,
    pub display_pos: ImVec2,
    pub display_size: ImVec2,
    pub framebuffer_scale: ImVec2,
    pub owner_viewport: *mut c_void,
    pub textures: *mut ImVector<*mut ImTextureData>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ImTextureRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ImTextureData {
    pub unique_id: i32,
    pub status: i32,
    pub backend_user_data: *mut c_void,
    pub tex_id: u64,
    pub format: i32,
    pub width: i32,
    pub height: i32,
    pub bytes_per_pixel: i32,
    pub pixels: *mut u8,
    pub used_rect: ImTextureRect,
    pub update_rect: ImTextureRect,
    pub updates: ImVector<ImTextureRect>,
    pub unused_frames: i32,
    pub ref_count: u16,
    pub use_colors: u8,
    pub want_destroy_next_frame: u8,
}

pub const IM_TEXTURE_FORMAT_RGBA32: i32 = 0;
pub const IM_TEXTURE_STATUS_OK: i32 = 0;
pub const IM_TEXTURE_STATUS_DESTROYED: i32 = 1;
pub const IM_TEXTURE_STATUS_WANT_CREATE: i32 = 2;
pub const IM_TEXTURE_STATUS_WANT_UPDATES: i32 = 3;
pub const IM_TEXTURE_STATUS_WANT_DESTROY: i32 = 4;
pub const IM_DRAW_CALLBACK_RESET_RENDER_STATE: isize = -8;

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    const CDEF: &str = include_str!("../../packages/stone/prebuilt/cimgui/cdef.lua");

    fn assert_struct_prefix(name: &str, expected: &[&str]) {
        let (_, rest) = CDEF
            .split_once(&format!("\nstruct {name}\n{{"))
            .unwrap_or_else(|| panic!("cdef.lua no longer declares struct {name}"));
        let (body, _) = rest.split_once('}').expect("struct end");
        let actual: Vec<_> = body
            .lines()
            .map(str::trim)
            .filter(|line| line.ends_with(';'))
            .take(expected.len())
            .collect();
        assert_eq!(actual, expected, "struct {name}");
    }

    #[test]
    fn consumed_struct_prefixes_match_shipped_cdef() {
        assert_struct_prefix(
            "ImDrawData",
            &[
                "_Bool Valid;",
                "int CmdListsCount;",
                "int TotalIdxCount;",
                "int TotalVtxCount;",
                "ImVector_ImDrawListPtr CmdLists;",
                "ImVec2 DisplayPos;",
                "ImVec2 DisplaySize;",
                "ImVec2 FramebufferScale;",
                "ImGuiViewport* OwnerViewport;",
                "ImVector_ImTextureDataPtr* Textures;",
            ],
        );
        assert_struct_prefix(
            "ImDrawCmd",
            &[
                "ImVec4 ClipRect;",
                "ImTextureRef TexRef;",
                "unsigned int VtxOffset;",
                "unsigned int IdxOffset;",
                "unsigned int ElemCount;",
                "ImDrawCallback UserCallback;",
                "void* UserCallbackData;",
                "int UserCallbackDataSize;",
                "int UserCallbackDataOffset;",
            ],
        );
        assert_struct_prefix("ImDrawVert", &["ImVec2 pos;", "ImVec2 uv;", "ImU32 col;"]);
        assert!(CDEF.contains("typedef unsigned short ImDrawIdx;"));
        assert_struct_prefix(
            "ImDrawList",
            &[
                "ImVector_ImDrawCmd CmdBuffer;",
                "ImVector_ImDrawIdx IdxBuffer;",
                "ImVector_ImDrawVert VtxBuffer;",
            ],
        );
        assert_struct_prefix(
            "ImTextureData",
            &[
                "int UniqueID;",
                "ImTextureStatus Status;",
                "void* BackendUserData;",
                "ImTextureID TexID;",
                "ImTextureFormat Format;",
                "int Width;",
                "int Height;",
                "int BytesPerPixel;",
                "unsigned char* Pixels;",
                "ImTextureRect UsedRect;",
                "ImTextureRect UpdateRect;",
                "ImVector_ImTextureRect Updates;",
                "int UnusedFrames;",
                "unsigned short RefCount;",
                "_Bool UseColors;",
                "_Bool WantDestroyNextFrame;",
            ],
        );
    }

    #[test]
    fn consumed_wire_has_the_expected_64_bit_layout() {
        assert_eq!(size_of::<ImVec2>(), 8);
        assert_eq!(size_of::<ImVec4>(), 16);
        assert_eq!(size_of::<ImTextureRect>(), 8);
        assert_eq!(size_of::<ImTextureRef>(), 16);
        assert_eq!(size_of::<ImDrawIdx>(), 2);
        assert_eq!(size_of::<ImDrawVert>(), 20);
        assert_eq!(size_of::<ImVector<u8>>(), 16);
        assert_eq!(size_of::<ImDrawData>(), 72);
        assert_eq!(size_of::<ImTextureData>(), 88);
        assert_eq!(align_of::<ImDrawData>(), 8);
        assert_eq!(align_of::<ImTextureData>(), 8);

        assert_eq!(offset_of!(ImDrawVert, pos), 0);
        assert_eq!(offset_of!(ImDrawVert, uv), 8);
        assert_eq!(offset_of!(ImDrawVert, col), 16);

        assert_eq!(offset_of!(ImDrawList, cmd_buffer), 0);
        assert_eq!(offset_of!(ImDrawList, idx_buffer), 16);
        assert_eq!(offset_of!(ImDrawList, vtx_buffer), 32);

        assert_eq!(offset_of!(ImDrawData, cmd_lists), 16);
        assert_eq!(offset_of!(ImDrawData, display_pos), 32);
        assert_eq!(offset_of!(ImDrawData, display_size), 40);
        assert_eq!(offset_of!(ImDrawData, framebuffer_scale), 48);
        assert_eq!(offset_of!(ImDrawData, owner_viewport), 56);
        assert_eq!(offset_of!(ImDrawData, textures), 64);

        assert_eq!(offset_of!(ImDrawCmd, clip_rect), 0);
        assert_eq!(offset_of!(ImDrawCmd, tex_ref), 16);
        assert_eq!(offset_of!(ImDrawCmd, vtx_offset), 32);
        assert_eq!(offset_of!(ImDrawCmd, idx_offset), 36);
        assert_eq!(offset_of!(ImDrawCmd, elem_count), 40);
        assert_eq!(offset_of!(ImDrawCmd, user_callback), 48);
        assert_eq!(offset_of!(ImDrawCmd, user_callback_data), 56);
        assert_eq!(offset_of!(ImDrawCmd, user_callback_data_size), 64);
        assert_eq!(offset_of!(ImDrawCmd, user_callback_data_offset), 68);
        assert_eq!(size_of::<ImDrawCmd>(), 72);

        assert_eq!(offset_of!(ImTextureData, unique_id), 0);
        assert_eq!(offset_of!(ImTextureData, status), 4);
        assert_eq!(offset_of!(ImTextureData, backend_user_data), 8);
        assert_eq!(offset_of!(ImTextureData, tex_id), 16);
        assert_eq!(offset_of!(ImTextureData, format), 24);
        assert_eq!(offset_of!(ImTextureData, width), 28);
        assert_eq!(offset_of!(ImTextureData, height), 32);
        assert_eq!(offset_of!(ImTextureData, bytes_per_pixel), 36);
        assert_eq!(offset_of!(ImTextureData, pixels), 40);
        assert_eq!(offset_of!(ImTextureData, used_rect), 48);
        assert_eq!(offset_of!(ImTextureData, update_rect), 56);
        assert_eq!(offset_of!(ImTextureData, updates), 64);
        assert_eq!(offset_of!(ImTextureData, unused_frames), 80);
        assert_eq!(offset_of!(ImTextureData, ref_count), 84);
        assert_eq!(offset_of!(ImTextureData, use_colors), 86);
        assert_eq!(offset_of!(ImTextureData, want_destroy_next_frame), 87);
    }
}
