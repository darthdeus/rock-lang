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
    use std::path::Path;

    fn cdef_source() -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .join("packages/stone/prebuilt/cimgui/cdef.lua");
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
    }

    fn fields(src: &str, name: &str) -> Vec<(String, String)> {
        let marker = format!("struct {name}\n{{");
        let start = src
            .find(&marker)
            .unwrap_or_else(|| panic!("cdef.lua no longer declares `{marker}`"));
        let body_start = src[start..].find('{').expect("struct body") + start + 1;
        let body_end = src[body_start..].find('}').expect("struct end") + body_start;
        src[body_start..body_end]
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && line.ends_with(';'))
            .map(|line| {
                let decl = line.trim_end_matches(';').trim();
                let (ty, name) = decl.rsplit_once(char::is_whitespace).unwrap_or(("", decl));
                (
                    ty.trim().to_owned(),
                    name.trim_start_matches('*').to_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn consumed_struct_prefixes_match_shipped_cdef() {
        let src = cdef_source();
        assert_eq!(
            fields(&src, "ImDrawData")[..10],
            [
                ("_Bool".into(), "Valid".into()),
                ("int".into(), "CmdListsCount".into()),
                ("int".into(), "TotalIdxCount".into()),
                ("int".into(), "TotalVtxCount".into()),
                ("ImVector_ImDrawListPtr".into(), "CmdLists".into()),
                ("ImVec2".into(), "DisplayPos".into()),
                ("ImVec2".into(), "DisplaySize".into()),
                ("ImVec2".into(), "FramebufferScale".into()),
                ("ImGuiViewport*".into(), "OwnerViewport".into()),
                ("ImVector_ImTextureDataPtr*".into(), "Textures".into()),
            ]
        );
        assert_eq!(
            fields(&src, "ImDrawCmd")[..9],
            [
                ("ImVec4".into(), "ClipRect".into()),
                ("ImTextureRef".into(), "TexRef".into()),
                ("unsigned int".into(), "VtxOffset".into()),
                ("unsigned int".into(), "IdxOffset".into()),
                ("unsigned int".into(), "ElemCount".into()),
                ("ImDrawCallback".into(), "UserCallback".into()),
                ("void*".into(), "UserCallbackData".into()),
                ("int".into(), "UserCallbackDataSize".into()),
                ("int".into(), "UserCallbackDataOffset".into()),
            ]
        );
        assert_eq!(
            fields(&src, "ImDrawVert")[..3],
            [
                ("ImVec2".into(), "pos".into()),
                ("ImVec2".into(), "uv".into()),
                ("ImU32".into(), "col".into()),
            ]
        );
        assert!(src.contains("typedef unsigned short ImDrawIdx;"));
        assert_eq!(
            fields(&src, "ImDrawList")[..3],
            [
                ("ImVector_ImDrawCmd".into(), "CmdBuffer".into()),
                ("ImVector_ImDrawIdx".into(), "IdxBuffer".into()),
                ("ImVector_ImDrawVert".into(), "VtxBuffer".into()),
            ]
        );
        assert_eq!(
            fields(&src, "ImTextureData")[..16],
            [
                ("int".into(), "UniqueID".into()),
                ("ImTextureStatus".into(), "Status".into()),
                ("void*".into(), "BackendUserData".into()),
                ("ImTextureID".into(), "TexID".into()),
                ("ImTextureFormat".into(), "Format".into()),
                ("int".into(), "Width".into()),
                ("int".into(), "Height".into()),
                ("int".into(), "BytesPerPixel".into()),
                ("unsigned char*".into(), "Pixels".into()),
                ("ImTextureRect".into(), "UsedRect".into()),
                ("ImTextureRect".into(), "UpdateRect".into()),
                ("ImVector_ImTextureRect".into(), "Updates".into()),
                ("int".into(), "UnusedFrames".into()),
                ("unsigned short".into(), "RefCount".into()),
                ("_Bool".into(), "UseColors".into()),
                ("_Bool".into(), "WantDestroyNextFrame".into()),
            ]
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
