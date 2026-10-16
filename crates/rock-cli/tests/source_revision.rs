use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn write_project(dir: &Path) {
    std::fs::write(
        dir.join("stick.toml"),
        "[package]\nname = \"source-revision-test\"\ntype = \"bin\"\n\n[build.vars]\nGIT_SHA = { builtin = \"source_revision\" }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("main.rock"),
        "fn main() { print(build_var(\"GIT_SHA\")) }\n",
    )
    .unwrap();
}

fn run(project: &Path, context_root: Option<&Path>, source_revision: Option<&str>) -> Output {
    let repo = repo_root();
    let mut command = crate::command::rock_bin();
    command
        .current_dir(&repo)
        .env("ROCK_DIR", &repo)
        .args(["run", project.to_str().unwrap()]);
    match context_root {
        Some(root) => {
            command.env("ROCK_CONTEXT_ROOT", root);
        }
        None => {
            command.env_remove("ROCK_CONTEXT_ROOT");
        }
    }
    match source_revision {
        Some(revision) => {
            command.env("ROCK_SOURCE_REVISION", revision);
        }
        None => {
            command.env_remove("ROCK_SOURCE_REVISION");
        }
    }
    command.output().expect("spawn source revision fixture")
}

#[test]
fn valid_context_uses_the_stamped_source_revision() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("game");
    let context = temp.path().join("context");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(context.join("sdk")).unwrap();
    std::fs::create_dir_all(context.join(".context")).unwrap();
    std::fs::write(context.join(".context/context.json"), "{}\n").unwrap();
    write_project(&project);

    let output = run(
        &project,
        Some(&context),
        Some("context-stamped-revision"),
    );
    assert!(
        output.status.success(),
        "context source revision failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "context-stamped-revision\n"
    );
}

#[test]
fn repository_build_uses_short_git_revision_without_context() {
    let repo = repo_root();
    let tmp_root = repo.join("tmp");
    std::fs::create_dir_all(&tmp_root).unwrap();
    let temp = tempfile::tempdir_in(&tmp_root).unwrap();
    write_project(temp.path());
    let expected = String::from_utf8(
        Command::new("git")
            .current_dir(&repo)
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();

    let output = run(temp.path(), None, None);
    assert!(
        output.status.success(),
        "repository source revision failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
}

#[test]
fn context_without_a_revision_stamp_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("game");
    let context = temp.path().join("context");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(context.join("sdk")).unwrap();
    std::fs::create_dir_all(context.join(".context")).unwrap();
    std::fs::write(context.join(".context/context.json"), "{}\n").unwrap();
    write_project(&project);

    let output = run(&project, Some(&context), None);
    assert!(!output.status.success(), "an unstamped context must fail closed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ROCK_SOURCE_REVISION") && stderr.contains("GIT_SHA"),
        "failure must name the missing context stamp:\n{stderr}"
    );
}
