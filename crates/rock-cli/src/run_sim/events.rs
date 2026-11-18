use super::{
    ImageSnapshotOp, ReloadCompletion, ReloadEditReq, ReloadTerminal, SimSummary, SiteMessage,
};
use anyhow::{Result, anyhow};
use colored::Colorize;
use serde_json::Value;
use std::sync::mpsc::Sender;

/// Foreign stderr writers can leave a prefix before a complete marker. All
/// consumers validate the entire payload before interpreting any operation.
pub(super) fn parse_sim_marker(payload: &str, line: &str) -> Result<Value> {
    let raw = payload
        .strip_prefix(' ')
        .ok_or_else(|| anyhow!("invalid sim marker: missing payload separator: {line}"))?;
    let event: Value = serde_json::from_str(raw)
        .map_err(|error| anyhow!("invalid sim marker ({error}): {line}"))?;
    if !event.is_object()
        || event
            .get("op")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return Err(anyhow!(
            "invalid sim marker: expected an object with a nonempty string op: {line}"
        ));
    }
    Ok(event)
}

/// Compress a `@call_site` path to `<stem>:<line>` for display — the
/// directory is noise and every sim file ends in `.rock`. Drops the
/// workspace prefix implicitly by taking only the basename.
fn compress_site(path: &str) -> String {
    let (file_part, line_part) = match path.rsplit_once(':') {
        Some((f, l)) => (f, Some(l)),
        None => (path, None),
    };
    let basename = file_part.rsplit('/').next().unwrap_or(file_part);
    let stem = basename.strip_suffix(".rock").unwrap_or(basename);
    match line_part {
        Some(l) => format!("{stem}:{l}"),
        None => stem.to_string(),
    }
}

pub(super) struct SimEvent<'a> {
    site: String,
    op: SimOp<'a>,
}

enum SimOp<'a> {
    StartGame(&'a str),
    SetEnv(&'a str, &'a str),
    Key(&'a str),
    Click(Option<&'a Value>, Option<&'a Value>),
    Custom(&'a str, Option<&'a Value>),
    ExpectOk(&'a str),
    ExpectFail(&'a str, Option<(&'a str, &'a str)>),
    Error(&'a str),
    Log(&'a str),
    Stop,
    Exclude(&'a str),
    NoChecks(&'a str),
    Snapshot {
        name: &'a str,
        path: &'a str,
        tol: f64,
        save_ok: bool,
    },
    Cleanup(Option<&'a str>),
    ReloadEdit {
        request_id: Option<u64>,
        file: &'a str,
        find: &'a str,
        replace: &'a str,
    },
    ReloadDone(Option<ReloadCompletion>),
    Census(&'a Value),
    Other,
}

impl<'a> SimEvent<'a> {
    pub(super) fn decode(op: &str, fields: &'a Value) -> Self {
        use SimOp::*;
        let text = |key: &str| fields[key].as_str().unwrap_or_default();
        let site = text("site");
        let op = match op {
            "start_game" => StartGame(text("name")),
            "set_env" => SetEnv(text("key"), text("value")),
            "key" => Key(text("name")),
            "click" => Click(fields.get("x"), fields.get("y")),
            "custom" => Custom(text("name"), fields.get("args")),
            "expect_ok" => ExpectOk(text("message")),
            "expect_fail" => ExpectFail(
                text("message"),
                fields["actual"].as_str().zip(fields["expected"].as_str()),
            ),
            "error" => Error(text("message")),
            "log" => Log(text("message")),
            "stop" => Stop,
            "exclude" => Exclude(fields["reason"].as_str().unwrap_or("unspecified")),
            "no_checks" => NoChecks(text("reason")),
            "image_snapshot" => Snapshot {
                name: text("name"),
                path: text("path"),
                tol: fields["tol"].as_f64().unwrap_or_default(),
                save_ok: fields["ok"].as_bool().unwrap_or_default(),
            },
            "cleanup_path" => Cleanup(fields["path"].as_str()),
            "reload_edit" => ReloadEdit {
                request_id: fields["request_id"].as_u64(),
                file: text("file"),
                find: text("find"),
                replace: text("replace"),
            },
            "reload_terminal" => ReloadDone(
                match text("status") {
                    "applied" => Some(ReloadTerminal::Applied),
                    "failed" => Some(ReloadTerminal::Failed),
                    _ => None,
                }
                .map(|terminal| ReloadCompletion {
                    request_id: fields["request_id"].as_u64(),
                    terminal,
                }),
            ),
            "allocation_census" => Census(fields),
            _ => Other,
        };
        Self {
            site: compress_site(site),
            op,
        }
    }

    pub(super) fn progress(&self) -> Option<String> {
        use SimOp::*;
        let json = |v: Option<&Value>| v.map(ToString::to_string).unwrap_or_default();
        let body = match &self.op {
            StartGame(n) => format!("{} {}", "start".cyan(), n.yellow()),
            SetEnv(k, v) => format!("{} {}={}", "env".cyan(), k.yellow(), v),
            Key(n) => format!("{} {}", "key".cyan(), n.yellow()),
            Click(x, y) => format!("{} {} {}", "click".cyan(), json(*x), json(*y)),
            Custom(n, a) => format!("{} {} {}", "custom".cyan(), n.bright_cyan(), json(*a)),
            ExpectOk(m) => format!("{} {}  {}", "expect".cyan(), m, "OK".green().bold()),
            ExpectFail(m, diff) => {
                let head = format!("{} {}  {}", "expect".cyan(), m, "FAIL".red().bold());
                match diff {
                    Some((a, e)) => format!(
                        "{head}\n        {} {}\n        {} {}",
                        "actual:  ".red().bold(),
                        a.red(),
                        "expected:".green().bold(),
                        e.green()
                    ),
                    None => head,
                }
            }
            Snapshot { name, .. } => format!("{} {}", "image_snapshot".cyan(), name.yellow()),
            Cleanup(p) => format!("{} {}", "cleanup_path".cyan(), p.unwrap_or_default()),
            ReloadEdit { file, .. } => format!("{} {}", "reload_edit".cyan(), file.yellow()),
            Log(m) => format!("{} {}", "log".cyan(), m),
            Error(m) => format!("{} {}", "error:".red().bold(), m),
            Stop => "stop".cyan().to_string(),
            _ => return None,
        };
        if self.site.is_empty() {
            Some(body)
        } else {
            Some(format!(
                "{}{body}",
                format!("[{}] ", self.site).bright_black()
            ))
        }
    }

    pub(super) fn forward_reload(
        &self,
        reload_tx: &Sender<ReloadEditReq>,
        reload_completion_tx: &Sender<ReloadCompletion>,
    ) {
        match self.op {
            SimOp::ReloadEdit {
                request_id,
                file,
                find,
                replace,
            } => {
                let _ = reload_tx.send(ReloadEditReq {
                    request_id,
                    file: file.into(),
                    find: find.into(),
                    replace: replace.into(),
                    site: self.site.clone(),
                });
            }
            SimOp::ReloadDone(Some(done)) => {
                let _ = reload_completion_tx.send(done);
            }
            _ => {}
        }
    }

    pub(super) fn record(self, summary: &mut SimSummary) {
        let Self { site, op } = self;
        use SimOp::*;
        let message = |text: &str, site| SiteMessage {
            message: text.into(),
            site,
        };
        match op {
            ExpectOk(m) => summary.expects_pass.push(message(m, site)),
            ExpectFail(m, _) => summary.expects_fail.push(message(m, site)),
            Error(m) => summary.errors.push(message(m, site)),
            Stop => summary.saw_stop = true,
            Exclude(reason) => {
                summary.excluded = Some(reason.into());
                summary.saw_stop = true;
            }
            NoChecks(reason) => summary.no_checks = Some(reason.into()),
            Census(fields) => match serde_path_to_error::deserialize(fields) {
                Ok(census) if summary.allocation_census.is_none() => {
                    summary.allocation_census = Some(census)
                }
                Ok(_) => summary
                    .errors
                    .push(message("duplicate allocation census result", site)),
                Err(e) => summary.errors.push(message(
                    &format!("invalid allocation census result: {e}"),
                    site,
                )),
            },
            Snapshot {
                name,
                path,
                tol,
                save_ok,
            } => summary.image_snapshots.push(ImageSnapshotOp {
                name: name.into(),
                path: path.into(),
                tol,
                save_ok,
                site,
            }),
            Cleanup(Some(path)) => summary.cleanup_paths.push(path.into()),
            _ => {} // set_env / start_game / etc. are informational
        }
    }
}
