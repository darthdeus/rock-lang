use super::*;

const RENAME_PARAM_FROM_CALLER_NAMED_LABEL: &str = r#"fn spawn_it(count: int, size: float) {
    print(count)
    print(size)
}

fn spawn_it(count: string) {
    print(count)
}

fn main() {
    spawn_it(count: 3, size: 1.0)
    spawn_it(count: "a")
}"#;

#[test]
fn rename_param_from_caller_named_label() {
    let source = RENAME_PARAM_FROM_CALLER_NAMED_LABEL;
    let project = Project::single("rename_param_from_caller_named_label", source);
    let reply = project.cli("rename", Some((10, 13)), &["amount"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        (r#"fn spawn_it(amount: int, size: float) {
    print(amount)
    print(size)
}

fn spawn_it(count: string) {
    print(count)
}

fn main() {
    spawn_it(amount: 3, size: 1.0)
    spawn_it(count: "a")
}"#)
        .trim_end()
    );
}

#[test]
fn rename_param_from_caller_named_label_other_overload() {
    let source = RENAME_PARAM_FROM_CALLER_NAMED_LABEL;
    let project = Project::single(
        "rename_param_from_caller_named_label_other_overload",
        source,
    );
    let reply = project.cli("rename", Some((11, 13)), &["label"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        (r#"fn spawn_it(count: int, size: float) {
    print(count)
    print(size)
}

fn spawn_it(label: string) {
    print(label)
}

fn main() {
    spawn_it(count: 3, size: 1.0)
    spawn_it(label: "a")
}"#)
        .trim_end()
    );
}

const RENAME_MACRO_NAME_FROM_CALL_REFUSED: &str = r#"macro double(x: expr) -> int { x + x }

fn main() {
    print(double(5))
}"#;

#[test]
fn rename_macro_name_from_call_refused() {
    let source = RENAME_MACRO_NAME_FROM_CALL_REFUSED;
    let project = Project::single("rename_macro_name_from_call_refused", source);
    let reply = project.cli("rename", Some((3, 10)), &["twice"]);
    contains_text(string(&reply["error"]), &["cannot rename a macro"]);
    assert_no_rename_changes(&reply);
}

#[test]
fn rename_macro_name_from_decl_refused() {
    let source = RENAME_MACRO_NAME_FROM_CALL_REFUSED;
    let project = Project::single("rename_macro_name_from_decl_refused", source);
    let reply = project.cli("rename", Some((0, 6)), &["twice"]);
    contains_text(string(&reply["error"]), &["cannot rename a macro"]);
    assert_no_rename_changes(&reply);
}

#[test]
fn rename_local_used_through_caller_scope_macro_refused() {
    let source = r#"macro bump() { ^count = ^count + 1 }

fn main() {
    let count = 0
    bump()
    print(count)
}"#;
    let project = Project::single(
        "rename_local_used_through_caller_scope_macro_refused",
        source,
    );
    let reply = project.cli("rename", Some((3, 8)), &["total"]);
    contains_text(
        string(&reply["error"]),
        &[".rock:5 that rename cannot rewrite"],
    );
    assert_no_rename_changes(&reply);
}

#[test]
fn rename_global_used_through_macro_global_refused() {
    let source = r#"global SCORE: int = 0

macro bump() { _G.SCORE = _G.SCORE + 1 }

fn main() {
    bump()
    print(SCORE)
}"#;
    let project = Project::single("rename_global_used_through_macro_global_refused", source);
    let reply = project.cli("rename", Some((0, 7)), &["POINTS"]);
    contains_text(
        string(&reply["error"]),
        &[".rock:6 that rename cannot rewrite"],
    );
    assert_no_rename_changes(&reply);
}

#[test]
fn rename_macro_param() {
    let source = RENAME_MACRO_NAME_FROM_CALL_REFUSED;
    let project = Project::single("rename_macro_param", source);
    let reply = project.cli("rename", Some((0, 13)), &["v"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        (&source.replace(
            "macro double(x: expr) -> int { x + x }",
            "macro double(v: expr) -> int { v + v }"
        ))
            .trim_end()
    );
}

#[test]
fn rename_local_passed_as_macro_arg() {
    let source = r#"macro double(x: expr) -> int { x + x }

fn main() {
    let n = 5
    print(double(n))
}"#;
    let project = Project::single("rename_local_passed_as_macro_arg", source);
    let reply = project.cli("rename", Some((3, 8)), &["m"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        (&source
            .replace("    let n = 5", "    let m = 5")
            .replace("    print(double(n))", "    print(double(m))"))
            .trim_end()
    );
}

#[test]
fn rename_fn_called_inside_macro_body() {
    let source = r#"fn helper(x: int) -> int {
    return x + 1
}

macro twice(x: expr) -> int { helper(helper(x)) }

fn main() {
    print(twice(1))
    print(helper(2))
}"#;
    let project = Project::single("rename_fn_called_inside_macro_body", source);
    let reply = project.cli("rename", Some((0, 3)), &["inc"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        (&source.replace("helper", "inc")).trim_end()
    );
}

fn rename_inline_lua_param_refused_source() -> String {
    RENAME_MACRO_NAME_FROM_CALL_REFUSED
        .replace(
            "macro double(x: expr) -> int { x + x }",
            "inline(lua) fn probe(x: int) -> int = \"return x + 1\"",
        )
        .replace("    print(double(5))", "    print(probe(1))")
}

#[test]
fn rename_inline_lua_param_refused() {
    let source = &rename_inline_lua_param_refused_source();
    let project = Project::single("rename_inline_lua_param_refused", source);
    let reply = project.cli("rename", Some((0, 21)), &["y"]);
    contains_text(
        string(&reply["error"]),
        &["cannot rename `x`: the opaque inline(lua) body of `probe` ("],
    );
    assert_no_rename_changes(&reply);
}

#[test]
fn rename_inline_js_param_refused() {
    let source = &rename_inline_lua_param_refused_source().replace(
        "inline(lua) fn probe(x: int) -> int = \"return x + 1\"",
        "inline(js) fn probe(x: int) -> int = \"return x + 1;\"",
    );
    let project = Project::single("rename_inline_js_param_refused", source);
    let reply = project.cli("rename", Some((0, 20)), &["y"]);
    contains_text(
        string(&reply["error"]),
        &["the opaque inline(js) body of `probe`"],
    );
    assert_no_rename_changes(&reply);
}

#[test]
fn rename_nomangle_fn_called_from_inline_body_refused() {
    let source = r#"@nomangle
fn helper(x: int) -> int {
    return x * 2
}

inline(lua) fn call_helper(x: int) -> int = "return helper(x)"

fn main() {
    print(call_helper(1))
}"#;
    let project = Project::single("rename_nomangle_fn_called_from_inline_body_refused", source);
    let reply = project.cli("rename", Some((1, 3)), &["helper2"]);
    contains_text(
        string(&reply["error"]),
        &["cannot rename `helper`: `@nomangle` keeps its emitted name"],
    );
    assert_no_rename_changes(&reply);
}

#[test]
fn rename_nomangle_fn_from_call_site_refused() {
    let source = r#"@nomangle
fn helper(x: int) -> int {
    return x * 2
}

fn main() {
    print(helper(1))
}"#;
    let project = Project::single("rename_nomangle_fn_from_call_site_refused", source);
    let reply = project.cli("rename", Some((6, 10)), &["helper2"]);
    contains_text(
        string(&reply["error"]),
        &["`@nomangle` keeps its emitted name, which code outside Rock calls"],
    );
    assert_no_rename_changes(&reply);
}

#[test]
fn rename_field_read_by_inline_body_refused() {
    let source = r#"struct Mob {
    hp: int,
}

inline(lua) fn peek(m: Mob) -> int = "return m.hp"

fn main() {
    let m = Mob { hp: 4 }
    print(peek(m))
}"#;
    let project = Project::single("rename_field_read_by_inline_body_refused", source);
    let reply = project.cli("rename", Some((1, 4)), &["health"]);
    contains_text(
        string(&reply["error"]),
        &["the opaque inline(lua) body of `peek`"],
    );
    assert_no_rename_changes(&reply);
}

#[test]
fn rename_global_read_by_inline_body_refused() {
    let source = r#"global SCORE: int = 0

inline(lua) fn peek() -> int = "return SCORE"

fn main() {
    print(peek())
}"#;
    let project = Project::single("rename_global_read_by_inline_body_refused", source);
    let reply = project.cli("rename", Some((0, 7)), &["POINTS"]);
    contains_text(
        string(&reply["error"]),
        &["the opaque inline(lua) body of `peek`"],
    );
    assert_no_rename_changes(&reply);
}

#[test]
fn rename_struct_named_by_inline_type_tag_refused() {
    let source = r#"struct Mob {
    hp: int,
}

inline(lua) fn is_mob(v: any) -> bool = "return v.__ty == 'Mob'"

fn main() {
    print(is_mob(Mob { hp: 1 }))
}"#;
    let project = Project::single("rename_struct_named_by_inline_type_tag_refused", source);
    let reply = project.cli("rename", Some((0, 7)), &["Beast"]);
    contains_text(
        string(&reply["error"]),
        &["the opaque inline(lua) body of `is_mob`"],
    );
    assert_no_rename_changes(&reply);
}

#[test]
fn rename_inline_fn_name_is_fine() {
    let source = &rename_inline_lua_param_refused_source();
    let project = Project::single("rename_inline_fn_name_is_fine", source);
    let reply = project.cli("rename", Some((0, 15)), &["probe2"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        (&source.replace("probe", "probe2")).trim_end()
    );
}

#[test]
fn rename_field_not_named_by_any_inline_body() {
    let source = r#"struct Mob {
    hp: int,
    mana: int,
}

inline(lua) fn peek(m: Mob) -> int = "return m.mana"

fn main() {
    let m = Mob { hp: 4, mana: 2 }
    print(m.hp + peek(m))
}"#;
    let project = Project::single("rename_field_not_named_by_any_inline_body", source);
    let reply = project.cli("rename", Some((1, 4)), &["health"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        (&source.replace("hp", "health")).trim_end()
    );
}

#[test]
fn rename_param_sharing_a_word_with_another_inline_body() {
    let source = &rename_inline_lua_param_refused_source()
        .replace("fn main() {", "fn twice(x: int) -> int {")
        .replace("    print(probe(1))", "    return probe(x) * 2");
    let project = Project::single(
        "rename_param_sharing_a_word_with_another_inline_body",
        source,
    );
    let reply = project.cli("rename", Some((2, 9)), &["n"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        (&source
            .replace("fn twice(x: int) -> int {", "fn twice(n: int) -> int {")
            .replace("    return probe(x) * 2", "    return probe(n) * 2"))
            .trim_end()
    );
}

#[test]
fn rename_fn_named_in_a_string_literal() {
    let source = r#"fn helper(x: int) -> int {
    return x * 2
}

fn main() {
    print("helper")
    print(helper(1))
}"#;
    let project = Project::single("rename_fn_named_in_a_string_literal", source);
    let reply = project.cli("rename", Some((0, 3)), &["helper2"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        (&source
            .replace("fn helper(x: int) -> int {", "fn helper2(x: int) -> int {")
            .replace("    print(helper(1))", "    print(helper2(1))"))
            .trim_end()
    );
}

#[test]
fn rename_global_read_by_inline_body_in_another_file_refused() {
    let project = Project::package(
        "state.rock",
        &[
            ("state.rock", "global SCORE: int = 0"),
            (
                "main.rock",
                r#"inline(lua) fn peek() -> int = "return SCORE"

fn main() {
    SCORE = 2
    print(peek())
}"#,
            ),
        ],
    );
    let reply = project.cli("rename", Some((0, 7)), &["POINTS"]);
    contains_text(
        string(&reply["error"]),
        &["the opaque inline(lua) body of `peek` (main.rock:1)"],
    );
    assert_no_rename_changes(&reply);
}

#[test]
fn rename_global_across_files_without_inline_reader() {
    let project = Project::package(
        "state.rock",
        &[
            ("state.rock", "global SCORE: int = 0"),
            (
                "main.rock",
                r#"inline(lua) fn peek() -> int = "return 1"

fn main() {
    SCORE = 2
    print(peek())
}"#,
            ),
        ],
    );
    let reply = project.cli("rename", Some((0, 7)), &["POINTS"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, Some("state.rock")).trim_end(),
        ("global POINTS: int = 0").trim_end()
    );
    assert_eq!(
        project.renamed(&reply, Some("main.rock")).trim_end(),
        (r#"inline(lua) fn peek() -> int = "return 1"

fn main() {
    POINTS = 2
    print(peek())
}"#)
        .trim_end()
    );
}

#[test]
fn rename_shadowing_inner_local() {
    let source = r#"fn main() {
    let x = 1
    if true {
        let x = 2
        print(x)
    }
    print(x)
}"#;
    let project = Project::single("rename_shadowing_inner_local", source);
    let reply = project.cli("rename", Some((3, 12)), &["y"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        (&source
            .replace("        let x = 2", "        let y = 2")
            .replace("        print(x)", "        print(y)"))
            .trim_end()
    );
}

#[test]
fn rename_shadowed_local_in_same_block() {
    let source = r#"fn main() {
    let x = 1
    let x = x + 1
    print(x)
}"#;
    let project = Project::single("rename_shadowed_local_in_same_block", source);
    let reply = project.cli("rename", Some((1, 8)), &["y"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        (&source
            .replace("    let x = 1", "    let y = 1")
            .replace("    let x = x + 1", "    let x = y + 1"))
            .trim_end()
    );
}

#[test]
fn rename_fn_from_ufcs_call() {
    let source = r#"struct Circle {
    radius: float,
}

fn area(c: Circle) -> float {
    return c.radius * c.radius
}

fn main() {
    let c = Circle { radius: 1.0 }
    print(c.area())
    print(area(c))
}"#;
    let project = Project::single("rename_fn_from_ufcs_call", source);
    let reply = project.cli("rename", Some((10, 12)), &["size"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        (&source.replace("area", "size")).trim_end()
    );
}

#[test]
fn rename_receiver_form_fn() {
    let source = r#"struct Point {
    x: int,
}

fn Point.shift(p, dx: int) -> int {
    return p.x + dx
}

fn main() {
    let p = Point { x: 1 }
    print(p.shift(1))
    print(shift(p, 2))
}"#;
    let project = Project::single("rename_receiver_form_fn", source);
    let reply = project.cli("rename", Some((4, 9)), &["nudge"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        (&source.replace("shift", "nudge")).trim_end()
    );
}

#[test]
fn rename_field_leaves_same_named_field_of_other_struct() {
    let source = r#"struct A {
    hp: int,
}

struct B {
    hp: int,
}

fn main() {
    let a = A { hp: 1 }
    let b = B { hp: 2 }
    print(a.hp + b.hp)
}"#;
    let project = Project::single(
        "rename_field_leaves_same_named_field_of_other_struct",
        source,
    );
    let reply = project.cli("rename", Some((1, 4)), &["health"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        (r#"struct A {
    health: int,
}

struct B {
    hp: int,
}

fn main() {
    let a = A { health: 1 }
    let b = B { hp: 2 }
    print(a.health + b.hp)
}"#)
        .trim_end()
    );
}

#[test]
fn rename_local_inside_string_interpolation() {
    let source = r#"fn main() {
    let n = 5
    print("n = {n}")
}"#;
    let project = Project::single("rename_local_inside_string_interpolation", source);
    let reply = project.cli("rename", Some((1, 8)), &["count"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        (&source
            .replace("    let n = 5", "    let count = 5")
            .replace("    print(\"n = {n}\")", "    print(\"n = {count}\")"))
            .trim_end()
    );
}

#[test]
fn rename_lambda_param() {
    let source = r#"fn main() {
    let f = (a: int) => a + 1
    print(f(1))
}"#;
    let project = Project::single("rename_lambda_param", source);
    let reply = project.cli("rename", Some((1, 13)), &["b"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        (&source.replace(
            "    let f = (a: int) => a + 1",
            "    let f = (b: int) => b + 1"
        ))
            .trim_end()
    );
}

#[test]
fn rename_struct_generic_param_reaches_its_uses() {
    let source = r#"struct Box<T> {
    value: T,
}

fn main() {
    let b = Box { value: 1 }
    print(b.value)
}"#;
    let project = Project::single("rename_struct_generic_param_reaches_its_uses", source);
    let reply = project.cli("rename", Some((0, 11)), &["U"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        (&source.replace("T", "U")).trim_end()
    );
}
