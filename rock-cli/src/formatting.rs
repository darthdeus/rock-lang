
use crate::*;
pub fn do_fmt(path: &str, in_place: bool, check: bool) -> Result<()> {
    use fs_err as fs;

    let mut files_to_format = Vec::new();

    // Determine if we're dealing with a file or directory
    let metadata = fs::metadata(path)?;
    if metadata.is_file() {
        if path.ends_with(".rock") {
            files_to_format.push(path.to_string());
        } else {
            eprintln!("File '{}' is not a Rock file (.rock)", path);
            return Ok(());
        }
    } else if metadata.is_dir() {
        // Walk the directory to find all .rock files
        for entry in WalkDir::new(path) {
            let entry = entry?;
            if entry.file_type().is_file() {
                let file_path = entry.path();
                if let Some(ext) = file_path.extension() {
                    if ext == "rock" {
                        files_to_format.push(file_path.to_string_lossy().to_string());
                    }
                }
            }
        }
    } else {
        eprintln!("Path '{}' is neither a file nor a directory", path);
        return Ok(());
    }

    if files_to_format.is_empty() {
        eprintln!("No Rock files found in '{}'", path);
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
        std::process::exit(1);
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
    let input = fs_err::read_to_string(file_path)?;
    let formatted = skull_fmt::format_code(&input);

    // Defensive reparse: never overwrite a source file with output that no
    // longer parses. skull-fmt returns the input unchanged on a parse
    // failure, so a *changed* output that doesn't parse is a formatter bug
    // -- refuse it rather than corrupt the file (cf. rock-format, which
    // silently produced unparseable output for stone/engine/lib.rock).
    if formatted != input {
        let file: ustr::Ustr = format!("<reparse:{file_path}>").into();
        let ast = skull_parser::parse_module(&formatted, file).map_err(|e| {
            anyhow::anyhow!("formatter produced unparseable output for {}: {:?}", file_path, e)
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
