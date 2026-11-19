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
    fn single(name: &str, source: &str) -> Self {
        let file = format!("{}.rock", name.replace([' ', '/'], "_"));
        Self::new(&file, &[(&file, source)])
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
        let mut command = crate::support::rock(&crate::support::repo_root());
        let output = command
            .args(["lsp", method, &target])
            .args(args)
            .output()
            .unwrap();
        let output = crate::support::success(output);
        serde_json::from_str(&output).unwrap_or_else(|error| panic!("{error}: {output}"))
    }

    fn renamed(&self, reply: &Value, file: Option<&str>) -> String {
        let file = file.map_or_else(
            || self.file.clone(),
            |name| self.dir.path().join(name).canonicalize().unwrap(),
        );
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
}

impl Document {
    fn open(project: Project, source: &str) -> Self {
        let mut document = Self {
            server: rock_lsp::LspServer::new(),
            project,
            text: source.to_owned(),
            version: 1,
        };
        document.notify("textDocument/didOpen", json!({
            "textDocument": {"uri": document.project.uri, "version": 1, "languageId": "rock", "text": source}
        }));
        document
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.server
            .handle(
                Notification::new(method.into(), params).into(),
                &mut Vec::new(),
            )
            .unwrap();
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

fn matches_fields(actual: &Value, expected: &Value) -> bool {
    if let Some(text) = expected["text"].as_str() {
        return actual.as_str().is_some_and(|value| {
            let value = value.trim();
            expected["strip_prefix"]
                .as_str()
                .and_then(|prefix| value.strip_prefix(prefix))
                .unwrap_or(value)
                .trim()
                == text
        });
    }
    match expected {
        Value::Object(fields) => fields.iter().all(|(key, value)| {
            actual
                .get(key)
                .is_some_and(|actual| matches_fields(actual, value))
        }),
        _ => actual == expected,
    }
}

#[track_caller]
fn contains_items(actual: &Value, expected: &[Value]) {
    for item in expected {
        assert!(
            array(actual)
                .iter()
                .any(|value| matches_fields(value, item)),
            "missing {item} in {actual}"
        );
    }
}

#[track_caller]
fn contains_text(actual: &str, expected: &[&str]) {
    for fragment in expected {
        assert!(
            actual.contains(fragment),
            "missing {fragment:?} in {actual:?}"
        );
    }
}

#[track_caller]
fn assert_no_rename_changes(reply: &Value) {
    let changes = &reply["changes"];
    assert!(
        changes.is_null()
            || object(changes)
                .values()
                .all(|edits| { edits.is_null() || array(edits).is_empty() }),
        "{reply}"
    );
}

#[track_caller]
fn reference_starts(actual: &Value, expected: &[(u32, u32)]) {
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
    assert_eq!(actual, expected);
}

#[track_caller]
fn completion_labels(reply: &Value, expected: &[&str]) {
    let labels: Vec<_> = array(&reply["completions"])
        .iter()
        .map(|v| string(&v["label"]))
        .collect();
    for fragment in expected {
        assert!(
            labels.iter().any(|label| label.contains(fragment)),
            "missing {fragment:?} in {labels:?}"
        );
    }
}

mod completion_basic;
mod completion_field_ref;
mod completion_named_args;
mod completion_struct_and_methods;
mod document_symbols;
mod edit_sequence;
mod edit_sequence_hover;
mod edit_sequence_references;
mod edit_sequence_stress;
mod edit_sequence_typing;
mod front_end_shapes;
mod goto_definition;
mod goto_type_definition;
mod hover_basic;
mod hover_declarations;
mod hover_field_ref;
mod hover_match_pattern;
mod hover_struct_literal;
mod hover_type_annotation;
mod inlay_hints;
mod inline_local_variable;
mod method_syntax;
mod modules_v1;
mod multi_file;
mod partial_compilation;
mod references;
mod rename;
mod rename_hazards;
mod rename_overloaded;
mod semantic_tokens;
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
