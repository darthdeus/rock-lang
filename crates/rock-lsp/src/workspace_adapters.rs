/// Adapter functions to make existing LSP operations work with the workspace model
use crate::*;
use anyhow::Result;
use lsp_types::*;

fn get_file_path_from_params(params: &TextDocumentPositionParams) -> Result<String> {
    Ok(params.text_document.uri.to_file_path()?)
}

/// Workspace-aware go to definition
pub(crate) fn go_to_definition_workspace(
    workspace: &mut LspWorkspace,
    params: &GotoDefinitionParams,
) -> Result<Option<GotoDefinitionResponse>> {
    crate::scope!("go_to_definition_workspace");
    let file_path = get_file_path_from_params(&params.text_document_position_params)?;
    let pos = params.text_document_position_params.position;
    Ok(workspace
        .bone
        .definition(&file_path, pos)
        .map(|loc| GotoDefinitionResponse::Array(vec![loc])))
}

/// Workspace-aware go to type definition (LSP `textDocument/typeDefinition`):
/// jump to the declaration of the *type* of the value under the cursor.
pub(crate) fn go_to_type_definition_workspace(
    workspace: &mut LspWorkspace,
    params: &GotoTypeDefinitionParams,
) -> Result<Option<GotoDefinitionResponse>> {
    crate::scope!("go_to_type_definition_workspace");
    let file_path = get_file_path_from_params(&params.text_document_position_params)?;
    let pos = params.text_document_position_params.position;
    Ok(workspace
        .bone
        .type_definition(&file_path, pos)
        .map(|loc| GotoDefinitionResponse::Array(vec![loc])))
}

/// Workspace-aware find references
pub(crate) fn find_references_workspace(
    workspace: &mut LspWorkspace,
    params: &ReferenceParams,
) -> Result<Option<Vec<Location>>> {
    crate::scope!("find_references_workspace");
    let file_path = get_file_path_from_params(&params.text_document_position)?;
    let pos = params.text_document_position.position;
    let locs = workspace.bone.references(&file_path, pos);
    Ok(if locs.is_empty() { None } else { Some(locs) })
}

/// Workspace-aware document highlight: occurrences of the symbol under the
/// cursor within the current file (bone's symbol-aware references query,
/// filtered to this file).
pub fn do_document_highlight_workspace(
    workspace: &mut LspWorkspace,
    params: &DocumentHighlightParams,
) -> Result<Option<Vec<DocumentHighlight>>> {
    crate::scope!("do_document_highlight_workspace");
    let file_path = params
        .text_document_position_params
        .text_document
        .uri
        .to_file_path()?;
    let pos = params.text_document_position_params.position;
    let hl = workspace.bone.document_highlight(&file_path, pos);
    Ok((!hl.is_empty()).then_some(hl))
}

/// Workspace-aware hover
pub(crate) fn do_hover_workspace(
    workspace: &mut LspWorkspace,
    params: &HoverParams,
) -> Result<Option<Hover>> {
    crate::scope!("do_hover_workspace");
    let file_path = get_file_path_from_params(&params.text_document_position_params)?;
    let pos = params.text_document_position_params.position;
    Ok(workspace.bone.hover(&file_path, pos))
}

/// Workspace-aware signature help
pub(crate) fn do_signature_help_workspace(
    workspace: &mut LspWorkspace,
    params: &SignatureHelpParams,
) -> Result<Option<SignatureHelp>> {
    crate::scope!("do_signature_help_workspace");
    let file_path = get_file_path_from_params(&params.text_document_position_params)?;
    let pos = params.text_document_position_params.position;
    Ok(workspace.bone.signature_help(&file_path, pos))
}

/// Workspace-aware document symbols.
pub fn do_document_symbols_workspace(
    workspace: &mut LspWorkspace,
    params: &DocumentSymbolParams,
) -> Result<Option<DocumentSymbolResponse>> {
    crate::scope!("do_document_symbols_workspace");
    let file_path = params.text_document.uri.to_file_path()?;
    let symbols = workspace.bone.document_symbols(&file_path);
    Ok(Some(DocumentSymbolResponse::Nested(symbols)))
}

/// Workspace-aware workspace symbols (project-wide, matched against the query).
/// The query spans every project the bone session has loaded.
pub fn do_workspace_symbols_workspace(
    workspace: &mut LspWorkspace,
    params: &WorkspaceSymbolParams,
) -> Result<Option<WorkspaceSymbolResponse>> {
    crate::scope!("do_workspace_symbols_workspace");
    let symbols = workspace.bone.workspace_symbols(&params.query);
    Ok(Some(WorkspaceSymbolResponse::Flat(symbols)))
}

/// Workspace-aware inlay hints
pub(crate) fn do_inlay_hints_workspace(
    workspace: &mut LspWorkspace,
    params: &InlayHintParams,
) -> Result<Option<Vec<InlayHint>>> {
    crate::scope!("do_inlay_hints_workspace");
    let _span = tracing::debug_span!("do_inlay_hints_workspace").entered();
    let file_path = params.text_document.uri.to_file_path()?;
    Ok(Some(workspace.bone.inlay_hints(&file_path, params.range)))
}

/// Workspace-aware completion
pub fn do_completion_workspace(
    workspace: &mut LspWorkspace,
    params: &CompletionParams,
) -> Result<Option<CompletionResponse>> {
    crate::scope!("do_completion_workspace");
    let _span = tracing::debug_span!("do_completion_workspace").entered();
    let file_path = get_file_path_from_params(&params.text_document_position)?;
    let pos = params.text_document_position.position;
    // Was this auto-fired by the space trigger character? If so, a
    // whitespace-only line prefix is `<CR>` auto-indent, and bone suppresses the
    // popup (so `<CR><CR>` inserts two newlines instead of accepting item 1).
    let space_triggered = params.context.as_ref().is_some_and(|c| {
        c.trigger_kind == lsp_types::CompletionTriggerKind::TRIGGER_CHARACTER
            && c.trigger_character.as_deref() == Some(" ")
    });
    let mut items = workspace
        .bone
        .completion_with_trigger(&file_path, pos, space_triggered);
    // Corpus recorder (ROCK_LSP_RECORD): snapshot the request + tag items
    // with the accept command. No-op when recording is off.
    crate::completion_record::stash_and_tag(&workspace.bone, &file_path, pos, &mut items);
    // Both the FILTERING and the match-quality RANKING are prefix-sensitive,
    // so the result for an empty/short prefix is not a superset the client
    // can correctly re-filter: the popup-open response (often empty-prefix,
    // every item match_quality 0, ranked by distance) would freeze in place
    // while the user types -- e.g. `tile_center(pg|` shows the empty-prefix
    // order (dist-1 `prop.image_dims()` above dist-2 `player.movable.grid`)
    // because cmp client-filters the cached list instead of re-asking. Mark
    // the list incomplete so the client re-requests per keystroke and the
    // server re-scores against the actual typed prefix.
    Ok(Some(CompletionResponse::List(CompletionList {
        is_incomplete: true,
        items,
    })))
}

/// Workspace-aware prepare rename
pub(crate) fn prepare_rename_workspace(
    workspace: &mut LspWorkspace,
    params: &TextDocumentPositionParams,
) -> Result<Option<PrepareRenameResponse>> {
    let file_path = get_file_path_from_params(params)?;
    // A refusal becomes the request's error, which editors show as the reason.
    Ok(workspace
        .bone
        .prepare_rename(&file_path, params.position)?
        .map(PrepareRenameResponse::Range))
}

/// Workspace-aware rename
pub(crate) fn do_rename_workspace(
    workspace: &mut LspWorkspace,
    params: &RenameParams,
) -> Result<Option<WorkspaceEdit>> {
    let file_path = get_file_path_from_params(&params.text_document_position)?;
    let pos = params.text_document_position.position;
    Ok(workspace.bone.rename(&file_path, pos, &params.new_name)?)
}
