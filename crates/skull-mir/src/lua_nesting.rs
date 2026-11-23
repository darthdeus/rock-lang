//! Emitted Lua whose nesting must stay under LuaJIT's parser cap.
//!
//! LuaJIT refuses a chunk nested deeper than `LJ_MAX_XLEVEL` syntax levels
//! ("chunk has too many syntax levels", at load). Every `..` of a concat run
//! costs a level, as does every parenthesized operand and enclosing block, so
//! an expression whose length grows with a field or statement count must be
//! split into statements.
//!
//! Display helper bodies built here append to a local in runs of at most
//! [`EXPRESSION_LEVEL_BUDGET`] operands; the Lua emitter bounds its folds by the
//! same budget.

use std::fmt::Write as _;

/// `LJ_MAX_XLEVEL` (`lj_def.h`), checked by `synlevel_begin` in `lj_parse.c`.
pub(crate) const LJ_MAX_XLEVEL: u32 = 200;

/// Leaves the rest of the cap to the enclosing statements, blocks and functions.
pub const EXPRESSION_LEVEL_BUDGET: u32 = LJ_MAX_XLEVEL - 50;

const DISPLAY_CONCAT_RUN: usize = EXPRESSION_LEVEL_BUDGET as usize;

const ACCUMULATOR: &str = "__rock_text";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LuaDisplayPart {
    /// A Lua expression evaluating to a string.
    Expr(String),
    /// `"nil"` when the Lua expression `value` is nil, otherwise `parts`.
    UnlessNil {
        value: String,
        parts: Vec<LuaDisplayPart>,
    },
    /// The scalar read `value` as text; `lua` prints a zero that arithmetic
    /// left at -0 as `0`.
    Leaf { value: String, lua: String },
    /// The enum `name` rebuilt from the leaves under `v["<prefix>.<leaf>"]`;
    /// `lua` is its Lua spelling.
    Enum {
        name: ustr::Ustr,
        prefix: String,
        lua: String,
    },
}

impl LuaDisplayPart {
    /// `text` is spelled verbatim between quotes.
    pub fn literal(text: &str) -> Self {
        LuaDisplayPart::Expr(format!("\"{text}\""))
    }

    pub fn leaf(value: &str) -> Self {
        LuaDisplayPart::Leaf {
            value: value.to_string(),
            lua: format!(
                "((type({value}) == \"number\" and {value} == 0) and \"0\" or tostring({value}))"
            ),
        }
    }
}

/// `<open>` `<label_0>` `value_0` `", "` `<label_1>` `value_1` .. `<close>`.
pub fn display_record(
    open: &str,
    fields: impl IntoIterator<Item = (String, Vec<LuaDisplayPart>)>,
    close: &str,
) -> Vec<LuaDisplayPart> {
    let mut parts = vec![LuaDisplayPart::literal(open)];
    for (i, (label, value)) in fields.into_iter().enumerate() {
        if i > 0 {
            parts.push(LuaDisplayPart::literal(", "));
        }
        parts.push(LuaDisplayPart::literal(&label));
        parts.extend(value);
    }
    parts.push(LuaDisplayPart::literal(close));
    parts
}

fn operand_count(parts: &[LuaDisplayPart]) -> usize {
    parts
        .iter()
        .map(|p| match p {
            LuaDisplayPart::Expr(_) | LuaDisplayPart::Leaf { .. } | LuaDisplayPart::Enum { .. } => {
                1
            }
            LuaDisplayPart::UnlessNil { parts, .. } => 1 + operand_count(parts),
        })
        .sum()
}

fn write_expr(out: &mut String, parts: &[LuaDisplayPart]) {
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push_str(" .. ");
        }
        match part {
            LuaDisplayPart::Expr(e)
            | LuaDisplayPart::Leaf { lua: e, .. }
            | LuaDisplayPart::Enum { lua: e, .. } => out.push_str(e),
            LuaDisplayPart::UnlessNil { value, parts } => {
                let _ = write!(out, "({value} == nil and \"nil\" or (");
                write_expr(out, parts);
                out.push_str("))");
            }
        }
    }
}

/// `None` when the result does not fit in one concat run.
pub fn display_expr(parts: &[LuaDisplayPart]) -> Option<String> {
    if operand_count(parts) > DISPLAY_CONCAT_RUN {
        return None;
    }
    let mut out = String::new();
    write_expr(&mut out, parts);
    Some(out)
}

pub fn display_return(parts: &[LuaDisplayPart], indent: &str) -> String {
    if let Some(expr) = display_expr(parts) {
        return format!("{indent}return {expr}\n");
    }
    let mut appender = Appender {
        out: String::new(),
        run: Vec::new(),
        declared: false,
    };
    appender.append(parts, indent);
    appender.flush(indent);
    let _ = writeln!(appender.out, "{indent}return {ACCUMULATOR}");
    appender.out
}

struct Appender<'a> {
    out: String,
    run: Vec<&'a str>,
    declared: bool,
}

impl<'a> Appender<'a> {
    fn append(&mut self, parts: &'a [LuaDisplayPart], indent: &str) {
        for part in parts {
            match part {
                LuaDisplayPart::Expr(e)
                | LuaDisplayPart::Leaf { lua: e, .. }
                | LuaDisplayPart::Enum { lua: e, .. } => {
                    if self.run.len() == DISPLAY_CONCAT_RUN {
                        self.flush(indent);
                    }
                    self.run.push(e);
                }
                LuaDisplayPart::UnlessNil { value, parts } => {
                    // Declares the accumulator outside the branches.
                    self.flush(indent);
                    let _ = writeln!(self.out, "{indent}if {value} == nil then");
                    let _ = writeln!(
                        self.out,
                        "{indent}  {ACCUMULATOR} = {ACCUMULATOR} .. \"nil\""
                    );
                    let _ = writeln!(self.out, "{indent}else");
                    let inner = format!("{indent}  ");
                    self.append(parts, &inner);
                    self.flush(&inner);
                    let _ = writeln!(self.out, "{indent}end");
                }
            }
        }
    }

    fn flush(&mut self, indent: &str) {
        if self.run.is_empty() && self.declared {
            return;
        }
        let run = if self.run.is_empty() {
            "\"\"".to_string()
        } else {
            self.run.join(" .. ")
        };
        self.run.clear();
        if self.declared {
            let _ = writeln!(self.out, "{indent}{ACCUMULATOR} = {ACCUMULATOR} .. {run}");
        } else {
            let _ = writeln!(self.out, "{indent}local {ACCUMULATOR} = {run}");
            self.declared = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exprs(n: usize) -> Vec<LuaDisplayPart> {
        (0..n)
            .map(|i| LuaDisplayPart::Expr(format!("e{i}")))
            .collect()
    }

    #[test]
    fn a_nullable_group_branches_outside_the_runs() {
        let mut parts = exprs(DISPLAY_CONCAT_RUN);
        parts.push(LuaDisplayPart::UnlessNil {
            value: "v.m".to_string(),
            parts: exprs(2),
        });
        let body = display_return(&parts, "");
        assert!(body.contains("\nif v.m == nil then\n  __rock_text = __rock_text .. \"nil\"\nelse\n  __rock_text = __rock_text .. e0 .. e1\nend\nreturn __rock_text\n"), "{body}");
        let short = vec![LuaDisplayPart::UnlessNil {
            value: "v.m".to_string(),
            parts: exprs(2),
        }];
        assert_eq!(
            display_expr(&short).unwrap(),
            "(v.m == nil and \"nil\" or (e0 .. e1))"
        );
    }
}
