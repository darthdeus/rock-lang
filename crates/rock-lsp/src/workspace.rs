use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Global counter for semantic token result IDs
static SEMANTIC_TOKEN_RESULT_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Generate a unique result ID for semantic tokens
fn generate_result_id() -> String {
    let id = SEMANTIC_TOKEN_RESULT_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("semantic_tokens_{}", id)
}

/// Returns true if a SemanticTokensResult has no token data.
///
/// Empty results must not be cached: the presence of a cache entry (even
/// an empty one) short-circuits the `cached_is_current` path and returns
/// whatever is in the cache. A transient empty result thus locks in empty
/// highlighting until the document version changes or the LSP restarts.
pub fn semantic_tokens_result_is_empty(result: &lsp_types::SemanticTokensResult) -> bool {
    match result {
        lsp_types::SemanticTokensResult::Tokens(t) => t.data.is_empty(),
        lsp_types::SemanticTokensResult::Partial(p) => p.data.is_empty(),
    }
}

/// Semantic token cache entry with version tracking
#[derive(Clone, Debug)]
pub struct SemanticTokenCache {
    pub result_id: String,
    pub tokens: lsp_types::SemanticTokensResult,
    pub document_version: Option<i32>,
}

/// Inlay hints cache entry with version tracking
#[derive(Clone, Debug)]
pub struct InlayHintCache {
    pub hints: Vec<lsp_types::InlayHint>,
    pub document_version: Option<i32>,
    pub range: lsp_types::Range,
}

/// Document version tracker
#[derive(Default)]
pub struct DocumentVersionTracker {
    pub versions: HashMap<String, i32>,
}

/// Workspace managing the bone session plus per-document LSP caches.
pub struct LspWorkspace {
    /// bone-backed state: the per-root sessions that serve every query.
    pub bone: bone_query::BoneWorkspace,

    /// Semantic token cache with document version tracking
    semantic_token_cache: HashMap<String, SemanticTokenCache>,

    /// Inlay hints cache with document version tracking
    inlay_hint_cache: HashMap<String, InlayHintCache>,

    /// Document version tracker
    document_versions: DocumentVersionTracker,

    /// Last diagnostics published per file path, so the bone publish path can
    /// skip re-sending unchanged files. `bone.update` returns diagnostics for
    /// EVERY tracked file (the whole compilation unit, incl. deps) on every
    /// keystroke; without this we'd re-publish ~130 notifications/edit, almost
    /// all empty clears for files the editor never opened.
    published_diagnostics: HashMap<String, Vec<lsp_types::Diagnostic>>,
}

impl LspWorkspace {
    pub fn new() -> Self {
        Self::with_policy(crate::CompilerPolicy::default())
    }

    pub fn with_policy(policy: crate::CompilerPolicy) -> Self {
        Self {
            bone: policy.workspace(),
            semantic_token_cache: HashMap::new(),
            inlay_hint_cache: HashMap::new(),
            document_versions: DocumentVersionTracker::default(),
            published_diagnostics: HashMap::new(),
        }
    }

    /// True if `diagnostics` differ from what was last published for `path`
    /// (an untracked path is treated as previously-empty). Records the new
    /// state when it differs, so the caller publishes only changed files.
    /// Empty diagnostics for a never-published (or already-empty) file are a
    /// no-op -- we never emit a redundant "clear".
    pub fn diagnostics_changed(
        &mut self,
        path: &str,
        diagnostics: &[lsp_types::Diagnostic],
    ) -> bool {
        let prev_empty = self
            .published_diagnostics
            .get(path)
            .map(|v| v.is_empty())
            .unwrap_or(true);
        if diagnostics.is_empty() {
            if prev_empty {
                return false;
            }
            self.published_diagnostics.remove(path);
            return true;
        }
        if self.published_diagnostics.get(path).map(|v| v.as_slice()) == Some(diagnostics) {
            return false;
        }
        self.published_diagnostics
            .insert(path.to_string(), diagnostics.to_vec());
        true
    }

    /// Update document version and invalidate caches
    pub fn update_document_version(&mut self, uri: &str, version: Option<i32>) {
        if let Some(v) = version {
            // Update version tracker
            self.document_versions.versions.insert(uri.to_string(), v);

            // SMART CACHE INVALIDATION: Don't remove semantic tokens immediately
            // They will be replaced when new tokens are successfully computed
            // This prevents semantic tokens from disappearing during recompilation

            // Still invalidate inlay hints as they're less critical
            self.inlay_hint_cache.remove(uri);
        }
    }

    /// Get cached semantic tokens if valid for the current document version
    pub fn get_cached_semantic_tokens(
        &self,
        uri: &str,
        current_version: Option<i32>,
    ) -> Option<&SemanticTokenCache> {
        let cache_entry = self.semantic_token_cache.get(uri)?;
        // SMART CACHE: Return stale tokens if we have them. The cache will be
        // updated when new tokens are successfully computed, so tokens don't
        // disappear during edits. (We return the entry whether or not its
        // version matches `current_version`.)
        let _ = current_version;
        Some(cache_entry)
    }

    /// Cache semantic tokens for a document version
    pub fn cache_semantic_tokens(
        &mut self,
        uri: &str,
        tokens: lsp_types::SemanticTokensResult,
        document_version: Option<i32>,
    ) -> String {
        // Defense in depth: never cache an empty SemanticTokensResult.
        //
        // An empty result in the cache is always worse than no cache:
        // - If there's an existing non-empty entry, overwriting it with
        //   empty would wipe the editor's highlighting.
        // - If there's no existing entry, caching empty makes the next
        //   `cached_is_current` check return the empty value without
        //   recomputing, which means a transient compute failure can lock
        //   in empty tokens until the document version changes or the
        //   LSP restarts.
        //
        // Refusing to cache empty results forces the handler to recompute
        // on the next request, giving the system a chance to recover.
        if semantic_tokens_result_is_empty(&tokens) {
            if let Some(existing) = self.semantic_token_cache.get(uri) {
                log::warn!(
                    "Rejected empty semantic tokens update for {} (version {:?}); \
                     preserving existing cache entry (version {:?}).",
                    uri,
                    document_version,
                    existing.document_version
                );
                return existing.result_id.clone();
            }
            log::warn!(
                "Rejected empty first-write semantic tokens update for {} (version {:?}); \
                 next request will recompute.",
                uri,
                document_version
            );
            // No existing entry and nothing meaningful to store. Return a
            // fresh result_id for LSP protocol compliance, but do NOT insert
            // anything into the cache.
            return generate_result_id();
        }

        let result_id = generate_result_id();

        let cache_entry = SemanticTokenCache {
            result_id: result_id.clone(),
            tokens,
            document_version,
        };

        self.semantic_token_cache
            .insert(uri.to_string(), cache_entry);
        result_id
    }

    /// Get current document version
    pub fn get_document_version(&self, uri: &str) -> Option<i32> {
        self.document_versions.versions.get(uri).copied()
    }

    /// Get cached inlay hints if valid for the current document version and range
    pub fn get_cached_inlay_hints(
        &self,
        uri: &str,
        current_version: Option<i32>,
        range: &lsp_types::Range,
    ) -> Option<&InlayHintCache> {
        let cache_entry = self.inlay_hint_cache.get(uri)?;
        // Check if the cached hints are for the current document version and range
        if cache_entry.document_version == current_version && cache_entry.range == *range {
            Some(cache_entry)
        } else {
            None
        }
    }

    /// Cache inlay hints for a document version and range
    pub fn cache_inlay_hints(
        &mut self,
        uri: &str,
        hints: Vec<lsp_types::InlayHint>,
        document_version: Option<i32>,
        range: lsp_types::Range,
    ) {
        let cache_entry = InlayHintCache {
            hints,
            document_version,
            range,
        };
        self.inlay_hint_cache.insert(uri.to_string(), cache_entry);
    }

    /// Clear inlay hints cache for a specific URI
    /// This ensures stale hints are not displayed after document changes
    pub fn clear_inlay_hints_cache(&mut self, uri: &str) {
        self.inlay_hint_cache.remove(uri);
    }

    /// Drop all per-document caches for a URI the editor has closed.
    /// Called from the `textDocument/didClose` handler. The bone session entry
    /// for the file is dropped separately (`workspace.bone.remove`).
    pub fn close_document(&mut self, uri: &str) {
        self.document_versions.versions.remove(uri);
        self.inlay_hint_cache.remove(uri);
        self.semantic_token_cache.remove(uri);
    }

    /// Test accessors for the cache layer. Count methods let tests assert
    /// that eviction reclaimed the right amount of state without exposing
    /// the underlying maps.
    #[doc(hidden)]
    pub fn document_versions_count(&self) -> usize {
        self.document_versions.versions.len()
    }
    #[doc(hidden)]
    pub fn semantic_token_cache_count(&self) -> usize {
        self.semantic_token_cache.len()
    }
    #[doc(hidden)]
    pub fn inlay_hint_cache_count(&self) -> usize {
        self.inlay_hint_cache.len()
    }

    /// Force invalidate inlay hints cache by setting a special marker
    /// This will cause the next request to return empty hints, forcing editor refresh
    pub fn force_invalidate_inlay_hints(&mut self, uri: &str) {
        // Insert a special cache entry that will force empty hints on next request
        let invalidation_marker = InlayHintCache {
            hints: Vec::new(),
            document_version: Some(-1), // Special marker version
            range: lsp_types::Range::new(
                lsp_types::Position::new(0, 0),
                lsp_types::Position::new(0, 0),
            ),
        };
        self.inlay_hint_cache
            .insert(uri.to_string(), invalidation_marker);
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
