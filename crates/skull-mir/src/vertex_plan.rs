//! A vertex buffer is laid out by C rules, not std140: a `Vec3` occupies 12
//! bytes with the next attribute immediately after.

use crate::gpu_layout::{AttrKind, vertex_attrs};
use crate::lower::RepArena;
use bone_core::workspace::Workspace;
use skull_parser::CompilerError;
use ustr::Ustr;

const COMPONENT_BYTES: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttrSlot {
    pub name: Ustr,
    pub location: u32,
    pub kind: AttrKind,
    pub byte_offset: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VertexPlan {
    pub struct_name: Ustr,
    pub slots: Vec<AttrSlot>,
    /// Bytes between consecutive elements -- the struct's own C size, so it
    /// includes any trailing padding, which is what the GPU must step by.
    pub byte_stride: usize,
}

impl VertexPlan {
    pub fn float_unit_sizes(&self) -> Result<Vec<u32>, String> {
        self.reject_integers()?;
        self.check_cumulative_offsets()?;
        Ok(self.slots.iter().map(|s| s.kind.components()).collect())
    }

    pub fn float_unit_stride(&self) -> Result<usize, String> {
        self.reject_integers()?;
        self.check_cumulative_offsets()?;
        if !self.byte_stride.is_multiple_of(COMPONENT_BYTES) {
            return Err(format!(
                "`{}` has a {}-byte stride, which is not a whole number of floats",
                self.struct_name, self.byte_stride
            ));
        }
        Ok(self.byte_stride / COMPONENT_BYTES)
    }

    /// The float-unit view carries no offsets; the renderer rebuilds them
    /// cumulatively, so every attribute must sit at `4 * cumulative components`.
    fn check_cumulative_offsets(&self) -> Result<(), String> {
        let mut want = 0usize;
        for s in &self.slots {
            if s.byte_offset != want {
                return Err(format!(
                    "`{}` field `{}` is at byte {} but the float-unit view can only \
                     describe it at {want} (offsets are rebuilt cumulatively from \
                     component counts, so this layout would bind at the wrong position)",
                    self.struct_name, s.name, s.byte_offset
                ));
            }
            want += s.kind.components() as usize * COMPONENT_BYTES;
        }
        // The engine checks `sum(sizes) == stride`, so trailing padding cannot be
        // described either.
        if want != self.byte_stride {
            return Err(format!(
                "`{}` has a {}-byte stride but its attributes span {want} bytes -- the \
                 float-unit view states only component counts and a stride, so it cannot \
                 describe the {} trailing bytes",
                self.struct_name,
                self.byte_stride,
                self.byte_stride.saturating_sub(want)
            ));
        }
        Ok(())
    }

    fn reject_integers(&self) -> Result<(), String> {
        for s in &self.slots {
            if s.kind.is_integer() {
                return Err(format!(
                    "`{}` field `{}` is an integer attribute, which cannot ride the float \
                     buffer path (it needs glVertexAttribIPointer; through \
                     glVertexAttribPointer its bits would be read as a float)",
                    self.struct_name, s.name
                ));
            }
        }
        Ok(())
    }
}

pub fn vertex_plan(
    ws: &Workspace,
    rep_arena: &RepArena,
    struct_name: Ustr,
) -> Result<VertexPlan, CompilerError> {
    let attrs = vertex_attrs(ws, struct_name)?;
    let (leaves, byte_stride, _align) = rep_arena
        .nominal_leaf_layout(ws, skull_parser::ast::FnBackend::Any, struct_name)
        .map_err(|e| {
            CompilerError::new_no_location(format!("vertex plan: `{struct_name}`: {}", e.message))
        })?;

    let reject = |msg: String| CompilerError::new_no_location(format!("vertex plan: {msg}"));

    let mut slots = Vec::with_capacity(attrs.attrs.len());
    let mut next = 0usize;
    for a in &attrs.attrs {
        let want = a.kind.components() as usize;
        let group = leaves.get(next..next + want).ok_or_else(|| {
            reject(format!(
                "`{struct_name}` field `{}` needs {want} leaves but the C layout has \
                 {} left",
                a.name,
                leaves.len().saturating_sub(next)
            ))
        })?;

        let want_cty = if a.kind.is_integer() { "int" } else { "float" };
        for (i, l) in group.iter().enumerate() {
            let path = l.path.as_str();
            let owned = path == a.name.as_str()
                || path
                    .strip_prefix(a.name.as_str())
                    .is_some_and(|r| r.starts_with('.'));
            if !owned {
                return Err(reject(format!(
                    "`{struct_name}` field `{}` expected leaf {i} to belong to it, but the \
                     C layout has `{path}` there",
                    a.name
                )));
            }
            if l.cty != want_cty {
                return Err(reject(format!(
                    "`{struct_name}` field `{}` is a {want_cty} attribute but its leaf \
                     `{path}` is a {}",
                    a.name, l.cty
                )));
            }
            // A vecN binds as one pointer with a stride, so its components must
            // be adjacent. Padding inside one attribute is not expressible.
            let expect = group[0].byte_off + i * COMPONENT_BYTES;
            if l.byte_off != expect {
                return Err(reject(format!(
                    "`{struct_name}` field `{}` is not contiguous: leaf `{path}` is at \
                     byte {}, expected {expect}",
                    a.name, l.byte_off
                )));
            }
        }

        slots.push(AttrSlot {
            name: a.name,
            location: a.location,
            kind: a.kind,
            byte_offset: group[0].byte_off,
        });
        next += want;
    }

    if next != leaves.len() {
        return Err(reject(format!(
            "`{struct_name}` has {} C leaves but its attributes account for {next} -- \
             the attribute grouping and the C layout disagree about the struct",
            leaves.len()
        )));
    }

    Ok(VertexPlan {
        struct_name,
        slots,
        byte_stride,
    })
}
