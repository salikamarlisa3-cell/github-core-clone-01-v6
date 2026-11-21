//! The MCP endpoint at /mcp: JSON-RPC over streamable HTTP.
//!
//! Both protocol eras are served: the modern `server/discover` (the
//! client settles as "modern" and never handshakes) and the legacy
//! `initialize` flow. Six tools operate on the same store as REST, so an
//! object created here is the same object everywhere. Only `tools/call`
//! requires an Authorization header (HTTP 401 without one); malformed
//! JSON answers HTTP 400 with a -32700 parse error, unknown tools get
//! -32602, unknown methods -32601.

use crate::json::Json;
use crate::rest;
use crate::store::VIEWER_LOGIN;
use crate::{store, Request, Response};

const SERVER_NAME: &str = "forge-mcp";
const SERVER_VERSION: &str = "1.0.0";

/// The tool table, built once: `tools/list` answers must be identical
/// byte for byte across calls, so the serialized form is cached too.
fn tools() -> Vec<Json> {
    let schema = |props: Vec<(&str, &str)>, required: Vec<&str>| -> Json {
        let properties: Vec<(String, Json)> = props
            .into_iter()
            .map(|(name, kind)| {
                (
                    name.to_string(),
                    Json::obj(vec![("type", Json::Str(kind.to_string()))]),
                )
            })
            .collect();
        Json::obj(vec![
            ("type", Json::Str("object".to_string())),
            ("properties", Json::Obj(properties)),
            (
                "required",
                Json::Arr(required.into_iter().map(|r| Json::Str(r.to_string())).collect()),
            ),
        ])
    };
    vec![
        Json::obj(vec![
            ("name", Json::Str("create_repo".to_string())),
            ("description", Json::Str("Create a new repository for the authenticated user".to_string())),
            (
                "inputSchema",
                schema(
                    vec![("name", "string"), ("description", "string"), ("private", "boolean")],
                    vec!["name"],
                ),
            ),
        ]),
        Json::obj(vec![
            ("name", Json::Str("get_repo".to_string())),
            ("description", Json::Str("Get a repository by owner and name".to_string())),
            (
                "inputSchema",
                schema(vec![("owner", "string"), ("repo", "string")], vec!["owner", "repo"]),
            ),
        ]),
        Json::obj(vec![
            ("name", Json::Str("create_issue".to_string())),
            ("description", Json::Str("Create an issue in a repository".to_string())),
            (
                "inputSchema",
                schema(
                    vec![("owner", "string"), ("repo", "string"), ("title", "string"), ("body", "string")],
                    vec!["owner", "repo", "title"],
                ),
            ),
        ]),
        Json::obj(vec![
            ("name", Json::Str("list_issues".to_string())),
            ("description", Json::Str("List the issues of a repository".to_string())),
            (
                "inputSchema",
                schema(
                    vec![("owner", "string"), ("repo", "string"), ("state", "string")],
                    vec!["owner", "repo"],
                ),
            ),
        ]),
        Json::obj(vec![
            ("name", Json::Str("create_pull_request".to_string())),
            ("description", Json::Str("Create a pull request between two branches".to_string())),
            (
                "inputSchema",
                schema(
                    vec![
                        ("owner", "string"),
                        ("repo", "string"),
                        ("title", "string"),
                        ("body", "string"),
                        ("head", "string"),
                        ("base", "string"),
                    ],
                    vec!["owner", "repo", "title", "head", "base"],
                ),
            ),
        ]),
        Json::obj(vec![
            ("name", Json::Str("merge_pull_request".to_string())),
            ("description", Json::Str("Merge a pull request (squash)".to_string())),
            (
                "inputSchema",
                schema(
                    vec![("owner", "string"), ("repo", "string"), ("pull_number", "integer")],
                    vec!["owner", "repo", "pull_number"],
                ),
            ),
        ]),
    ]
}



// ------------------------------------------------------------ JSON-RPC core

fn rpc_result(id: &Json, result: Json) -> Json {
    Json::obj(vec![
        ("jsonrpc", Json::Str("2.0".to_string())),
        ("id", id.clone()),
        ("result", result),
    ])
}

fn rpc_error(id: &Json, code: i64, message: &str) -> Json {
    Json::obj(vec![
        ("jsonrpc", Json::Str("2.0".to_string())),
        ("id", id.clone()),
        (
            "error",
            Json::obj(vec![
                ("code", Json::Num(code as f64)),
                ("message", Json::Str(message.to_string())),
            ]),
        ),
    ])
}

fn tool_result(payload: Json) -> Json {
    Json::obj(vec![
        (
            "content",
            Json::Arr(vec![Json::obj(vec![
                ("type", Json::Str("text".to_string())),
                ("text", Json::Str(payload.to_string_lossy())),
            ])]),
        ),
        ("structuredContent", payload),
        ("isError", Json::Bool(false)),
    ])
}

fn tool_error(message: &str) -> Json {
    Json::obj(vec![
        (
            "content",
            Json::Arr(vec![Json::obj(vec![
                ("type", Json::Str("text".to_string())),
                ("text", Json::Str(message.to_string())),
            ])]),
        ),
        ("isError", Json::Bool(true)),
    ])
}

pub fn handle(req: &Request) -> Response {
    if req.method != "POST" {
        return Response::error(405, "Method Not Allowed");
    }
    let parsed = match Json::parse(&req.body) {
        Ok(v) => v,
        Err(_) => {
            return Response::json(
                400,
                rpc_error(&Json::Null, -32700, "Parse error: the body is not valid JSON"),
            );
        }
    };
    // The one hard auth rule of the agent surface: a tools/call with no
    // Authorization header is an HTTP 401, decided before any tool runs.
    let is_call = parsed.get("method").and_then(|m| m.as_str()) == Some("tools/call");
    if is_call && !req.authed() {
        let id = parsed.get("id").cloned().unwrap_or(Json::Null);
        return Response::json(
            401,
            rpc_error(&id, -32001, "Unauthorized: an Authorization header is required"),
        );
    }
    match parsed {
        Json::Arr(entries) => {
            let answers: Vec<Json> = entries
                .iter()
                .filter_map(|entry| handle_one(req, entry))
                .collect();
            Response::json(200, Json::Arr(answers))
        }
        entry => match handle_one(req, &entry) {
            Some(answer) => Response::json(200, answer),
            None => Response::bytes(202, "application/json", Vec::new()),
        },
    }
}

/// Handle one JSON-RPC message; None for notifications (no answer).
fn handle_one(req: &Request, message: &Json) -> Option<Json> {
    let method = message.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = message.get("id").cloned();
    let is_notification = id.is_none();
    let params = message.get("params").cloned().unwrap_or(Json::Null);
    let answer = |value: Json| -> Option<Json> {
        if is_notification {
            None
        } else {
            Some(value)
        }
    };
    let id_or_null = id.unwrap_or(Json::Null);
    match method {
        "server/discover" => answer(rpc_result(
            &id_or_null,
            Json::obj(vec![
                ("resultType", Json::Str("complete".to_string())),
                (
                    "supportedVersions",
                    Json::Arr(vec![
                        Json::Str("2026-07-28".to_string()),
                        Json::Str("2025-11-25".to_string()),
                        Json::Str("2025-06-18".to_string()),
                    ]),
                ),
                (
                    "capabilities",
                    Json::obj(vec![("tools", Json::obj(vec![("listChanged", Json::Bool(false))]))]),
                ),
                (
                    "serverInfo",
                    Json::obj(vec![
                        ("name", Json::Str(SERVER_NAME.to_string())),
                        ("version", Json::Str(SERVER_VERSION.to_string())),
                    ]),
                ),
            ]),
        )),
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("2025-11-25")
                .to_string();
            let known = ["2026-07-28", "2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
            let negotiated = if known.contains(&requested.as_str()) {
                requested
            } else {
                "2025-11-25".to_string()
            };
            answer(rpc_result(
                &id_or_null,
                Json::obj(vec![
                    ("protocolVersion", Json::Str(negotiated)),
                    (
                        "capabilities",
                        Json::obj(vec![("tools", Json::obj(vec![("listChanged", Json::Bool(false))]))]),
                    ),
                    (
                        "serverInfo",
                        Json::obj(vec![
                            ("name", Json::Str(SERVER_NAME.to_string())),
                            ("version", Json::Str(SERVER_VERSION.to_string())),
                        ]),
                    ),
                ]),
            ))
        }
        "ping" => answer(rpc_result(&id_or_null, Json::obj(vec![]))),
        "tools/list" => answer(rpc_result(
            &id_or_null,
            Json::obj(vec![("tools", Json::Arr(tools()))]),
        )),
        "tools/call" => Some(call_tool(&id_or_null, &params)),
        _ if method.starts_with("notifications/") => None,
        _ => answer(rpc_error(&id_or_null, -32601, &format!("Method not found: {method}"))),
    }
}

// ------------------------------------------------------------------- tools

fn arg_str(args: &Json, name: &str) -> Option<String> {
    args.get(name).and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn required_missing(tool: &Json, args: &Json) -> Vec<String> {
    let required = tool
        .get("inputSchema")
        .and_then(|s| s.get("required"))
        .and_then(|r| r.as_arr())
        .cloned()
        .unwrap_or_default();
    required
        .iter()
        .filter_map(|r| r.as_str())
        .filter(|name| args.get(name).is_none() || args.get(name).map(|v| v.is_null()).unwrap_or(false))
        .map(|s| s.to_string())
        .collect()
}

fn call_tool(id: &Json, params: &Json) -> Json {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Json::obj(vec![]));
    let tool = tools().into_iter().find(|t| t.get("name").and_then(|n| n.as_str()) == Some(name));
    let tool = match tool {
        Some(t) => t,
        None => return rpc_error(id, -32602, &format!("Unknown tool: {name}")),
    };
    let missing = required_missing(&tool, &args);
    if !missing.is_empty() {
        return rpc_error(id, -32602, &format!("Missing required arguments: {}", missing.join(", ")));
    }
    let result = match name {
        "create_repo" => tool_create_repo(&args),
        "get_repo" => tool_get_repo(&args),
        "create_issue" => tool_create_issue(&args),
        "list_issues" => tool_list_issues(&args),
        "create_pull_request" => tool_create_pull(&args),
        "merge_pull_request" => tool_merge_pull(&args),
        _ => return rpc_error(id, -32602, &format!("Unknown tool: {name}")),
    };
    rpc_result(id, result)
}

/// A tool's outcome as a tool result (never a protocol error).
fn wrap(outcome: Result<Json, String>) -> Json {
    match outcome {
        Ok(payload) => tool_result(payload),
        Err(message) => tool_error(&message),
    }
}

fn rest_message(resp: &Response) -> String {
    Json::parse(&resp.body)
        .ok()
        .and_then(|j| j.get("message").and_then(|m| m.as_str()).map(|s| s.to_string()))
        .unwrap_or_else(|| "the operation failed".to_string())
}

fn tool_create_repo(args: &Json) -> Json {
    let name = arg_str(args, "name").unwrap_or_default();
    let description = arg_str(args, "description").unwrap_or_default();
    let private = args.get("private").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut guard = store().write().unwrap();
    if guard.repo(VIEWER_LOGIN, &name).is_some() {
        return tool_error("Repository creation failed: name already exists on this account");
    }
    let repo = guard.create_repo(VIEWER_LOGIN, &name, &description, private);
    tool_result(rest::repo_json(repo))
}

fn tool_get_repo(args: &Json) -> Json {
    let owner = arg_str(args, "owner").unwrap_or_default();
    let repo_name = arg_str(args, "repo").unwrap_or_default();
    let guard = store().read().unwrap();
    match guard.repo(&owner, &repo_name) {
        Some(repo) => tool_result(rest::repo_json(repo)),
        None => tool_error("Not Found"),
    }
}

fn tool_create_issue(args: &Json) -> Json {
    let owner = arg_str(args, "owner").unwrap_or_default();
    let repo_name = arg_str(args, "repo").unwrap_or_default();
    let title = arg_str(args, "title").unwrap_or_default();
    let body = arg_str(args, "body").unwrap_or_default();
    let mut guard = store().write().unwrap();
    let outcome = rest::do_create_issue(&mut guard, &owner, &repo_name, &title, &body, &[])
        .map(|(_, json)| json)
        .map_err(|resp| rest_message(&resp));
    wrap(outcome)
}

fn tool_list_issues(args: &Json) -> Json {
    let owner = arg_str(args, "owner").unwrap_or_default();
    let repo_name = arg_str(args, "repo").unwrap_or_default();
    let state = arg_str(args, "state").unwrap_or_else(|| "open".to_string());
    let guard = store().read().unwrap();
    match guard.repo(&owner, &repo_name) {
        Some(repo) => {
            let issues: Vec<Json> = repo
                .issues
                .iter()
                .filter(|i| state == "all" || i.state == state)
                .map(|i| rest::issue_json(repo, i))
                .collect();
            tool_result(Json::Arr(issues))
        }
        None => tool_error("Not Found"),
    }
}

fn tool_create_pull(args: &Json) -> Json {
    let owner = arg_str(args, "owner").unwrap_or_default();
    let repo_name = arg_str(args, "repo").unwrap_or_default();
    let title = arg_str(args, "title").unwrap_or_default();
    let body = arg_str(args, "body").unwrap_or_default();
    let head = arg_str(args, "head").unwrap_or_default();
    let base = arg_str(args, "base").unwrap_or_default();
    let mut guard = store().write().unwrap();
    let outcome = rest::do_create_pull(&mut guard, &owner, &repo_name, &title, &body, &head, &base)
        .map(|(_, json)| json)
        .map_err(|resp| rest_message(&resp));
    wrap(outcome)
}

fn tool_merge_pull(args: &Json) -> Json {
    let owner = arg_str(args, "owner").unwrap_or_default();
    let repo_name = arg_str(args, "repo").unwrap_or_default();
    let number = args.get("pull_number").and_then(|v| v.as_i64()).unwrap_or(0);
    let mut guard = store().write().unwrap();
    match rest::do_merge_pull(&mut guard, &owner, &repo_name, number, "squash") {
        Ok(_sha) => {
            let rendered = match guard.repo(&owner, &repo_name) {
                Some(repo) => repo.pull(number).map(|p| rest::pull_json(repo, p)),
                None => None,
            };
            match rendered {
                Some(json) => tool_result(json),
                None => tool_error("Not Found"),
            }
        }
        Err(resp) => tool_error(&rest_message(&resp)),
    }
}
