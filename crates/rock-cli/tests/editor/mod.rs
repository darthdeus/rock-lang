use bone_query::{FileUriExt, offset_at_line_col};
use lsp_server::{Message, Notification, Request};
use lsp_types::{Range, Uri};
use serde_json::{Value, json};
use std::path::PathBuf;

struct Project {
    dir: tempfile::TempDir,
    file: PathBuf,
    uri: Uri,
}

impl Project {
    fn single(source: &str) -> Self {
        Self::new("main.rock", &[("main.rock", source)])
    }

    fn package(primary: &str, files: &[(&str, &str)]) -> Self {
        const MANIFEST: &str = "[package]\nname = \"lsp_test\"\nversion = \"0.1.0\"\n";
        let project = Self::new(primary, files);
        std::fs::write(project.dir.path().join("stick.toml"), MANIFEST).unwrap();
        project
    }

    fn new(primary: &str, files: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        crate::support::write_files(dir.path(), files);
        let file = dir.path().join(primary).canonicalize().unwrap();
        let uri = Uri::from_file_path(file.to_str().unwrap()).unwrap();
        Self { dir, file, uri }
    }

    fn cli(&self, method: &str, position: Option<(u32, u32)>, args: &[&str]) -> Value {
        let file = self.file.display();
        let target = match position {
            Some((line, column)) => format!("{file}:{}:{}", line + 1, column + 1),
            None => file.to_string(),
        };
        self.lsp(&[&[method, &target], args].concat())
    }

    fn lsp(&self, args: &[&str]) -> Value {
        let mut command = crate::support::rock(&crate::support::repo_root());
        let output = command.arg("lsp").args(args).output().unwrap();
        let output = crate::support::success(output);
        serde_json::from_str(&output).unwrap_or_else(|error| panic!("{error}: {output}"))
    }

    fn hover(&self, at: (u32, u32)) -> String {
        let reply = self.cli("hover", Some(at), &[]);
        string(&reply["hover"]["contents"]).trim().to_owned()
    }

    fn definition(&self, method: &str, at: (u32, u32)) -> Option<(u64, u64)> {
        let reply = self.cli(method, Some(at), &[]);
        let start = &array(&reply["definitions"]).first()?["range"]["start"];
        Some((start["line"].as_u64()?, start["character"].as_u64()?))
    }

    fn renamed(&self, reply: &Value, name: &str) -> String {
        let file = self.dir.path().join(name).canonicalize().unwrap();
        let uri = Uri::from_file_path(file.to_str().unwrap()).unwrap();
        let changes = object(&reply["changes"]);
        let edits = changes
            .get(uri.as_str())
            .map_or(&[][..], |edits| array(edits));
        let source = std::fs::read_to_string(file).unwrap();
        apply_edits(&source, edits.iter(), "new_text")
    }
}

struct Document {
    server: rock_lsp::LspServer,
    project: Project,
    text: String,
    version: i64,
    diagnostics: Value,
}

impl Document {
    fn single(source: &str) -> Self {
        let mut document = Self {
            server: rock_lsp::LspServer::new(),
            project: Project::single(source),
            text: source.to_owned(),
            version: 1,
            diagnostics: Value::Null,
        };
        document.notify("textDocument/didOpen", json!({
            "textDocument": {"uri": document.project.uri, "version": 1, "languageId": "rock", "text": source}
        }));
        document
    }

    fn notify(&mut self, method: &str, params: Value) {
        let mut messages = Vec::new();
        self.server
            .handle(
                Notification::new(method.into(), params).into(),
                &mut messages,
            )
            .unwrap();
        for message in messages {
            if let Message::Notification(note) = message
                && note.method == "textDocument/publishDiagnostics"
                && note.params["uri"] == self.project.uri.as_str()
            {
                self.diagnostics = note.params["diagnostics"].clone();
            }
        }
    }

    fn request(&mut self, method: &str, fields: Value) -> Value {
        let mut params = fields;
        params["textDocument"] = json!({ "uri": self.project.uri });
        let request = Request::new(1.into(), method.into(), params);
        let mut messages = Vec::new();
        self.server.handle(request.into(), &mut messages).unwrap();
        let reply = messages
            .into_iter()
            .find_map(|message| match message {
                Message::Response(response) => Some(response),
                _ => None,
            })
            .expect("server response");
        assert!(reply.error.is_none(), "{method}: {:?}", reply.error);
        reply.result.expect("server result")
    }

    fn at(&mut self, method: &str, line: u32, character: u32) -> Value {
        let position = json!({"line": line, "character": character});
        let context = json!({"includeDeclaration": true});
        self.request(method, json!({"position": position, "context": context}))
    }

    fn hover_text(&mut self, line: u32, character: u32) -> String {
        let reply = self.at("textDocument/hover", line, character);
        string(&reply["contents"]["value"]).to_owned()
    }

    fn tokens(&mut self) -> Value {
        self.request("textDocument/semanticTokens/full", json!({}))["data"].take()
    }

    #[track_caller]
    fn assert_tokens_match_fresh(&mut self) {
        let tokens = self.tokens();
        assert!(!array(&tokens).is_empty(), "no tokens for {:?}", self.text);
        let fresh = Document::single(&self.text).tokens();
        assert_eq!(
            tokens, fresh,
            "edited and fresh tokens differ for {:?}",
            self.text
        );
    }

    fn change(&mut self) {
        self.version += 1;
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": self.project.uri, "version": self.version},
                "contentChanges": [{"text": self.text}]
            }),
        );
    }

    fn replace(&mut self, source: &str) {
        self.text = source.into();
        std::fs::write(&self.project.file, &self.text).unwrap();
        self.change();
    }

    fn type_at(&mut self, line: usize, col: usize, text: &str) {
        let (mut line, mut col) = (line, col);
        for ch in text.chars() {
            let mut lines: Vec<_> = self.text.lines().map(str::to_owned).collect();
            lines.resize_with(lines.len().max(line + 1), String::new);
            let column = col.min(lines[line].len());
            if ch == '\n' {
                let rest = lines[line].split_off(column);
                lines.insert(line + 1, rest);
                line += 1;
                col = 0;
            } else {
                lines[line].insert(column, ch);
                col += ch.len_utf8();
            }
            self.text = lines.join("\n");
            self.change();
        }
        std::fs::write(&self.project.file, &self.text).unwrap();
    }

    fn delete_at(&mut self, line: usize, col: usize, count: usize) {
        let (mut line, mut col) = (line, col);
        for _ in 0..count {
            let mut lines: Vec<_> = self.text.lines().map(str::to_owned).collect();
            if line < lines.len() {
                col = col.min(lines[line].len());
                if col > 0 {
                    col -= 1;
                    lines[line].remove(col);
                } else if line > 0 {
                    let current = lines.remove(line);
                    line -= 1;
                    col = lines[line].len();
                    lines[line].push_str(&current);
                }
                self.text = lines.join("\n");
            }
            self.change();
        }
        std::fs::write(&self.project.file, &self.text).unwrap();
    }

    fn apply_action(&self, reply: &Value, title: &str) -> String {
        let actions = array(reply);
        let action = actions
            .iter()
            .find(|action| action["title"] == title)
            .unwrap_or_else(|| panic!("missing action {title:?} in {reply}"));
        let mut edits = Vec::new();
        if let Some(changes) = action["edit"]["changes"].as_object() {
            for (uri, file_edits) in changes {
                assert_eq!(uri, self.project.uri.as_str());
                edits.extend(array(file_edits));
            }
        }
        if let Some(changes) = action["edit"]["documentChanges"].as_array() {
            for change in changes {
                assert_eq!(change["textDocument"]["uri"], self.project.uri.as_str());
                edits.extend(array(&change["edits"]));
            }
        }
        assert!(!edits.is_empty(), "{action}");
        apply_edits(&self.text, edits, "newText")
    }
}

fn apply_edits<'a>(
    source: &str,
    edits: impl IntoIterator<Item = &'a Value>,
    field: &str,
) -> String {
    let mut spans: Vec<_> = edits
        .into_iter()
        .map(|edit| {
            let range: Range = serde_json::from_value(edit["range"].clone()).unwrap();
            let offset =
                |pos: lsp_types::Position| offset_at_line_col(source, pos.line, pos.character);
            (offset(range.start), offset(range.end), string(&edit[field]))
        })
        .collect();
    spans.sort_by_key(|&(start, end, _)| std::cmp::Reverse((start, end)));
    let mut result = source.to_owned();
    let mut limit = source.len();
    for (start, end, text) in spans {
        assert!(start <= end && end <= limit, "invalid or overlapping edits");
        result.replace_range(start..end, text);
        limit = start;
    }
    result
}

#[track_caller]
fn array(value: &Value) -> &[Value] {
    value
        .as_array()
        .unwrap_or_else(|| panic!("expected an array, got {value}"))
}

#[track_caller]
fn object(value: &Value) -> &serde_json::Map<String, Value> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("expected an object, got {value}"))
}

#[track_caller]
fn string(value: &Value) -> &str {
    value
        .as_str()
        .unwrap_or_else(|| panic!("expected a string, got {value}"))
}

#[track_caller]
fn contains_items(actual: &Value, expected: &[Value]) {
    for item in expected {
        let matches = |value: &Value| object(item).iter().all(|(key, field)| &value[key] == field);
        assert!(
            array(actual).iter().any(matches),
            "missing {item} in {actual}"
        );
    }
}

#[track_caller]
fn contains_text(actual: &str, fragment: &str) {
    assert!(
        actual.contains(fragment),
        "missing {fragment:?} in {actual:?}"
    );
}

#[track_caller]
fn reference_starts(case: &str, actual: &Value, expected: &[(u32, u32)]) {
    let mut actual: Vec<_> = array(actual)
        .iter()
        .map(|value| {
            let pos: lsp_types::Position =
                serde_json::from_value(value["range"]["start"].clone()).unwrap();
            (pos.line, pos.character)
        })
        .collect();
    actual.sort_unstable();
    let mut expected = expected.to_vec();
    expected.sort_unstable();
    assert_eq!(actual, expected, "{case}");
}

#[track_caller]
fn completion_labels(case: &str, reply: &Value, expected: &[&str]) {
    let labels: Vec<_> = array(&reply["completions"])
        .iter()
        .map(|item| string(&item["label"]))
        .collect();
    for fragment in expected {
        assert!(
            labels.iter().any(|label| label.contains(fragment)),
            "{case}: missing {fragment:?} in {labels:?}"
        );
    }
}

mod completion;
mod diagnostics;
mod document_symbols;
mod edit_sequence;
mod inlay_hints;
mod modules_v1;
mod navigation;
mod rename;
mod semantic_tokens;
mod server_requests;
mod signature_help;
mod workspace_symbols;

#[test]
fn retired_fixture_reports_how_to_run_editor_tests() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("probe.rock.lsp");
    std::fs::write(&file, "tests = []").unwrap();
    let output = crate::support::invoke(
        &crate::support::repo_root(),
        &["test", file.to_str().unwrap()],
    );
    crate::support::failure(output, "cargo test -p rock-cli editor::");
}
