use crate::*;
use anyhow::Result;
use bone_query::{BoneWorkspace, FileUriExt};
use lsp_types::*;

fn doc_path(doc: &TextDocumentIdentifier) -> Result<String> {
    Ok(doc.uri.to_file_path()?)
}

/// Run a bone query at the request's cursor.
pub(crate) fn at_position<T>(
    workspace: &LspWorkspace,
    params: &TextDocumentPositionParams,
    query: impl FnOnce(&BoneWorkspace, &str, Position) -> T,
) -> Result<T> {
    let file_path = doc_path(&params.text_document)?;
    Ok(query(&workspace.bone, &file_path, params.position))
}

pub(crate) fn find_references_workspace(
    workspace: &mut LspWorkspace,
    params: &TextDocumentPositionParams,
) -> Result<Option<Vec<Location>>> {
    let locs = at_position(workspace, params, BoneWorkspace::references)?;
    Ok((!locs.is_empty()).then_some(locs))
}

pub fn do_document_highlight_workspace(
    workspace: &mut LspWorkspace,
    params: &TextDocumentPositionParams,
) -> Result<Option<Vec<DocumentHighlight>>> {
    let hl = at_position(workspace, params, BoneWorkspace::document_highlight)?;
    Ok((!hl.is_empty()).then_some(hl))
}

pub fn do_document_symbols_workspace(
    workspace: &mut LspWorkspace,
    params: &DocumentSymbolParams,
) -> Result<Option<Vec<DocumentSymbol>>> {
    let file_path = doc_path(&params.text_document)?;
    Ok(Some(workspace.bone.document_symbols(&file_path)))
}

pub fn do_workspace_symbols_workspace(
    workspace: &mut LspWorkspace,
    params: &WorkspaceSymbolParams,
) -> Result<Option<Vec<SymbolInformation>>> {
    Ok(Some(workspace.bone.workspace_symbols(&params.query)))
}

pub(crate) fn do_inlay_hints_workspace(
    workspace: &mut LspWorkspace,
    params: &InlayHintParams,
) -> Result<Option<Vec<InlayHint>>> {
    let file_path = doc_path(&params.text_document)?;
    Ok(Some(workspace.bone.inlay_hints(&file_path, params.range)))
}

pub fn do_completion_workspace(
    workspace: &mut LspWorkspace,
    params: &CompletionParams,
) -> Result<Option<CompletionList>> {
    let file_path = doc_path(&params.text_document_position.text_document)?;
    let pos = params.text_document_position.position;
    // On a space trigger, a whitespace-only prefix is `<CR>` auto-indent, so bone
    // suppresses the popup (`<CR><CR>` inserts two newlines).
    let space_triggered = params.context.as_ref().is_some_and(|c| {
        c.trigger_kind == lsp_types::CompletionTriggerKind::TRIGGER_CHARACTER
            && c.trigger_character.as_deref() == Some(" ")
    });
    let mut items = workspace
        .bone
        .completion_with_trigger(&file_path, pos, space_triggered);
    crate::completion_record::stash_and_tag(&workspace.bone, &file_path, pos, &mut items);
    // Re-request on each keystroke because ranking depends on the typed prefix.
    Ok(Some(CompletionList {
        is_incomplete: true,
        items,
    }))
}

pub(crate) fn do_rename_workspace(
    workspace: &mut LspWorkspace,
    params: &RenameParams,
) -> Result<Option<WorkspaceEdit>> {
    let file_path = doc_path(&params.text_document_position.text_document)?;
    let pos = params.text_document_position.position;
    Ok(workspace.bone.rename(&file_path, pos, &params.new_name)?)
}
