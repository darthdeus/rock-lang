use crate::common::{bone_ws, completion};

/// Named-arg completion skips the params a `..spread` operand supplies.
#[test]
fn spread_covered_params_are_skipped() {
    let src = "\
struct Dims { width: int, height: int }
fn configure(width: int, height: int, title: string) {}
fn r() {
  let d = Dims { width: 10, height: 20 }
  configure(..d, )
}
";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock");
    let labels: Vec<String> = completion(&mut bone_ws(&path, src), &path, 4, 17)
        .items
        .into_iter()
        .map(|i| i.label)
        .collect();
    assert!(labels.iter().any(|l| l == "title:"), "{labels:?}");
    assert!(
        !labels.iter().any(|l| l == "width:" || l == "height:"),
        "{labels:?}"
    );
}
