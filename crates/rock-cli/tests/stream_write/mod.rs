use super::*;

#[test]
fn streamed_test_program_matches_concatenation_and_excerpt() {
    for lua in ["return 42".to_string(), format!("{}{}tail", "x".repeat(3994), "\u{1f600}")] {
        for harness in ["", "\n-- harness\n"] {
            let expected = format!("preamble\n{lua}\nsetup_stone()\n{harness}");
            let program = TestProgram {
                preamble: "preamble\n".into(),
                lua: lua.clone(),
                setup: "\nsetup_stone()\n",
            };
            let (file, excerpt) = write_test_program(program, harness).unwrap();
            assert_eq!(std::fs::read(file.path()).unwrap(), expected.as_bytes());
            assert_eq!(excerpt, truncate(&expected));
        }
    }
}
