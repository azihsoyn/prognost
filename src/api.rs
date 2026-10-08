//! Every operation goes through this type, same house rule as `td` and
//! `hide`: the TUI's keys, `--api`, and an agent asking over JSON all build
//! the same `Request` and hand it to the same dispatch, so none of them can
//! drift into doing something the others cannot.

use std::path::PathBuf;

use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::graph::{self, Graph};
use crate::origin::{self, Scope};
use crate::rev::Rev;
use crate::workspace;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// The graph's starting nodes: the files a diff touched, or one
    /// explicit `file` (`file:line` accepted, the line is not part of a
    /// file-level graph and is only echoed back to the caller). No edges —
    /// nothing is reached until something is expanded.
    Origin {
        #[serde(default)]
        scope: Option<Scope>,
        #[serde(default)]
        path: Option<String>,
    },
    /// Origin plus `hops` steps outward in both directions, folded into
    /// one graph. For a caller that wants the whole picture in one call —
    /// the TUI instead expands one node at a time, on request, by calling
    /// the same functions this dispatches to.
    Graph {
        #[serde(default)]
        scope: Option<Scope>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        hops: Option<u32>,
    },
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Graph { graph: Graph },
    Error { message: String },
}

/// The envelope a response is returned in, matching the `td`/`hide` shape
/// so anything already handling one can handle this too.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct Envelope {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Response>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

impl Envelope {
    pub fn wrap(id: &str, res: Response) -> Self {
        match res {
            Response::Error { message } => Self {
                id: id.to_string(),
                result: None,
                error: Some(ErrorBody {
                    code: "request_failed".into(),
                    message,
                }),
            },
            other => Self {
                id: id.to_string(),
                result: Some(other),
                error: None,
            },
        }
    }
}

pub fn request_id(req: &Request) -> &'static str {
    match req {
        Request::Origin { .. } => "cli:origin",
        Request::Graph { .. } => "cli:graph",
    }
}

pub fn dispatch(req: Request) -> Response {
    match apply(req) {
        Ok(r) => r,
        Err(e) => Response::Error {
            message: e.to_string(),
        },
    }
}

fn apply(req: Request) -> Result<Response> {
    let root = crate::repo::root()?;
    let rev = Rev::working();
    let workspace = workspace::discover(&root, &rev)?;
    match req {
        Request::Origin { scope, path } => {
            let files = starting_files(&root, scope, path)?;
            Ok(Response::Graph {
                graph: graph::origin(&root, &files, &workspace),
            })
        }
        Request::Graph { scope, path, hops } => {
            let files = starting_files(&root, scope, path)?;
            Ok(Response::Graph {
                graph: graph::build(&root, &rev, &files, hops.unwrap_or(1), &workspace),
            })
        }
    }
}

fn starting_files(
    root: &std::path::Path,
    scope: Option<Scope>,
    path: Option<String>,
) -> Result<Vec<PathBuf>> {
    let files = match path {
        Some(p) => vec![origin::parse_file_arg(&p).0],
        None => origin::changed_files(root, scope.unwrap_or_default())?,
    };
    if files.is_empty() {
        bail!("nothing changed (try --scope branch, or pass a file)");
    }
    Ok(files)
}

/// The schema for this API, derived from the request and response types
/// rather than written beside them, so it cannot describe something the
/// code does not do.
pub fn schema_document() -> serde_json::Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "prognost map API",
        "version": SCHEMA_VERSION,
        "request": schemars::schema_for!(Request),
        "response": schemars::schema_for!(Envelope),
        // `prognost --impact --json` prints one of these.
        "impact": schemars::schema_for!(crate::impact::ImpactReport),
        // `prognost plan --json` prints one of these.
        "plan": schemars::schema_for!(crate::plan::PlanReport),
        // `prognost assess --json` prints one of these.
        "assess": schemars::schema_for!(crate::assess::Assessment),
    })
}

/// Raised when a change breaks compatibility.
pub const SCHEMA_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA_ARTIFACT: &str = "docs/api/prognost.schema.json";

    /// Fails when the committed schema no longer matches the types, so a
    /// new request cannot be added without the published schema following
    /// it.
    #[test]
    fn generated_schema_artifact_is_current() {
        let actual = format!(
            "{}\n",
            serde_json::to_string_pretty(&schema_document()).unwrap()
        );
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(SCHEMA_ARTIFACT);
        if std::env::var_os("PROGNOST_UPDATE_API_SCHEMA").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &actual).unwrap();
            return;
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(
            expected, actual,
            "schema artifact is stale; run PROGNOST_UPDATE_API_SCHEMA=1 cargo test"
        );
    }

    #[test]
    fn request_round_trips_through_json() {
        let req: Request = serde_json::from_str(r#"{"type":"origin","scope":"staged"}"#).unwrap();
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["type"], "origin");
        assert_eq!(json["scope"], "staged");
    }

    #[test]
    fn failure_is_carried_as_an_error() {
        let e = Envelope::wrap(
            "cli:origin",
            Response::Error {
                message: "no".into(),
            },
        );
        let json = serde_json::to_value(&e).unwrap();
        assert!(json.get("result").is_none());
        assert_eq!(json["error"]["code"], "request_failed");
    }
}
