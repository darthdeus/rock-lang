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

use std::fmt::{self, Write as _};

use crate::display::{DisplayPart, DisplayRead};

/// `LJ_MAX_XLEVEL` (`lj_def.h`), checked by `synlevel_begin` in `lj_parse.c`.
pub(crate) const LJ_MAX_XLEVEL: u32 = 200;

/// Leaves the rest of the cap to the enclosing statements, blocks and functions.
pub const EXPRESSION_LEVEL_BUDGET: u32 = LJ_MAX_XLEVEL - 50;

const DISPLAY_CONCAT_RUN: usize = EXPRESSION_LEVEL_BUDGET as usize;

const ACCUMULATOR: &str = "__rock_text";

struct LuaRead<'a>(&'a DisplayRead);

impl fmt::Display for LuaRead<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            DisplayRead::Member(key) => write!(out, "v.{key}"),
            DisplayRead::Key(key) => write!(out, "v[\"{key}\"]"),
            DisplayRead::Index(index) => write!(out, "v[{index}]"),
        }
    }
}

fn operand_count(parts: &[DisplayPart]) -> usize {
    parts
        .iter()
        .map(|p| match p {
            DisplayPart::UnlessNil { parts, .. } => 1 + operand_count(parts),
            _ => 1,
        })
        .sum()
}

fn write_operand(out: &mut String, part: &DisplayPart) {
    match part {
        DisplayPart::LuaExpr(e) | DisplayPart::Enum { lua: e, .. } => out.push_str(e),
        DisplayPart::Leaf(read) => {
            let value = LuaRead(read);
            let _ = write!(
                out,
                "((type({value}) == \"number\" and {value} == 0) and \"0\" or tostring({value}))"
            );
        }
        DisplayPart::Literal(text) => {
            let _ = write!(out, "\"{text}\"");
        }
        DisplayPart::UnlessNil { value, parts } => {
            let value = LuaRead(value);
            let _ = write!(out, "({value} == nil and \"nil\" or (");
            write_expr(out, parts);
            out.push_str("))");
        }
    }
}

fn write_expr<'a>(out: &mut String, parts: impl IntoIterator<Item = &'a DisplayPart>) {
    for (i, part) in parts.into_iter().enumerate() {
        if i > 0 {
            out.push_str(" .. ");
        }
        write_operand(out, part);
    }
}

/// `None` when the result does not fit in one concat run.
pub fn display_expr(parts: &[DisplayPart]) -> Option<String> {
    if operand_count(parts) > DISPLAY_CONCAT_RUN {
        return None;
    }
    let mut out = String::new();
    write_expr(&mut out, parts);
    Some(out)
}

pub fn display_return(parts: &[DisplayPart], indent: &str) -> String {
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
    run: Vec<&'a DisplayPart>,
    declared: bool,
}

impl<'a> Appender<'a> {
    fn append(&mut self, parts: &'a [DisplayPart], indent: &str) {
        for part in parts {
            match part {
                DisplayPart::UnlessNil { value, parts } => {
                    // Declares the accumulator outside the branches.
                    self.flush(indent);
                    let _ = writeln!(self.out, "{indent}if {} == nil then", LuaRead(value));
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
                _ => {
                    if self.run.len() == DISPLAY_CONCAT_RUN {
                        self.flush(indent);
                    }
                    self.run.push(part);
                }
            }
        }
    }

    fn flush(&mut self, indent: &str) {
        if self.run.is_empty() && self.declared {
            return;
        }
        if self.declared {
            let _ = write!(self.out, "{indent}{ACCUMULATOR} = {ACCUMULATOR} .. ");
        } else {
            let _ = write!(self.out, "{indent}local {ACCUMULATOR} = ");
            self.declared = true;
        }
        if self.run.is_empty() {
            self.out.push_str("\"\"");
        }
        write_expr(&mut self.out, self.run.drain(..));
        self.out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exprs(n: usize) -> Vec<DisplayPart> {
        (0..n)
            .map(|i| DisplayPart::LuaExpr(format!("e{i}")))
            .collect()
    }

    #[test]
    fn a_nullable_group_branches_outside_the_runs() {
        let mut parts = exprs(DISPLAY_CONCAT_RUN);
        parts.push(DisplayPart::UnlessNil {
            value: DisplayRead::Member("m".into()),
            parts: exprs(2),
        });
        let body = display_return(&parts, "");
        assert!(body.contains("\nif v.m == nil then\n  __rock_text = __rock_text .. \"nil\"\nelse\n  __rock_text = __rock_text .. e0 .. e1\nend\nreturn __rock_text\n"), "{body}");
        let short = vec![DisplayPart::UnlessNil {
            value: DisplayRead::Member("m".into()),
            parts: exprs(2),
        }];
        assert_eq!(
            display_expr(&short).unwrap(),
            "(v.m == nil and \"nil\" or (e0 .. e1))"
        );
    }
}
