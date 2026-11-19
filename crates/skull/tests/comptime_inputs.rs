use bone_semantic::inference::{ComptimeInputs, ComptimeVarValue};

use crate::common::{
    body_id, check_incremental, emit_full_of, function_body, global_body, run_fn,
    try_emit_module_with,
};

fn inputs_with_var(name: &str, value: &str) -> ComptimeInputs {
    let mut inputs = ComptimeInputs::default();
    inputs.vars.insert(
        ustr::Ustr::from(name),
        ComptimeVarValue {
            value: value.to_string(),
            secret: false,
        },
    );
    inputs
}

#[test]
fn build_var_inlines_the_resolved_value() {
    let src = "fn channel_tag() { print(build_var(\"CHANNEL\")) }";
    for volatile in [false, true] {
        let mut inputs = inputs_with_var("CHANNEL", "steam-test-value");
        if volatile {
            inputs.volatile_vars.insert("CHANNEL".into());
        }
        let lua = try_emit_module_with(src, inputs).expect("declared var expands");
        let body = function_body(&lua, "channel_tag");
        assert!(body.contains("steam-test-value"));
        assert!(!body.contains("build_var"));
        assert_eq!(run_fn(&lua, "channel_tag"), "steam-test-value");
    }
    let mut inputs = inputs_with_var("CHANNEL", "steam-test-value");
    inputs.volatile_vars.insert("MISSING".into());
    let error = try_emit_module_with(src, inputs).expect_err("unknown volatile input");
    assert!(
        error
            .message
            .contains("volatile metadata declaration `MISSING`")
    );
}

#[test]
fn build_var_misuse_in_uninstantiated_generic_rejects() {
    let src = "fn dead_generic<T>(x: T) -> string { return build_var(\"NOPE\") }";
    let err = try_emit_module_with(src, ComptimeInputs::default())
        .expect_err("misuse in an uninstantiated generic must still reject");
    assert!(err.message.contains("NOPE"));
}

#[test]
fn embed_string_inlines_file_contents() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("greeting.txt"), "hello-from-embed\n").unwrap();
    let src = r#"
fn greeting() -> string {
    return embed_string("greeting.txt")
}
"#;
    let inputs = ComptimeInputs {
        embed_root: Some(dir.path().to_path_buf()),
        ..ComptimeInputs::default()
    };
    let lua = try_emit_module_with(src, inputs).expect("embed expands");
    let body = function_body(&lua, "greeting");
    assert!(body.contains("hello-from-embed"));
}

#[test]
fn embed_resolves_against_the_calling_files_own_package() {
    let pkg = tempfile::tempdir().unwrap();
    std::fs::write(
        pkg.path().join("stick.toml"),
        "[package]\nname = \"libpkg\"\ntype = \"lib\"\n",
    )
    .unwrap();
    std::fs::write(pkg.path().join("payload.txt"), "from-the-library-package").unwrap();
    let lib_file = pkg.path().join("lib.rock");
    std::fs::write(&lib_file, "").unwrap();

    let elsewhere = tempfile::tempdir().unwrap();
    let src = r#"
fn payload() -> string {
    return embed_string("payload.txt")
}
"#;
    let inputs = ComptimeInputs {
        embed_root: Some(elsewhere.path().to_path_buf()),
        ..ComptimeInputs::default()
    };
    let lua = crate::common::try_emit_module_with_file(src, lib_file.to_str().unwrap(), inputs)
        .expect("package-relative embed resolves");
    assert!(lua.contains("from-the-library-package"));
}

fn memory_inputs(files: &[(&str, &str)]) -> ComptimeInputs {
    let files = files
        .iter()
        .map(|(path, text)| (path.to_string(), text.to_string()));
    ComptimeInputs {
        embed_files: bone_core::comptime_files::EmbedFiles(std::sync::Arc::new(
            bone_core::comptime_files::MemoryComptimeFiles::new(files),
        )),
        ..ComptimeInputs::default()
    }
}

#[test]
fn embed_string_reads_host_supplied_files() {
    let src = r#"
fn greeting() -> string {
    return embed_string("data/greeting.txt")
}
"#;
    let inputs = memory_inputs(&[("data/greeting.txt", "hello-from-memory")]);
    let lua = try_emit_module_with(src, inputs).expect("supplied embed expands");
    let body = function_body(&lua, "greeting");
    assert!(body.contains("hello-from-memory"));
}

fn ws_with_inputs(
    src: &str,
    inputs: bone_semantic::inference::ComptimeInputs,
) -> bone_semantic::inference::Workspace {
    let mut ws = crate::common::ws_with_stdlib(src);
    ws.comptime_inputs = inputs;
    ws
}

fn assert_input_change_reaches_every_consumer(
    src: &str,
    before: (ComptimeInputs, &str),
    after: (ComptimeInputs, &str),
    move_input: impl FnOnce(),
    consumers: usize,
) {
    use skull::mir_emit::emit_overlay;

    let ws1 = ws_with_inputs(src, before.0);
    let (full, mut incremental_cache) = emit_full_of(&ws1, "emit_full v1");
    let (_, mut overlay_cache) = emit_full_of(&ws1, "emit_full v1");
    assert_eq!(full.lua.matches(before.1).count(), consumers);

    move_input();
    let ws2 = ws_with_inputs(src, after.0);
    let edit = [body_id(&ws2, "other")];
    let incremental = check_incremental(&ws2, &mut incremental_cache, &edit);
    assert_eq!(incremental.lua.matches(after.1).count(), consumers);
    assert!(!incremental.lua.contains(before.1));

    let overlay = emit_overlay(&ws2, &mut overlay_cache, &edit).expect("emit_overlay");
    assert_eq!(overlay.lua.matches(after.1).count(), consumers);
    assert!(overlay.lua.contains("TAG = "));
}

#[test]
fn a_build_var_change_reaches_every_recorded_consumer() {
    let src = r#"
fn first() -> string {
    return build_var("SHA")
}
fn second() -> string {
    return build_var("SHA")
}
const TAG: string = build_var("SHA")
fn other() -> int {
    return 1
}
fn main() {
    print(first())
    print(second())
    print(TAG)
}
"#;
    assert_input_change_reaches_every_consumer(
        src,
        (inputs_with_var("SHA", "all-first"), "all-first"),
        (inputs_with_var("SHA", "all-second"), "all-second"),
        || {},
        3,
    );
}

#[test]
fn an_embedded_file_change_reaches_every_recorded_consumer() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data.txt");
    std::fs::write(&data, "embed-v1").unwrap();
    let src = r#"
fn first() -> string {
    return embed_string("data.txt")
}
const TAG: string = embed_string("data.txt")
fn other() -> int {
    return 1
}
fn main() {
    print(first())
    print(TAG)
}
"#;
    let inputs = ComptimeInputs {
        embed_root: Some(dir.path().to_path_buf()),
        ..ComptimeInputs::default()
    };
    assert_input_change_reaches_every_consumer(
        src,
        (inputs.clone(), "embed-v1"),
        (inputs, "embed-v2"),
        || std::fs::write(&data, "embed-v2").unwrap(),
        2,
    );
}

#[test]
fn global_body_edit_replaces_the_recorded_comptime_read_set() {
    use skull::mir_emit::emit_overlay;

    let dir = tempfile::tempdir().unwrap();
    let first_path = dir.path().join("first.txt");
    let second_path = dir.path().join("second.txt");
    std::fs::write(&first_path, "first-v1").unwrap();
    std::fs::write(&second_path, "second-v1").unwrap();
    let v1 = r#"
const TAG: string = embed_string("first.txt")
fn other() -> int {
    return 1
}
fn main() {
    print(TAG)
}
"#;
    let v2 = v1.replace("first.txt", "second.txt");
    let inputs = bone_semantic::inference::ComptimeInputs {
        embed_root: Some(dir.path().to_path_buf()),
        ..bone_semantic::inference::ComptimeInputs::default()
    };
    let ws1 = ws_with_inputs(v1, inputs.clone());
    let (_, mut cache) = emit_full_of(&ws1, "emit_full v1");

    let ws2 = ws_with_inputs(&v2, inputs.clone());
    let first = emit_overlay(&ws2, &mut cache, &[global_body(&ws2, "TAG")])
        .expect("global body edit overlay");
    assert!(first.lua.contains("second-v1"));
    let first_canonical = first_path.canonicalize().unwrap();
    let second_canonical = second_path.canonicalize().unwrap();
    let files: Vec<_> = cache
        .comptime_file_hashes()
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    assert!(
        files
            .iter()
            .any(|file| std::path::Path::new(file.as_str()) == second_canonical)
            && !files
                .iter()
                .any(|file| std::path::Path::new(file.as_str()) == first_canonical)
    );

    std::fs::write(&second_path, "second-v2").unwrap();
    let ws3 = ws_with_inputs(&v2, inputs);
    let second =
        emit_overlay(&ws3, &mut cache, &[body_id(&ws3, "other")]).expect("new input overlay");
    assert!(second.lua.contains("second-v2"));
}

#[test]
fn comptime_misuse_rejects_with_a_specific_diagnostic() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("proj")).unwrap();
    std::fs::write(dir.path().join("secret.txt"), "outside\n").unwrap();
    let rooted = |root: std::path::PathBuf| ComptimeInputs {
        embed_root: Some(root),
        ..ComptimeInputs::default()
    };
    for (body, inputs, fragments) in [
        (
            "return build_var(\"NOPE\")",
            ComptimeInputs::default(),
            &["NOPE", "build.vars"] as &[&str],
        ),
        (
            "return build_var(name)",
            inputs_with_var("CHANNEL", "x"),
            &["literal"],
        ),
        (
            "return embed_string(\"absent.txt\")",
            rooted(dir.path().to_path_buf()),
            &["absent.txt"],
        ),
        (
            "return embed_string(\"../secret.txt\")",
            rooted(dir.path().join("proj")),
            &["escapes"],
        ),
        (
            "return embed_string(\"absent.txt\")",
            memory_inputs(&[]),
            &["absent.txt", "host supplied no file"],
        ),
        (
            "return embed_string(\"x.txt\")",
            ComptimeInputs::default(),
            &["project root"],
        ),
    ] {
        let src = format!("fn broken(name: string) -> string {{ {body} }}");
        let err = try_emit_module_with(&src, inputs).expect_err(body);
        for fragment in fragments {
            assert!(err.message.contains(fragment), "{body}: {err:?}");
        }
    }
}
