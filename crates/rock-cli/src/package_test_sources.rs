use crate::{SourceFiles, TokenKind, Tokenizer, Ustr};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Default)]
struct Scan {
    definitions: HashSet<Ustr>,
    mentions: HashSet<Ustr>,
    keep_effects: bool,
    has_test: bool,
}

fn literal_names(text: &str, names: &mut HashSet<Ustr>) {
    for word in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if word.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') {
            names.insert(Ustr::from(word));
        }
    }
}

fn declaration_name(tokens: &[TokenKind]) -> Option<Ustr> {
    let TokenKind::Ident(mut name) = *tokens.first()? else { return None };
    let mut i = 1;
    loop {
        if tokens.get(i) == Some(&TokenKind::LessThan) {
            let mut depth = 1;
            i += 1;
            while depth > 0 {
                match tokens.get(i)? {
                    TokenKind::LessThan => depth += 1,
                    TokenKind::GreaterThan => depth -= 1,
                    TokenKind::RightShift => depth -= 2,
                    _ => {}
                }
                i += 1;
            }
        }
        if !matches!(tokens.get(i), Some(TokenKind::Dot | TokenKind::DoubleColon)) {
            return Some(name);
        }
        let Some(TokenKind::Ident(next)) = tokens.get(i + 1) else { return None };
        name = *next;
        i += 2;
    }
}

fn literal_constant(tokens: &[TokenKind]) -> bool {
    let mut i = 1;
    if !matches!(tokens.get(i), Some(TokenKind::Ident(_))) { return false }
    i += 1;
    if tokens.get(i) == Some(&TokenKind::Colon) {
        if !matches!(tokens.get(i + 1), Some(TokenKind::Ident(name))
            if matches!(name.as_str(), "int" | "uint" | "float" | "bool" | "string")) {
            return false;
        }
        i += 2;
    }
    if tokens.get(i) != Some(&TokenKind::Assign) { return false }
    i += 1;
    if matches!(tokens.get(i), Some(TokenKind::Minus | TokenKind::Plus)) {
        i += 1;
        if !matches!(tokens.get(i), Some(TokenKind::Int(..) | TokenKind::Float(_))) { return false }
    } else if !matches!(tokens.get(i), Some(TokenKind::Int(..) | TokenKind::Float(_)
        | TokenKind::True | TokenKind::False | TokenKind::Null | TokenKind::RawString(_)))
        && !matches!(tokens.get(i), Some(TokenKind::String(text)) if !text.as_str().contains(['{', '}'])) {
        return false;
    }
    matches!(tokens.get(i + 1), Some(TokenKind::Newline | TokenKind::EOF | TokenKind::Semicolon))
}

fn scan(path: &str, source: &str) -> Result<Scan> {
    let tokens = Tokenizer::from_code(path, source).tokenize()
        .map_err(|error| anyhow::anyhow!("{path}: {}", error.message))?;
    let tokens: Vec<_> = tokens.into_iter().flat_map(|token| {
        std::iter::once(token.kind).chain((token.trailing_newlines > 0).then_some(TokenKind::Newline))
    }).collect();
    let constant_effects = tokens.iter().enumerate().any(|(i, token)| {
        *token == TokenKind::Const && !literal_constant(&tokens[i..])
    });
    let tokens: Vec<_> = tokens.into_iter().filter(|token| !matches!(token,
        TokenKind::Comment(_) | TokenKind::Newline | TokenKind::EOF
    )).collect();
    let mut scan = Scan { keep_effects: constant_effects, ..Scan::default() };
    let has_sim = tokens.windows(2).any(|pair| {
        matches!(pair, [TokenKind::At, TokenKind::Ident(name)] if name.as_str() == "sim")
    });
    for (i, token) in tokens.iter().enumerate() {
        match token {
            TokenKind::Ident(name) => { scan.mentions.insert(*name); }
            TokenKind::String(text) | TokenKind::RawString(text) => {
                // String dispatch, interpolations and inline Lua are invisible
                // to identifier tokens, but can still name a sim-file helper.
                literal_names(text.as_str(), &mut scan.mentions);
            }
            TokenKind::Extern | TokenKind::Operator | TokenKind::Deco | TokenKind::Macro => scan.keep_effects = true,
            TokenKind::Global => {
                // Initializers are module-load effects; the name scan cannot
                // prove that an unreferenced initializer is pure.
                scan.keep_effects = true;
            }
            _ => {}
        }
        let declares = matches!(token, TokenKind::Fn | TokenKind::Struct | TokenKind::Enum
            | TokenKind::Newtype | TokenKind::Type | TokenKind::Global | TokenKind::Const | TokenKind::Macro)
            || (matches!(token, TokenKind::Ident(name) if matches!(name.as_str(), "value" | "trait"))
                && matches!(tokens.get(i + 1), Some(TokenKind::Ident(_))));
        if declares {
            if let Some(name) = declaration_name(&tokens[i + 1..]) {
                scan.definitions.insert(name);
                if name.as_str().starts_with("__") || matches!(name.as_str(),
                    "iter" | "next" | "to_string" | "hash" | "eq" | "cmp" | "len") {
                    scan.keep_effects = true;
                }
            } else if !matches!(tokens.get(i + 1), Some(TokenKind::OpenParen | TokenKind::Operator)) {
                anyhow::bail!("{path}: cannot enumerate declaration after `{token}`");
            }
        }
        if *token == TokenKind::At {
            let Some(TokenKind::Ident(name)) = tokens.get(i + 1) else {
                anyhow::bail!("{path}: cannot classify annotation");
            };
            scan.has_test |= name.as_str() == "test";
            let neutral = matches!(name.as_str(), "sim" | "sim_env" | "pure" | "inline" | "allow");
            let sim_phase = has_sim && matches!(name.as_str(), "update" | "scene_update"
                | "late_update" | "prepare" | "draw" | "late_draw" | "post_imgui_draw");
            scan.keep_effects |= !neutral && !sim_phase;
            if name.as_str() == "sim" {
                if let Some(TokenKind::String(wire) | TokenKind::RawString(wire)) = tokens.get(i + 3) {
                    literal_names(wire.as_str(), &mut scan.definitions);
                }
            }
        }
    }
    Ok(scan)
}

fn sim_path(path: &str, root: &Path) -> bool {
    let Ok(relative) = Path::new(path).strip_prefix(root) else { return false };
    let components: Vec<_> = relative.components().collect();
    components.windows(2).any(|pair| pair[0].as_os_str() == "tests" && pair[1].as_os_str() == "sims")
}

/// Only package-test callers use this filter. Sim discovery and execution keep
/// their full source set. False positives cost compilation; uncertain scans
/// retain everything with a reason, and selected-source errors still reject.
pub(crate) fn select(sources: &SourceFiles, project_root: &Path) -> Result<SourceFiles> {
    let root = project_root.canonicalize().context("cannot resolve package test root")?;
    let package_roots: HashMap<_, _> = sources.package_roots.iter()
        .map(|(id, root)| (*id, root.as_path())).collect();
    let candidates = sources.files.iter().map(|source| {
        let file = source.borrow();
        let owner = match &file.package {
            Some(tag) => *package_roots.get(&tag.id)
                .with_context(|| format!("source {} has no root for package {}", file.file.to_id_path(), tag.id))?,
            None => root.as_path(),
        };
        Ok(sim_path(&file.file.to_id_path(), owner))
    }).collect::<Result<Vec<_>>>()?;
    let total = candidates.iter().filter(|candidate| **candidate).count();
    if total == 0 { return Ok(sources.clone()) }

    let scanned = sources.files.iter().map(|source| {
        let file = source.borrow();
        scan(&file.file.to_id_path(), &file.contents)
    }).collect::<Result<Vec<_>>>();
    let scanned = match scanned {
        Ok(scanned) => scanned,
        Err(error) => {
            log::warn!(target: "TEST_SOURCES", "package test sim closure kept all {total} sim files: {error:#}");
            return Ok(sources.clone());
        }
    };
    let mut definitions: HashMap<Ustr, Vec<usize>> = HashMap::new();
    let mut selected = vec![false; sources.files.len()];
    let mut pending = Vec::new();
    let mut test_files = 0;
    for (i, scanned) in scanned.iter().enumerate() {
        if candidates[i] {
            for name in &scanned.definitions { definitions.entry(*name).or_default().push(i); }
            test_files += usize::from(scanned.has_test);
        }
        if !candidates[i] || scanned.keep_effects || scanned.definitions.is_empty() {
            selected[i] = true;
            pending.push(i);
        }
    }
    while let Some(i) = pending.pop() {
        for name in &scanned[i].mentions {
            for &provider in definitions.get(name).into_iter().flatten() {
                if !selected[provider] {
                    selected[provider] = true;
                    pending.push(provider);
                }
            }
        }
    }
    let kept = candidates.iter().zip(&selected).filter(|(candidate, selected)| **candidate && **selected).count();
    log::info!(target: "TEST_SOURCES", "package test sim closure: {kept}/{total} sim files, {test_files} files containing @test");
    Ok(SourceFiles {
        files: sources.files.iter().zip(selected).filter_map(|(file, selected)| selected.then(|| file.clone())).collect(),
        package_roots: sources.package_roots.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_keeps_global_effects_and_implicit_lookup() {
        for source in [
            "@test fn check() {}", "@nomangle fn callback() {}",
            "@on_reload fn reload() {}", "@shader(fragment) fn shader() {}",
            "global x: int = effect()", "const x: int = effect()",
            "fn iter() {}", "fn __op_add() {}", "extern fn host()",
            "@update fn global_frame() {}",
        ] {
            assert!(scan("fixture.rock", source).unwrap().keep_effects, "{source}");
        }
        assert!(!scan("fixture.rock", "@update fn frame() {} @sim fn simulation() {}").unwrap().keep_effects);
        assert!(!scan("fixture.rock", "const COUNT: int = -10\nfn unused() {}\n").unwrap().keep_effects);
    }

    #[test]
    fn scan_closes_methods_generics_strings_and_inline_lua() {
        let scanned = scan("fixture.rock", r#"
fn Wrapper<T>.method(x: int) {}
fn helper<T>(x: T) {}
@sim("wire_alias") fn simulation() {}
inline(lua) fn dispatch() = "return string_helper()"
fn literal() -> string { "{interpolated_helper()}" }
"#).unwrap();
        for name in ["method", "helper", "simulation", "wire_alias", "dispatch", "literal"] {
            assert!(scanned.definitions.contains(&Ustr::from(name)), "{name}");
        }
        for name in ["string_helper", "interpolated_helper"] {
            assert!(scanned.mentions.contains(&Ustr::from(name)), "{name}");
        }
        assert!(!scanned.definitions.contains(&Ustr::from("Wrapper")));
    }

    #[test]
    fn uncertain_scans_return_a_full_set_reason() {
        assert!(scan("fixture.rock", "fn bad() { \"unterminated").is_err());
        assert!(scan("fixture.rock", "fn ???").is_err());
    }

    #[test]
    fn sim_paths_are_relative_to_the_owning_package() {
        assert!(sim_path("/repo/tests/package/tests/sims/helper.rock", Path::new("/repo/tests/package")));
        assert!(!sim_path("/repo/tests/sims/package/main.rock", Path::new("/repo/tests/sims/package")));
        assert!(!sim_path("/other/tests/sims/helper.rock", Path::new("/repo/package")));
    }
}
