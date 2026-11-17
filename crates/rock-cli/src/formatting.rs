use crate::*;

pub(crate) fn collect_rock_files(paths: &[String]) -> Result<Vec<String>> {
    use fs_err as fs;

    let mut collected = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |file: String, collected: &mut Vec<String>| {
        if seen.insert(file.clone()) {
            collected.push(file);
        }
    };

    for path in paths {
        let metadata = fs::metadata(path)?;
        if metadata.is_file() {
            if path.ends_with(".rock") {
                push(path.to_string(), &mut collected);
            } else {
                eprintln!("File '{}' is not a Rock file (.rock)", path);
            }
        } else if metadata.is_dir() {
            let base = fs::canonicalize(path)?;
            for file in rock_host::project::find_rock_files_at_path(&base)? {
                let relative = Path::new(&file).strip_prefix(&base)?;
                push(
                    Path::new(path)
                        .join(relative)
                        .to_string_lossy()
                        .into_owned(),
                    &mut collected,
                );
            }
        } else {
            eprintln!("Path '{}' is neither a file nor a directory", path);
        }
    }

    Ok(collected)
}

pub(crate) fn do_fmt(paths: &[String], check: bool) -> Result<()> {
    use fs_err as fs;

    let files_to_format = collect_rock_files(paths)?;

    if files_to_format.is_empty() {
        eprintln!("No Rock files found in '{}'", paths.join("', '"));
        return Ok(());
    }

    let mut needs_formatting = false;
    let mut formatted_count = 0;

    for file_path in &files_to_format {
        // A format error already names the file; the caller prints it once.
        let formatted = format_rock_file(file_path)?;
        if check {
            let original = fs::read_to_string(file_path)?;
            if original.trim() != formatted.trim() {
                println!("File '{}' is not formatted", file_path);
                needs_formatting = true;
            }
        } else {
            let original = fs::read_to_string(file_path)?;
            if original != formatted {
                fs::write(file_path, &formatted)?;
                println!("Formatted '{}'", file_path);
                formatted_count += 1;
            }
        }
    }

    if check && needs_formatting {
        crate::invocation_log::exit(1);
    }

    if !check && formatted_count > 0 {
        println!(
            "Formatted {} file{}",
            formatted_count,
            if formatted_count == 1 { "" } else { "s" }
        );
    }

    Ok(())
}

fn format_rock_file(file_path: &str) -> Result<String> {
    let source = rock_host::source_files::source_file_new_with_context(file_path)?;
    let input = source.contents;
    let formatted =
        skull_fmt::try_format_named(&input, &skull_fmt::FormatConfig::default(), file_path)
            .map_err(|error| anyhow::anyhow!("cannot format '{}':\n{}", file_path, error))?;

    // Defensive reparse: never overwrite a source file with output that no
    // longer parses. skull-fmt returns the input unchanged on a parse
    // failure, so a *changed* output that doesn't parse is a formatter bug
    // -- refuse it rather than corrupt the file (cf. rock-format, which
    // silently produced unparseable output for engine/engine/lib.rock).
    if formatted != input {
        let file: ustr::Ustr = format!("<reparse:{file_path}>").into();
        let ast = skull_parser::parse_module(&formatted, file).map_err(|e| {
            anyhow::anyhow!(
                "formatter produced unparseable output for {}: {:?}",
                file_path,
                e
            )
        })?;
        if !ast.recovery_errors.is_empty() {
            anyhow::bail!(
                "formatter produced output with parse-recovery errors for {}: {:?}",
                file_path,
                ast.recovery_errors
            );
        }
    }

    Ok(formatted)
}

#[cfg(test)]
mod tests {
    use super::collect_rock_files;

    #[test]
    fn every_argument_contributes_its_rock_files_once() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str| {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "fn main() {}\n").unwrap();
            path.to_string_lossy().into_owned()
        };
        let first = write("a.rock");
        let second = write("b.rock");
        let text = write("notes.txt");
        let nested = write("nested/deep.rock");
        write("nested/notes.txt");
        let top = dir.path().to_string_lossy().into_owned();

        let collected =
            collect_rock_files(&[first.clone(), text, second.clone(), first.clone(), top]).unwrap();
        assert_eq!(collected, vec![first.clone(), second, nested]);

        let missing = dir.path().join("gone.rock").to_string_lossy().into_owned();
        assert!(collect_rock_files(&[first, missing]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_yields_sorted_files_including_symlinked_sources() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        for name in ["b.rock", "a.rock"] {
            std::fs::write(dir.path().join(name), "fn main() {}\n").unwrap();
        }
        let target = outside.path().join("shared.rock");
        std::fs::write(&target, "fn main() {}\n").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("c.rock")).unwrap();

        let top = dir.path().to_string_lossy().into_owned();
        let names: Vec<String> = collect_rock_files(&[top])
            .unwrap()
            .iter()
            .map(|path| {
                std::path::Path::new(path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, ["a.rock", "b.rock", "c.rock"]);
    }
}
