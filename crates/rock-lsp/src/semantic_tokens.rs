use bone_query::FileUriExt;
use lsp_types::{
    SemanticToken, SemanticTokens, SemanticTokensDelta, SemanticTokensDeltaParams,
    SemanticTokensFullDeltaResult, SemanticTokensLegend, SemanticTokensParams,
    SemanticTokensRangeParams, Uri,
};

use crate::LspWorkspace;

pub fn get_semantic_tokens_legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: bone_query::semantic_tokens_legend(),
        token_modifiers: bone_query::semantic_tokens_modifiers_legend(),
    }
}

/// `start`/`delete_count` are flat integer units (5 per token), per the LSP spec.
pub fn semantic_tokens_delta_edits(
    old: &[SemanticToken],
    new: &[SemanticToken],
) -> Vec<lsp_types::SemanticTokensEdit> {
    let max_pre = old.len().min(new.len());
    let mut p = 0;
    while p < max_pre && old[p] == new[p] {
        p += 1;
    }
    let max_suf = (old.len() - p).min(new.len() - p);
    let mut s = 0;
    while s < max_suf && old[old.len() - 1 - s] == new[new.len() - 1 - s] {
        s += 1;
    }
    let removed = old.len() - p - s;
    let added = &new[p..new.len() - s];
    if removed == 0 && added.is_empty() {
        return Vec::new();
    }
    vec![lsp_types::SemanticTokensEdit {
        start: (p * 5) as u32,
        delete_count: (removed * 5) as u32,
        data: Some(added.to_vec()),
    }]
}

pub(crate) fn range_semantic_tokens(
    workspace: &mut LspWorkspace,
    params: &SemanticTokensRangeParams,
) -> anyhow::Result<Option<SemanticTokens>> {
    let file_path = params.text_document.uri.to_file_path()?;
    Ok(Some(
        workspace
            .bone
            .semantic_tokens_range(&file_path, params.range),
    ))
}

/// `uri` decodes to the key `bone.update` registers on didChange.
pub fn compute_semantic_tokens_workspace(
    workspace: &mut LspWorkspace,
    uri: &Uri,
) -> anyhow::Result<SemanticTokens> {
    let file_path = uri.to_file_path()?;
    Ok(workspace.bone.semantic_tokens(&file_path))
}

/// A failed or empty recompute falls back to the cached tokens.
pub(crate) fn full_semantic_tokens(
    workspace: &mut LspWorkspace,
    params: &SemanticTokensParams,
) -> anyhow::Result<Option<SemanticTokens>> {
    let uri = params.text_document.uri.to_string();
    let version = workspace.get_document_version(&uri);
    let cached = workspace.cached_semantic_tokens(&uri);
    if let Some(entry) = cached.filter(|c| c.document_version == version) {
        return Ok(Some(entry.tokens.clone()));
    }
    let stale = cached.map(|entry| entry.tokens.clone());
    match compute_semantic_tokens_workspace(workspace, &params.text_document.uri) {
        Ok(tokens) if !tokens.data.is_empty() || stale.is_none() => {
            Ok(Some(workspace.label_semantic_tokens(&uri, tokens, version)))
        }
        Ok(_) => {
            log::warn!(
                "compute_semantic_tokens returned empty for {uri}; \
                 returning stale non-empty cache to preserve highlighting."
            );
            Ok(stale)
        }
        Err(e) if stale.is_none() => Err(e),
        Err(_) => Ok(stale),
    }
}

pub(crate) fn delta_semantic_tokens(
    workspace: &mut LspWorkspace,
    params: &SemanticTokensDeltaParams,
) -> anyhow::Result<Option<SemanticTokensFullDeltaResult>> {
    let uri = params.text_document.uri.to_string();
    let version = workspace.get_document_version(&uri);
    match compute_semantic_tokens_workspace(workspace, &params.text_document.uri) {
        Ok(new) if !new.data.is_empty() => {
            let edits = workspace
                .cached_semantic_tokens(&uri)
                .filter(|old| old.tokens.result_id.as_deref() == Some(&params.previous_result_id))
                .map(|old| semantic_tokens_delta_edits(&old.tokens.data, &new.data));
            let tokens = workspace.label_semantic_tokens(&uri, new, version);
            Ok(Some(match edits {
                Some(edits) => SemanticTokensFullDeltaResult::TokensDelta(SemanticTokensDelta {
                    result_id: tokens.result_id,
                    edits,
                }),
                None => SemanticTokensFullDeltaResult::Tokens(tokens),
            }))
        }
        other => {
            if let Err(e) = other {
                log::warn!("semantic tokens delta compute failed for {uri}: {e:?}");
            }
            Ok(workspace
                .cached_semantic_tokens(&uri)
                .map(|c| SemanticTokensFullDeltaResult::Tokens(c.tokens.clone())))
        }
    }
}
