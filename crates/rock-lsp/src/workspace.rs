use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Global counter for semantic token result IDs
static SEMANTIC_TOKEN_RESULT_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Generate a unique result ID for semantic tokens
fn generate_result_id() -> String {
    let id = SEMANTIC_TOKEN_RESULT_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("semantic_tokens_{}", id)
}

pub(crate) struct SemanticTokenCache {
    pub tokens: lsp_types::SemanticTokens,
    pub document_version: Option<i32>,
}

/// Workspace managing the bone session plus per-document LSP caches.
pub struct LspWorkspace {
    /// bone-backed state: the per-root sessions that serve every query.
    pub bone: bone_query::BoneWorkspace,

    /// Semantic token cache with document version tracking
    semantic_token_cache: HashMap<String, SemanticTokenCache>,

    document_versions: HashMap<String, i32>,

    /// Last diagnostics published per file path, so the bone publish path can
    /// skip re-sending unchanged files. `bone.update` returns diagnostics for
    /// EVERY tracked file (the whole compilation unit, incl. deps) on every
    /// keystroke; without this we'd re-publish ~130 notifications/edit, almost
    /// all empty clears for files the editor never opened.
    published_diagnostics: HashMap<String, Vec<lsp_types::Diagnostic>>,
}

impl LspWorkspace {
    pub fn new() -> Self {
        Self {
            bone: bone_query::BoneWorkspace::new(),
            semantic_token_cache: HashMap::new(),
            document_versions: HashMap::new(),
            published_diagnostics: HashMap::new(),
        }
    }

    /// True if `diagnostics` differ from what was last published for `path`
    /// (an untracked path is treated as previously-empty). Records the new
    /// state when it differs, so the caller publishes only changed files.
    /// Empty diagnostics for a never-published (or already-empty) file are a
    /// no-op -- we never emit a redundant "clear".
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

    /// Update the document version.
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

    /// Get current document version
    pub(crate) fn get_document_version(&self, uri: &str) -> Option<i32> {
        self.document_versions.get(uri).copied()
    }

    /// Drop all per-document caches for a URI the editor has closed.
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

    /// `diagnostics_changed` gates the bone publish path so only files whose
    /// diagnostics actually changed are re-sent. Pins every transition: a
    /// never-published clean file is a no-op (no redundant clear), an error
    /// publishes once, a clear after an error publishes once, and steady state
    /// is silent.
    #[test]
    fn diagnostics_changed_dedups_unchanged_and_clears_once() {
        let mut ws = LspWorkspace::new();
        // Clean + never published -> nothing to publish.
        assert!(!ws.diagnostics_changed("a", &[]));
        assert!(!ws.diagnostics_changed("a", &[]));
        // First error -> publish; same error again -> skip.
        assert!(ws.diagnostics_changed("a", &[diag("E")]));
        assert!(!ws.diagnostics_changed("a", &[diag("E")]));
        // Different error -> publish.
        assert!(ws.diagnostics_changed("a", &[diag("E2")]));
        // Becomes clean -> publish the clear ONCE, then silent.
        assert!(ws.diagnostics_changed("a", &[]));
        assert!(!ws.diagnostics_changed("a", &[]));
        // A different file is independent.
        assert!(ws.diagnostics_changed("b", &[diag("E")]));
        assert!(!ws.diagnostics_changed("a", &[]));
    }
}
