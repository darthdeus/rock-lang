//! The single scan of a string literal's body, over RAW source.
//!
//! Three consumers need the same answer and used to derive it three ways: the
//! tokenizer (which `"` terminates the literal), the parser (where each `{...}`
//! hole sits, so its sub-expression gets a real file span) and the LSP (which
//! region a cursor is in). The parser's copy worked off the token's *processed*
//! content and so drifted one byte per resolved escape and two per `"""`
//! delimiter -- goto-definition and hover missed holes behind an escape.
//!
//! Every range here is a byte range into the `src` handed to [`scan_string`], so
//! offsets are exact by construction rather than reconstructed. The scan never
//! fails: an unterminated literal runs to end of input, which is what an editor
//! buffer looks like between keystrokes.

/// The delimiter style of a string literal, which decides its escape and
/// interpolation rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// `"..."` -- backslash escapes, interpolation.
    Quoted,
    /// `"""..."""` -- no escapes, interpolation.
    Triple,
    /// `r"..."` -- neither.
    Raw,
    /// `r"""..."""` -- neither.
    RawTriple,
    /// `'...'` -- neither.
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

    /// Whether `{...}` opens an interpolation hole. Raw and single-quoted
    /// literals are inert text.
    pub fn interpolates(self) -> bool {
        matches!(self, Flavor::Quoted | Flavor::Triple)
    }

    /// Whether the closing delimiter still terminates the literal while inside a
    /// hole. `"` does not (a hole may hold its own `"a"` literal); `"""` does,
    /// so an unclosed brace can't swallow the rest of the file.
    fn closes_inside_hole(self) -> bool {
        self != Flavor::Quoted
    }
}

/// One lexical piece of a literal's body, in `src` coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// Literal text. `text` has backslash escapes resolved per the flavor;
    /// `{{`/`}}` are left doubled -- collapsing them is the parser's step, since
    /// the token's content keeps them.
    Literal { range: (usize, usize), text: String },
    /// A `{expr}` / `{expr:spec}` hole. `range` covers the braces; `expr` and
    /// `spec` are the halves inside them. `closed` is false when the hole ran
    /// into the literal's terminator or end of input without its `}`.
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
    /// A hole's expression source for the sub-parser, plus the map from each of
    /// its byte offsets back to `src`. The enclosing literal's escape pass runs
    /// here rather than at the token, so `"{f(\"a\")}"` reaches the parser as
    /// `f("a")` while every offset still points at real source.
    pub fn hole_source(&self, src: &str, expr: (usize, usize)) -> (String, Vec<usize>) {
        let raw = &src[expr.0..expr.1];
        let mut out = String::with_capacity(raw.len());
        // `map[k]` is the `src` offset of output byte `k`; the trailing entry
        // maps the end, so an exclusive range maps as one.
        let mut map: Vec<usize> = Vec::with_capacity(raw.len() + 1);
        let mut i = 0;
        while i < raw.len() {
            let c = raw[i..].chars().next().expect("byte index on a char boundary");
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
    // A literal piece ends at the next hole or at the closer; flush what has
    // accumulated since `lit_start`.
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

    while i < n {
        if b[i..].starts_with(close) {
            flush_literal!(i);
            return Scan { flavor, pieces, end: i + close.len(), terminated: true, bad_escapes };
        }
        if flavor.escapes() && b[i] == b'\\' {
            let e = resolve_escape(src, i + 1);
            if let Some(message) = e.bad {
                bad_escapes.push(BadEscape { at: i, len: 1 + e.len, message });
            }
            if let Some(ch) = e.ch {
                lit_text.push(ch);
            }
            // A trailing `\` resolves to itself with no following bytes, so
            // this still advances and the loop cannot spin.
            i += 1 + e.len;
            continue;
        }
        if flavor.interpolates()
            && (b[i] == b'{' || b[i] == b'}')
            && b.get(i + 1) == Some(&b[i])
        {
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
        let c = src[i..].chars().next().expect("byte index on a char boundary");
        lit_text.push(c);
        i += c.len_utf8();
    }
    flush_literal!(n);
    Scan { flavor, pieces, end: n, terminated: false, bad_escapes }
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

/// One resolved escape: the char it denotes, and how many bytes FOLLOW the
/// backslash. `bad` is set for an escape that does not denote anything, in
/// which case `ch` is a best-effort stand-in that keeps the scan walking.
struct Esc {
    /// None for an escape that denotes no character at all -- a line
    /// continuation is the only one.
    ch: Option<char>,
    len: usize,
    bad: Option<String>,
}

impl Esc {
    fn ok(ch: char, len: usize) -> Esc {
        Esc { ch: Some(ch), len, bad: None }
    }
    fn empty(len: usize) -> Esc {
        Esc { ch: None, len, bad: None }
    }
    fn bad(ch: char, len: usize, message: impl Into<String>) -> Esc {
        Esc { ch: Some(ch), len, bad: Some(message.into()) }
    }
}

const RAW_HINT: &str = "; write `r\"..\"` for a literal backslash";

/// Resolve the escape whose backslash sits just before `at`.
///
/// Unknown escapes REJECT rather than standing for the escaped char: silently
/// dropping the backslash is how a `\r` becomes the letter `r`, which is a
/// string the author did not write and which no test can see.
fn resolve_escape(src: &str, at: usize) -> Esc {
    let Some(c) = src[at..].chars().next() else {
        // Trailing `\` at end of input: keep it as itself for the partial tree.
        return Esc::ok('\\', 0);
    };
    let w = c.len_utf8();
    match c {
        'n' => Esc::ok('\n', w),
        't' => Esc::ok('\t', w),
        'r' => Esc::ok('\r', w),
        '\\' | '"' | '\'' => Esc::ok(c, w),
        'u' => resolve_unicode_escape(src, at + w),
        // Line continuation: the newline and the next line's indentation are
        // both consumed, so a wrapped message is one line of text. Dropping
        // only the backslash would leave the newline and the indent IN the
        // string, which is what every wrapped literal in the corpus was
        // silently getting.
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

/// `\u{H..}` -- one to six hex digits naming a Unicode scalar value. Braced
/// rather than fixed-width so an astral codepoint is written directly; a
/// surrogate is not a scalar and is refused at both ends of its range.
fn resolve_unicode_escape(src: &str, at: usize) -> Esc {
    let b = src.as_bytes();
    if b.get(at) != Some(&b'{') {
        return Esc::bad('u', 1, "`\\u` must be written `\\u{H..}` with braces, e.g. `\\u{00c1}`");
    }
    let digits_at = at + 1;
    let mut i = digits_at;
    while i < b.len() && b[i] != b'}' {
        i += 1;
    }
    // Span consumed on every exit below, so a rejection still advances past the
    // whole escape and cannot cascade into the rest of the literal.
    let consumed = |end: usize| end - (at - 1);
    if i >= b.len() {
        return Esc::bad('u', consumed(i), "unterminated `\\u{` escape: expected a closing `}`");
    }
    let digits = &src[digits_at..i];
    let len = consumed(i + 1);
    if digits.is_empty() {
        return Esc::bad('u', len, "`\\u{}` needs at least one hex digit");
    }
    if digits.len() > 6 {
        return Esc::bad('u', len, format!(
            "`\\u{{{digits}}}` has {} hex digits; a scalar takes at most six",
            digits.len()
        ));
    }
    let Ok(cp) = u32::from_str_radix(digits, 16) else {
        return Esc::bad('u', len, format!("`\\u{{{digits}}}` is not hex"));
    };
    if (0xD800..=0xDFFF).contains(&cp) {
        return Esc::bad('u', len, format!(
            "U+{cp:04X} is a surrogate, not a Unicode scalar value"
        ));
    }
    match char::from_u32(cp) {
        Some(ch) => Esc::ok(ch, len),
        None => Esc::bad('u', len, format!(
            "U+{cp:X} is above the maximum scalar value U+10FFFF"
        )),
    }
}

/// Scan the hole opening at `src[open]` (a `{`). Returns the index just past its
/// `}` (end of input when unclosed) and the piece.
fn scan_hole(src: &str, open: usize, flavor: Flavor) -> (usize, Piece) {
    let b = src.as_bytes();
    let n = b.len();
    let close = flavor.close().as_bytes();
    let mut i = open + 1;
    let expr_start = i;
    // Bracket depth WITHIN the hole, so a `:` or `}` belonging to a nested
    // `(...)`/`[...]`/`{...}` (named args, struct literals) is not the spec
    // split or the terminator.
    let mut depth: i32 = 0;
    let mut spec_start: Option<usize> = None;
    let mut expr_end = n;
    while i < n {
        // `"""` still terminates its literal from inside a hole (see
        // `closes_inside_hole`), so an unclosed brace can't run away.
        // Regardless of bracket depth: an `inline(lua)` body's braces and
        // quotes are text, and nothing inside a hole may outlive the literal.
        if flavor.closes_inside_hole() && b[i..].starts_with(close) {
            break;
        }
        // A `\X` escape is consumed by the enclosing literal's escape pass, so
        // `\"` inside a hole is an escaped quote, NOT the start of a nested
        // literal. `hole_source` replays that pass for the sub-parser.
        if flavor.escapes() && b[i] == b'\\' {
            i += 1 + src[i + 1..].chars().next().map_or(0, char::len_utf8);
            continue;
        }
        match b[i] {
            b'}' if depth == 0 => {
                let expr = (expr_start, spec_start.map_or(i, |s| s - 1));
                let spec = spec_start.map(|s| (s, i));
                return (i + 1, Piece::Hole { range: (open, i + 1), expr, spec, closed: true });
            }
            // `::` is a path separator, not the spec split: `{Col::Green}`.
            b':' if depth == 0 && spec_start.is_none() && b.get(i + 1) == Some(&b':') => {
                i += 2;
                continue;
            }
            b':' if depth == 0 && spec_start.is_none() => {
                spec_start = Some(i + 1);
                i += 1;
                continue;
            }
            // A literal inside the hole is opaque: its quotes, braces and colons
            // belong to it, not to the hole. Only `"..."` does this -- inside
            // `\"\"\"` a lone `"` is ordinary text (that is the whole point of the
            // form), so recursing there would let a stray `{` in an
            // `inline(lua)` body run past the terminator.
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
    // Unclosed hole: everything to the terminator (or end of input) is the expr.
    let end = i.min(n);
    expr_end = expr_end.min(end);
    let expr = (expr_start, spec_start.map_or(expr_end, |s| s - 1));
    let spec = spec_start.map(|s| (s, end));
    (end, Piece::Hole { range: (open, end), expr, spec, closed: false })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(src: &str) -> Scan {
        scan_string(src, 0)
    }

    fn holes(src: &str) -> Vec<&str> {
        scan(src)
            .pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Hole { expr, .. } => Some(&src[expr.0..expr.1]),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn flavors() {
        assert_eq!(Flavor::at("\"a\"", 0), Some(Flavor::Quoted));
        assert_eq!(Flavor::at("\"\"\"a\"\"\"", 0), Some(Flavor::Triple));
        assert_eq!(Flavor::at("r\"a\"", 0), Some(Flavor::Raw));
        assert_eq!(Flavor::at("r\"\"\"a\"\"\"", 0), Some(Flavor::RawTriple));
        assert_eq!(Flavor::at("'a'", 0), Some(Flavor::SingleQuoted));
        assert_eq!(Flavor::at("abc", 0), None);
    }

    #[test]
    fn content_matches_the_tokenizer_convention() {
        // Escapes resolved, `{{` kept doubled, hole text verbatim.
        let src = "\"a\\nb{x}c{{d}}\"";
        assert_eq!(scan(src).content(src), "a\nb{x}c{{d}}");
        let src = "\"\"\"a\\nb{x}\"\"\"";
        assert_eq!(scan(src).content(src), "a\\nb{x}");
        let src = "r\"a\\nb{x}\"";
        assert_eq!(scan(src).content(src), "a\\nb{x}");
    }

    #[test]
    fn hole_ranges_are_raw_source_offsets() {
        let src = "\"a\\nb{x} c\"";
        let s = scan(src);
        let Piece::Hole { range, expr, .. } = s.pieces[1] else { panic!("{s:?}") };
        assert_eq!(&src[range.0..range.1], "{x}");
        assert_eq!(&src[expr.0..expr.1], "x");
        // The `\n` is two source bytes; deriving from processed content loses one.
        assert_eq!(expr.0, src.find('x').unwrap());
    }

    #[test]
    fn triple_quote_hole_offsets_account_for_three_delimiters() {
        let src = "\"\"\"ab{x}\"\"\"";
        let Piece::Hole { expr, .. } = scan(src).pieces[1] else { panic!() };
        assert_eq!(expr.0, src.find('x').unwrap());
    }

    #[test]
    fn raw_and_single_quoted_do_not_interpolate() {
        assert!(holes("r\"a {x} b\"").is_empty());
        assert!(holes("r\"\"\"a {x} b\"\"\"").is_empty());
        assert!(holes("'a {x} b'").is_empty());
    }

    #[test]
    fn doubled_braces_are_not_holes() {
        assert!(holes("\"{{x}}\"").is_empty());
        assert_eq!(holes("\"{{a}} {b}\""), vec!["b"]);
    }

    #[test]
    fn nesting_inside_a_hole_is_opaque() {
        assert_eq!(holes("\"{f(v, \"a}b\")}\""), vec!["f(v, \"a}b\")"]);
        assert_eq!(holes("\"{S { a: 1 }}\""), vec!["S { a: 1 }"]);
        assert_eq!(holes("\"{m[\"k:v\"]}\""), vec!["m[\"k:v\"]"]);
    }

    #[test]
    fn format_spec_splits_at_a_depth_zero_colon() {
        let src = "\"{x:>8.2}\"";
        let Piece::Hole { expr, spec, .. } = scan(src).pieces[0].clone() else { panic!() };
        assert_eq!(&src[expr.0..expr.1], "x");
        let spec = spec.unwrap();
        assert_eq!(&src[spec.0..spec.1], ">8.2");
        // A `:` inside brackets belongs to the expression.
        let src = "\"{m[\"a:b\"]}\"";
        let Piece::Hole { expr, spec, .. } = scan(src).pieces[0].clone() else { panic!() };
        assert_eq!(&src[expr.0..expr.1], "m[\"a:b\"]");
        assert_eq!(spec, None);
    }

    #[test]
    fn terminator_is_flavor_aware() {
        // A `"` inside a hole belongs to a nested literal, not the terminator.
        let src = "\"{f(\"x\")} tail\"";
        let s = scan(src);
        assert!(s.terminated && s.end == src.len(), "{s:?}");
        // `"""` terminates even from inside an unclosed hole.
        let src = "\"\"\"{oops\"\"\"";
        let s = scan(src);
        assert!(s.terminated && s.end == src.len(), "{s:?}");
    }

    #[test]
    fn unterminated_runs_to_end_of_input() {
        let s = scan("\"some text");
        assert!(!s.terminated && s.end == 10);
        let s = scan("\"x={he");
        assert!(!s.terminated);
        assert_eq!(holes("\"x={he"), vec!["he"]);
    }

    #[test]
    fn escaped_quotes_in_a_hole_are_escapes_not_a_nested_literal() {
        // Real corpus shape (`stone/engine/debug/profiling.rock`): the escape
        // belongs to the enclosing literal, so the `"` does not open a nested
        // one and the outer literal still closes where it should.
        let src = "\"[{join(parts, \\\",\\\")}]\"";
        let s = scan(src);
        assert!(s.terminated && s.end == src.len(), "{s:?}");
        assert_eq!(holes(src), vec!["join(parts, \\\",\\\")"]);
    }

    #[test]
    fn hole_source_replays_the_escape_pass_and_maps_back() {
        let src = "\"[{join(p, \\\",\\\")}]\"";
        let s = scan(src);
        let Piece::Hole { expr, .. } = s.pieces[1] else { panic!("{s:?}") };
        let (text, map) = s.hole_source(src, expr);
        assert_eq!(text, "join(p, \",\")");
        assert_eq!(map.len(), text.len() + 1);
        // Every output byte maps to the source byte it came from.
        for (k, off) in map.iter().enumerate().take(text.len()) {
            assert!(*off >= expr.0 && *off < expr.1, "map[{k}]={off} outside {expr:?}");
        }
        // The identifier is byte-identical at its mapped position.
        let j = text.find("join").unwrap();
        assert_eq!(&src[map[j]..map[j] + 4], "join");
        let p = text.find("p,").unwrap();
        assert_eq!(&src[map[p]..map[p] + 1], "p");
    }

    #[test]
    fn hole_source_is_identity_without_escapes() {
        let src = "\"{a.b(c)}\"";
        let s = scan(src);
        let Piece::Hole { expr, .. } = s.pieces[0] else { panic!() };
        let (text, map) = s.hole_source(src, expr);
        assert_eq!(text, "a.b(c)");
        assert_eq!(map, (expr.0..=expr.1).collect::<Vec<_>>());
    }

    #[test]
    fn non_ascii_text_keeps_byte_offsets() {
        let src = "\"caf\u{e9} {x}\"";
        let Piece::Hole { expr, .. } = scan(src).pieces[1] else { panic!() };
        assert_eq!(expr.0, src.find('x').unwrap());
    }
}
