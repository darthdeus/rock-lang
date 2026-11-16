//! Control lines and label references shared by both Lua body layouts.

use super::*;

#[derive(Clone, Copy)]
pub(crate) enum Label {
    Follower,
    Continue,
    Header,
    Block,
}

impl Label {
    fn prefix(self) -> &'static str {
        match self {
            Self::Follower => "L",
            Self::Continue => "c",
            Self::Header => "w",
            Self::Block => "bb",
        }
    }

    fn key(self, number: u32) -> u64 {
        u64::from(number) * 4 + self as u64
    }
}

pub(crate) enum Target {
    Line(u32, u8),
    Label(u64),
}

pub(crate) struct Jump {
    pub(crate) from: u32,
    // A flat conditional edge can lie anywhere in its line's instruction range.
    pub(crate) first: u8,
    pub(crate) last: u8,
    pub(crate) target: Target,
}

#[derive(Default)]
pub(crate) struct ControlLines {
    pub(crate) lines: Vec<(usize, u8)>,
    pub(crate) jumps: Vec<Jump>,
    pub(crate) labels: Vec<(u64, u32)>,
    pub(crate) names: usize,
    /// The last whole-line goto: its byte range and target, for flat fallthrough removal.
    pub(crate) last_goto: Option<(usize, usize, Idx<Block>)>,
}

impl ControlLines {
    pub(crate) fn line(&mut self, offset: usize, instructions: u8) -> u32 {
        self.lines.push((offset, instructions));
        (self.lines.len() - 1) as u32
    }

    pub(crate) fn jump(&mut self, from: (u32, u8), to: (u32, u8)) {
        self.jumps.push(Jump {
            from: from.0,
            first: from.1,
            last: from.1,
            target: Target::Line(to.0, to.1),
        });
    }

    pub(crate) fn goto(&mut self, at: u32, first: u8, last: u8, kind: Label, number: u32) {
        self.names += 1;
        self.jumps.push(Jump {
            from: at,
            first,
            last,
            target: Target::Label(kind.key(number)),
        });
    }

    pub(crate) fn label(&mut self, offset: usize, kind: Label, number: u32) {
        let at = self.line(offset, 0);
        self.names += 1;
        self.labels.push((kind.key(number), at));
    }

    pub(crate) fn pop_goto(&mut self, offset: usize) {
        debug_assert_eq!(self.lines.last().map(|line| line.0), Some(offset));
        self.lines.pop();
        self.jumps.pop();
        self.names -= 1;
    }

    /// Preserve bbN label and goto accounting inside spliced Lua.
    pub(crate) fn raw_lua(&mut self, body: &str, base: usize) {
        let mut offset = base;
        let code_lines = lua_line_starts_in_code(body);
        for (line_index, line) in body.split_inclusive('\n').enumerate() {
            let text = line.trim();
            if code_lines[line_index]
                && let Some(number) = text
                    .strip_prefix("::")
                    .and_then(|text| text.strip_suffix("::"))
                    .and_then(label_number)
            {
                self.label(offset, Label::Block, number);
            } else if code_lines[line_index]
                && let Some((targets, instructions)) = goto_line(text)
            {
                let at = self.line(offset, instructions);
                for target in targets.into_iter().flatten() {
                    self.goto(at, 0, instructions - 1, Label::Block, target);
                }
            }
            offset += line.len();
        }
    }
}

impl Emitter<'_> {
    pub(crate) fn write_label(&mut self, out: &mut String, kind: Label, b: Idx<Block>) {
        self.control
            .label(out.len(), kind, self.func.emit_block_id(b));
        out.push_str(" ::");
        self.write_label_ref(out, kind, b);
        out.push_str("::\n");
    }

    pub(crate) fn write_goto(&mut self, out: &mut String, kind: Label, b: Idx<Block>) {
        // A structured loop-header backedge includes LuaJIT's LOOP instruction.
        let start = out.len();
        let jump = u8::from(matches!(kind, Label::Header));
        let at = self.control.line(out.len(), jump + 1);
        self.control
            .goto(at, jump, jump, kind, self.func.emit_block_id(b));
        out.push_str(" goto ");
        self.write_label_ref(out, kind, b);
        out.push('\n');
        self.control.last_goto = Some((start, out.len(), b));
    }

    pub(crate) fn write_label_ref(&self, out: &mut String, kind: Label, b: Idx<Block>) {
        out.push_str(kind.prefix());
        push_uint(out, self.func.emit_block_id(b));
    }
}

fn label_number(text: &str) -> Option<u32> {
    let digits = text.strip_prefix("bb")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// A line that transfers control: its targets, and the instructions it takes
/// at least. A condition can fold away, so only the jumps themselves count;
/// `if .. else ..` also jumps over its else arm.
fn goto_line(line: &str) -> Option<([Option<u32>; 2], u8)> {
    if let Some(rest) = line.strip_prefix("goto ") {
        return Some(([Some(label_number(rest)?), None], 1));
    }
    if let Some(rest) = line.strip_prefix("else goto ") {
        let target = rest.strip_suffix(" end")?;
        return Some(([Some(label_number(target)?), None], 1));
    }
    if !line.starts_with("if ") && !line.starts_with("elseif ") {
        return None;
    }
    if let Some(body) = line.strip_suffix(" end") {
        let (head, otherwise) = body.rsplit_once(" else goto ")?;
        let (_, then) = head.rsplit_once(" then goto ")?;
        return Some((
            [Some(label_number(then)?), Some(label_number(otherwise)?)],
            3,
        ));
    }
    let (_, then) = line.rsplit_once(" then goto ")?;
    Some(([Some(label_number(then)?), None], 1))
}
