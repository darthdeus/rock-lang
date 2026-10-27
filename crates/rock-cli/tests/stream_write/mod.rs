use super::*;

#[test]
fn streamed_test_program_matches_concatenation_and_excerpt() {
    for lua in [String::new(), "return 42".to_string(), format!("{}{}tail", "x".repeat(3989), "\u{1f600}")] {
        for harness in ["", "\n-- harness\n"] {
            let expected = format!("preamble\n{lua}\nsetup_runtime()\n{harness}");
            let program = TestProgram {
                preamble: "preamble\n".into(),
                lua: lua.clone(),
                setup: "\nsetup_runtime()\n",
            };
            let (file, excerpt) = write_test_program(program, harness).unwrap();
            assert_eq!(std::fs::read(file.path()).unwrap(), expected.as_bytes());
            assert_eq!(excerpt, truncate(&expected));
        }
    }
}

#[test]
fn streamed_game_main_matches_metadata_splice_and_entry() {
    let marker = "__rock_globals_install_bridge()\n";
    for emits_main in [false, true] {
        let lua = format!("head\n{marker}tail\n{marker}last\n");
        let metadata = "-- metadata\n";
        let mut old = lua.clone();
        old.insert_str(old.find(marker).unwrap() + marker.len(), metadata);
        let expected = format!("preamble\n{old}{}", if emits_main { "\n__rock_main_wrapper(main)\n" } else { "" });
        let program = GameProgram::with_metadata(lua, metadata.into()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main.lua");
        let overlay = dir.path().join("incremental.lua");
        std::fs::write(&main, "old").unwrap();
        std::fs::write(&overlay, "overlay").unwrap();
        let bytes = write_full_main("preamble\n", &program, emits_main, &main, &overlay).unwrap();
        assert_eq!(bytes, expected.len());
        assert_eq!(std::fs::read(&main).unwrap(), expected.as_bytes());
        assert!(!overlay.exists());
    }
    assert!(GameProgram::with_metadata("no bridge".into(), "metadata".into()).is_err());
}

#[test]
fn streamed_atomic_write_failure_preserves_destination_and_cleans_temp() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.lua");
    for old in [None, Some("old complete program")] {
        if let Some(old) = old { std::fs::write(&path, old).unwrap(); }
        let result = rock_host::project::write_atomic_streamed(&path, |file| {
            file.write_all(b"partial new program")?;
            Err(std::io::Error::other("injected write failure"))
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&path).ok().as_deref(), old);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), usize::from(old.is_some()));
    }
}

#[cfg(unix)]
#[test]
fn streamed_atomic_replace_failure_preserves_old_file() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.lua");
    std::fs::write(&path, "old complete program").unwrap();
    let result = rock_host::project::write_atomic_streamed(&path, |file| {
        file.write_all(b"new complete program")?;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500))?;
        Ok(())
    });
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "old complete program");
}
