//! The parsed stdlib: the one loader the CLI pipeline and the LSP session both
//! warm from, so they cannot disagree about what a broken stdlib means.
//!
//! The stdlib has no recovery story -- a dropped item would surface later as
//! "callee not found" in user code -- so any syntax error, recovered or not,
//! fails the whole load with an error naming the file.

use std::sync::{Arc, LazyLock};

use rock_core::CompilerError;
use rock_stdlib::StdlibFile;
use ustr::Ustr;

use crate::Ast;

/// The process's stdlib ([`rock_stdlib::stdlib_files`]) parsed in load order
/// under the stdlib session ids. Parsed once and shared, never edited: a
/// module set holds it by reference ([`ModuleSet`]), so a cache over its
/// checked form recognizes it by identity ([`is_process_stdlib`]).
pub fn cached() -> &'static Result<Arc<[Ast]>, CompilerError> {
    static STDLIB: LazyLock<Result<Arc<[Ast]>, CompilerError>> =
        LazyLock::new(|| parse(rock_stdlib::stdlib_files()).map(Arc::from));
    &STDLIB
}

/// The source text of every process stdlib file under its session id, in
/// load order, for a cache that keys emitted bodies on their inputs.
pub fn source_texts() -> Vec<(Ustr, &'static str)> {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            (
                Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str()),
                f.contents,
            )
        })
        .collect()
}

/// Whether `stdlib` is the process stdlib itself ([`cached`]), not a copy or
/// another parse.
pub fn is_process_stdlib(stdlib: &Arc<[Ast]>) -> bool {
    cached()
        .as_ref()
        .is_ok_and(|cached| Arc::ptr_eq(cached, stdlib))
}

/// A compile's module set: a shared stdlib prefix, held by reference and
/// never edited, then the modules a build owns. A pass that must edit the
/// prefix (expand its macros) takes an owned copy first ([`Self::make_owned`]).
#[derive(Clone, Debug)]
pub struct ModuleSet {
    shared: Arc<[Ast]>,
    owned: Vec<Ast>,
}

impl ModuleSet {
    /// The process stdlib with `user` modules after it. A stdlib that does
    /// not parse is the error.
    pub fn with_stdlib(user: impl IntoIterator<Item = Ast>) -> Result<Self, CompilerError> {
        let stdlib = cached().as_ref().map_err(Clone::clone)?;
        Ok(Self::new(stdlib.clone(), user.into_iter().collect()))
    }

    /// `owned` modules after the shared `prefix`.
    pub fn new(prefix: Arc<[Ast]>, owned: Vec<Ast>) -> Self {
        Self {
            shared: prefix,
            owned,
        }
    }

    /// A set of modules all owned, with no shared prefix.
    pub fn owned(modules: Vec<Ast>) -> Self {
        Self::new(Arc::from([]), modules)
    }

    /// The shared prefix (empty once [`Self::make_owned`] ran).
    pub fn shared(&self) -> &Arc<[Ast]> {
        &self.shared
    }

    /// The modules after the shared prefix.
    pub fn rest(&self) -> &[Ast] {
        &self.owned
    }

    pub fn rest_mut(&mut self) -> &mut [Ast] {
        &mut self.owned
    }

    /// Append an owned module.
    pub fn push(&mut self, module: Ast) {
        self.owned.push(module);
    }

    /// Every module, the shared prefix first.
    pub fn iter(&self) -> std::iter::Chain<std::slice::Iter<'_, Ast>, std::slice::Iter<'_, Ast>> {
        self.shared.iter().chain(&self.owned)
    }

    pub fn len(&self) -> usize {
        self.shared.len() + self.owned.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every module as one owned slice, copying the shared prefix in on the
    /// first call.
    pub fn make_owned(&mut self) -> &mut [Ast] {
        if !self.shared.is_empty() {
            let mut all = self.shared.to_vec();
            all.append(&mut self.owned);
            self.owned = all;
            self.shared = Arc::from([]);
        }
        &mut self.owned
    }

    /// The shared prefix and the modules after it.
    pub fn into_parts(self) -> (Arc<[Ast]>, Vec<Ast>) {
        (self.shared, self.owned)
    }

    /// Every module, owned.
    pub fn into_vec(mut self) -> Vec<Ast> {
        self.make_owned();
        self.owned
    }
}

impl<'a> IntoIterator for &'a ModuleSet {
    type Item = &'a Ast;
    type IntoIter = std::iter::Chain<std::slice::Iter<'a, Ast>, std::slice::Iter<'a, Ast>>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Parse `files` as a stdlib. The first file that does not parse cleanly is
/// the error.
pub fn parse(files: &[StdlibFile]) -> Result<Vec<Ast>, CompilerError> {
    files
        .iter()
        .map(|f| {
            let id = Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
            crate::parse_module_clean(f.contents, id).map_err(|e| {
                let mut error = e.clone();
                error.message =
                    format!("stdlib `{}` does not parse: {}", f.display_path, e.message);
                error
            })
        })
        .collect()
}
