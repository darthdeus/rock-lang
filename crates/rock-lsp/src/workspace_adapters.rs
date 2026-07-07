/// Adapter functions to make existing LSP operations work with the workspace model
use crate::*;
use anyhow::Result;
use lsp_types::*;

fn file_path_from_uri(uri: &lsp_types::Uri) -> String {
    let s = uri.as_str();
    s.strip_prefix("file://").unwrap_or(s).to_string()
}

fn get_file_path_from_params(params: &TextDocumentPositionParams) -> Result<String> {
    Ok(file_path_from_uri(&params.text_document.uri))
}

pub fn get_file_path_from_code_action_params(params: &CodeActionParams) -> Result<String> {
    Ok(file_path_from_uri(&params.text_document.uri))
}

/// Workspace-aware go to definition
pub fn go_to_definition_workspace(
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
pub fn go_to_type_definition_workspace(
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
pub fn find_references_workspace(
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
    // Plain `file://` strip to match the key bone.update registers on didChange.
    let uri_str = params.text_document_position_params.text_document.uri.as_str();
    let file_path = uri_str.strip_prefix("file://").unwrap_or(uri_str);
    let pos = params.text_document_position_params.position;
    let hl = workspace.bone.document_highlight(file_path, pos);
    Ok((!hl.is_empty()).then_some(hl))
}

/// Workspace-aware hover
pub fn do_hover_workspace(
    workspace: &mut LspWorkspace,
    params: &HoverParams,
) -> Result<Option<Hover>> {
    crate::scope!("do_hover_workspace");
    let file_path = get_file_path_from_params(&params.text_document_position_params)?;
    let pos = params.text_document_position_params.position;
    Ok(workspace.bone.hover(&file_path, pos))
}

/// Workspace-aware signature help
pub fn do_signature_help_workspace(
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
    let uri_str = params.text_document.uri.as_str();
    let file_path = uri_str.strip_prefix("file://").unwrap_or(uri_str);
    let symbols = workspace.bone.document_symbols(file_path);
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
pub fn do_inlay_hints_workspace(
    workspace: &mut LspWorkspace,
    params: &InlayHintParams,
) -> Result<Option<Vec<InlayHint>>> {
    crate::scope!("do_inlay_hints_workspace");
    let _span = tracing::debug_span!("do_inlay_hints_workspace").entered();
    let uri_str = params.text_document.uri.as_str();
    let file_path = if uri_str.starts_with("file://") {
        &uri_str[7..]
    } else {
        uri_str
    };
    Ok(Some(workspace.bone.inlay_hints(file_path, params.range)))
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
    let mut items = workspace.bone.completion(&file_path, pos);
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
pub fn prepare_rename_workspace(
    workspace: &mut LspWorkspace,
    params: &TextDocumentPositionParams,
) -> Result<Option<PrepareRenameResponse>> {
    let file_path = get_file_path_from_params(params)?;
    Ok(workspace
        .bone
        .prepare_rename(&file_path, params.position)
        .map(PrepareRenameResponse::Range))
}

/// Workspace-aware rename
pub fn do_rename_workspace(
    workspace: &mut LspWorkspace,
    params: &RenameParams,
) -> Result<Option<WorkspaceEdit>> {
    let file_path = get_file_path_from_params(&params.text_document_position)?;
    let pos = params.text_document_position.position;
    Ok(workspace.bone.rename(&file_path, pos, &params.new_name))
}
