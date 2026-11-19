/// The delimiter style of a string literal, which decides its escape and
/// interpolation rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Quoted,
    Triple,
    Raw,
    RawTriple,
    SingleQuoted,
}

impl Flavor {
    /// The opening delimiter at `src[start..]`, or `None` if no literal starts
    /// there.
    pub fn at(src: &str, start: usize) -> Option<Flavor> {
        let rest = src.get(start..)?;
        Some(if rest.starts_with("r\"\"\"") {
            Flavor::RawTriple
        } else if rest.starts_with("r\"") {
            Flavor::Raw
        } else if rest.starts_with("\"\"\"") {
            Flavor::Triple
        } else if rest.starts_with('"') {
            Flavor::Quoted
        } else if rest.starts_with('\'') {
            Flavor::SingleQuoted
        } else {
            return None;
        })
    }

    fn open(self) -> &'static str {
        match self {
            Flavor::Quoted => "\"",
            Flavor::Triple => "\"\"\"",
            Flavor::Raw => "r\"",
            Flavor::RawTriple => "r\"\"\"",
            Flavor::SingleQuoted => "'",
        }
    }

    fn close(self) -> &'static str {
        match self {
            Flavor::Quoted | Flavor::Raw => "\"",
            Flavor::Triple | Flavor::RawTriple => "\"\"\"",
            Flavor::SingleQuoted => "'",
        }
    }

    fn escapes(self) -> bool {
        self == Flavor::Quoted
    }

    pub(crate) fn interpolates(self) -> bool {
        matches!(self, Flavor::Quoted | Flavor::Triple)
    }

    /// `"""` still closes the literal inside a hole, so an unclosed brace cannot swallow
    /// the rest of the file; `"` does not (a hole may hold its own `"a"`).
    fn closes_inside_hole(self) -> bool {
        self != Flavor::Quoted
    }
}

/// One lexical piece of a literal's body, in `src` coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// `text` has escapes resolved; `{{`/`}}` stay doubled, the parser collapses them.
    Literal { range: (usize, usize), text: String },
    Hole {
        range: (usize, usize),
        expr: (usize, usize),
        spec: Option<(usize, usize)>,
        closed: bool,
    },
}

impl Piece {
    pub fn range(&self) -> (usize, usize) {
        match self {
            Piece::Literal { range, .. } | Piece::Hole { range, .. } => *range,
        }
    }
}

/// A scanned string literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    pub flavor: Flavor,
    /// Body pieces in source order. Empty-text literal pieces are not emitted.
    pub pieces: Vec<Piece>,
    /// Byte index just past the closing delimiter, or `src.len()` when
    /// unterminated.
    pub end: usize,
    pub terminated: bool,
    /// Escapes that denote nothing, in source order. The scan stays infallible
    /// for the LSP's partial trees; the tokenizer rejects on the first of these.
    pub bad_escapes: Vec<BadEscape>,
}

impl Scan {
    /// A hole's source for the sub-parser plus `map[k]` = the `src` offset of output byte
    /// `k` (with a trailing end entry). The enclosing literal's escape pass runs here, so
    /// `"{f(\"a\")}"` reaches the parser as `f("a")` with offsets still on real source.
    pub fn hole_source(&self, src: &str, expr: (usize, usize)) -> (String, Vec<usize>) {
        let raw = &src[expr.0..expr.1];
        let mut out = String::with_capacity(raw.len());
        let mut map: Vec<usize> = Vec::with_capacity(raw.len() + 1);
        let mut i = 0;
        while i < raw.len() {
            let c = raw[i..]
                .chars()
                .next()
                .expect("byte index on a char boundary");
            let (ch, width) = if self.flavor.escapes() && c == '\\' {
                let e = resolve_escape(raw, i + 1);
                (e.ch, 1 + e.len)
            } else {
                (Some(c), c.len_utf8())
            };
            if let Some(ch) = ch {
                out.push(ch);
                map.resize(out.len(), expr.0 + i);
            }
            i += width;
        }
        map.push(expr.1);
        (out, map)
    }

    /// Rebuild what the lexer stores for a literal: decoded chunks spliced
    /// with the untouched `{...}` spans straight from the source.
    pub fn content(&self, src: &str) -> String {
        let mut buf = String::with_capacity(src.len().min(64));
        for part in &self.pieces {
            match part {
                Piece::Literal { text, .. } => buf.push_str(text),
                Piece::Hole { range, .. } => buf.push_str(&src[range.0..range.1]),
            }
        }
        buf
    }
}

/// Scan the string literal opening at `src[start..]`. Panics if no literal
/// opens there -- callers dispatch on [`Flavor::at`] first.
/// Every range is a byte range into `src`. Never fails: an unterminated literal runs to
/// the end of input, as an editor buffer does between keystrokes.
pub fn scan_string(src: &str, start: usize) -> Scan {
    let flavor = Flavor::at(src, start).expect("no string literal at `start`");
    let b = src.as_bytes();
    let n = b.len();
    let close = flavor.close().as_bytes();
    let mut i = start + flavor.open().len();

    let mut pieces = Vec::new();
    let mut bad_escapes: Vec<BadEscape> = Vec::new();
    let mut lit_start = i;
    let mut lit_text = String::new();
    macro_rules! flush_literal {
        ($end:expr) => {{
            let end = $end;
            if end > lit_start {
                pieces.push(Piece::Literal {
                    range: (lit_start, end),
                    text: std::mem::take(&mut lit_text),
                });
            } else {
                lit_text.clear();
            }
        }};
    }

    let plain = |c: u8| {
        c != close[0]
            && !(flavor.escapes() && c == b'\\')
            && !(flavor.interpolates() && (c == b'{' || c == b'}'))
    };
    while i < n {
        let run = b[i..].iter().position(|&c| !plain(c)).map_or(n, |p| i + p);
        if run > i {
            lit_text.push_str(&src[i..run]);
            i = run;
            continue;
        }
        if b[i..].starts_with(close) {
            flush_literal!(i);
            return Scan {
                flavor,
                pieces,
                end: i + close.len(),
                terminated: true,
                bad_escapes,
            };
        }
        if flavor.escapes() && b[i] == b'\\' {
            let e = resolve_escape(src, i + 1);
            if let Some(message) = e.bad {
                bad_escapes.push(BadEscape {
                    at: i,
                    len: 1 + e.len,
                    message,
                });
            }
            if let Some(ch) = e.ch {
                lit_text.push(ch);
            }
            // A trailing `\` resolves to itself with no following bytes; this still advances.
            i += 1 + e.len;
            continue;
        }
        if flavor.interpolates() && (b[i] == b'{' || b[i] == b'}') && b.get(i + 1) == Some(&b[i]) {
            // `{{` / `}}` emit a literal brace and never open a hole. Kept
            // doubled here; see `Piece::Literal`.
            lit_text.push(b[i] as char);
            lit_text.push(b[i] as char);
            i += 2;
            continue;
        }
        if flavor.interpolates() && b[i] == b'{' {
            flush_literal!(i);
            let hole = scan_hole(src, i, flavor);
            i = hole.0;
            pieces.push(hole.1);
            lit_start = i;
            continue;
        }
        let c = src[i..]
            .chars()
            .next()
            .expect("byte index on a char boundary");
        lit_text.push(c);
        i += c.len_utf8();
    }
    flush_literal!(n);
    Scan {
        flavor,
        pieces,
        end: n,
        terminated: false,
        bad_escapes,
    }
}

/// Why an escape is not a valid one. The scan stays infallible so the LSP keeps
/// getting a tree; the tokenizer turns the first of these into a rejection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadEscape {
    /// Byte index of the backslash in `src`.
    pub at: usize,
    /// Byte length of the whole escape, so the label underlines `\\u{XY}`
    /// rather than just its backslash.
    pub len: usize,
    pub message: String,
}

struct Esc {
    /// `None` only for a line continuation.
    ch: Option<char>,
    len: usize,
    bad: Option<String>,
}

impl Esc {
    fn ok(ch: char, len: usize) -> Esc {
        Esc {
            ch: Some(ch),
            len,
            bad: None,
        }
    }
    fn empty(len: usize) -> Esc {
        Esc {
            ch: None,
            len,
            bad: None,
        }
    }
    fn bad(ch: char, len: usize, message: impl Into<String>) -> Esc {
        Esc {
            ch: Some(ch),
            len,
            bad: Some(message.into()),
        }
    }
}

const RAW_HINT: &str = "; write `r\"..\"` for a literal backslash";

/// Unknown escapes reject: silently dropping the backslash turns `\r` into the letter `r`.
fn resolve_escape(src: &str, at: usize) -> Esc {
    let Some(c) = src[at..].chars().next() else {
        return Esc::ok('\\', 0);
    };
    let w = c.len_utf8();
    match c {
        'n' => Esc::ok('\n', w),
        't' => Esc::ok('\t', w),
        'r' => Esc::ok('\r', w),
        '\\' | '"' | '\'' => Esc::ok(c, w),
        'u' => resolve_unicode_escape(src, at + w),
        // Line continuation consumes the newline and the next line's indentation.
        '\n' => {
            let b = src.as_bytes();
            let mut j = at + w;
            while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
                j += 1;
            }
            Esc::empty(j - at)
        }
        '\r' if src[at + w..].starts_with('\n') => {
            let b = src.as_bytes();
            let mut j = at + w + 1;
            while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
                j += 1;
            }
            Esc::empty(j - at)
        }
        other => Esc::bad(
            other,
            w,
            format!("unknown string escape `\\{other}`{RAW_HINT}"),
        ),
    }
}

fn resolve_unicode_escape(src: &str, at: usize) -> Esc {
    let b = src.as_bytes();
    if b.get(at) != Some(&b'{') {
        return Esc::bad(
            'u',
            1,
            "`\\u` must be written `\\u{H..}` with braces, e.g. `\\u{00c1}`",
        );
    }
    let digits_at = at + 1;
    let mut i = digits_at;
    while i < b.len() && b[i] != b'}' {
        i += 1;
    }
    // Every exit consumes the whole escape, so a rejection cannot cascade into the rest of the literal.
    let consumed = |end: usize| end - (at - 1);
    if i >= b.len() {
        return Esc::bad(
            'u',
            consumed(i),
            "unterminated `\\u{` escape: expected a closing `}`",
        );
    }
    let digits = &src[digits_at..i];
    let len = consumed(i + 1);
    if digits.is_empty() {
        return Esc::bad('u', len, "`\\u{}` needs at least one hex digit");
    }
    if digits.len() > 6 {
        return Esc::bad(
            'u',
            len,
            format!(
                "`\\u{{{digits}}}` has {} hex digits; a scalar takes at most six",
                digits.len()
            ),
        );
    }
    let Ok(cp) = u32::from_str_radix(digits, 16) else {
        return Esc::bad('u', len, format!("`\\u{{{digits}}}` is not hex"));
    };
    if (0xD800..=0xDFFF).contains(&cp) {
        return Esc::bad(
            'u',
            len,
            format!("U+{cp:04X} is a surrogate, not a Unicode scalar value"),
        );
    }
    match char::from_u32(cp) {
        Some(ch) => Esc::ok(ch, len),
        None => Esc::bad(
            'u',
            len,
            format!("U+{cp:X} is above the maximum scalar value U+10FFFF"),
        ),
    }
}

fn scan_hole(src: &str, open: usize, flavor: Flavor) -> (usize, Piece) {
    let b = src.as_bytes();
    let n = b.len();
    let close = flavor.close().as_bytes();
    let mut i = open + 1;
    let expr_start = i;
    // Bracket depth within the hole: a `:` or `}` inside a nested bracket is neither the spec split nor the terminator.
    let mut depth: i32 = 0;
    let mut spec_start: Option<usize> = None;
    let mut expr_end = n;
    while i < n {
        // Regardless of depth: nothing inside a hole may outlive the literal.
        if flavor.closes_inside_hole() && b[i..].starts_with(close) {
            break;
        }
        // `\"` inside a hole is an escaped quote, not a nested literal; `hole_source` replays the pass.
        if flavor.escapes() && b[i] == b'\\' {
            i += 1 + src[i + 1..].chars().next().map_or(0, char::len_utf8);
            continue;
        }
        match b[i] {
            b'}' if depth == 0 => {
                let expr = (expr_start, spec_start.map_or(i, |s| s - 1));
                let spec = spec_start.map(|s| (s, i));
                return (
                    i + 1,
                    Piece::Hole {
                        range: (open, i + 1),
                        expr,
                        spec,
                        closed: true,
                    },
                );
            }
            b':' if depth == 0 && spec_start.is_none() && b.get(i + 1) == Some(&b':') => {
                i += 2;
                continue;
            }
            b':' if depth == 0 && spec_start.is_none() => {
                spec_start = Some(i + 1);
                i += 1;
                continue;
            }
            // A nested `".."` literal is opaque. Not inside `"""`, where a lone `"` is plain text.
            b'"' | b'\'' if !flavor.closes_inside_hole() => {
                if let Some(inner) = Flavor::at(src, i) {
                    i = scan_string(src, i).end.max(i + inner.open().len());
                    continue;
                }
                i += 1;
                continue;
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    let end = i.min(n);
    expr_end = expr_end.min(end);
    let expr = (expr_start, spec_start.map_or(expr_end, |s| s - 1));
    let spec = spec_start.map(|s| (s, end));
    (
        end,
        Piece::Hole {
            range: (open, end),
            expr,
            spec,
            closed: false,
        },
    )
}
