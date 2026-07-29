//! Where each vertex/instance attribute sits in the CPU buffer, and how far
//! apart elements are.
//!
//! This is the join between the two halves that neither owns alone:
//! [`crate::gpu_layout`] gives the GROUPING (a top-level field is one attribute
//! at one location, `pos` = `F32x2`), and [`value_leaf_layout_checked`] gives
//! the OFFSETS (the C layout, flattened to primitive leaves `pos.x`, `pos.y`).
//!
//! # Why the offsets come from there and not from arithmetic here
//!
//! Every attribute kind admitted today (`float`/`int`/`Vec2`/`Vec3`/`Vec4`) is
//! 4-byte aligned, so `4 * cumulative components` happens to equal the real C
//! layout for every struct currently expressible -- computing offsets here
//! would look correct and test green. It has a known expiry: the first `Mat4`,
//! packed-u8 colour or `bool`-adjacent attribute breaks it silently, and a
//! wrong vertex offset corrupts geometry rather than failing.
//!
//! `value_leaf_layout_checked` is the offset model the emitted chunk
//! cross-checks against LuaJIT (`ffi.offsetof` / `ffi.sizeof` assertions per
//! FFI-clean value struct, aborting at load on drift). Taking offsets from
//! anywhere else is a second, unvalidated model -- the bypass the raw walker is
//! private to prevent. See `docs/shader-binding-layout-plan.md` § Layout
//! ownership.
//!
//! A vertex buffer is laid out by C rules, NOT std140: a `Vec3` occupies 12
//! bytes with the next attribute immediately after.

use crate::gpu_layout::{vertex_attrs, AttrKind};
use crate::lower::value_leaf_layout_checked;
use bone_semantic::inference::Workspace;
use skull_parser::CompilerError;
use ustr::Ustr;

/// One attribute's place in the buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttrSlot {
    pub name: Ustr,
    pub location: u32,
    pub kind: AttrKind,
    /// Byte offset of this attribute's first component within one element.
    pub byte_offset: usize,
}

/// The buffer plan for one vertex or instance struct, in location order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VertexPlan {
    pub struct_name: Ustr,
    pub slots: Vec<AttrSlot>,
    /// Bytes between consecutive elements -- the struct's own C size, so it
    /// includes any trailing padding, which is what the GPU must step by.
    pub byte_stride: usize,
}

impl VertexPlan {
    /// Per-attribute component counts, in location order.
    ///
    /// REJECTS an integer attribute. The `gpu_mesh_*` API takes `Ptr<float>`
    /// and binds every attribute through `glVertexAttribPointer`, so an int
    /// riding that path is read as the float reinterpretation of its bits --
    /// a silent wrong-value class. `AttrKind::I32` needs
    /// `glVertexAttribIPointer`, which that API cannot express today.
    pub fn float_unit_sizes(&self) -> Result<Vec<u32>, String> {
        self.reject_integers()?;
        Ok(self.slots.iter().map(|s| s.kind.components()).collect())
    }

    /// Element stride counted in floats -- the unit `gpu_mesh_set_instances`
    /// takes. Rejects the same integer case, and a stride that is not a whole
    /// number of floats (which would mean the element is not float-packed at
    /// all, so the whole `float*` view is wrong).
    pub fn float_unit_stride(&self) -> Result<usize, String> {
        self.reject_integers()?;
        if self.byte_stride % 4 != 0 {
            return Err(format!(
                "`{}` has a {}-byte stride, which is not a whole number of floats",
                self.struct_name, self.byte_stride
            ));
        }
        Ok(self.byte_stride / 4)
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

/// The buffer plan for `struct_name`, or a spanned rejection.
pub fn vertex_plan(ws: &Workspace, struct_name: Ustr) -> Result<VertexPlan, CompilerError> {
    let attrs = vertex_attrs(ws, struct_name)?;
    let (leaves, byte_stride, _align) = value_leaf_layout_checked(ws, struct_name)
        .map_err(|e| {
            CompilerError::new_no_location(format!("vertex plan: `{struct_name}`: {e}"))
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

        // The two walks are independent, and this one matches attributes to
        // leaves POSITIONALLY. Check the names line up rather than trusting
        // that they do: a drift here binds an attribute to a neighbour's bytes
        // while every offset stays individually valid.
        let want_cty = if a.kind.is_integer() { "int" } else { "float" };
        for (i, l) in group.iter().enumerate() {
            let path = l.path.as_str();
            let owned = path == a.name.as_str()
                || path.strip_prefix(a.name.as_str()).is_some_and(|r| r.starts_with('.'));
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
            let expect = group[0].byte_off + i * 4;
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

    Ok(VertexPlan { struct_name, slots, byte_stride })
}
