use super::{accept, consumed, enabled, reject, PROBE};
use serde_json::{json, Value};
use stone_imgui_abi::{ImDrawData, ImVector, IM_DRAW_CALLBACK_RESET_RENDER_STATE};

unsafe fn vector<'a, T>(v: &ImVector<T>) -> Result<&'a [T], String> {
    if v.size < 0 || v.capacity < v.size || (v.size > 0 && v.data.is_null()) {
        return Err("invalid ImGui vector".into());
    }
    if v.size == 0 {
        return Ok(&[]);
    }
    Ok(unsafe { std::slice::from_raw_parts(v.data, v.size as usize) })
}

fn floats(xs: &[f32]) -> Result<Value, String> {
    if xs.iter().any(|v| !v.is_finite()) {
        return Err("nonfinite ImGui geometry".into());
    }
    Ok(json!({"values": xs, "f32_bits": xs.iter().map(|v| v.to_bits()).collect::<Vec<_>>() }))
}

unsafe fn snapshot(data: *const ImDrawData) -> Result<Value, String> {
    let data = unsafe { data.as_ref() }.ok_or("null ImGui draw-data")?;
    if data.valid == 0 {
        return Err("invalid ImGui draw-data".into());
    }
    let lists = unsafe { vector(&data.cmd_lists) }?;
    if data.cmd_lists_count < 0 || lists.len() != data.cmd_lists_count as usize {
        return Err("ImGui command-list population differs".into());
    }
    let mut rows = Vec::new();
    let mut vertex_count = 0;
    let mut index_count = 0;
    for list in lists {
        let list = unsafe { list.as_ref() }.ok_or("null ImGui draw-list")?;
        let vertices = unsafe { vector(&list.vtx_buffer) }?;
        let indices = unsafe { vector(&list.idx_buffer) }?;
        let commands = unsafe { vector(&list.cmd_buffer) }?;
        vertex_count += vertices.len();
        index_count += indices.len();
        let vertices: Vec<_> = vertices.iter().map(|v| {
            Ok::<_, String>(json!({"geometry": floats(&[v.pos.x, v.pos.y, v.uv.x, v.uv.y])?, "color": v.col}))
        }).collect::<Result<_, _>>()?;
        let mut captured_commands = Vec::new();
        for cmd in commands {
            let start = cmd.idx_offset as usize;
            let end = start
                .checked_add(cmd.elem_count as usize)
                .ok_or("ImGui index range overflow")?;
            if end > indices.len()
                || indices[start..end]
                    .iter()
                    .any(|i| (*i as usize + cmd.vtx_offset as usize) >= vertices.len())
            {
                return Err("ImGui command references geometry outside its list".into());
            }
            let callback = if cmd.user_callback.is_null() {
                json!(null)
            } else if cmd.user_callback as isize == IM_DRAW_CALLBACK_RESET_RENDER_STATE {
                json!("reset_state")
            } else {
                json!({"kind": "callback", "handle": cmd.user_callback as usize})
            };
            captured_commands.push(json!({"clip": floats(&[cmd.clip_rect.x, cmd.clip_rect.y, cmd.clip_rect.z, cmd.clip_rect.w])?,
                "texture": {"kind": if cmd.tex_ref.tex_data.is_null() { "imgui_texture" } else { "imgui_texture_data" },
                    "handle": if cmd.tex_ref.tex_data.is_null() { cmd.tex_ref.tex_id } else { cmd.tex_ref.tex_data as usize as u64 }},
                "vertex_offset": cmd.vtx_offset, "index_offset": cmd.idx_offset,
                "element_count": cmd.elem_count, "callback": callback}));
        }
        rows.push(json!({"vertices": vertices, "indices": indices, "commands": captured_commands}));
    }
    if vertex_count != data.total_vtx_count.max(0) as usize
        || index_count != data.total_idx_count.max(0) as usize
    {
        return Err("ImGui geometry population differs from draw-data totals".into());
    }
    Ok(
        json!({"display_pos": floats(&[data.display_pos.x, data.display_pos.y])?,
        "display_size": floats(&[data.display_size.x, data.display_size.y])?,
        "framebuffer_scale": floats(&[data.framebuffer_scale.x, data.framebuffer_scale.y])?,
        "vertex_count": vertex_count, "index_count": index_count, "lists": rows}),
    )
}

pub(crate) unsafe fn imgui_prepared(data: *const ImDrawData) {
    if !enabled() {
        return;
    }
    match unsafe { snapshot(data) } {
        Ok(packet) => {
            if packet["vertex_count"] == 0 || packet["index_count"] == 0 {
                return;
            }
            let id = accept("imgui", "imgui_draw_data", packet);
            PROBE.with(|p| {
                let mut p = p.borrow_mut();
                p.imgui_pending = Some(id);
                p.stack.pop();
                p.active = p.stack.last().copied();
            });
        }
        Err(e) => reject(e),
    }
}

pub(crate) unsafe fn imgui_consumed(data: *const ImDrawData) {
    if !enabled() {
        return;
    }
    match unsafe { snapshot(data) } {
        Ok(packet) => {
            let id = PROBE.with(|p| p.borrow_mut().imgui_pending.take());
            if let Some(id) = id {
                PROBE.with(|p| {
                    let mut p = p.borrow_mut();
                    p.stack.push(id);
                    p.active = Some(id);
                });
                consumed("imgui", packet);
                super::stone_ui_probe_end_operation();
            }
        }
        Err(e) => reject(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;
    use stone_imgui_abi::*;

    fn vec<T>(v: &mut [T]) -> ImVector<T> {
        ImVector {
            size: v.len() as i32,
            capacity: v.len() as i32,
            data: v.as_mut_ptr(),
        }
    }

    #[test]
    fn imgui_trace_copies_clip_offsets_texture_and_geometry() {
        let mut vertices = [ImDrawVert {
            pos: ImVec2 { x: 1.25, y: 2.0 },
            uv: ImVec2 { x: 0.0, y: 1.0 },
            col: 0x11223344,
        }; 4];
        let mut indices = [0u16, 1, 2];
        let mut commands = [ImDrawCmd {
            clip_rect: ImVec4 {
                x: 2.0,
                y: 3.0,
                z: 8.0,
                w: 9.0,
            },
            tex_ref: ImTextureRef {
                tex_data: ptr::null_mut(),
                tex_id: 73,
            },
            vtx_offset: 1,
            idx_offset: 0,
            elem_count: 3,
            user_callback: ptr::null_mut(),
            user_callback_data: ptr::null_mut(),
            user_callback_data_size: 0,
            user_callback_data_offset: 0,
        }];
        let mut list = ImDrawList {
            cmd_buffer: vec(&mut commands),
            idx_buffer: vec(&mut indices),
            vtx_buffer: vec(&mut vertices),
        };
        let mut lists = [&mut list as *mut ImDrawList];
        let data = ImDrawData {
            valid: 1,
            cmd_lists_count: 1,
            total_idx_count: 3,
            total_vtx_count: 4,
            cmd_lists: vec(&mut lists),
            display_pos: ImVec2::default(),
            display_size: ImVec2 { x: 100.0, y: 50.0 },
            framebuffer_scale: ImVec2 { x: 2.0, y: 2.0 },
            owner_viewport: ptr::null_mut(),
            textures: ptr::null_mut(),
        };
        let held = unsafe { snapshot(&data) }.unwrap();
        vertices[0].pos.x = 90.0;
        commands[0].clip_rect.x = 99.0;
        assert_eq!(vertices[0].pos.x, 90.0);
        assert_eq!(commands[0].clip_rect.x, 99.0);
        assert_eq!(
            held["lists"][0]["vertices"][0]["geometry"]["values"][0],
            1.25
        );
        assert_eq!(held["lists"][0]["commands"][0]["clip"]["values"][0], 2.0);
        assert_eq!(held["lists"][0]["commands"][0]["vertex_offset"], 1);
        assert_eq!(held["lists"][0]["commands"][0]["texture"]["handle"], 73);
        let mut texture: ImTextureData = unsafe { std::mem::zeroed() };
        commands[0].tex_ref.tex_data = &mut texture;
        assert!(!commands[0].tex_ref.tex_data.is_null());
        let logical = unsafe { snapshot(&data) }.unwrap();
        assert_eq!(
            logical["lists"][0]["commands"][0]["texture"]["kind"],
            "imgui_texture_data"
        );
        texture.tex_id = 99;
        assert_eq!(texture.tex_id, 99);
        assert_eq!(unsafe { snapshot(&data) }.unwrap(), logical);
    }
}
