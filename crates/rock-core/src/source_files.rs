use crate::prelude::*;
use std::sync::Arc;
use atomic_refcell::AtomicRefCell;

#[derive(Clone, Debug)]
pub struct SourceFiles {
    pub files: Vec<Arc<AtomicRefCell<SourceFile>>>,
}

impl SourceFiles {
    pub fn from_single_file(file: SourceFile) -> Self {
        Self {
            files: vec![Arc::new(AtomicRefCell::new(file))],
        }
    }

    pub fn add_or_update_file(&mut self, file: SourceFile) {
        if let Some(existing) = self
            .files
            .iter_mut()
            .find(|f| f.borrow().file.to_id_path() == file.file.to_id_path())
        {
            *existing.borrow_mut() = file;
        } else {
            self.files.push(Arc::new(AtomicRefCell::new(file)));
        }
    }

    pub fn get_line(&self, line_col: &LineCol) -> Option<String> {
        let file = self.file_for_linecol(line_col)?;
        let file = file.borrow();
        file.contents
            .lines()
            .nth(line_col.line)
            .map(|s| s.to_string())
    }

    pub fn file_for_linecol(&self, line_col: &LineCol) -> Option<&Arc<AtomicRefCell<SourceFile>>> {
        self.files
            .iter()
            .find(|f| *f.borrow().file.to_id_path() == *line_col.file)
    }

    /// A copy with every test-only file removed. Test-only is the path
    /// convention: any file under a `tests/` directory component (sims live in
    /// `<pkg>/tests/sims/`, unit tests in `<pkg>/tests/`). Game builds
    /// (`rock run/stone/frame/perf/compile <game>`) use this so a compile error
    /// in a sim/test can't break the running game. Test/sim/lint/lsp paths use
    /// the full set. See `docs/sim-layering.md`.
    pub fn production_only(&self) -> SourceFiles {
        SourceFiles {
            files: self
                .files
                .iter()
                .filter(|f| !Self::is_test_only_path(&f.borrow().file.to_id_path()))
                .cloned()
                .collect(),
        }
    }

    /// Path-convention test-only check: true if any path component is `tests`.
    pub fn is_test_only_path(path: &str) -> bool {
        std::path::Path::new(path)
            .components()
            .any(|c| c.as_os_str() == "tests")
    }
}
