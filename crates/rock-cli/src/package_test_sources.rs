use anyhow::{Context, Result};
use rock_core::{CompilerError, SourceFiles};
use rock_tokenizer::{Token, TokenKind};
use rustc_hash::{FxHashMap, FxHashSet};
use skull_parser::{
    Ast, EnumVariantKind, ExprKind, FunctionBody, GlobalDef, Item, ItemKind, TypeExprKind,
};
use std::path::Path;
use ustr::Ustr;

pub(crate) fn candidates(sources: &SourceFiles, project_root: &Path) -> Result<Vec<bool>> {
    let root = project_root
        .canonicalize()
        .context("cannot resolve package test root")?;
    sources
        .files
        .iter()
        .map(|source| {
            let file = source.borrow();
            let path = file.file.to_id_path();
            let owner = match &file.package {
                Some(tag) => sources
                    .package_roots
                    .iter()
                    .find(|(id, _)| *id == tag.id)
                    .map(|(_, root)| root.as_path())
                    .with_context(|| format!("source {path} has no root for package {}", tag.id))?,
                None => root.as_path(),
            };
            Ok(sim_path(&path, owner))
        })
        .collect()
}

fn sim_path(path: &str, root: &Path) -> bool {
    let Ok(relative) = Path::new(path).strip_prefix(root) else {
        return false;
    };
    let mut parts = relative.iter();
    let mut previous = parts.next();
    for part in parts {
        if previous.is_some_and(|p| p == "tests") && part == "sims" {
            return true;
        }
        previous = Some(part);
    }
    false
}

/// Reuse the trees used for selection for semantic checking. Every file must
/// parse before we can tell which fragments a test needs.
pub(crate) fn parse(
    sources: &mut SourceFiles,
    candidates: &[bool],
) -> Result<(Vec<Ast>, Ustr), Vec<CompilerError>> {
    let total = candidates.iter().filter(|&&candidate| candidate).count();
    if total == 0 {
        let (modules, entry, _) = crate::bone_backend::parse_user_modules(sources)?;
        return Ok((modules, entry));
    }

    let parsed = crate::bone_backend::parse_user_files(sources, |source, file| {
        skull_parser::parse_module_with_tokens(source, file, mentions)
    })?;
    let syntax = skull::pipeline::syntax_errors(parsed.iter().map(|(ast, _)| ast));
    if !syntax.is_empty() {
        return Err(syntax);
    }

    let mut providers = FxHashMap::<Ustr, Vec<usize>>::default();
    let mut keep = vec![true; parsed.len()];
    let mut pending = Vec::new();
    let mut test_files = 0;
    for (i, (ast, _)) in parsed.iter().enumerate() {
        if candidates[i] {
            let mut facts = Fragment::default();
            facts.read(ast, &ast.items);
            test_files += usize::from(facts.has_test);
            keep[i] = ast
                .module_header
                .as_ref()
                .is_some_and(|header| header.vis.is_some())
                || facts.keep
                || (facts.has_phase && !facts.has_sim)
                || facts.names.is_empty();
            for name in facts.names {
                providers.entry(name).or_default().push(i);
            }
        }
        if keep[i] {
            pending.push(i);
        }
    }
    while let Some(i) = pending.pop() {
        for name in &parsed[i].1 {
            for &provider in providers.get(name).into_iter().flatten() {
                if !keep[provider] {
                    keep[provider] = true;
                    pending.push(provider);
                }
            }
        }
    }
    let kept = candidates
        .iter()
        .zip(&keep)
        .filter(|&(&candidate, &keep)| candidate && keep)
        .count();
    log::info!(target: "TEST_SOURCES", "package test sim closure: {kept}/{total} sim files, {test_files} files containing @test");

    let modules: Vec<_> = parsed
        .into_iter()
        .zip(&keep)
        .filter(|(_, keep)| **keep)
        .map(|((ast, _), _)| ast)
        .collect();
    let entry = crate::bone_backend::project_entry(&modules)?;
    *sources = sources.retaining(&keep);
    Ok((modules, entry))
}

fn words(text: &str) -> impl Iterator<Item = Ustr> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| word.starts_with(|c: char| c.is_alphabetic() || c == '_'))
        .map(Ustr::from)
}

fn mentions(tokens: &[Token]) -> FxHashSet<Ustr> {
    let mut names = FxHashSet::default();
    for token in tokens {
        match token.kind {
            TokenKind::Ident(name) => {
                names.insert(name);
            }
            // String dispatch and inline backend code can name a helper too.
            TokenKind::String(text) | TokenKind::RawString(text) => {
                names.extend(words(text.as_str()))
            }
            _ => {}
        }
    }
    names
}

#[derive(Default)]
struct Fragment {
    names: FxHashSet<Ustr>,
    keep: bool,
    has_test: bool,
    has_sim: bool,
    has_phase: bool,
}

impl Fragment {
    fn read(&mut self, ast: &Ast, items: &[Item]) {
        for item in items {
            for annotation in &item.annotations {
                match annotation.name.name.as_str() {
                    "sim" => {
                        self.has_sim = true;
                        if let Some(arg) = annotation.args.first()
                            && let ExprKind::String(wire) = ast.exprs[arg.value].kind
                        {
                            self.names.extend(words(wire.as_str()));
                        }
                    }
                    "test" => {
                        self.has_test = true;
                        self.keep = true;
                    }
                    "sim_env" | "pure" | "inline" | "allow" => {}
                    "update" | "scene_update" | "late_update" | "prepare" | "draw"
                    | "late_draw" | "post_imgui_draw" => self.has_phase = true,
                    _ => self.keep = true,
                }
            }
            let name = match &item.kind {
                ItemKind::Function(def) => {
                    self.keep |= def.is_macro
                        || !matches!(def.body, FunctionBody::Rock(_) | FunctionBody::Inline(..))
                        || def.params.iter().any(|param| !param.annotations.is_empty());
                    def.name.name
                }
                ItemKind::Struct(def) => {
                    self.keep |= def.fields.iter().any(|field| !field.annotations.is_empty());
                    def.name.name
                }
                ItemKind::Enum(def) => {
                    self.keep |= def.variants.iter().any(|variant| {
                        matches!(&variant.kind, EnumVariantKind::Record(fields) if fields.iter().any(|field| !field.annotations.is_empty()))
                    });
                    def.name.name
                }
                ItemKind::Newtype(def) => def.name.name,
                ItemKind::Global(def) => {
                    // Initializers can run effects at module load.
                    self.keep |= !literal_constant(ast, def);
                    def.name.name
                }
                ItemKind::Use(decl) => {
                    self.keep |= decl.vis.is_some();
                    continue;
                }
                ItemKind::Decorator(_) | ItemKind::MacroInvocation { .. } => {
                    self.keep = true;
                    continue;
                }
                ItemKind::Comment(_) | ItemKind::CompilerDirective(_) => continue,
            };
            // Overload resolution and builtins look up these names implicitly.
            self.keep |= name.as_str().starts_with("__")
                || matches!(
                    name.as_str(),
                    "iter" | "next" | "to_string" | "hash" | "eq" | "cmp" | "len"
                );
            self.names.insert(name);
        }
    }
}

fn literal_constant(ast: &Ast, def: &GlobalDef) -> bool {
    def.is_const
        && def.ty.is_none_or(|ty| {
            let ty = &ast.types[ty];
            !ty.is_nullable
                && matches!(
                    ty.kind,
                    TypeExprKind::Int
                        | TypeExprKind::Uint
                        | TypeExprKind::Float
                        | TypeExprKind::Bool
                        | TypeExprKind::String
                )
        })
        && def.value.is_some_and(|value| match ast.exprs[value].kind {
            ExprKind::Int(..)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::String(_)
            | ExprKind::Null => true,
            ExprKind::UnaryMinus(operand) => matches!(
                ast.exprs[operand].kind,
                ExprKind::Int(..) | ExprKind::Float(_)
            ),
            _ => false,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sim_paths_are_relative_to_the_owning_package() {
        assert!(sim_path(
            "/repo/tests/package/tests/sims/helper.rock",
            Path::new("/repo/tests/package")
        ));
        assert!(!sim_path(
            "/repo/tests/sims/package/main.rock",
            Path::new("/repo/tests/sims/package")
        ));
        assert!(!sim_path(
            "/other/tests/sims/helper.rock",
            Path::new("/repo/package")
        ));
    }
}
