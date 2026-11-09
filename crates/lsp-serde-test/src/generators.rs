use rand::rngs::StdRng;
use rand::Rng;
use serde_json::{json, Value};

// ── primitives ────────────────────────────────────────────────────────────────

pub(crate) fn gen_position(rng: &mut StdRng) -> Value {
    json!({ "line": rng.random::<u16>(), "character": rng.random::<u16>() })
}

pub(crate) fn gen_range(rng: &mut StdRng) -> Value {
    let line = rng.random::<u16>() as u64;
    let ch = rng.random::<u16>() as u64;
    json!({
        "start": { "line": line, "character": ch },
        "end":   { "line": line, "character": ch + rng.random::<u8>() as u64 }
    })
}

pub(crate) fn gen_uri(_rng: &mut StdRng) -> Value {
    json!("file:///tmp/test.rock")
}

// ── hover client capabilities ─────────────────────────────────────────────────

pub(crate) fn gen_hover_client_capabilities(_rng: &mut StdRng) -> Vec<Value> {
    vec![
        json!({}),
        json!({ "dynamicRegistration": true }),
        json!({ "dynamicRegistration": false }),
        json!({ "contentFormat": ["markdown", "plaintext"] }),
        json!({ "dynamicRegistration": true, "contentFormat": ["markdown"] }),
    ]
}

// ── hover ─────────────────────────────────────────────────────────────────────
// MarkedString: string | { language: string, value: string }
// HoverContents: MarkedString | MarkedString[] | MarkupContent

pub(crate) fn gen_hover(rng: &mut StdRng) -> Vec<Value> {
    let range = gen_range(rng);
    vec![
        // plain string contents (MarkedString variant)
        json!({ "contents": "some hover text" }),
        // language-tagged MarkedString
        json!({ "contents": { "language": "rust", "value": "fn foo() {}" } }),
        // array of MarkedStrings (with language-tagged entries to avoid upstream
        // struct-from-sequence quirk where plain-string arrays are misread as LanguageString)
        json!({ "contents": [{ "language": "rust", "value": "let x = 1;" }, { "language": "text", "value": "result" }] }),
        // MarkupContent plaintext
        json!({ "contents": { "kind": "plaintext", "value": "plain text hover" } }),
        // MarkupContent markdown
        json!({ "contents": { "kind": "markdown", "value": "**bold** hover" } }),
        // with optional range
        json!({ "contents": "hover with range", "range": range }),
    ]
}

// ── semantic tokens ───────────────────────────────────────────────────────────
// SemanticTokensFullOptions: bool | { delta?: bool }
// SemanticTokensServerCapabilities: SemanticTokensOptions | SemanticTokensRegistrationOptions

fn gen_semantic_tokens_legend() -> Value {
    json!({
        "tokenTypes": ["keyword", "variable", "function"],
        "tokenModifiers": ["declaration", "readonly"]
    })
}

pub(crate) fn gen_semantic_tokens_full_options(_rng: &mut StdRng) -> Vec<Value> {
    vec![
        json!(true),
        json!(false),
        json!({ "delta": true }),
        json!({ "delta": false }),
        json!({}),
    ]
}

pub(crate) fn gen_semantic_tokens_options(rng: &mut StdRng) -> Vec<Value> {
    let legend = gen_semantic_tokens_legend();
    let full_variants = gen_semantic_tokens_full_options(rng);
    let mut out = Vec::new();
    for full in full_variants {
        out.push(json!({
            "legend": legend,
            "full": full
        }));
    }
    // range variants
    out.push(json!({ "legend": legend, "range": true }));
    out.push(json!({ "legend": legend, "range": false }));
    out.push(json!({ "legend": legend }));
    out
}

pub(crate) fn gen_semantic_tokens_server_capabilities(rng: &mut StdRng) -> Vec<Value> {
    // Both SemanticTokensOptions and SemanticTokensRegistrationOptions are valid;
    // the untagged enum picks the first that deserializes successfully.
    let legend = gen_semantic_tokens_legend();
    let mut out = gen_semantic_tokens_options(rng);
    // SemanticTokensRegistrationOptions adds documentSelector
    out.push(json!({
        "legend": legend,
        "full": true,
        "documentSelector": [{ "language": "rust" }]
    }));
    out
}

// ── completion ────────────────────────────────────────────────────────────────
// CompletionOptions.triggerCharacters, resolveProvider, etc.

pub(crate) fn gen_completion_options(_rng: &mut StdRng) -> Vec<Value> {
    vec![
        json!({}),
        json!({ "triggerCharacters": [".", ":"], "resolveProvider": true }),
        json!({ "triggerCharacters": [], "resolveProvider": false, "workDoneProgress": true }),
        json!({
            "triggerCharacters": ["("],
            "completionItem": { "labelDetailsSupport": true }
        }),
        // Optional fields are absent by default, but malformed present values
        // must be rejected just like serde-derived fields.
        json!({ "resolveProvider": "yes" }),
        json!({ "triggerCharacters": [".", 1] }),
        json!({ "allCommitCharacters": [1] }),
        json!({ "completionItem": { "labelDetailsSupport": 1 } }),
    ]
}

pub(crate) fn gen_completion_item_capability(_rng: &mut StdRng) -> Vec<Value> {
    vec![
        json!({}),
        json!({ "snippetSupport": null }),
        json!({ "snippetSupport": "yes" }),
        json!({ "commitCharactersSupport": 1 }),
        json!({ "documentationFormat": "markdown" }),
        json!({ "deprecatedSupport": "yes" }),
        json!({ "preselectSupport": "yes" }),
        json!({ "tagSupport": true }),
        json!({ "tagSupport": false }),
        json!({ "tagSupport": { "valueSet": [1] } }),
        json!({ "tagSupport": { "valueSet": "bad" } }),
        json!({ "insertReplaceSupport": 1 }),
        json!({ "resolveSupport": { "properties": "documentation" } }),
        json!({ "insertTextModeSupport": { "valueSet": "asIs" } }),
        json!({ "labelDetailsSupport": 1 }),
    ]
}

pub(crate) fn gen_document_symbol_client_capabilities(_rng: &mut StdRng) -> Vec<Value> {
    vec![
        json!({ "extraCapability": true }),
        json!({ "dynamicRegistration": "yes" }),
        json!({ "symbolKind": { "valueSet": "bad" } }),
        json!({ "hierarchicalDocumentSymbolSupport": 1 }),
        json!({ "tagSupport": { "valueSet": ["bad"] } }),
        json!({ "tagSupport": { "other": true } }),
        json!({ "tagSupport": true }),
        json!({ "tagSupport": false }),
    ]
}

pub(crate) fn gen_workspace_symbol_client_capabilities(_rng: &mut StdRng) -> Vec<Value> {
    vec![
        json!({ "extraCapability": true }),
        json!({ "dynamicRegistration": "yes" }),
        json!({ "symbolKind": { "valueSet": "bad" } }),
        json!({ "tagSupport": { "valueSet": ["bad"] } }),
        json!({ "tagSupport": { "other": true } }),
        json!({ "resolveSupport": { "properties": "name" } }),
        json!({ "tagSupport": true }),
        json!({ "tagSupport": false }),
    ]
}

pub(crate) fn gen_document_on_type_formatting_registration_options(
    _rng: &mut StdRng,
) -> Vec<Value> {
    vec![
        json!({ "firstTriggerCharacter": "." }),
        json!({ "firstTriggerCharacter": ".", "documentSelector": null }),
        json!({ "firstTriggerCharacter": ".", "documentSelector": "bad" }),
        json!({ "firstTriggerCharacter": ".", "moreTriggerCharacter": [";", 1] }),
    ]
}

pub(crate) fn gen_initialize_params(_rng: &mut StdRng) -> Vec<Value> {
    vec![
        json!({ "capabilities": {} }),
        json!({ "capabilities": {}, "processId": null }),
        json!({ "capabilities": {}, "processId": "bad" }),
        json!({ "capabilities": {}, "rootUri": 1 }),
        json!({ "capabilities": {}, "rootPath": 1 }),
        json!({ "capabilities": {}, "trace": 1 }),
    ]
}

pub(crate) fn gen_message_action_item(_rng: &mut StdRng) -> Vec<Value> {
    vec![
        json!({ "title": "Continue", "extraString": "value" }),
        json!({ "title": "Continue", "extraNull": null }),
        json!({ "title": "Continue", "extraArray": [1, 2] }),
        json!({ "title": "Continue", "extraObject": { "key": true } }),
    ]
}

// ── diagnostic ────────────────────────────────────────────────────────────────

pub(crate) fn gen_diagnostic(rng: &mut StdRng) -> Vec<Value> {
    vec![
        json!({
            "range": gen_range(rng),
            "message": "something went wrong"
        }),
        json!({
            "range": gen_range(rng),
            "severity": 1,
            "code": 42,
            "message": "error with code"
        }),
        json!({
            "range": gen_range(rng),
            "severity": 2,
            "code": "E0001",
            "source": "rustc",
            "message": "warning with string code"
        }),
    ]
}

// ── code action ───────────────────────────────────────────────────────────────
// CodeActionOrCommand: Command | CodeAction (untagged — discriminated by presence of "command" field)

pub(crate) fn gen_code_action_or_command(rng: &mut StdRng) -> Vec<Value> {
    vec![
        // Command: must have "command" string field
        json!({ "title": "Run", "command": "editor.action.run" }),
        // CodeAction: must have "title" but no required "command"
        json!({ "title": "Fix it", "kind": "quickfix" }),
        json!({
            "title": "Extract variable",
            "kind": "refactor.extract",
            "edit": {
                "changes": {
                    "file:///tmp/test.rock": [{
                        "range": gen_range(rng),
                        "newText": "let x = 42;"
                    }]
                }
            }
        }),
    ]
}

// ── inlay hint ────────────────────────────────────────────────────────────────
// InlayHintLabel: string | InlayHintLabelPart[] (untagged)

pub(crate) fn gen_inlay_hint(rng: &mut StdRng) -> Vec<Value> {
    vec![
        json!({ "position": gen_position(rng), "label": "i32" }),
        json!({ "position": gen_position(rng), "label": [{ "value": "i32" }] }),
        json!({
            "position": gen_position(rng),
            "label": [{ "value": ": ", "tooltip": "type annotation" }, { "value": "i32" }]
        }),
    ]
}

// ── workspace symbols ────────────────────────────────────────────────────────
// WorkspaceSymbol.location: Location | { uri: DocumentUri } (untagged)

pub(crate) fn gen_workspace_symbol(rng: &mut StdRng) -> Vec<Value> {
    vec![
        json!({
            "name": "MyStruct",
            "kind": 23,
            "location": { "uri": gen_uri(rng), "range": gen_range(rng) }
        }),
        json!({
            "name": "MyStruct",
            "kind": 23,
            "location": { "uri": gen_uri(rng) }
        }),
    ]
}

// ── initialize ────────────────────────────────────────────────────────────────

// ── lsp_enum integer newtypes ─────────────────────────────────────────────────
// SymbolKind/DiagnosticSeverity/CompletionItemKind etc. wrap an i32 with no
// validation on either side — both should accept any int verbatim and reject
// non-int. Tests pin that contract.

pub(crate) fn gen_int_newtype(rng: &mut StdRng) -> Vec<Value> {
    vec![
        json!(0),
        json!(1),
        json!(-1),
        json!(rng.random::<i32>()),
        json!(i32::MIN),
        json!(i32::MAX),
        // both sides should reject non-int
        json!("foo"),
        json!(true),
        json!(null),
        json!(1.5),
    ]
}

// ── FormattingOptions ─────────────────────────────────────────────────────────
// Custom impl with HashMap<String, FormattingProperty> catch-all.
// FormattingProperty = bool | i32 | string. Required: tabSize (u32), insertSpaces (bool).
// Optional: trimTrailingWhitespace, insertFinalNewline, trimFinalNewlines.
// Extra keys accept bool/int/string values and reject other JSON values.

pub(crate) fn gen_formatting_options(rng: &mut StdRng) -> Vec<Value> {
    vec![
        // minimal
        json!({ "tabSize": 4, "insertSpaces": true }),
        // all optional flags set
        json!({
            "tabSize": 2,
            "insertSpaces": false,
            "trimTrailingWhitespace": true,
            "insertFinalNewline": true,
            "trimFinalNewlines": false,
        }),
        // extra known-shape properties (bool/int/string) — both sides
        // collect into the HashMap
        json!({
            "tabSize": 8,
            "insertSpaces": true,
            "customBool": true,
            "customNum": rng.random::<i32>(),
            "customStr": "hello",
        }),
        // missing required tabSize
        json!({ "insertSpaces": true }),
        // missing required insertSpaces
        json!({ "tabSize": 4 }),
        // negative tab_size — u32 rejects
        json!({ "tabSize": -1, "insertSpaces": true }),
        // tabSize as string — type mismatch
        json!({ "tabSize": "4", "insertSpaces": true }),
        // Malformed known fields and flattened formatting properties must fail.
        json!({ "tabSize": 4, "insertSpaces": true, "trimTrailingWhitespace": "yes" }),
        json!({ "tabSize": 4, "insertSpaces": true, "insertFinalNewline": 1 }),
        json!({ "tabSize": 4, "insertSpaces": true, "trimFinalNewlines": 1 }),
        json!({ "tabSize": 4, "insertSpaces": true, "customArray": [1, 2] }),
        json!({ "tabSize": 4, "insertSpaces": true, "customNull": null }),
    ]
}

pub(crate) fn gen_tag_support(_rng: &mut StdRng) -> Vec<Value> {
    vec![
        json!({ "valueSet": [1] }),
        json!({ "valueSet": [1], "extra": true }),
        json!({ "valueSet": [] }),
        json!({ "valueSet": "bad" }),
        json!({ "unknown": true }),
    ]
}

// ── PrepareRenameResponse ─────────────────────────────────────────────────────
// Three variants discriminated by field presence:
//  - DefaultBehavior: { defaultBehavior: bool }
//  - RangeWithPlaceholder: { range, placeholder: string }
//  - Range: any Range object

pub(crate) fn gen_prepare_rename_response(rng: &mut StdRng) -> Vec<Value> {
    let r = gen_range(rng);
    vec![
        // DefaultBehavior
        json!({ "defaultBehavior": true }),
        json!({ "defaultBehavior": false }),
        // RangeWithPlaceholder
        json!({ "range": r.clone(), "placeholder": "newName" }),
        // Plain Range
        r.clone(),
        // Range with extra ignored keys
        json!({
            "start": r["start"], "end": r["end"], "extra": "ignored"
        }),
        // Note: an input mixing `defaultBehavior` + `placeholder` + `range`
        // intentionally diverges. Upstream's untagged enum tries
        // RangeWithPlaceholder first and matches; manual checks
        // `defaultBehavior` first and emits DefaultBehavior. Such input is
        // malformed (no LSP client sends it); the variants are mutually
        // exclusive by spec. We don't replicate the upstream tie-break.
    ]
}

// ── WatchKind (bitflag u8: 1=Create, 2=Change, 4=Delete) ──────────────────────

pub(crate) fn gen_watch_kind(_rng: &mut StdRng) -> Vec<Value> {
    vec![
        json!(0), // empty flags
        json!(1),
        json!(2),
        json!(3),
        json!(4),
        json!(5),
        json!(6),
        json!(7),
        json!(8), // invalid bit — both reject
        json!(255),
        json!(-1), // negative — u8 rejects
        json!("create"),
    ]
}

// ── PositionEncodingKind ──────────────────────────────────────────────────────
// String newtype — accepts any string.

pub(crate) fn gen_position_encoding_kind(_rng: &mut StdRng) -> Vec<Value> {
    vec![
        json!("utf-8"),
        json!("utf-16"),
        json!("utf-32"),
        json!("custom"),
        json!(""),
        json!(123), // non-string — both reject
        json!(null),
    ]
}

pub(crate) fn gen_semantic_tokens(_rng: &mut StdRng) -> Vec<Value> {
    vec![
        json!({ "data": [] }),
        json!({ "data": [], "resultId": null }),
        json!({ "data": [], "resultId": 123 }),
    ]
}

// ── lsp-server message types ──────────────────────────────────────────────────
// RequestId = i32 | string. Adversarial:
//  - canonical small ints, big edge ints
//  - i64 outside i32 range (manual bug: silently truncated; fixed in this PR)
//  - empty / unicode strings
//  - non-int/non-string types (both reject)

pub(crate) fn gen_request_id(_rng: &mut StdRng) -> Vec<Value> {
    vec![
        json!(0),
        json!(1),
        json!(-1),
        json!(i32::MIN),
        json!(i32::MAX),
        // Out-of-i32-range — both must reject. (manual previously truncated;
        // fixed by adding range-checked visit_i64/visit_u64.)
        json!(i32::MAX as i64 + 1),
        json!(i32::MIN as i64 - 1),
        json!(u64::MAX),
        json!(""),
        json!("abc"),
        json!("with spaces"),
        json!("ñoño"),
        // both reject
        json!(null),
        json!(true),
        json!(1.5),
        json!([1, 2]),
        json!({}),
    ]
}

pub(crate) fn gen_response_error(_rng: &mut StdRng) -> Vec<Value> {
    vec![
        json!({ "code": -32700, "message": "Parse error" }),
        json!({ "code": 0, "message": "" }),
        json!({ "code": -32602, "message": "Invalid params", "data": null }),
        json!({ "code": -32602, "message": "Invalid params", "data": { "field": "x" } }),
        json!({ "code": -32602, "message": "Invalid params", "data": [1, 2, 3] }),
        json!({ "code": -32602, "message": "Invalid params", "data": "string-data" }),
        json!({ "code": i32::MIN, "message": "min" }),
        json!({ "code": i32::MAX, "message": "max" }),
        // unknown extra keys ignored on both sides
        json!({ "code": 1, "message": "x", "extra": true }),
        // missing required code
        json!({ "message": "x" }),
        // missing required message
        json!({ "code": 1 }),
        // wrong type
        json!({ "code": "1", "message": "x" }),
        // out-of-range code
        json!({ "code": i64::from(i32::MAX) + 1, "message": "x" }),
    ]
}

pub(crate) fn gen_lsp_request(rng: &mut StdRng) -> Vec<Value> {
    let id = json!(rng.random::<u16>());
    vec![
        // canonical
        json!({ "id": 1, "method": "initialize" }),
        json!({ "id": 1, "method": "initialize", "params": {} }),
        json!({ "id": 1, "method": "initialize", "params": null }),
        json!({ "id": 1, "method": "x", "params": [1, 2, 3] }),
        json!({ "id": 1, "method": "x", "params": "string" }),
        json!({ "id": 1, "method": "x", "params": 42 }),
        json!({ "id": "uuid-1", "method": "textDocument/hover", "params": { "textDocument": { "uri": "file:///x" } } }),
        // unknown extra keys ignored
        json!({ "id": id, "method": "x", "extra": [1,2] }),
        // missing required id
        json!({ "method": "x" }),
        // missing required method
        json!({ "id": 1 }),
        // string id
        json!({ "id": "abc", "method": "x" }),
    ]
}

pub(crate) fn gen_lsp_response(_rng: &mut StdRng) -> Vec<Value> {
    vec![
        json!({ "id": 1 }),
        json!({ "id": 1, "result": null }),
        json!({ "id": 1, "result": 42 }),
        json!({ "id": 1, "result": { "ok": true } }),
        json!({ "id": 1, "result": [1, 2, 3] }),
        json!({ "id": "abc", "result": "value" }),
        json!({ "id": 1, "error": { "code": -32602, "message": "Invalid" } }),
        json!({ "id": 1, "error": { "code": -32602, "message": "Invalid", "data": "more" } }),
        // both result and error (not spec-compliant but parsers should accept)
        json!({ "id": 1, "result": null, "error": { "code": 1, "message": "x" } }),
        // missing id
        json!({ "result": 42 }),
        // unknown extra
        json!({ "id": 1, "result": null, "extra": true }),
    ]
}

pub(crate) fn gen_lsp_notification(_rng: &mut StdRng) -> Vec<Value> {
    vec![
        json!({ "method": "exit" }),
        json!({ "method": "exit", "params": null }),
        json!({ "method": "x", "params": {} }),
        json!({ "method": "x", "params": [1, 2] }),
        json!({ "method": "x", "params": "raw" }),
        json!({ "method": "" }),
        // missing method
        json!({ "params": {} }),
        // unknown extra
        json!({ "method": "x", "extra": "ignored" }),
    ]
}

pub(crate) fn gen_lsp_message(rng: &mut StdRng) -> Vec<Value> {
    let mut out = Vec::new();
    out.extend(gen_lsp_request(rng));
    out.extend(gen_lsp_response(rng));
    out.extend(gen_lsp_notification(rng));
    out.extend([
        // empty / non-object — both reject
        json!({}),
        json!([]),
        json!(null),
        json!("string"),
        json!(42),
        // id+method+result mix — Request wins (both sides agree because
        // upstream's untagged tries Request first and it succeeds, ignoring
        // result; manual dispatches by id+method → Request)
        json!({ "id": 1, "method": "x", "result": 42 }),
        // id-only with extra unknown — Response
        json!({ "id": 1, "junk": [] }),
        // method-only — Notification
        json!({ "method": "x", "extra": null }),
        // null-typed dispatch keys — JSON-RPC spec disallows these. Two
        // intentionally-excluded cases worth pinning here as comments because
        // they expose dispatch-strategy differences:
        //   {"id": null, "method": "x"}
        //     upstream: Request and Response variants fail (id type), falls
        //               through to Notification (method present) → Ok.
        //     manual:   has_id=true, has_method=true → routes to Request,
        //               which fails on id type. Errors.
        //   {"id": "x", "method": null}
        //     upstream: Request fails (method type), falls through to
        //               Response (method ignored) → Ok.
        //     manual:   routes to Request → fails. Errors.
        // Both inputs are out-of-spec; the manual fork's stricter dispatch
        // is acceptable and arguably safer (no silent variant fallback).
        // Both-side-error happy paths instead:
        json!({ "id": null }),     // both error
        json!({ "method": null }), // both error
        json!({ "id": null, "method": null }), // both error
                                   // {"id": [], "method": "x"} also diverges: upstream's untagged
                                   // fallback yields Notification {method:"x"}; manual routes to Request
                                   // and rejects on id type. Same fallback pattern as the null cases.
    ]);
    out
}

pub(crate) fn gen_initialize_result(rng: &mut StdRng) -> Vec<Value> {
    let semantic_caps = gen_semantic_tokens_server_capabilities(rng);
    let mut out = Vec::new();
    for sc in semantic_caps {
        out.push(json!({
            "capabilities": {
                "textDocumentSync": 1,
                "hoverProvider": true,
                "completionProvider": { "triggerCharacters": ["."] },
                "semanticTokensProvider": sc
            }
        }));
    }
    out
}
