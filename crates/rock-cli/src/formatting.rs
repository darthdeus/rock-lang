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

    let mut changed = 0;
    for path in &files_to_format {
        let input = rock_host::source_files::source_file_new_with_context(path)?.contents;
        let formatted = format_rock_file(path, &input)?;
        if check {
            if input.trim() != formatted.trim() {
                println!("File '{path}' is not formatted");
                changed += 1;
            }
        } else if input != formatted {
            fs::write(path, formatted)?;
            println!("Formatted '{path}'");
            changed += 1;
        }
    }

    if changed > 0 {
        if check {
            crate::invocation_log::exit(1);
        }
        println!(
            "Formatted {changed} file{}",
            if changed == 1 { "" } else { "s" }
        );
    }

    Ok(())
}

fn format_rock_file(file_path: &str, input: &str) -> Result<String> {
    let formatted =
        skull_fmt::try_format_named(input, &skull_fmt::FormatConfig::default(), file_path)
            .map_err(|error| anyhow::anyhow!("cannot format '{}':\n{}", file_path, error))?;

    // Check changed output before allowing it to replace the source file.
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
}
