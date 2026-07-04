//! Type-vs-expression position classification for completion.
//!
//! Whether the identifier the cursor is completing sits in a TYPE slot
//! (`let x: F|`, `fn f(p: F|)`, `-> F|`, `Vec<F|>`, `struct S { f: F| }`) or an
//! EXPRESSION slot (`let x = F|`, `return F|`, `foo(F|)`, `Point { x: F| }`,
//! `[F|]`, `x == F|`, `F| =>`). Enum completion keys off this: a type slot
//! offers the bare enum `Foo`, an expression slot offers qualified `Foo::Variant`
//! values. Pure byte heuristics, no parse -- mirrors rock-lsp's
//! `is_type_annotation_position` (`rock-lsp/src/completion.rs`) so the two
//! backends agree.

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// True if the identifier starting at `ident_start` is in a type-annotation
/// slot. Positive-evidence: only returns true when the slot is provably a type.
pub(crate) fn is_type_position(source: &[u8], ident_start: usize) -> bool {
    let mut i = ident_start.min(source.len());
    while i > 0 && source[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 {
        return false;
    }
    if i >= 2 && &source[i - 2..i] == b"->" {
        return true; // return type
    }
    if source[i - 1] == b'<' {
        // a real generic `<` is preceded by an identifier (`Vec<F|>`)
        return source
            .get(i - 2)
            .copied()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_');
    }
    if source[i - 1] == b',' && is_inside_generic_args(source, i - 1) {
        return true; // `Map<K, V|>`
    }
    // `Variant(T|)` / `Variant(A, T|)` -- a tuple-variant payload inside an
    // `enum { ... }` body is a type slot, even though `Ident(` reads as a call.
    if (source[i - 1] == b'(' || source[i - 1] == b',')
        && enclosing_paren_is_enum_variant_payload(source, i - 1)
    {
        return true;
    }
    if source[i - 1] == b':' {
        if i >= 2 && source[i - 2] == b':' {
            return false; // `::` scope resolution, not a type annotation
        }
        return colon_is_type_annotation(source, i - 1);
    }
    // `expr as F|` -- type cast. Walk back one identifier.
    if is_ident(source[i - 1]) {
        let end = i;
        let mut k = i;
        while k > 0 && is_ident(source[k - 1]) {
            k -= 1;
        }
        if &source[k..end] == b"as" && (k == 0 || !is_ident(source[k - 1])) {
            return true;
        }
    }
    // `[T|]` -- an array type. `[` is valid in both type slots (array type) and
    // expr slots (array literal / indexing), so re-classify at the bracket.
    if source[i - 1] == b'[' {
        return is_type_position(source, i - 1);
    }
    false
}

/// Positive-evidence classifier for `:` -- true only when the slot is provably a
/// declaration (`let x:`, `global x:`, `fn f(x:`, `struct S { x:`), not a
/// struct-literal field value (`Point { x: F| }`, where `:` is an expr slot).
fn colon_is_type_annotation(source: &[u8], colon_pos: usize) -> bool {
    let mut j = colon_pos;
    while j > 0 && source[j - 1].is_ascii_whitespace() {
        j -= 1;
    }
    while j > 0 && is_ident(source[j - 1]) {
        j -= 1;
    }
    while j > 0 && source[j - 1].is_ascii_whitespace() {
        j -= 1;
    }
    if j == 0 {
        return false;
    }
    match source[j - 1] {
        b'(' => true,                                // `fn f(x:` -- param decl
        b',' => is_inside_paren_list(source, j - 1), // later param `fn f(a: int, x:`
        b'{' => preceded_by_struct_keyword(source, j - 1), // struct DEF field, not a literal
        _ => {
            // walk back one more identifier, looking for `let` / `global`
            let id_end = j;
            while j > 0 && is_ident(source[j - 1]) {
                j -= 1;
            }
            matches!(&source[j..id_end], b"let" | b"global")
        }
    }
}

fn is_inside_paren_list(source: &[u8], start: usize) -> bool {
    let mut depth_brack = 0i32;
    let mut depth_brace = 0i32;
    let mut depth_angle = 0i32;
    let mut depth_paren = 0i32;
    let mut i = start;
    while i > 0 {
        match source[i - 1] {
            b')' => depth_paren += 1,
            b'(' if depth_paren > 0 => depth_paren -= 1,
            b'(' if depth_brack == 0 && depth_brace == 0 && depth_angle == 0 => return true,
            b']' => depth_brack += 1,
            b'[' if depth_brack > 0 => depth_brack -= 1,
            b'[' => return false,
            b'}' => depth_brace += 1,
            b'{' if depth_brace > 0 => depth_brace -= 1,
            b'{' => return false,
            // a `>` that is the tail of `->` / `=>` is an arrow, not a generic close
            b'>' if i >= 2 && (source[i - 2] == b'-' || source[i - 2] == b'=') => {}
            b'>' => depth_angle += 1,
            b'<' if depth_angle > 0 => depth_angle -= 1,
            _ => {}
        }
        i -= 1;
    }
    false
}

/// True if the `{` at `brace_pos` belongs to a `struct Name {` decl rather than a
/// struct literal `Name { ... }`.
fn preceded_by_struct_keyword(source: &[u8], brace_pos: usize) -> bool {
    preceded_by_keyword(source, brace_pos, b"struct")
}

/// True if the `{` at `brace_pos` belongs to an `enum Name {` decl.
fn preceded_by_enum_keyword(source: &[u8], brace_pos: usize) -> bool {
    preceded_by_keyword(source, brace_pos, b"enum")
}

/// `{` is preceded by `<kw> Name` (skipping whitespace and the name identifier).
fn preceded_by_keyword(source: &[u8], brace_pos: usize, kw: &[u8]) -> bool {
    let mut k = brace_pos;
    while k > 0 && source[k - 1].is_ascii_whitespace() {
        k -= 1;
    }
    let end = k;
    while k > 0 && is_ident(source[k - 1]) {
        k -= 1;
    }
    if k == end {
        return false; // no name before `{`
    }
    while k > 0 && source[k - 1].is_ascii_whitespace() {
        k -= 1;
    }
    let n = kw.len();
    k >= n && &source[k - n..k] == kw && (k == n || !is_ident(source[k - n - 1]))
}

/// True when `pos` (a `(` or a `,`) sits in a tuple-variant payload list inside
/// an `enum { ... }` body -- `Variant(T|)` / `Variant(A, T|)`. Such payloads are
/// type slots even though `Ident(` reads like a call in expression position.
fn enclosing_paren_is_enum_variant_payload(source: &[u8], pos: usize) -> bool {
    let paren = if source.get(pos) == Some(&b'(') {
        pos
    } else {
        // balance back to the open paren
        let mut depth_paren = 0i32;
        let mut depth_brack = 0i32;
        let mut depth_brace = 0i32;
        let mut depth_angle = 0i32;
        let mut i = pos;
        loop {
            if i == 0 {
                return false;
            }
            match source[i - 1] {
                b')' => depth_paren += 1,
                b'(' if depth_paren > 0 => depth_paren -= 1,
                b'(' if depth_brack == 0 && depth_brace == 0 && depth_angle == 0 => break,
                b']' => depth_brack += 1,
                b'[' if depth_brack > 0 => depth_brack -= 1,
                b'[' => return false,
                b'}' => depth_brace += 1,
                b'{' if depth_brace > 0 => depth_brace -= 1,
                b'{' => return false,
                b'>' if i >= 2 && (source[i - 2] == b'-' || source[i - 2] == b'=') => {}
                b'>' => depth_angle += 1,
                b'<' if depth_angle > 0 => depth_angle -= 1,
                _ => {}
            }
            i -= 1;
        }
        i - 1
    };
    // identifier (variant name) directly before the `(`
    let mut j = paren;
    while j > 0 && source[j - 1].is_ascii_whitespace() {
        j -= 1;
    }
    let id_end = j;
    while j > 0 && is_ident(source[j - 1]) {
        j -= 1;
    }
    if j == id_end {
        return false;
    }
    // nearest enclosing `{` must be an `enum Name {` header
    let mut depth_brace = 0i32;
    let mut k = j;
    while k > 0 {
        match source[k - 1] {
            b'}' => depth_brace += 1,
            b'{' if depth_brace > 0 => depth_brace -= 1,
            b'{' => return preceded_by_enum_keyword(source, k - 1),
            _ => {}
        }
        k -= 1;
    }
    false
}

fn is_inside_generic_args(bytes: &[u8], start: usize) -> bool {
    let mut depth_paren = 0i32;
    let mut depth_brack = 0i32;
    let mut depth_brace = 0i32;
    let mut i = start;
    while i > 0 {
        match bytes[i - 1] {
            b')' => depth_paren += 1,
            b'(' if depth_paren > 0 => depth_paren -= 1,
            b'(' => return false,
            b']' => depth_brack += 1,
            b'[' if depth_brack > 0 => depth_brack -= 1,
            b'[' => return false,
            b'}' => depth_brace += 1,
            b'{' if depth_brace > 0 => depth_brace -= 1,
            b'{' => return false,
            b'<' if depth_paren == 0 && depth_brack == 0 && depth_brace == 0 => {
                return bytes
                    .get(i - 2)
                    .copied()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_');
            }
            _ => {}
        }
        i -= 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::is_type_position;

    /// `|` marks the cursor (removed). Classifies at the START of the identifier
    /// the cursor is completing -- as the backend does (`offset - prefix.len()`).
    fn classify(src: &str) -> bool {
        let at = src.find('|').expect("mark the cursor with `|`");
        let cleaned: String = src[..at].to_string() + &src[at + 1..];
        let b = cleaned.as_bytes();
        let mut start = at;
        while start > 0 && (b[start - 1].is_ascii_alphanumeric() || b[start - 1] == b'_') {
            start -= 1;
        }
        is_type_position(b, start)
    }

    #[test]
    fn type_positions() {
        assert!(classify("let x: F|"), "let annotation");
        assert!(classify("fn f(p: F|)"), "param type");
        assert!(classify("fn f(a: int, p: F|)"), "later param type");
        assert!(classify("fn f() -> F|"), "return type");
        assert!(classify("struct S { field: F| }"), "struct field type");
        assert!(classify("global g: F|"), "global annotation");
        assert!(classify("let v: Vec<F|>"), "generic argument");
        assert!(classify("let m: Map<K, V|>"), "later generic argument");
        assert!(classify("let x = y as F|"), "type cast");
        assert!(classify("let a: [F|]"), "array type element");
        assert!(classify("enum E { V(F|) }"), "tuple-variant payload");
    }

    #[test]
    fn expression_positions() {
        assert!(!classify("let x = F|"), "rhs of assignment");
        assert!(!classify("return F|"), "return value");
        assert!(!classify("foo(F|)"), "call argument");
        assert!(!classify("Point { x: F| }"), "struct-literal field value");
        assert!(!classify("let a = [F|]"), "array literal element");
        assert!(!classify("if x == F|"), "comparison rhs");
        assert!(!classify("match v { F| }"), "match arm pattern");
        assert!(!classify("let s = SpellState::F|"), "after scope resolution");
    }
}
