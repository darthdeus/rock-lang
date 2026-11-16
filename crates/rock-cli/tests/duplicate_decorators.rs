use crate::support::{invoke, text};

#[test]
fn check_and_lint_report_a_duplicate_decorator_once() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.rock"),
        "deco init reg {}\ndeco init reg {}\nfn main() {}\n",
    )
    .unwrap();
    for command in ["check", "lint"] {
        let out = invoke(dir.path(), &[command, "main.rock"]);
        let output = text(&out);
        assert!(!out.status.success(), "{command}: {output}");
        assert_eq!(
            output
                .lines()
                .filter(|line| line.contains("Error: duplicate definition of decorator `reg`"))
                .count(),
            1,
            "{command}: {output}"
        );
    }
}
