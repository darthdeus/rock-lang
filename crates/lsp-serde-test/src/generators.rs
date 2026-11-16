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

pub(crate) fn gen_code_action(rng: &mut StdRng) -> Vec<Value> {
    vec![
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

// ── semantic tokens ──────────────────────────────────────────────────────────

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
