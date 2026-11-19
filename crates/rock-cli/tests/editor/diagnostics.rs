use super::*;

const BROKEN: &str = "fn main() {\n    let n: int = \"\u{1f319}\"\n    print(n)\n}";

fn utf16_range(start: u32, end: u32) -> Value {
    json!({"start": {"line": 1, "character": start}, "end": {"line": 1, "character": end}})
}

#[test]
fn package_diagnostics_report_errors_and_lints_per_file() {
    let project = Project::package(
        "main.rock",
        &[
            ("main.rock", BROKEN),
            ("util.rock", "fn helper() -> int { 1 }\n"),
        ],
    );
    let reply = project.lsp(&["diagnostics", project.dir.path().to_str().unwrap()]);
    let files = array(&reply["files"]);
    let of = |name: &str| {
        let file = files
            .iter()
            .find(|file| string(&file["file"]).ends_with(name));
        array(&file.unwrap_or_else(|| panic!("no {name} in {reply}"))["diagnostics"]).to_vec()
    };
    let main = of("main.rock");
    assert_eq!(main.len(), 1, "{reply}");
    assert_eq!(main[0]["severity"], 1);
    assert_eq!(main[0]["range"], utf16_range(17, 21));
    let util = of("util.rock");
    assert_eq!(util.len(), 1, "{reply}");
    assert_eq!(util[0]["code"], "unused_fn");
}

#[test]
fn diagnostics_publish_utf16_ranges_and_clear_after_a_fix() {
    let mut doc = Document::single(BROKEN);
    let diagnostics = array(&doc.diagnostics).to_vec();
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0]["range"], utf16_range(17, 21));
    doc.replace(&BROKEN.replace("\"\u{1f319}\"", "1"));
    assert_eq!(doc.diagnostics, json!([]));
}
