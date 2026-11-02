//! Emitted Lua whose nesting must stay under LuaJIT's parser cap.
//!
//! LuaJIT refuses a chunk nested deeper than `LJ_MAX_XLEVEL` syntax levels
//! ("chunk has too many syntax levels", at load, with no Rock location). Every
//! `..` of a concat run costs a level, because concat is right-associative and
//! the parser recurses per operand, and so does every parenthesized operand and
//! every enclosing block. An emitted expression whose length grows with a field
//! or statement count must therefore be split into statements.
//!
//! The generated display helpers (`__tostring`, C-struct `inspect`) are such
//! expressions: their bodies are built here, appending to a local in runs of at
//! most [`EXPRESSION_LEVEL_BUDGET`] operands once they are longer than that.
//! The Lua emitter bounds the expressions it folds by the same budget
//! (`bound_fold_levels` in `skull-codegen-lua`).

use std::fmt::Write as _;

/// `LJ_MAX_XLEVEL` (`lj_def.h`), checked by `synlevel_begin` in `lj_parse.c`.
pub const LJ_MAX_XLEVEL: u32 = 200;

/// The syntax levels one emitted expression may use, leaving the rest of the
/// cap to the statements, blocks and function bodies that enclose it.
pub const EXPRESSION_LEVEL_BUDGET: u32 = LJ_MAX_XLEVEL - 50;

/// The most operands one display concat run holds.
const DISPLAY_CONCAT_RUN: usize = EXPRESSION_LEVEL_BUDGET as usize;

/// The local a long display body accumulates into.
const ACCUMULATOR: &str = "__rock_text";

/// One piece of a display helper's result, in rendering order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LuaDisplayPart {
    /// A Lua expression evaluating to a string.
    Expr(String),
    /// `"nil"` when the Lua expression `value` is nil, otherwise `parts`.
    UnlessNil { value: String, parts: Vec<LuaDisplayPart> },
}

impl LuaDisplayPart {
    /// A Lua string literal of `text`, spelled verbatim between quotes.
    pub fn literal(text: &str) -> Self {
        LuaDisplayPart::Expr(format!("\"{text}\""))
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
            LuaDisplayPart::Expr(_) => 1,
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
            LuaDisplayPart::Expr(e) => out.push_str(e),
            LuaDisplayPart::UnlessNil { value, parts } => {
                let _ = write!(out, "({value} == nil and \"nil\" or (");
                write_expr(out, parts);
                out.push_str("))");
            }
        }
    }
}

/// The whole result as one expression, when it fits in one concat run.
pub fn display_expr(parts: &[LuaDisplayPart]) -> Option<String> {
    if operand_count(parts) > DISPLAY_CONCAT_RUN {
        return None;
    }
    let mut out = String::new();
    write_expr(&mut out, parts);
    Some(out)
}

/// Statements returning the result, each line prefixed with `indent`.
pub fn display_return(parts: &[LuaDisplayPart], indent: &str) -> String {
    if let Some(expr) = display_expr(parts) {
        return format!("{indent}return {expr}\n");
    }
    let mut appender = Appender { out: String::new(), run: Vec::new(), declared: false };
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
                LuaDisplayPart::Expr(e) => {
                    if self.run.len() == DISPLAY_CONCAT_RUN {
                        self.flush(indent);
                    }
                    self.run.push(e);
                }
                LuaDisplayPart::UnlessNil { value, parts } => {
                    // Declares the accumulator outside the branches.
                    self.flush(indent);
                    let _ = writeln!(self.out, "{indent}if {value} == nil then");
                    let _ = writeln!(self.out, "{indent}  {ACCUMULATOR} = {ACCUMULATOR} .. \"nil\"");
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
        let run = if self.run.is_empty() { "\"\"".to_string() } else { self.run.join(" .. ") };
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
        (0..n).map(|i| LuaDisplayPart::Expr(format!("e{i}"))).collect()
    }

    #[test]
    fn a_short_body_is_one_return() {
        assert_eq!(display_return(&exprs(3), "  "), "  return e0 .. e1 .. e2\n");
    }

    #[test]
    fn a_long_body_appends_in_bounded_runs() {
        let body = display_return(&exprs(DISPLAY_CONCAT_RUN * 2 + 1), "");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 4, "{body}");
        assert!(lines[0].starts_with("local __rock_text = e0 .. "), "{body}");
        assert!(lines[1].starts_with("__rock_text = __rock_text .. "), "{body}");
        assert_eq!(lines[2], format!("__rock_text = __rock_text .. e{}", DISPLAY_CONCAT_RUN * 2));
        assert_eq!(lines[3], "return __rock_text");
        assert!(lines.iter().all(|l| l.matches(" .. ").count() <= DISPLAY_CONCAT_RUN), "{body}");
    }

    #[test]
    fn a_nullable_group_branches_outside_the_runs() {
        let mut parts = exprs(DISPLAY_CONCAT_RUN);
        parts.push(LuaDisplayPart::UnlessNil { value: "v.m".to_string(), parts: exprs(2) });
        let body = display_return(&parts, "");
        assert!(body.contains("\nif v.m == nil then\n  __rock_text = __rock_text .. \"nil\"\nelse\n  __rock_text = __rock_text .. e0 .. e1\nend\nreturn __rock_text\n"), "{body}");
        let short = vec![LuaDisplayPart::UnlessNil { value: "v.m".to_string(), parts: exprs(2) }];
        assert_eq!(display_expr(&short).unwrap(), "(v.m == nil and \"nil\" or (e0 .. e1))");
    }
}
