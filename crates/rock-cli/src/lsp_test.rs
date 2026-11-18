use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Suite {
    tests: Vec<CaseSpec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseSpec {
    name: String,
    kind: String,
    #[serde(default)]
    position: Option<[usize; 2]>,
    #[serde(default)]
    primary_file: Option<String>,
    #[serde(default)]
    source: Option<Content>,
    #[serde(default)]
    files: Vec<FileSpec>,
    #[serde(default)]
    request: Option<Request>,
    #[serde(default)]
    checks: Vec<Check>,
    #[serde(default)]
    steps: Vec<Step>,
}

#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum Content {
    Text(String),
    Reference(ContentReference),
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct ContentReference {
    #[serde(default)]
    from_test: Option<String>,
    #[serde(default)]
    from_step: Option<usize>,
    #[serde(default)]
    from_fixture: Option<bool>,
    #[serde(default)]
    replace: Vec<[String; 2]>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct FileSpec {
    path: String,
    text: String,
}

#[derive(Deserialize, Clone)]
#[serde(tag = "transport", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Cli { argv: Vec<String> },
    Rpc { method: String, params: Value },
}

#[derive(Deserialize, Clone)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Check {
    Contains {
        path: Vec<PathPart>,
        value: Value,
        #[serde(default)]
        index_from: Option<Vec<PathPart>>,
    },
    ContainsAll {
        path: Vec<PathPart>,
        value: Vec<Value>,
    },
    Equals {
        path: Vec<PathPart>,
        value: Value,
    },
    TrimmedEquals {
        path: Vec<PathPart>,
        value: String,
    },
    UnorderedEqual {
        path: Vec<PathPart>,
        value: Vec<Value>,
    },
    Gte {
        path: Vec<PathPart>,
        value: u64,
    },
    Absent {
        path: Vec<PathPart>,
        value: Value,
        #[serde(default)]
        null_is_empty: bool,
    },
    Empty {
        path: Vec<PathPart>,
        #[serde(default)]
        null_is_empty: bool,
        #[serde(default)]
        missing_is_empty: bool,
    },
    AppliedTextEquals {
        path: Vec<PathPart>,
        file: String,
        expected: Content,
        #[serde(default)]
        selector: Option<Value>,
    },
    AppliedTextContains {
        path: Vec<PathPart>,
        value: Vec<String>,
    },
}

#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum PathPart {
    Key(String),
    Index(usize),
}

#[derive(Deserialize, Clone)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum Step {
    Replace {
        source: Content,
        #[serde(default)]
        checks: Vec<StepCheck>,
    },
    TypeAt {
        line: usize,
        col: usize,
        text: String,
        #[serde(default)]
        checks: Vec<StepCheck>,
    },
    DeleteAt {
        line: usize,
        col: usize,
        count: usize,
        #[serde(default)]
        checks: Vec<StepCheck>,
    },
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct StepCheck {
    request: Request,
    assertions: Vec<Check>,
}

#[derive(Clone)]
struct Fixture {
    name: String,
    kind: String,
    code: String,
    files: Vec<(String, String)>,
    primary_file: String,
    position: Option<(usize, usize)>,
    request: Option<Request>,
    checks: Vec<Check>,
    steps: Vec<Step>,
}

fn fixture_error(path: &Path, name: &str, message: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{}: test '{}': {}", path.display(), name, message)
}

fn parse_suite(path: &Path) -> Result<Vec<Fixture>> {
    let text = fs_err::read_to_string(path)?;
    let suite: Suite =
        toml::from_str(&text).map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
    let mut resolved = Vec::with_capacity(suite.tests.len());
    for spec in suite.tests {
        let fixture = resolve_case(spec, &resolved, path)?;
        resolved.push(fixture);
    }
    Ok(resolved)
}

fn resolve_case(spec: CaseSpec, previous: &[Fixture], path: &Path) -> Result<Fixture> {
    let name = spec.name.clone();
    let mut files = spec
        .files
        .into_iter()
        .map(|file| (file.path, file.text))
        .collect::<Vec<_>>();
    let mut primary_file = spec.primary_file.unwrap_or_default();
    let code = match spec.source {
        Some(Content::Text(text)) => text,
        Some(Content::Reference(reference)) => {
            let (mut code, shared_files, shared_primary) = match (
                reference.from_test.as_deref(),
                reference.from_step,
                reference.from_fixture,
            ) {
                (Some(test_name), None, None) => {
                    let source = previous
                        .iter()
                        .find(|test| test.name == test_name)
                        .ok_or_else(|| {
                            fixture_error(
                                path,
                                &name,
                                format!("unknown earlier SOURCE {test_name:?}"),
                            )
                        })?;
                    (
                        source.code.clone(),
                        source.files.clone(),
                        source.primary_file.clone(),
                    )
                }
                (None, None, None) => {
                    return Err(fixture_error(
                        path,
                        &name,
                        "source reference needs from_test",
                    ));
                }
                _ => return Err(fixture_error(path, &name, "invalid case source reference")),
            };
            if !shared_files.is_empty() && !reference.replace.is_empty() {
                return Err(fixture_error(
                    path,
                    &name,
                    "source replacements require a single file",
                ));
            }
            apply_replacements(&mut code, &reference.replace, path, &name)?;
            if files.is_empty() {
                files = shared_files;
            }
            if primary_file.is_empty() {
                primary_file = shared_primary;
            }
            code
        }
        None if !files.is_empty() => {
            if primary_file.is_empty() {
                primary_file = files[0].0.clone();
            }
            files
                .iter()
                .find(|(file, _)| file == &primary_file)
                .map(|(_, content)| content.clone())
                .unwrap_or_default()
        }
        None if spec.kind == "edit_sequence" => String::new(),
        None => return Err(fixture_error(path, &name, "missing source")),
    };

    if spec.kind == "edit_sequence" {
        if spec.steps.is_empty() || spec.request.is_some() || !spec.checks.is_empty() {
            return Err(fixture_error(path, &name, "edit_sequence needs steps only"));
        }
        validate_steps(&spec.steps, path, &name)?;
    } else if spec.request.is_none() || !spec.steps.is_empty() || spec.checks.is_empty() {
        return Err(fixture_error(
            path,
            &name,
            "request fixture needs a request and checks",
        ));
    }
    validate_request(
        spec.request.as_ref(),
        &spec.checks,
        spec.position,
        path,
        &name,
    )?;
    for step in &spec.steps {
        for check in step.checks() {
            validate_request(Some(&check.request), &check.assertions, None, path, &name)?;
        }
    }

    Ok(Fixture {
        name,
        kind: spec.kind,
        code,
        files,
        primary_file,
        position: spec.position.map(|[line, character]| (line, character)),
        request: spec.request,
        checks: spec.checks,
        steps: spec.steps,
    })
}

fn validate_steps(steps: &[Step], path: &Path, name: &str) -> Result<()> {
    for (index, step) in steps.iter().enumerate() {
        match step {
            Step::Replace {
                source: Content::Reference(reference),
                ..
            } => {
                if reference.from_test.is_some()
                    || reference.from_fixture.is_some()
                    || !reference.from_step.is_some_and(|source| {
                        source > 0
                            && source <= index
                            && matches!(steps.get(source - 1), Some(Step::Replace { .. }))
                    })
                {
                    return Err(fixture_error(
                        path,
                        name,
                        "SOURCE must name an earlier full-document step",
                    ));
                }
            }
            Step::Replace { .. } => {}
            _ if index == 0 => {
                return Err(fixture_error(
                    path,
                    name,
                    "first edit_sequence step must replace the document",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

impl Step {
    fn checks(&self) -> &[StepCheck] {
        match self {
            Self::Replace { checks, .. }
            | Self::TypeAt { checks, .. }
            | Self::DeleteAt { checks, .. } => checks,
        }
    }
}

fn validate_request(
    request: Option<&Request>,
    checks: &[Check],
    position: Option<[usize; 2]>,
    path: &Path,
    name: &str,
) -> Result<()> {
    let Some(request) = request else {
        return Ok(());
    };
    if let Request::Cli { argv } = request {
        if argv.is_empty() {
            return Err(fixture_error(path, name, "CLI request has no arguments"));
        }
        if argv.iter().any(|arg| arg == "$target") && position.is_none() {
            return Err(fixture_error(
                path,
                name,
                "CLI request target needs a position",
            ));
        }
    }
    if matches!(request, Request::Rpc { method, .. } if method.is_empty()) {
        return Err(fixture_error(path, name, "RPC request has no method"));
    }
    for check in checks {
        validate_check(check, path, name)?;
    }
    Ok(())
}

fn validate_check(check: &Check, path: &Path, name: &str) -> Result<()> {
    let check_path = match check {
        Check::Contains { path, .. }
        | Check::ContainsAll { path, .. }
        | Check::Equals { path, .. }
        | Check::TrimmedEquals { path, .. }
        | Check::UnorderedEqual { path, .. }
        | Check::Gte { path, .. }
        | Check::Absent { path, .. }
        | Check::Empty { path, .. }
        | Check::AppliedTextEquals { path, .. }
        | Check::AppliedTextContains { path, .. } => path,
    };
    if check_path.is_empty() {
        return Err(fixture_error(path, name, "check path is empty"));
    }
    match check {
        Check::ContainsAll { value, .. } if value.is_empty() => {
            return Err(fixture_error(
                path,
                name,
                "contains_all check has no values",
            ));
        }
        Check::AppliedTextContains { value, .. } if value.is_empty() => {
            return Err(fixture_error(
                path,
                name,
                "applied_text_contains check has no values",
            ));
        }
        _ => {}
    }
    Ok(())
}

fn apply_replacements(
    source: &mut String,
    replacements: &[[String; 2]],
    path: &Path,
    name: &str,
) -> Result<()> {
    for [old, new] in replacements {
        if old.is_empty() || !source.contains(old) {
            return Err(fixture_error(
                path,
                name,
                "REPLACE source is empty or absent",
            ));
        }
        *source = source.replace(old, new);
    }
    Ok(())
}
fn new_fixture_project(name: &str, with_manifest: bool) -> Result<(String, PathBuf)> {
    let test_dir = PathBuf::from(".lsp-test-tmp");
    fs_err::create_dir_all(&test_dir)?;
    let safe_name: String = name
        .chars()
        .map(|character| {
            if character == ' ' || character == '/' {
                '_'
            } else {
                character
            }
        })
        .collect();
    static NEXT_PROJECT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT_PROJECT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let project_dir = test_dir.join(format!("{safe_name}-{}-{n}", std::process::id()));
    fs_err::create_dir_all(&project_dir)?;
    if with_manifest {
        fs_err::write(
            project_dir.join("stick.toml"),
            "[package]\nname = \"lsp_test\"\nversion = \"0.1.0\"\n",
        )?;
    }
    Ok((safe_name, project_dir))
}

fn write_fixture_project(test: &Fixture) -> Result<(PathBuf, PathBuf)> {
    let (safe_name, project_dir) = new_fixture_project(&test.name, !test.files.is_empty())?;
    if !test.files.is_empty() {
        for (name, content) in &test.files {
            if name != "stick.toml" {
                fs_err::write(project_dir.join(name), content)?;
            }
        }
        let primary = if test.primary_file.is_empty() {
            test.files
                .first()
                .map(|(name, _)| name.clone())
                .unwrap_or_default()
        } else {
            test.primary_file.clone()
        };
        Ok((project_dir.join(primary), project_dir))
    } else {
        let file = project_dir.join(format!("{safe_name}.rock"));
        fs_err::write(&file, &test.code)?;
        Ok((file, project_dir))
    }
}

fn request_label(test: &Fixture) -> String {
    format!("{} test '{}'", test.kind, test.name)
}

fn expand_string(
    value: &str,
    test_file: &Path,
    uri: &str,
    position: Option<(usize, usize)>,
) -> Result<String> {
    match value {
        "$file" => Ok(test_file.display().to_string()),
        "$uri" => Ok(uri.to_string()),
        "$target" => {
            let (line, character) =
                position.ok_or_else(|| anyhow::anyhow!("request target has no position"))?;
            Ok(format!(
                "{}:{}:{}",
                test_file.display(),
                line + 1,
                character + 1
            ))
        }
        _ => Ok(value.to_string()),
    }
}

fn expand_json(
    value: &Value,
    test_file: &Path,
    uri: &str,
    position: Option<(usize, usize)>,
) -> Result<Value> {
    match value {
        Value::String(value) if value == "$file" => {
            Ok(Value::String(test_file.display().to_string()))
        }
        Value::String(value) if value == "$uri" => Ok(Value::String(uri.to_string())),
        Value::String(value) if value == "$position" => {
            let (line, character) =
                position.ok_or_else(|| anyhow::anyhow!("request position is missing"))?;
            Ok(json!({ "line": line, "character": character }))
        }
        Value::Array(values) => Ok(Value::Array(
            values
                .iter()
                .map(|value| expand_json(value, test_file, uri, position))
                .collect::<Result<_>>()?,
        )),
        Value::Object(values) => {
            let mut expanded = serde_json::Map::new();
            for (key, value) in values {
                expanded.insert(key.clone(), expand_json(value, test_file, uri, position)?);
            }
            Ok(Value::Object(expanded))
        }
        _ => Ok(value.clone()),
    }
}

fn normalize_rpc_reply(mut reply: Value) -> Value {
    let contents = reply.pointer("/result/contents").cloned();
    if let Some(value) = contents.and_then(|contents| {
        contents
            .get("value")
            .and_then(Value::as_str)
            .map(str::to_string)
    }) {
        reply["result"]["contents"] = Value::String(value);
    }
    reply
}

fn normalize_cli_reply(body: Value) -> Value {
    let error = body.get("error").and_then(Value::as_str).map(str::to_owned);
    let mut reply = json!({ "result": body });
    if let Some(error) = error {
        reply["error"] = json!({ "message": error });
    }
    reply
}

fn run_cli_request(
    test: &Fixture,
    request: &Request,
    server: &Path,
    test_file: &Path,
    uri: &str,
) -> Result<Option<(Value, String)>> {
    let Request::Cli { argv } = request else {
        anyhow::bail!("{}: expected a CLI request", request_label(test));
    };
    let mut command = Command::new(server);
    command.arg("lsp");
    for argument in argv {
        command.arg(expand_string(argument, test_file, uri, test.position)?);
    }
    let output = command.output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
            eprintln!(
                "    LSP command failed with exit code: {:?}",
                output.status.code()
            );
            if !stderr.is_empty() {
                eprintln!("    LSP stderr: {}", stderr);
            }
        }
        return Ok(None);
    }
    let body: Value = serde_json::from_str(&stdout).map_err(|error| {
        anyhow::anyhow!(
            "{}: reply is not JSON ({error}); raw reply: {stdout}",
            request_label(test)
        )
    })?;
    Ok(Some((normalize_cli_reply(body), stdout)))
}

fn file_uri(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not UTF-8: {}", path.display()))?;
    Ok(lsp_types::Uri::from_file_path(path)?.as_str().to_string())
}

fn request_rpc(
    server: &mut LspServer,
    request: &Request,
    test_file: &Path,
    uri: &str,
    position: Option<(usize, usize)>,
) -> Result<Value> {
    let Request::Rpc { method, params } = request else {
        anyhow::bail!("expected a JSON-RPC request");
    };
    server.request(method, expand_json(params, test_file, uri, position)?)
}

fn run_rpc_request(
    test: &Fixture,
    request: &Request,
    server_cmd: &Path,
    test_file: &Path,
    project_dir: &Path,
    uri: &str,
) -> Result<(Value, String)> {
    let mut server = start_lsp_server(server_cmd, project_dir, &test.name)?;
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri, "languageId": "rock", "version": 1, "text": test.code
        }}),
    )?;
    let response = request_rpc(&mut server, request, test_file, uri, test.position);
    server.shutdown()?;
    let mut response = normalize_rpc_reply(response?);
    let raw = serde_json::to_string(&response)?;
    if test.kind == "code_action" {
        response["_fixture_uri"] = Value::String(uri.to_string());
    }
    Ok((response, raw))
}

fn run_lsp_test(test: &Fixture, server: &Path) -> Result<bool> {
    let (test_file, project_dir) = write_fixture_project(test)?;
    let result = (|| {
        let test_file = std::fs::canonicalize(test_file)?;
        let uri = file_uri(&test_file)?;
        let request = test
            .request
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("{}: fixture has no request", request_label(test)))?;
        let response = match request {
            Request::Cli { .. } => run_cli_request(test, request, server, &test_file, &uri)?,
            Request::Rpc { .. } => Some(run_rpc_request(
                test,
                request,
                server,
                &test_file,
                &project_dir,
                &uri,
            )?),
        };
        let Some((response, raw)) = response else {
            return Ok(false);
        };
        let passed = check_assertions(test, &test.checks, &response, &raw, None)?;
        if std::env::var("ROCK_TEST_VERBOSE").is_ok() && !passed {
            eprintln!("    Actual output: {raw}");
        }
        Ok(passed)
    })();
    let _ = fs_err::remove_dir_all(&project_dir);
    result
}

fn check_assertions(
    test: &Fixture,
    checks: &[Check],
    response: &Value,
    raw: &str,
    step_sources: Option<&[String]>,
) -> Result<bool> {
    if response.get("jsonrpc").is_some() {
        if let Some(error) = response.get("error") {
            return Err(anyhow::anyhow!(
                "{}: server answered with an error: {error}",
                request_label(test)
            ));
        }
    }
    let mut passed = true;
    for check in checks {
        if !check_response(test, check, response, raw, step_sources)? {
            passed = false;
        }
    }
    Ok(passed)
}

fn check_response(
    test: &Fixture,
    check: &Check,
    response: &Value,
    raw: &str,
    step_sources: Option<&[String]>,
) -> Result<bool> {
    match check {
        Check::Contains {
            path,
            value,
            index_from,
        } => {
            let values = select_values(
                response,
                path,
                index_from.as_deref(),
                false,
                &request_label(test),
                raw,
            )?;
            Ok(values.iter().any(|actual| value_contains(actual, value)))
        }
        Check::ContainsAll { path, value } => {
            let values = select_values(response, path, None, false, &request_label(test), raw)?;
            Ok(value
                .iter()
                .all(|expected| values.iter().any(|actual| value_contains(actual, expected))))
        }
        Check::Equals { path, value } => {
            let values = select_values(response, path, None, false, &request_label(test), raw)?;
            Ok(values.first().is_some_and(|actual| *actual == value))
        }
        Check::TrimmedEquals { path, value } => {
            let values = select_values(response, path, None, false, &request_label(test), raw)?;
            let Some(actual) = values.first().and_then(|actual| actual.as_str()) else {
                return Err(anyhow::anyhow!(
                    "{}: reply field is not a string; raw reply: {raw}",
                    request_label(test)
                ));
            };
            Ok(actual.trim() == value.trim())
        }
        Check::UnorderedEqual { path, value } => {
            validate_projection_arrays(response, path, &request_label(test), raw)?;
            let values = select_values(response, path, None, false, &request_label(test), raw)?;
            let mut actual = values
                .into_iter()
                .flat_map(|value| {
                    value
                        .as_array()
                        .map(|array| array.iter().collect::<Vec<_>>())
                        .unwrap_or_else(|| vec![value])
                })
                .map(Value::to_string)
                .collect::<Vec<_>>();
            let mut expected = value.iter().map(Value::to_string).collect::<Vec<_>>();
            actual.sort();
            expected.sort();
            Ok(actual == expected)
        }
        Check::Gte { path, value } => {
            let values = select_values(response, path, None, false, &request_label(test), raw)?;
            let Some(actual) = values.first() else {
                return Err(anyhow::anyhow!(
                    "{}: reply path has no value; raw reply: {raw}",
                    request_label(test)
                ));
            };
            let count = actual
                .as_u64()
                .or_else(|| actual.as_array().map(|array| array.len() as u64))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "{}: gte check requires a number or array; raw reply: {raw}",
                        request_label(test)
                    )
                })?;
            Ok(count >= *value)
        }
        Check::Absent {
            path,
            value,
            null_is_empty,
        } => {
            let values = select_values(
                response,
                path,
                None,
                *null_is_empty,
                &request_label(test),
                raw,
            )?;
            let mut actions = Vec::new();
            for actual in values {
                if actual.is_null() && *null_is_empty {
                    continue;
                }
                let array = actual.as_array().ok_or_else(|| {
                    anyhow::anyhow!(
                        "{}: reply field is not an array; raw reply: {raw}",
                        request_label(test)
                    )
                })?;
                actions.extend(array);
            }
            Ok(!actions.iter().any(|actual| value_contains(actual, value)))
        }
        Check::Empty {
            path,
            null_is_empty,
            missing_is_empty,
        } => {
            match select_values(
                response,
                path,
                None,
                *null_is_empty,
                &request_label(test),
                raw,
            ) {
                Ok(values) => check_values_empty(
                    &values,
                    *null_is_empty,
                    path.iter()
                        .any(|part| matches!(part, PathPart::Key(key) if key == "*")),
                    &request_label(test),
                    raw,
                ),
                Err(error) if *missing_is_empty && error.to_string().contains("reply has no ") => {
                    Ok(true)
                }
                Err(error) => Err(error),
            }
        }
        Check::AppliedTextEquals {
            path,
            file,
            expected,
            selector,
        } => check_applied_text(
            test,
            response,
            path,
            file,
            expected,
            selector.as_ref(),
            raw,
            step_sources,
            None,
        ),
        Check::AppliedTextContains { path, value } => check_applied_text(
            test,
            response,
            path,
            "$primary",
            &Content::Text(String::new()),
            None,
            raw,
            step_sources,
            Some(value),
        ),
    }
}

fn select_values<'a>(
    root: &'a Value,
    path: &[PathPart],
    index_from: Option<&[PathPart]>,
    null_is_empty: bool,
    request: &str,
    raw: &str,
) -> Result<Vec<&'a Value>> {
    let selected_index = if let Some(index_path) = index_from {
        let index = select_values(root, index_path, None, false, request, raw)?;
        Some(index.first().and_then(|value| value.as_u64()).unwrap_or(0) as usize)
    } else {
        None
    };
    let mut nodes = vec![root];
    let mut indexed = false;
    for part in path {
        let mut next = Vec::new();
        for node in nodes {
            match part {
                PathPart::Key(key) if key == "*" => {
                    if node.is_null() {
                        continue;
                    }
                    if let Some(array) = node.as_array() {
                        if !indexed {
                            if let Some(index) = selected_index {
                                if let Some(value) = array.get(index) {
                                    next.push(value);
                                }
                            } else {
                                next.extend(array.iter());
                            }
                        } else {
                            next.extend(array.iter());
                        }
                    } else if let Some(object) = node.as_object() {
                        next.extend(object.values());
                    }
                }
                PathPart::Key(key) => {
                    if node.is_null() && null_is_empty {
                        next.push(node);
                        continue;
                    }
                    let object = node.as_object().ok_or_else(|| {
                        anyhow::anyhow!(
                            "{request}: reply path expected an object at {key}; raw reply: {raw}"
                        )
                    })?;
                    next.push(object.get(key).ok_or_else(|| {
                        anyhow::anyhow!("{request}: reply has no {key} field; raw reply: {raw}")
                    })?);
                }
                PathPart::Index(index) => {
                    let array = node.as_array().ok_or_else(|| {
                        anyhow::anyhow!("{request}: reply path expected an array; raw reply: {raw}")
                    })?;
                    if let Some(value) = array.get(*index) {
                        next.push(value);
                    }
                }
            }
        }
        if matches!(part, PathPart::Key(key) if key == "*") && selected_index.is_some() {
            indexed = true;
        }
        nodes = next;
    }
    Ok(nodes)
}

fn value_contains(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::String(actual), Value::String(expected)) => actual.contains(expected),
        (Value::Array(actual), Value::Object(_)) => {
            actual.iter().any(|value| partial_match(value, expected))
        }
        (Value::Object(_), Value::Object(_)) => partial_match(actual, expected),
        _ => actual == expected,
    }
}

fn partial_match(actual: &Value, expected: &Value) -> bool {
    let Some(pattern) = expected.as_object() else {
        return actual == expected;
    };
    if let Some(text) = pattern.get("text").and_then(Value::as_str) {
        let Some(actual) = actual.as_str() else {
            return false;
        };
        let actual = actual.trim();
        let actual = pattern
            .get("strip_prefix")
            .and_then(Value::as_str)
            .and_then(|prefix| actual.strip_prefix(prefix))
            .unwrap_or(actual);
        return actual.trim() == text;
    }
    let Some(actual) = actual.as_object() else {
        return false;
    };
    pattern.iter().all(|(key, expected)| {
        actual
            .get(key)
            .is_some_and(|actual| partial_match(actual, expected))
    })
}

fn validate_projection_arrays(
    root: &Value,
    path: &[PathPart],
    request: &str,
    raw: &str,
) -> Result<()> {
    for (index, part) in path.iter().enumerate() {
        if !matches!(part, PathPart::Key(key) if key == "*") {
            continue;
        }
        for parent in select_values(root, &path[..index], None, false, request, raw)? {
            if !parent.is_array() {
                return Err(anyhow::anyhow!(
                    "{request}: reply projection source is not an array; raw reply: {raw}"
                ));
            }
        }
    }
    Ok(())
}

fn check_values_empty(
    values: &[&Value],
    null_is_empty: bool,
    projection: bool,
    request: &str,
    raw: &str,
) -> Result<bool> {
    if values.is_empty() {
        return Ok(true);
    }
    for value in values {
        let empty = if value.is_null() {
            null_is_empty
        } else if let Some(array) = value.as_array() {
            array.is_empty()
        } else if let Some(object) = value.as_object() {
            object.is_empty()
        } else if projection {
            true
        } else {
            return Err(anyhow::anyhow!(
                "{request}: empty check has the wrong reply type; raw reply: {raw}"
            ));
        };
        if !empty {
            return Ok(false);
        }
    }
    Ok(true)
}
fn resolve_content(
    content: &Content,
    test: &Fixture,
    step_sources: Option<&[String]>,
) -> Result<String> {
    match content {
        Content::Text(text) => Ok(text.clone()),
        Content::Reference(reference) => {
            let mut text = match (
                reference.from_test.as_deref(),
                reference.from_step,
                reference.from_fixture,
            ) {
                (None, None, Some(true)) => test.code.clone(),
                (None, Some(step), None) => step_sources
                    .and_then(|sources| step.checked_sub(1).and_then(|index| sources.get(index)))
                    .cloned()
                    .ok_or_else(|| {
                        anyhow::anyhow!("test '{}': SOURCE step is unavailable", test.name)
                    })?,
                (Some(_), None, None) => {
                    return Err(anyhow::anyhow!(
                        "test '{}': unresolved source test",
                        test.name
                    ));
                }
                _ => {
                    return Err(anyhow::anyhow!(
                        "test '{}': invalid content source",
                        test.name
                    ));
                }
            };
            apply_pairs(&mut text, &reference.replace, &test.name)?;
            Ok(text)
        }
    }
}

fn apply_pairs(text: &mut String, pairs: &[[String; 2]], name: &str) -> Result<()> {
    for [old, new] in pairs {
        if old.is_empty() || !text.contains(old) {
            return Err(anyhow::anyhow!(
                "test '{name}': REPLACE source is empty or absent"
            ));
        }
        *text = text.replace(old, new);
    }
    Ok(())
}

fn fixture_source(test: &Fixture, source: &str) -> Result<String> {
    if source == "primary" || source == "$primary" {
        return Ok(test.code.clone());
    }
    test.files
        .iter()
        .find(|(path, _)| path == source)
        .map(|(_, text)| text.clone())
        .ok_or_else(|| anyhow::anyhow!("test '{}': source file {source:?} is missing", test.name))
}

fn check_applied_text(
    test: &Fixture,
    response: &Value,
    path: &[PathPart],
    file: &str,
    expected: &Content,
    selector: Option<&Value>,
    raw: &str,
    step_sources: Option<&[String]>,
    contains: Option<&[String]>,
) -> Result<bool> {
    let request = if selector.is_some() {
        format!("code_action test '{}'", test.name)
    } else {
        request_label(test)
    };
    if selector.is_some() {
        if let Some(error) = response.get("error") {
            return Err(anyhow::anyhow!(
                "{request}: server answered with an error: {error}"
            ));
        }
    }
    let values = select_values(response, path, None, false, &request, raw)?;
    let source_text = fixture_source(test, file)?;
    let expected_text = if contains.is_some() {
        String::new()
    } else {
        resolve_content(expected, test, step_sources)?
    };
    let target_file = if file == "$primary" || file == "primary" {
        &test.primary_file
    } else {
        file
    };
    let actual = if let Some(selector) = selector {
        let actions = values
            .first()
            .and_then(|value| value.as_array())
            .ok_or_else(|| {
                anyhow::anyhow!("{request}: reply has no result array; raw reply: {raw}")
            })?;
        let action = actions
            .iter()
            .find(|action| partial_match(action, selector));
        let Some(action) = action else {
            let title = selector.get("title").and_then(Value::as_str).unwrap_or("");
            let offered: Vec<&str> = actions
                .iter()
                .filter_map(|action| action.get("title").and_then(Value::as_str))
                .collect();
            return Err(anyhow::anyhow!(
                "{request}: no action titled {title:?}; the server offered {offered:?}"
            ));
        };
        let uri = response
            .get("_fixture_uri")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("{request}: fixture URI is missing"))?;
        let edits = code_action_edits(action, uri)
            .map_err(|error| anyhow::anyhow!("{request}: {error}; action: {action}"))?;
        apply_lsp_text_edits(&source_text, &edits)
            .map_err(|error| anyhow::anyhow!("{request}: {error}; action: {action}"))?
    } else {
        let changes = values
            .first()
            .and_then(|value| value.as_object())
            .ok_or_else(|| {
                anyhow::anyhow!("{request}: reply has no changes object; raw reply: {raw}")
            })?;
        let edits: Vec<&Value> = if target_file.is_empty() || target_file == "$primary" {
            changes
                .values()
                .filter_map(Value::as_array)
                .flat_map(|array| array.iter())
                .collect()
        } else {
            changes
                .iter()
                .filter(|(uri, _)| uri.ends_with(&format!("/{target_file}")))
                .filter_map(|(_, edits)| edits.as_array())
                .flat_map(|array| array.iter())
                .collect()
        };
        apply_cli_text_edits(&source_text, &edits)
    };
    if let Some(needles) = contains {
        return Ok(needles.iter().all(|needle| actual.contains(needle)));
    }
    let matches = actual.trim_end() == expected_text.trim_end();
    if !matches && selector.is_some() {
        let title = selector
            .and_then(|selector| selector.get("title"))
            .and_then(Value::as_str)
            .unwrap_or("");
        return Err(anyhow::anyhow!(
            "{request}: result after {title:?} differs\n--- expected\n{expected_text}\n--- actual\n{actual}"
        ));
    }
    Ok(matches)
}

fn code_action_edits<'a>(action: &'a Value, uri: &str) -> Result<Vec<&'a Value>> {
    let edit = action
        .get("edit")
        .ok_or_else(|| anyhow::anyhow!("action carries no workspace edit"))?;
    let mut edits = Vec::new();
    let check_uri = |target: &str| {
        if target == uri {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "action edits {target}, not the fixture file {uri}"
            ))
        }
    };
    if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
        for (target, file_edits) in changes {
            check_uri(target)?;
            let file_edits = file_edits
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("edits for {target} are not an array"))?;
            edits.extend(file_edits);
        }
    }
    if let Some(changes) = edit.get("documentChanges").and_then(Value::as_array) {
        for change in changes {
            let target = change
                .pointer("/textDocument/uri")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("document change has no text document"))?;
            check_uri(target)?;
            let file_edits = change
                .get("edits")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow::anyhow!("document change for {target} has no edits"))?;
            edits.extend(file_edits);
        }
    }
    if edits.is_empty() {
        return Err(anyhow::anyhow!("action's workspace edit changes nothing"));
    }
    Ok(edits)
}

fn apply_cli_text_edits(source: &str, edits: &[&Value]) -> String {
    let mut edits = edits.to_vec();
    let position = |edit: &Value, which: &str| -> (u64, u64) {
        let range = edit.get("range").and_then(|range| range.get(which));
        let line = range
            .and_then(|value| value.get("line"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let column = range
            .and_then(|value| value.get("character"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        (line, column)
    };
    edits.sort_by_key(|edit| std::cmp::Reverse(position(edit, "start")));
    let mut result = source.to_string();
    for edit in edits {
        let Some(new_text) = edit.get("new_text").and_then(Value::as_str) else {
            continue;
        };
        let (start_line, start_column) = position(edit, "start");
        let (end_line, end_column) = position(edit, "end");
        let (start_line, start_column, end_line, end_column) = (
            start_line as usize,
            start_column as usize,
            end_line as usize,
            end_column as usize,
        );
        let lines: Vec<&str> = result.lines().collect();
        let mut new_lines = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if index < start_line || index > end_line {
                new_lines.push(line.to_string());
            } else if index == start_line && index == end_line {
                let before = &line[..start_column.min(line.len())];
                let after = &line[end_column.min(line.len())..];
                new_lines.push(format!("{before}{new_text}{after}"));
            } else if index == start_line {
                let before = &line[..start_column.min(line.len())];
                new_lines.push(format!("{before}{new_text}"));
            } else if index == end_line {
                let after = &line[end_column.min(line.len())..];
                if let Some(last) = new_lines.last_mut() {
                    last.push_str(after);
                }
            }
        }
        result = new_lines.join("\n");
    }
    result
}

fn lsp_offset(source: &str, line: u64, character: u64) -> Option<usize> {
    let mut start = 0;
    for _ in 0..line {
        start += source[start..].find('\n')? + 1;
    }
    let text = source[start..].split('\n').next().unwrap_or("");
    let mut units = 0;
    for (index, ch) in text.char_indices() {
        if units >= character {
            return Some(start + index);
        }
        units += ch.len_utf16() as u64;
    }
    Some(start + text.len())
}

fn apply_lsp_text_edits(source: &str, edits: &[&Value]) -> Result<String> {
    let offset_at = |edit: &Value, which: &str| -> Result<usize> {
        let position = edit
            .get("range")
            .and_then(|range| range.get(which))
            .ok_or_else(|| anyhow::anyhow!("edit has no range {which}: {edit}"))?;
        let field = |name: &str| position.get(name).and_then(Value::as_u64);
        let (Some(line), Some(character)) = (field("line"), field("character")) else {
            return Err(anyhow::anyhow!(
                "edit has a malformed range {which}: {edit}"
            ));
        };
        lsp_offset(source, line, character).ok_or_else(|| {
            anyhow::anyhow!("edit range {which} is past the end of the file: {edit}")
        })
    };
    let mut spans = Vec::with_capacity(edits.len());
    for edit in edits {
        let (start, end) = (offset_at(edit, "start")?, offset_at(edit, "end")?);
        let text = edit
            .get("newText")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("edit has no newText: {edit}"))?;
        if end < start {
            return Err(anyhow::anyhow!("edit range ends before it starts: {edit}"));
        }
        spans.push((start, end, text));
    }
    spans.sort_by_key(|&(start, end, _)| std::cmp::Reverse((start, end)));
    let mut result = source.to_string();
    let mut limit = source.len();
    for (start, end, text) in spans {
        if end > limit {
            return Err(anyhow::anyhow!("code action edits overlap"));
        }
        result.replace_range(start..end, text);
        limit = start;
    }
    Ok(result)
}

fn encode_jsonrpc(message: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(message).unwrap();
    format!("Content-Length: {}\r\n\r\n", body.len())
        .into_bytes()
        .into_iter()
        .chain(body)
        .collect()
}

fn read_jsonrpc(reader: &mut BufReader<std::process::ChildStdout>) -> Result<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            anyhow::bail!("LSP server closed stdout");
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = length.ok_or_else(|| anyhow::anyhow!("No Content-Length in JSON-RPC response"))?;
    if length == 0 {
        anyhow::bail!("No Content-Length in JSON-RPC response");
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

fn read_response(rx: &std::sync::mpsc::Receiver<Value>, id: i64) -> Result<Value> {
    loop {
        let message = rx
            .recv()
            .map_err(|error| anyhow::anyhow!("LSP reader channel closed: {error}"))?;
        if message.get("id").and_then(Value::as_i64) == Some(id) {
            return Ok(message);
        }
    }
}

struct LspServer {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    rx: std::sync::mpsc::Receiver<Value>,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    next_id: i64,
}

impl LspServer {
    fn send(&mut self, message: Value) -> Result<()> {
        self.stdin.write_all(&encode_jsonrpc(&message))?;
        self.stdin.flush()?;
        Ok(())
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0", "method":method, "params":params}))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}))?;
        read_response(&self.rx, id)
    }

    fn shutdown(mut self) -> Result<()> {
        let _ = self.request("shutdown", Value::Null);
        self.notify("exit", Value::Null)?;
        let _ = self.child.wait();
        self.done.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

fn new_server_stderr_log(dir: &Path) -> Result<PathBuf> {
    let file = tempfile::Builder::new()
        .prefix("rock-lsp-test-stderr-")
        .suffix(".log")
        .tempfile_in(dir)?;
    let (_, path) = file
        .keep()
        .map_err(|error| anyhow::anyhow!("cannot keep LSP stderr log: {error}"))?;
    Ok(path)
}

fn server_stderr_log() -> Result<PathBuf> {
    static LOG: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);
    let mut log = LOG
        .lock()
        .map_err(|_| anyhow::anyhow!("LSP stderr log lock poisoned"))?;
    if let Some(path) = log.as_ref() {
        return Ok(path.clone());
    }
    let path = new_server_stderr_log(&std::env::temp_dir())?;
    eprintln!("LSP server stderr: {}", path.display());
    *log = Some(path.clone());
    Ok(path)
}

fn start_lsp_server(command: &Path, root: &Path, name: &str) -> Result<LspServer> {
    let log = server_stderr_log()?;
    let stderr = || -> std::io::Result<Stdio> {
        Ok(Stdio::from(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)?,
        ))
    };
    let mut child = Command::new(command)
        .args(["lsp", "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr()?)
        .spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("Failed to open stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("Failed to open stdout"))?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        while let Ok(message) = read_jsonrpc(&mut reader) {
            if tx.send(message).is_err() {
                break;
            }
        }
    });

    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher_done = done.clone();
    let pid = child.id();
    let watcher_name = name.to_string();
    std::thread::spawn(move || {
        let start = bone_core::prelude::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(60) {
            if watcher_done.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if !watcher_done.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!(
                "[lsp-test watchdog] killing pid {pid} after 60s \u{2014} test '{watcher_name}' wedged"
            );
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    });

    let mut server = LspServer {
        child,
        stdin,
        rx,
        done,
        next_id: 1,
    };
    server.request("initialize", json!({
        "processId": std::process::id(),
        "capabilities": {"textDocument":{"semanticTokens":{"requests":{"full":true},"tokenTypes":[],"tokenModifiers":[]}}},
        "rootUri": file_uri(&std::fs::canonicalize(root)?)?,
        "workspaceFolders": null
    }))?;
    server.notify("initialized", json!({}))?;
    Ok(server)
}

fn insert_char_at(source: &str, line: usize, col: usize, ch: char) -> String {
    let mut lines = source.lines().map(str::to_owned).collect::<Vec<_>>();
    while lines.len() <= line {
        lines.push(String::new());
    }
    let target = &mut lines[line];
    target.insert(col.min(target.len()), ch);
    lines.join("\n")
}

fn delete_char_at(source: &str, line: usize, col: usize) -> (String, usize, usize) {
    let mut lines = source.lines().map(str::to_owned).collect::<Vec<_>>();
    if col > 0 && line < lines.len() {
        let col = col.min(lines[line].len());
        if col > 0 {
            lines[line].remove(col - 1);
            return (lines.join("\n"), line, col - 1);
        }
    }
    if line > 0 && line < lines.len() {
        let previous_len = lines[line - 1].len();
        let current = lines.remove(line);
        lines[line - 1].push_str(&current);
        return (lines.join("\n"), line - 1, previous_len);
    }
    (lines.join("\n"), line, col)
}

fn run_edit_sequence(test: &Fixture, server_cmd: &Path) -> Result<Vec<String>> {
    let Step::Replace { source, .. } = &test.steps[0] else {
        anyhow::bail!(
            "test '{}': first edit_sequence step must replace the document",
            test.name
        );
    };
    let initial = resolve_content(source, test, Some(&[]))?;
    let (safe_name, project) = new_fixture_project(&test.name, false)?;
    let file = project.join(format!("{safe_name}.rock"));
    fs_err::write(&file, &initial)?;
    let file = std::fs::canonicalize(file)?;
    let uri = file_uri(&file)?;
    let result = (|| {
        let mut server = start_lsp_server(server_cmd, &project, &test.name)?;
        let mut failures = Vec::new();
        let run = (|| {
            let mut version = 1i64;
            let mut document = initial;
            let mut history = Vec::new();
            server.notify(
                "textDocument/didOpen",
                json!({"textDocument": {
                    "uri": uri, "languageId": "rock", "version": version, "text": document
                }}),
            )?;
            for (step_index, step) in test.steps.iter().enumerate() {
                if step_index > 0 {
                    match step {
                        Step::Replace { source, .. } => {
                            document = resolve_content(source, test, Some(&history))?;
                            version += 1;
                            fs_err::write(&file, &document)?;
                            server.notify("textDocument/didChange", json!({"textDocument":{"uri":uri,"version":version},"contentChanges":[{"text":document}]}))?;
                        }
                        Step::TypeAt {
                            line, col, text, ..
                        } => {
                            if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                                eprintln!(
                                    "    Step {}: typing {} chars at {}:{}",
                                    step_index + 1,
                                    text.len(),
                                    line,
                                    col
                                );
                            }
                            let (mut cursor_line, mut cursor_col) = (*line, *col);
                            for ch in text.chars() {
                                if ch == '\n' {
                                    let mut lines =
                                        document.lines().map(str::to_owned).collect::<Vec<_>>();
                                    while lines.len() <= cursor_line {
                                        lines.push(String::new());
                                    }
                                    let column = cursor_col.min(lines[cursor_line].len());
                                    let rest = lines[cursor_line][column..].to_owned();
                                    lines[cursor_line].truncate(column);
                                    lines.insert(cursor_line + 1, rest);
                                    document = lines.join("\n");
                                    cursor_line += 1;
                                    cursor_col = 0;
                                } else {
                                    document =
                                        insert_char_at(&document, cursor_line, cursor_col, ch);
                                    cursor_col += ch.len_utf8();
                                }
                                version += 1;
                                server.notify("textDocument/didChange", json!({"textDocument":{"uri":uri,"version":version},"contentChanges":[{"text":document}]}))?;
                            }
                            fs_err::write(&file, &document)?;
                        }
                        Step::DeleteAt {
                            line, col, count, ..
                        } => {
                            if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                                eprintln!(
                                    "    Step {}: deleting {} chars at {}:{}",
                                    step_index + 1,
                                    count,
                                    line,
                                    col
                                );
                            }
                            let (mut cursor_line, mut cursor_col) = (*line, *col);
                            for _ in 0..*count {
                                (document, cursor_line, cursor_col) =
                                    delete_char_at(&document, cursor_line, cursor_col);
                                version += 1;
                                server.notify("textDocument/didChange", json!({"textDocument":{"uri":uri,"version":version},"contentChanges":[{"text":document}]}))?;
                            }
                            fs_err::write(&file, &document)?;
                        }
                    }
                }
                history.push(document.clone());
                let mut current = test.clone();
                current.code = document.clone();
                for check in step.checks() {
                    let response = request_rpc(&mut server, &check.request, &file, &uri, None)?;
                    let response = normalize_rpc_reply(response);
                    let raw = serde_json::to_string(&response)?;
                    if !check_assertions(
                        &current,
                        &check.assertions,
                        &response,
                        &raw,
                        Some(&history),
                    )? {
                        let label = sequence_check_label(&check.request);
                        failures.push(format!("step {} {label}", step_index + 1));
                        if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                            eprintln!("    Step {}: FAIL - {label}", step_index + 1);
                        }
                    }
                }
            }
            Ok(())
        })();
        let stopped = server.shutdown();
        run.and(stopped).map(|()| failures)
    })();
    let _ = fs_err::remove_dir_all(&project);
    result
}

fn sequence_check_label(request: &Request) -> String {
    let Request::Rpc { method, params } = request else {
        return "CLI request".to_string();
    };
    let line = params
        .pointer("/position/line")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let character = params
        .pointer("/position/character")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    match method.as_str() {
        "textDocument/semanticTokens/full" => "SemanticTokens".to_string(),
        "textDocument/completion" => {
            format!("Completion {{ line: {line}, character: {character} }}")
        }
        "textDocument/hover" => format!("Hover {{ line: {line}, character: {character} }}"),
        "textDocument/references" => {
            format!("References {{ line: {line}, character: {character} }}")
        }
        other => other.to_string(),
    }
}

struct FileTestResult {
    file_name: String,
    passed: usize,
    failed: usize,
    duration: std::time::Duration,
    failures: Vec<(String, String)>,
}

impl FileTestResult {
    fn record(&mut self, name: &str, kind: &str, failure: Option<String>) {
        if let Some(detail) = failure {
            self.failed += 1;
            self.failures.push((format!("{name} ({kind})"), detail));
        } else {
            self.passed += 1;
        }
    }
}

fn run_lsp_file(file: &Path, display_name: &str, server: &Path) -> FileTestResult {
    let started = bone_core::prelude::Instant::now();
    let mut result = FileTestResult {
        file_name: display_name.to_owned(),
        passed: 0,
        failed: 0,
        duration: std::time::Duration::ZERO,
        failures: Vec::new(),
    };
    let tests = match parse_suite(file) {
        Ok(tests) => tests,
        Err(error) => {
            result.failed = 1;
            result
                .failures
                .push(("parse".to_string(), error.to_string()));
            result.duration = started.elapsed();
            return result;
        }
    };
    for test in tests {
        let detail = match test.kind.as_str() {
            "edit_sequence" => match run_edit_sequence(&test, server) {
                Ok(failures) if failures.is_empty() => None,
                Ok(failures) => Some(failures.join(", ")),
                Err(error) => Some(error.to_string()),
            },
            _ => match run_lsp_test(&test, server) {
                Ok(true) => None,
                Ok(false) => Some(String::new()),
                Err(error) => Some(error.to_string()),
            },
        };
        result.record(&test.name, &test.kind, detail);
    }
    result.duration = started.elapsed();
    result
}

fn print_file_result(result: &FileTestResult) {
    use colored::Colorize;
    let total = result.passed + result.failed;
    let dots = ".".repeat(60_usize.saturating_sub(result.file_name.len().min(50)));
    let status = if result.failed == 0 {
        "ok".green()
    } else {
        "FAILED".red()
    };
    println!(
        "{} {} {} ({}/{} tests, {:.0}ms)",
        result.file_name,
        dots,
        status,
        result.passed,
        total,
        result.duration.as_secs_f64() * 1000.0
    );
    for (name, detail) in &result.failures {
        println!("  {} ... {}", name, "FAILED".red());
        for line in detail.lines() {
            println!("    {line}");
        }
    }
}

fn lsp_suite_root(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return (path.extension().and_then(|extension| extension.to_str()) == Some("lsp"))
            .then(|| path.to_path_buf());
    }
    if path.ends_with("tests/lsp") && path.is_dir() {
        return Some(path.to_path_buf());
    }
    if path
        .parent()
        .is_some_and(|parent| parent.ends_with("tests/lsp"))
    {
        return path
            .parent()
            .filter(|parent| parent.is_dir())
            .map(Path::to_path_buf);
    }
    let subdir = path.join("lsp");
    (subdir.is_dir() && !lsp_files_under(&subdir).is_empty()).then_some(subdir)
}

fn lsp_files_under(dir: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .flatten()
        .filter(|entry| {
            entry.file_type().is_file()
                && entry
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str())
                    == Some("lsp")
        })
        .map(|entry| entry.into_path())
        .collect()
}

pub(crate) fn run_lsp_suite(path: &Path) -> Result<usize> {
    use colored::Colorize;
    let Some(root) = lsp_suite_root(path) else {
        return Ok(0);
    };
    let (mut files, display_root) = if root.is_file() {
        println!("\nRunning LSP test file {}...", root.display());
        (
            vec![root.clone()],
            root.parent().unwrap_or(&root).to_path_buf(),
        )
    } else {
        println!("\nRunning LSP tests in {}...", root.display());
        (lsp_files_under(&root), root.clone())
    };
    if std::env::var("ROCK_LSP_RUN_STRESS").is_err() {
        let before = files.len();
        files.retain(|file| {
            !file
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("_stress.rock.lsp"))
        });
        let skipped = before - files.len();
        if skipped > 0 {
            println!(
                "(skipped {skipped} *_stress.rock.lsp file(s); set ROCK_LSP_RUN_STRESS=1 to include)"
            );
        }
    }
    files.sort();
    if files.is_empty() {
        return Ok(0);
    }
    let started = bone_core::prelude::Instant::now();
    let bin = std::env::current_exe().map_err(|error| {
        anyhow::anyhow!("cannot find the running executable to use as the LSP server: {error}")
    })?;
    let threads = std::thread::available_parallelism().map_or(4, |count| count.get());
    let results = crate::parallel_runner::map_in_order(&files, threads, |file| {
        let relative = file
            .strip_prefix(&display_root)
            .unwrap_or(file)
            .to_string_lossy();
        let display = relative.strip_suffix(".lsp").unwrap_or(&relative);
        run_lsp_file(file, display, &bin)
    });
    let (mut passed, mut failed) = (0, 0);
    for result in &results {
        passed += result.passed;
        failed += result.failed;
        if result.passed + result.failed > 0 {
            print_file_result(result);
        }
    }
    if passed + failed > 0 {
        let status = if failed == 0 {
            "ok".green()
        } else {
            "FAILED".red()
        };
        println!(
            "\nLSP tests: {}. {} passed; {} failed; {} ignored; finished in {:.2}s",
            status,
            passed,
            failed,
            0,
            started.elapsed().as_secs_f64()
        );
    }
    Ok(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_run_gets_its_own_server_stderr_log() {
        let dir = tempfile::tempdir().unwrap();
        let first = new_server_stderr_log(dir.path()).unwrap();
        let second = new_server_stderr_log(dir.path()).unwrap();
        assert_ne!(first, second);
        assert!(first.exists());
        assert!(second.exists());
    }
}
