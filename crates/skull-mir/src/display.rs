use std::fmt;

use bone_core::hir::VariantPayload;
use ustr::Ustr;

use crate::mir::MirFunc;

impl fmt::Display for MirFunc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fn {}({:?}) {{", self.name, self.params)?;
        for (id, block) in self.blocks.iter() {
            write!(f, "\n  {id:?}:")?;
            for &inst in &block.insts {
                write!(f, "\n    {inst:?}: {:?}", self.insts[inst])?;
            }
            write!(f, "\n    {:?}", block.term)?;
        }
        write!(f, "\n}}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayPart {
    /// A Lua expression evaluating to a string.
    LuaExpr(String),
    Literal(String),
    /// Render a missing inline value as nil.
    UnlessNil {
        value: DisplayRead,
        parts: Vec<DisplayPart>,
    },
    /// A scalar read, with negative zero rendered as zero.
    Leaf(DisplayRead),
    /// The enum `name` rebuilt from the leaves under `v["<prefix>.<leaf>"]`;
    /// `lua` is its Lua spelling.
    Enum {
        name: ustr::Ustr,
        prefix: String,
        lua: String,
    },
}

/// A read from the object being displayed, in the target's physical layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayRead {
    Member(String),
    Key(String),
    Index(usize),
}

impl DisplayPart {
    pub fn literal(text: &str) -> Self {
        Self::Literal(text.to_string())
    }
}

pub enum EnumDisplaySlot {
    Tuple(usize),
    Field(Ustr),
}

pub fn enum_display_parts(
    shown: &str,
    name: Ustr,
    payload: &VariantPayload,
    read: impl Fn(EnumDisplaySlot) -> DisplayRead,
) -> Vec<DisplayPart> {
    let value = |slot| vec![DisplayPart::Leaf(read(slot))];
    match payload {
        VariantPayload::Tuple(types) if !types.is_empty() => display_record(
            &format!("{shown}::{name} {{ "),
            (0..types.len()).map(|i| (format!("[{}] = ", i + 1), value(EnumDisplaySlot::Tuple(i)))),
            " }",
        ),
        VariantPayload::Record(fields) if !fields.is_empty() => {
            let mut names: Vec<_> = fields.iter().map(|field| field.name).collect();
            names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            display_record(
                &format!("{shown}::{name} {{ "),
                names
                    .into_iter()
                    .map(|field| (format!("{field} = "), value(EnumDisplaySlot::Field(field)))),
                " }",
            )
        }
        _ => vec![DisplayPart::Literal(format!("{shown}::{name}"))],
    }
}

/// `<open>` `<label_0>` `value_0` `", "` `<label_1>` `value_1` .. `<close>`.
pub fn display_record(
    open: &str,
    fields: impl IntoIterator<Item = (String, Vec<DisplayPart>)>,
    close: &str,
) -> Vec<DisplayPart> {
    let mut parts = vec![DisplayPart::literal(open)];
    for (i, (label, value)) in fields.into_iter().enumerate() {
        if i > 0 {
            parts.push(DisplayPart::literal(", "));
        }
        parts.push(DisplayPart::literal(&label));
        parts.extend(value);
    }
    parts.push(DisplayPart::literal(close));
    parts
}
