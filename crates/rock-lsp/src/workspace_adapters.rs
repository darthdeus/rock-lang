use crate::*;
use anyhow::Result;
use bone_query::FileUriExt;
use lsp_types::*;

fn doc_path(doc: &TextDocumentIdentifier) -> Result<String> {
    Ok(doc.uri.to_file_path()?)
}

pub(crate) fn go_to_definition_workspace(
    workspace: &mut LspWorkspace,
    params: &TextDocumentPositionParams,
) -> Result<Option<Vec<Location>>> {
    crate::scope!("go_to_definition_workspace");
    let file_path = doc_path(&params.text_document)?;
    let pos = params.position;
    Ok(workspace
        .bone
        .definition(&file_path, pos)
        .map(|loc| vec![loc]))
}

pub(crate) fn go_to_type_definition_workspace(
    workspace: &mut LspWorkspace,
    params: &TextDocumentPositionParams,
) -> Result<Option<Vec<Location>>> {
    crate::scope!("go_to_type_definition_workspace");
    let file_path = doc_path(&params.text_document)?;
    let pos = params.position;
    Ok(workspace
        .bone
        .type_definition(&file_path, pos)
        .map(|loc| vec![loc]))
}

pub(crate) fn find_references_workspace(
    workspace: &mut LspWorkspace,
    params: &TextDocumentPositionParams,
) -> Result<Option<Vec<Location>>> {
    crate::scope!("find_references_workspace");
    let file_path = doc_path(&params.text_document)?;
    let pos = params.position;
    let locs = workspace.bone.references(&file_path, pos);
    Ok(if locs.is_empty() { None } else { Some(locs) })
}

pub fn do_document_highlight_workspace(
    workspace: &mut LspWorkspace,
    params: &TextDocumentPositionParams,
) -> Result<Option<Vec<DocumentHighlight>>> {
    crate::scope!("do_document_highlight_workspace");
    let file_path = doc_path(&params.text_document)?;
    let pos = params.position;
    let hl = workspace.bone.document_highlight(&file_path, pos);
    Ok((!hl.is_empty()).then_some(hl))
}

pub(crate) fn do_hover_workspace(
    workspace: &mut LspWorkspace,
    params: &TextDocumentPositionParams,
) -> Result<Option<Hover>> {
    crate::scope!("do_hover_workspace");
    let file_path = doc_path(&params.text_document)?;
    let pos = params.position;
    Ok(workspace.bone.hover(&file_path, pos))
}

pub(crate) fn do_signature_help_workspace(
    workspace: &mut LspWorkspace,
    params: &TextDocumentPositionParams,
) -> Result<Option<SignatureHelp>> {
    crate::scope!("do_signature_help_workspace");
    let file_path = doc_path(&params.text_document)?;
    let pos = params.position;
    Ok(workspace.bone.signature_help(&file_path, pos))
}

pub fn do_document_symbols_workspace(
    workspace: &mut LspWorkspace,
    params: &DocumentSymbolParams,
) -> Result<Option<Vec<DocumentSymbol>>> {
    crate::scope!("do_document_symbols_workspace");
    let file_path = doc_path(&params.text_document)?;
    Ok(Some(workspace.bone.document_symbols(&file_path)))
}

pub fn do_workspace_symbols_workspace(
    workspace: &mut LspWorkspace,
    params: &WorkspaceSymbolParams,
) -> Result<Option<Vec<SymbolInformation>>> {
    crate::scope!("do_workspace_symbols_workspace");
    Ok(Some(workspace.bone.workspace_symbols(&params.query)))
}

pub(crate) fn do_inlay_hints_workspace(
    workspace: &mut LspWorkspace,
    params: &InlayHintParams,
) -> Result<Option<Vec<InlayHint>>> {
    crate::scope!("do_inlay_hints_workspace");
    let _span = tracing::debug_span!("do_inlay_hints_workspace").entered();
    let file_path = doc_path(&params.text_document)?;
    Ok(Some(workspace.bone.inlay_hints(&file_path, params.range)))
}

pub fn do_completion_workspace(
    workspace: &mut LspWorkspace,
    params: &CompletionParams,
) -> Result<Option<CompletionList>> {
    crate::scope!("do_completion_workspace");
    let _span = tracing::debug_span!("do_completion_workspace").entered();
    let file_path = doc_path(&params.text_document_position.text_document)?;
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
    // Re-request on each keystroke because ranking depends on the typed prefix.
    Ok(Some(CompletionList {
        is_incomplete: true,
        items,
    }))
}

pub(crate) fn prepare_rename_workspace(
    workspace: &mut LspWorkspace,
    params: &TextDocumentPositionParams,
) -> Result<Option<Range>> {
    let file_path = doc_path(&params.text_document)?;
    // A refusal becomes the request's error, which editors show as the reason.
    Ok(workspace.bone.prepare_rename(&file_path, params.position)?)
}

pub(crate) fn do_rename_workspace(
    workspace: &mut LspWorkspace,
    params: &RenameParams,
) -> Result<Option<WorkspaceEdit>> {
    let file_path = doc_path(&params.text_document_position.text_document)?;
    let pos = params.text_document_position.position;
    Ok(workspace.bone.rename(&file_path, pos, &params.new_name)?)
}
