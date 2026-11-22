use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

static SEMANTIC_TOKEN_RESULT_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

fn generate_result_id() -> String {
    let id = SEMANTIC_TOKEN_RESULT_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("semantic_tokens_{}", id)
}

pub(crate) struct SemanticTokenCache {
    pub tokens: lsp_types::SemanticTokens,
    pub document_version: Option<i32>,
}

pub struct LspWorkspace {
    pub bone: bone_query::BoneWorkspace,
    pub(crate) allocation_hints: bool,
    pub(crate) open_files: HashMap<String, Vec<lsp_types::Diagnostic>>,
    pub(crate) allocations: crate::allocations::AllocationWork,

    semantic_token_cache: HashMap<String, SemanticTokenCache>,

    document_versions: HashMap<String, i32>,

    /// `bone.update` returns diagnostics for every tracked file on every keystroke;
    /// this keeps unchanged files from being re-published.
    published_diagnostics: HashMap<String, Vec<lsp_types::Diagnostic>>,
}

impl LspWorkspace {
    pub fn new() -> Self {
        Self {
            bone: bone_query::BoneWorkspace::new(),
            allocation_hints: false,
            open_files: HashMap::new(),
            allocations: Default::default(),
            semantic_token_cache: HashMap::new(),
            document_versions: HashMap::new(),
            published_diagnostics: HashMap::new(),
        }
    }

    /// Records the new state when it differs; an untracked path counts as previously empty.
    pub(crate) fn diagnostics_changed(
        &mut self,
        path: &str,
        diagnostics: &[lsp_types::Diagnostic],
    ) -> bool {
        let previous = self
            .published_diagnostics
            .get(path)
            .map_or(&[][..], Vec::as_slice);
        if previous == diagnostics {
            return false;
        }
        if diagnostics.is_empty() {
            self.published_diagnostics.remove(path);
        } else {
            self.published_diagnostics
                .insert(path.to_string(), diagnostics.to_vec());
        }
        true
    }

    pub(crate) fn update_document_version(&mut self, uri: &str, version: Option<i32>) {
        if let Some(v) = version {
            self.document_versions.insert(uri.to_string(), v);
        }
    }

    /// The last non-empty tokens computed for `uri`, whatever version they
    /// were computed for.
    pub(crate) fn cached_semantic_tokens(&self, uri: &str) -> Option<&SemanticTokenCache> {
        self.semantic_token_cache.get(uri)
    }

    pub(crate) fn label_semantic_tokens(
        &mut self,
        uri: &str,
        mut tokens: lsp_types::SemanticTokens,
        document_version: Option<i32>,
    ) -> lsp_types::SemanticTokens {
        tokens.result_id = Some(generate_result_id());
        // An empty first result still needs an id, but the next request must recompute.
        if tokens.data.is_empty() {
            log::warn!(
                "Rejected empty first-write semantic tokens update for {} (version {:?}); \
                 next request will recompute.",
                uri,
                document_version
            );
        } else {
            self.semantic_token_cache.insert(
                uri.to_string(),
                SemanticTokenCache {
                    tokens: tokens.clone(),
                    document_version,
                },
            );
        }
        tokens
    }

    pub(crate) fn get_document_version(&self, uri: &str) -> Option<i32> {
        self.document_versions.get(uri).copied()
    }

    /// Called after the `textDocument/didClose` handler reconciles disk contents.
    pub(crate) fn close_document(&mut self, uri: &str) {
        self.document_versions.remove(uri);
        self.semantic_token_cache.remove(uri);
    }
}

impl Default for LspWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod published_diagnostics_tests {
    use super::*;

    fn diag(msg: &str) -> lsp_types::Diagnostic {
        lsp_types::Diagnostic {
            message: msg.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn diagnostics_changed_dedups_unchanged_and_clears_once() {
        let mut ws = LspWorkspace::new();
        assert!(!ws.diagnostics_changed("a", &[]));
        assert!(!ws.diagnostics_changed("a", &[]));
        assert!(ws.diagnostics_changed("a", &[diag("E")]));
        assert!(!ws.diagnostics_changed("a", &[diag("E")]));
        assert!(ws.diagnostics_changed("a", &[diag("E2")]));
        assert!(ws.diagnostics_changed("a", &[]));
        assert!(!ws.diagnostics_changed("a", &[]));
        assert!(ws.diagnostics_changed("b", &[diag("E")]));
        assert!(!ws.diagnostics_changed("a", &[]));
    }
}
