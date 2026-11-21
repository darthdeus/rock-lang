use anyhow::{Context, Result};
use serde_json::json;
use sha2::{Digest, Sha256};
use skull_parser::{Ast, EnumVariantKind, FunctionDef, Generic, Item, ItemKind, Param};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(serde::Serialize)]
struct Declaration {
    kind: &'static str,
    line: usize,
    name: String,
    signature: String,
    source: String,
}

pub fn run(inputs: &[String], output: Option<&str>) -> Result<()> {
    const USAGE: &str = "usage: rock docs-index <dir|file>... --output <file>";
    anyhow::ensure!(!inputs.is_empty(), USAGE);
    let output = output.context(USAGE)?;
    let cwd = fs::canonicalize(std::env::current_dir()?)
        .context("resolve declaration-index working directory")?;
    let sources = collect_sources_with_stdlib(inputs, &cwd, &rock_stdlib::stdlib_origin())?;
    let mut identity = Sha256::new();
    let mut source_values = Vec::with_capacity(sources.len());
    let mut declarations = Vec::new();
    for (display_path, path) in sources {
        let source = rock_host::source_files::source_file_new_with_context(&path)?;
        let file_id = ustr::Ustr::from(source.file.to_id_path().as_str());
        let text = source.contents;
        let ast = skull_parser::parse_module_clean(&text, file_id)
            .map_err(|error| anyhow::anyhow!("parse '{}': {error}", path.display()))?;
        let file_hash = digest(text.as_bytes());
        identity.update(display_path.as_bytes());
        identity.update([0]);
        identity.update(file_hash.as_bytes());
        identity.update(*b"\n");
        source_values.push(json!({
            "path": display_path,
            "sha256": file_hash,
            "bytes": text.len(),
            "lines": line_count(text.as_bytes()),
        }));
        for item in &ast.items {
            if let Some(declaration) = declaration(&ast, item, &display_path, &text)? {
                declarations.push(declaration);
            }
        }
    }
    declarations.sort_by(|a, b| {
        (&a.source, a.line, a.kind, &a.name, &a.signature).cmp(&(
            &b.source,
            b.line,
            b.kind,
            &b.name,
            &b.signature,
        ))
    });

    let document = json!({
        "declarations": declarations,
        "repository_revision": repository_revision(&cwd),
        "schema": 1,
        "source_identity": format!("sha256:{:x}", identity.finalize()),
        "sources": source_values,
    });
    let mut encoded = serde_json::to_vec_pretty(&document)?;
    encoded.push(b'\n');
    rock_host::project::write_atomic(output, encoded)
        .with_context(|| format!("write declaration index '{}'", output))?;
    Ok(())
}

fn collect_sources_with_stdlib(
    inputs: &[String],
    cwd: &Path,
    stdlib: &rock_stdlib::StdlibSource,
) -> Result<Vec<(String, PathBuf)>> {
    let mut paths = BTreeSet::new();
    for input in inputs {
        let candidate = Path::new(input);
        let candidate = if input == "stdlib" {
            match stdlib {
                rock_stdlib::StdlibSource::Dir(dir) => dir.clone(),
                rock_stdlib::StdlibSource::Embedded => {
                    anyhow::bail!(
                        "the stdlib is built in to this binary; no stdlib directory to index"
                    );
                }
            }
        } else if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            cwd.join(candidate)
        };
        let canonical = fs::canonicalize(&candidate)
            .with_context(|| format!("resolve declaration input '{}'", candidate.display()))?;
        if canonical.is_file() {
            ensure_rock_file(&canonical)?;
            paths.insert(canonical);
        } else if canonical.is_dir() {
            for path in rock_host::project::find_rock_files_at_path(&canonical)? {
                let path = fs::canonicalize(path)?;
                ensure_rock_file(&path)?;
                paths.insert(path);
            }
        } else {
            anyhow::bail!(
                "declaration input is neither a file nor directory: '{}'",
                input
            );
        }
    }
    if paths.is_empty() {
        anyhow::bail!("declaration inputs contain no .rock files");
    }
    let stdlib_canonical = match stdlib {
        rock_stdlib::StdlibSource::Dir(dir) => Some(fs::canonicalize(dir)?),
        rock_stdlib::StdlibSource::Embedded => None,
    };
    let mut entries = paths
        .into_iter()
        .map(|path| {
            let display = display_path(&path, cwd, stdlib_canonical.as_deref())?;
            Ok((display, path))
        })
        .collect::<Result<Vec<_>>>()?;
    entries.sort_unstable();
    Ok(entries)
}

fn ensure_rock_file(path: &Path) -> Result<()> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("rock") {
        anyhow::bail!(
            "declaration input is not a .rock file: '{}'",
            path.display()
        );
    }
    Ok(())
}

fn display_path(path: &Path, cwd: &Path, stdlib: Option<&Path>) -> Result<String> {
    let (prefix, relative) = match stdlib.and_then(|root| path.strip_prefix(root).ok()) {
        Some(relative) => ("stdlib/", relative),
        None => (
            "",
            path.strip_prefix(cwd).with_context(|| {
                format!(
                    "declaration input escapes the working directory: '{}'",
                    path.display()
                )
            })?,
        ),
    };
    Ok(format!(
        "{prefix}{}",
        relative.to_string_lossy().replace('\\', "/")
    ))
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn line_count(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        0
    } else {
        bytes.iter().filter(|byte| **byte == b'\n').count() + 1
    }
}

fn repository_revision(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let revision = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!revision.is_empty()).then_some(revision)
}

fn declaration(ast: &Ast, item: &Item, source: &str, text: &str) -> Result<Option<Declaration>> {
    let line = item.span.line_range().0 + 1;
    let (kind, name, signature) = match &item.kind {
        ItemKind::Function(function) if !function.is_synthetic => (
            "function",
            function_name(function),
            function_signature(ast, function, text)?,
        ),
        ItemKind::Struct(structure) => (
            "struct",
            structure.name.name.to_string(),
            struct_signature(ast, structure),
        ),
        ItemKind::Enum(enumeration) => (
            "enum",
            enumeration.name.name.to_string(),
            enum_signature(ast, enumeration),
        ),
        ItemKind::Newtype(newtype) => (
            "newtype",
            newtype.name.name.to_string(),
            format!(
                "newtype {}{} = {}",
                newtype.name.name,
                generic_suffix(ast, &newtype.generics),
                ast.type_surface(newtype.wrapped)
            ),
        ),
        ItemKind::Decorator(decorator) => (
            "decorator",
            decorator.name.name.to_string(),
            format!(
                "decorator {}({})",
                decorator.name.name,
                params_signature(ast, &decorator.params, text)?
            ),
        ),
        ItemKind::Global(global) => {
            let keyword = if global.is_const { "const" } else { "global" };
            let typed_name = global.ty.map_or_else(
                || global.name.name.to_string(),
                |ty| format!("{}: {}", global.name.name, ast.type_surface(ty)),
            );
            (
                "global",
                global.name.name.to_string(),
                format!("{keyword} {typed_name}"),
            )
        }
        _ => return Ok(None),
    };
    Ok(Some(Declaration {
        kind,
        name,
        signature,
        source: source.to_string(),
        line,
    }))
}

fn function_name(function: &FunctionDef) -> String {
    match &function.receiver_surface {
        Some((receiver, _)) => format!("{}.{}", receiver.name, function.name.name),
        None => function.name.name.to_string(),
    }
}

fn function_signature(ast: &Ast, function: &FunctionDef, text: &str) -> Result<String> {
    let keyword = if function.is_macro {
        "macro"
    } else if function.is_coroutine {
        "co fn"
    } else {
        "fn"
    };
    let return_type = function
        .return_ty
        .map_or_else(|| "()".to_string(), |ty| ast.type_surface(ty));
    Ok(format!(
        "{keyword} {}{}({}) -> {return_type}",
        function_name(function),
        generic_suffix(ast, &function.generics),
        params_signature(ast, &function.params, text)?
    ))
}

fn generic_suffix(ast: &Ast, generics: &[Generic]) -> String {
    if generics.is_empty() {
        return String::new();
    }
    format!(
        "<{}>",
        generics
            .iter()
            .map(|generic| {
                let mut result = generic.name.name.to_string();
                if let Some(bound) = generic.bound {
                    result.push_str(": ");
                    result.push_str(&ast.type_surface(bound));
                }
                if let Some(default) = generic.default {
                    result.push_str(" = ");
                    result.push_str(&ast.type_surface(default));
                }
                result
            })
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn params_signature(ast: &Ast, params: &[Param], text: &str) -> Result<String> {
    params
        .iter()
        .map(|param| {
            let prefix = if param.is_variadic { "..." } else { "" };
            let mut signature = format!(
                "{prefix}{}: {}",
                param.name.name,
                ast.type_surface(param.ty)
            );
            if let Some(default) = param.default {
                let (start, end) = ast.exprs[default].span.offset_range();
                // Source spelling preserves literals and grouping without an expression printer.
                let expression = text.get(start..end).with_context(|| {
                    format!(
                        "invalid default expression span for parameter '{}'",
                        param.name.name
                    )
                })?;
                signature.push_str(" = ");
                signature.push_str(expression);
            }
            Ok(signature)
        })
        .collect::<Result<Vec<_>>>()
        .map(|params| params.join(", "))
}

fn struct_signature(ast: &Ast, structure: &skull_parser::StructDef) -> String {
    let tag = if structure.is_nobox {
        "nobox struct"
    } else if structure.is_value {
        "value struct"
    } else {
        "struct"
    };
    let fields = structure
        .fields
        .iter()
        .map(|field| format!("{}: {}", field.name.name, ast.type_surface(field.ty)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{tag} {}{} {{ {fields} }}",
        structure.name.name,
        generic_suffix(ast, &structure.generics)
    )
}

fn enum_signature(ast: &Ast, enumeration: &skull_parser::EnumDef) -> String {
    let variants = enumeration
        .variants
        .iter()
        .map(|variant| match &variant.kind {
            EnumVariantKind::Unit => variant.name.name.to_string(),
            EnumVariantKind::Tuple(types) => format!(
                "{}({})",
                variant.name.name,
                types
                    .iter()
                    .map(|ty| ast.type_surface(*ty))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            EnumVariantKind::Record(fields) => format!(
                "{} {{ {} }}",
                variant.name.name,
                fields
                    .iter()
                    .map(|field| format!("{}: {}", field.name.name, ast.type_surface(field.ty)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })
        .collect::<Vec<_>>()
        .join(" | ");
    format!(
        "enum {}{} {{ {variants} }}",
        enumeration.name.name,
        generic_suffix(ast, &enumeration.generics)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdlib_input_when_built_in_says_so() {
        let cwd = std::env::current_dir().unwrap();
        let err = collect_sources_with_stdlib(
            &["stdlib".to_string()],
            &cwd,
            &rock_stdlib::StdlibSource::Embedded,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("built in"),
            "unexpected error: {err:#}"
        );
    }
}
