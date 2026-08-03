//! `stone/engine/web/imgui_stubs.rock` is GENERATED from the authoritative
//! declarations in `stone/engine/ui/imgui.rock`. This test regenerates it and
//! fails if the checked-in file differs, so the two cannot drift.
//!
//! It replaced a hand-maintained mirror. That mirror had silently lost seven
//! bridges, and the name-only gate that first caught them could not have seen a
//! changed SIGNATURE at all -- which is the drift that actually broke the web
//! build (`stone_play_sound` lost its `bus` param the same way). Comparing the
//! whole generated text catches names, parameters, return types and bodies
//! alike.
//!
//! Regenerate with `just gen-imgui-web-stubs`.
//!
//! Only `inline(lua)` declarations need a JS twin: a plain Rock fn emits
//! portably on every backend. The web runner never calls imgui, but decorated
//! debug systems are emit roots, so every bridge they reference must compile --
//! hence stubs rather than real ports. Widgets report not-shown/not-clicked;
//! a `(bool, T)` widget echoes its value parameter back unchanged.

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

const HEADER: &str = "\
// GENERATED from stone/engine/ui/imgui.rock -- do not edit by hand.
// Regenerate with `just gen-imgui-web-stubs`.
//
// The web runner never calls imgui, but decorated debug systems are emit roots,
// so every bridge they reference must compile. Widgets report not-shown /
// not-clicked; a `(bool, T)` widget echoes its value parameter back unchanged.
";

#[derive(Debug)]
struct Decl {
    annotations: String,
    name: String,
    params: Vec<(String, String)>,
    ret: String,
}

/// Pull every `inline(lua) fn` declaration out of imgui.rock. Deliberately not
/// a general Rock parser: it recognises the one decl shape this file uses and
/// panics on anything it cannot read, so an unhandled form is a loud failure
/// rather than a silently missing stub.
fn parse_inline_lua_decls(src: &str) -> Vec<Decl> {
    let mut out = Vec::new();
    let bytes = src.as_bytes();
    let mut search = 0usize;
    while let Some(rel) = src[search..].find("inline(lua)") {
        let at = search + rel;
        search = at + "inline(lua)".len();

        // A `"""` body may itself contain the text `inline(lua)` (Lua comments
        // discussing it) -- and, worse, a body's contents would desync the
        // parameter scan below. Only accept a match at TOP LEVEL.
        if inside_body(src, at) {
            continue;
        }

        // Skip a match inside a `//` comment line.
        let line_start = src[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let before = src[line_start..at].trim();
        if before.starts_with("//") {
            continue;
        }

        // Annotations sit before `inline(lua)` on the same line, and are the
        // ONLY thing allowed to; anything else means this is not a declaration.
        if !before.is_empty()
            && !before.split_whitespace().all(|t| t.starts_with('@'))
        {
            continue;
        }
        let annotations = before.to_string();

        let Some(fn_rel) = src[search..].find("fn ") else { continue };
        let name_start = search + fn_rel + 3;
        let name: String = src[name_start..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
            .collect();
        let after_name = name_start + name.len();
        assert!(
            bytes.get(after_name).copied() == Some(b'('),
            "imgui.rock: cannot read the parameter list of `{name}`"
        );

        // Balanced scan: a default value may itself contain parentheses.
        let mut depth = 0usize;
        let mut close = None;
        for (i, c) in src[after_name..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(after_name + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let close = close.unwrap_or_else(|| panic!("imgui.rock: unbalanced params for `{name}`"));
        let params = parse_params(&src[after_name + 1..close], &name);

        // `-> Ret` up to the `=` that introduces the body.
        let tail = &src[close + 1..];
        let eq = tail.find('=').unwrap_or_else(|| panic!("imgui.rock: no body for `{name}`"));
        let ret = tail[..eq].trim().trim_start_matches("->").trim().to_string();

        out.push(Decl { annotations, name, params, ret });
    }
    out
}

/// Is `at` inside a `"""..."""` body? Counts delimiters before it -- an odd
/// count means open.
fn inside_body(src: &str, at: usize) -> bool {
    src[..at].matches("\"\"\"").count() % 2 == 1
}

fn parse_params(raw: &str, owner: &str) -> Vec<(String, String)> {
    let mut params = Vec::new();
    let mut depth = 0usize;
    let mut cur = String::new();
    for c in raw.chars() {
        match c {
            '(' | '[' | '<' => {
                depth += 1;
                cur.push(c);
            }
            ')' | ']' | '>' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => {
                push_param(&mut params, &cur, owner);
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    push_param(&mut params, &cur, owner);
    params
}

fn push_param(params: &mut Vec<(String, String)>, raw: &str, owner: &str) {
    let raw = raw.trim();
    if raw.is_empty() {
        return;
    }
    let (name, rest) = raw
        .split_once(':')
        .unwrap_or_else(|| panic!("imgui.rock: parameter `{raw}` of `{owner}` has no type"));
    // Drop any default: the JS twin re-declares the type, not the default.
    let ty = rest.split('=').next().unwrap().trim().to_string();
    params.push((name.trim().to_string(), ty));
}

/// The stub body for a return type. Every imgui bridge is a widget or a draw
/// call, so "did not happen" is the whole contract; a value-returning widget
/// gives back the neutral value and a `(bool, T)` editor echoes its input.
fn stub_body(d: &Decl) -> String {
    let ret = d.ret.as_str();
    if ret.is_empty() {
        return String::new();
    }
    if let Some(inner) = ret.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
        let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
        assert_eq!(
            parts.len(),
            2,
            "imgui.rock: `{}` returns a {}-tuple; only (bool, T) editors are known",
            d.name,
            parts.len()
        );
        assert_eq!(parts[0], "bool", "imgui.rock: `{}` tuple does not lead with bool", d.name);
        // Convention: the label/id comes first and the edited value second.
        // Assert it rather than trusting it -- a reordered signature would
        // otherwise silently echo the label.
        let (vname, vty) = d.params.get(1).unwrap_or_else(|| {
            panic!("imgui.rock: `{}` returns (bool, T) but has no second parameter", d.name)
        });
        assert_eq!(
            vty, parts[1],
            "imgui.rock: `{}` returns (bool, {}) but its second parameter `{vname}` is {vty}; \
             the echo convention (value is the second parameter) no longer holds",
            d.name, parts[1]
        );
        return format!("return [false, {vname}];");
    }
    match ret {
        "bool" => "return false;".to_string(),
        "int" | "float" => "return 0;".to_string(),
        "Vec2" => "return { __ty: 'Vec2', x: 0, y: 0 };".to_string(),
        "string" => "return \"\";".to_string(),
        other => panic!(
            "imgui.rock: `{}` returns `{other}`, which has no known neutral stub value; \
             teach the generator or give the bridge a real JS body elsewhere",
            d.name
        ),
    }
}

fn generate(src: &str) -> String {
    let mut out = String::from(HEADER);
    out.push('\n');
    for d in parse_inline_lua_decls(src) {
        let params: Vec<String> =
            d.params.iter().map(|(n, t)| format!("{n}: {t}")).collect();
        let ret = if d.ret.is_empty() { String::new() } else { format!(" -> {}", d.ret) };
        let anns = if d.annotations.is_empty() {
            String::new()
        } else {
            format!("{} ", d.annotations)
        };
        out.push_str(&format!(
            "{anns}inline(js) fn {}({}){ret} = \"{}\"\n",
            d.name,
            params.join(", "),
            stub_body(&d)
        ));
    }
    out
}

#[test]
fn imgui_web_stubs_are_generated_from_imgui_rock() {
    let root = repo_root();
    let decls = std::fs::read_to_string(root.join("stone/engine/ui/imgui.rock"))
        .expect("read imgui.rock");
    let want = generate(&decls);
    let path = root.join("stone/engine/web/imgui_stubs.rock");

    if std::env::var("UPDATE_IMGUI_WEB_STUBS").is_ok() {
        std::fs::write(&path, &want).expect("write imgui_stubs.rock");
        return;
    }

    let got = std::fs::read_to_string(&path).expect("read imgui_stubs.rock");
    assert!(
        got == want,
        "stone/engine/web/imgui_stubs.rock is stale relative to stone/engine/ui/imgui.rock.\n\
         Regenerate with `just gen-imgui-web-stubs`.\n\
         --- checked in ---\n{got}\n--- generated ---\n{want}"
    );
}
