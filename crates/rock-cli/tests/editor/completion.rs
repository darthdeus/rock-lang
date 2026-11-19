use super::*;

#[test]
fn completion_cases() {
    let configure = "fn configure(width: int, height: int, title: string) {\n}\n\nfn main() {\n    configure()\n}";
    let cases: &[(&str, &str, (u32, u32), &[&str])] = &[
        (
            "named argument by prefix",
            "fn greet(greeting: string, name: string) -> string {\n    return greeting\n}\n\nfn main() {\n    let x = greet(gr)\n}",
            (5, 20),
            &["greeting:"],
        ),
        (
            "named arguments at an empty slot",
            configure,
            (4, 14),
            &["width:", "height:", "title:"],
        ),
        (
            "named arguments skip earlier labels",
            &configure.replace("configure()", "configure(width: 10, )"),
            (4, 25),
            &["height:", "title:"],
        ),
        (
            "named arguments skip positional slots",
            &configure.replace("configure()", "configure(10, )"),
            (4, 18),
            &["height:", "title:"],
        ),
        (
            "member fields and a UFCS method declared later",
            "struct FixVec2 { x: float, y: float }\n\nfn run() {\n    let v = FixVec2 { x: 1.0, y: 2.0 }\n    v.\n}\n\nfn dist(a: FixVec2) -> float { a.x }",
            (4, 6),
            &["x: float", "y: float", "dist"],
        ),
        (
            "macro with qualified type shapes",
            "macro typed_signature(\n    a: ref ::demo::Item,\n    b: demo::Box<int>,\n    c: (ref int)?,\n    d: [::demo::Item]\n) -> ::demo::Box<int> { 1 }\nfn main() {\n    typed_\n}",
            (7, 10),
            &["typed_signature"],
        ),
        (
            "scope inside an unclosed call",
            "fn add(a: int, b: int) -> int {\n    return a + b\n}\n\nfn main() {\n    let x = 42\n    let y = add(\n}",
            (6, 16),
            &["x", "y", "add"],
        ),
    ];
    for &(case, source, at, labels) in cases {
        let reply = Project::single(source).cli("completion", Some(at), &[]);
        completion_labels(case, &reply, labels);
    }
}
