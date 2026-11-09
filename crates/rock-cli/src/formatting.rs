use crate::*;

/// Expand every argument into the `.rock` files it names: a file argument
/// contributes itself, a directory argument everything under it. A file named
/// by more than one argument is formatted once.
///
/// `rock fmt` used to take a single path, so a file list silently formatted
/// (or `--check`ed) only its first entry and reported success for the rest.
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
            for entry in WalkDir::new(path) {
                let entry = entry?;
                if entry.file_type().is_file() {
                    let file_path = entry.path();
                    if let Some(ext) = file_path.extension() {
                        if ext == "rock" {
                            push(file_path.to_string_lossy().to_string(), &mut collected);
                        }
                    }
                }
            }
        } else {
            eprintln!("Path '{}' is neither a file nor a directory", path);
        }
    }

    Ok(collected)
}

pub(crate) fn do_fmt(paths: &[String], in_place: bool, check: bool) -> Result<()> {
    use fs_err as fs;

    let files_to_format = collect_rock_files(paths)?;

    if files_to_format.is_empty() {
        eprintln!("No Rock files found in '{}'", paths.join("', '"));
        return Ok(());
    }

    let mut needs_formatting = false;
    let mut formatted_count = 0;

    for file_path in &files_to_format {
        match format_rock_file(file_path) {
            Ok(formatted) => {
                if check {
                    let original = fs::read_to_string(file_path)?;
                    if original.trim() != formatted.trim() {
                        println!("File '{}' is not formatted", file_path);
                        needs_formatting = true;
                    }
                } else if !in_place {
                    // Print to stdout
                    if files_to_format.len() == 1 {
                        print!("{}", formatted);
                    } else {
                        println!("=== {} ===", file_path);
                        print!("{}", formatted);
                    }
                } else {
                    // Default: format in-place
                    let original = fs::read_to_string(file_path)?;
                    if original != formatted {
                        // Debug: print what's different
                        if std::env::var("DEBUG_FMT").is_ok() {
                            eprintln!(
                                "Original has {} chars, {} trailing newlines",
                                original.len(),
                                original.chars().rev().take_while(|&c| c == '\n').count()
                            );
                            eprintln!(
                                "Formatted has {} chars, {} trailing newlines",
                                formatted.len(),
                                formatted.chars().rev().take_while(|&c| c == '\n').count()
                            );
                        }
                        fs::write(file_path, &formatted)?;
                        println!("Formatted '{}'", file_path);
                        formatted_count += 1;
                    }
                }
            }
            Err(e) => {
                eprintln!("Error formatting '{}': {}", file_path, e);
                return Err(e);
            }
        }
    }

    if check && needs_formatting {
        crate::invocation_log::exit(1);
    }

    if in_place && !check && formatted_count > 0 {
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
    let dialect = source.dialect;
    let formatted =
        skull_fmt::try_format_with_dialect(&input, &skull_fmt::FormatConfig::default(), dialect)
            .map_err(|error| {
                anyhow::anyhow!("failed to format {} as {:?}: {}", file_path, dialect, error)
            })?;

    // Defensive reparse: never overwrite a source file with output that no
    // longer parses. skull-fmt returns the input unchanged on a parse
    // failure, so a *changed* output that doesn't parse is a formatter bug
    // -- refuse it rather than corrupt the file (cf. rock-format, which
    // silently produced unparseable output for engine/engine/lib.rock).
    if formatted != input {
        let file: ustr::Ustr = format!("<reparse:{file_path}>").into();
        let ast =
            skull_parser::parse_module_with_dialect(&formatted, file, dialect).map_err(|e| {
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
    use std::fs;

    fn write(dir: &std::path::Path, name: &str) -> String {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, "fn main() {}\n").unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn every_named_file_is_collected_not_only_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let first = write(dir.path(), "a.rock");
        let second = write(dir.path(), "b.rock");
        let third = write(dir.path(), "c.rock");

        let collected =
            collect_rock_files(&[first.clone(), second.clone(), third.clone()]).unwrap();

        assert_eq!(collected, vec![first, second, third]);
    }

    #[test]
    fn a_directory_argument_contributes_every_rock_file_under_it() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "top.rock");
        write(dir.path(), "nested/deep.rock");
        write(dir.path(), "nested/notes.txt");

        let collected = collect_rock_files(&[dir.path().to_string_lossy().to_string()]).unwrap();

        let mut names: Vec<_> = collected
            .iter()
            .map(|path| {
                std::path::Path::new(path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        names.sort();
        assert_eq!(names, vec!["deep.rock", "top.rock"]);
    }

    #[test]
    fn a_file_named_twice_is_collected_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = write(dir.path(), "a.rock");

        let collected = collect_rock_files(&[
            file.clone(),
            dir.path().to_string_lossy().to_string(),
            file.clone(),
        ])
        .unwrap();

        assert_eq!(collected, vec![file]);
    }

    #[test]
    fn a_non_rock_file_is_skipped_without_dropping_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let text = write(dir.path(), "notes.txt");
        let rock = write(dir.path(), "a.rock");

        let collected = collect_rock_files(&[text, rock.clone()]).unwrap();

        assert_eq!(collected, vec![rock]);
    }

    #[test]
    fn a_missing_path_is_an_error_rather_than_a_silent_skip() {
        let dir = tempfile::tempdir().unwrap();
        let rock = write(dir.path(), "a.rock");
        let missing = dir.path().join("gone.rock").to_string_lossy().to_string();

        assert!(collect_rock_files(&[rock, missing]).is_err());
    }
}
