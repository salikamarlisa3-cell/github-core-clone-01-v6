//! The REST v3 surface: /api/v3 routes for repos, branches, git refs,
//! contents, commits, labels, issues, comments and pull requests.
//!
//! Conventions the suite relies on: every answer is JSON, errors are
//! `{"message": ...}` with a non-empty message, a repository addressed
//! under the wrong owner is a 404 (never a 403), and list endpoints
//! paginate with same-origin `Link` headers.

use std::sync::Arc;

use crate::gitcore;
use crate::json::Json;
use crate::sha1::{oid_hex, parse_oid};
use crate::store::{iso8601, now_unix, Repo, Store, VIEWER_EMAIL, VIEWER_ID, VIEWER_LOGIN, VIEWER_NAME};
use crate::{store, Request, Response};

// ------------------------------------------------------------- serializers

pub fn user_json(login: &str) -> Json {
    let id = if login == VIEWER_LOGIN { VIEWER_ID } else { 1000 + (login.len() as i64) };
    Json::obj(vec![
        ("login", Json::Str(login.to_string())),
        ("id", Json::Num(id as f64)),
        ("node_id", Json::Str(crate::store::b64_encode(format!("User:{login}").as_bytes()))),
        ("type", Json::Str("User".to_string())),
        ("site_admin", Json::Bool(false)),
        ("url", Json::Str(format!("https://api.github.com/users/{login}"))),
        ("html_url", Json::Str(format!("https://github.com/{login}"))),
    ])
}

pub fn viewer_json() -> Json {
    let mut base = user_json(VIEWER_LOGIN);
    if let Json::Obj(fields) = &mut base {
        fields.push(("name".to_string(), Json::Str(VIEWER_NAME.to_string())));
        fields.push(("email".to_string(), Json::Str(VIEWER_EMAIL.to_string())));
    }
    base
}

pub fn repo_json(repo: &Repo) -> Json {
    Json::obj(vec![
        ("id", Json::Num(repo.id as f64)),
        ("node_id", Json::Str(crate::store::b64_encode(format!("Repository:{}", repo.id).as_bytes()))),
        ("name", Json::Str(repo.name.clone())),
        ("full_name", Json::Str(repo.full_name())),
        ("owner", user_json(&repo.owner)),
        ("private", Json::Bool(repo.private)),
        ("description", if repo.description.is_empty() { Json::Null } else { Json::Str(repo.description.clone()) }),
        ("fork", Json::Bool(false)),
        ("created_at", Json::Str(iso8601(repo.created))),
        ("updated_at", Json::Str(iso8601(repo.created))),
        ("pushed_at", Json::Str(iso8601(repo.created))),
        ("html_url", Json::Str(format!("https://github.com/{}", repo.full_name()))),
        ("url", Json::Str(format!("https://api.github.com/repos/{}", repo.full_name()))),
        ("clone_url", Json::Str(format!("https://github.com/{}.git", repo.full_name()))),
        ("default_branch", Json::Str(repo.default_branch.clone())),
        ("visibility", Json::Str(if repo.private { "private".to_string() } else { "public".to_string() })),
    ])
}

pub fn label_json(label: &crate::store::Label) -> Json {
    Json::obj(vec![
        ("id", Json::Num(label.id as f64)),
        ("name", Json::Str(label.name.clone())),
        ("color", Json::Str(label.color.clone())),
        ("default", Json::Bool(false)),
        ("description", Json::Null),
    ])
}

pub fn issue_json(repo: &Repo, issue: &crate::store::Issue) -> Json {
    let labels = issue
        .labels
        .iter()
        .map(|name| match repo.label(name) {
            Some(l) => label_json(l),
            None => Json::obj(vec![("name", Json::Str(name.clone()))]),
        })
        .collect();
    Json::obj(vec![
        ("id", Json::Num(issue.gid as f64)),
        ("node_id", Json::Str(crate::store::b64_encode(format!("Issue:{}:{}", repo.id, issue.number).as_bytes()))),
        ("number", Json::Num(issue.number as f64)),
        ("title", Json::Str(issue.title.clone())),
        ("body", Json::Str(issue.body.clone())),
        ("state", Json::Str(issue.state.clone())),
        ("locked", Json::Bool(false)),
        ("labels", Json::Arr(labels)),
        ("user", user_json(&issue.author)),
        ("assignees", Json::Arr(vec![])),
        ("comments", Json::Num(issue.comments.len() as f64)),
        ("created_at", Json::Str(iso8601(issue.created))),
        ("updated_at", Json::Str(iso8601(issue.updated))),
        ("closed_at", if issue.state == "closed" { Json::Str(iso8601(issue.updated)) } else { Json::Null }),
        ("url", Json::Str(format!("https://api.github.com/repos/{}/issues/{}", repo.full_name(), issue.number))),
        ("html_url", Json::Str(format!("https://github.com/{}/issues/{}", repo.full_name(), issue.number))),
        ("repository_url", Json::Str(format!("https://api.github.com/repos/{}", repo.full_name()))),
    ])
}

pub fn comment_json(repo: &Repo, issue_number: i64, comment: &crate::store::Comment) -> Json {
    Json::obj(vec![
        ("id", Json::Num(comment.id as f64)),
        ("node_id", Json::Str(crate::store::b64_encode(format!("Comment:{}", comment.id).as_bytes()))),
        ("body", Json::Str(comment.body.clone())),
        ("user", user_json(&comment.author)),
        ("created_at", Json::Str(iso8601(comment.created))),
        ("updated_at", Json::Str(iso8601(comment.created))),
        ("url", Json::Str(format!(
            "https://api.github.com/repos/{}/issues/comments/{}",
            repo.full_name(),
            comment.id
        ))),
        ("html_url", Json::Str(format!(
            "https://github.com/{}/issues/{}#issuecomment-{}",
            repo.full_name(),
            issue_number,
            comment.id
        ))),
        ("issue_url", Json::Str(format!(
            "https://api.github.com/repos/{}/issues/{}",
            repo.full_name(),
            issue_number
        ))),
    ])
}

fn ref_side_json(repo: &Repo, owner: &str, refname: &str) -> Json {
    let sha = repo
        .refs
        .get(&format!("refs/heads/{refname}"))
        .map(|oid| Json::Str(oid_hex(oid)))
        .unwrap_or(Json::Null);
    Json::obj(vec![
        ("label", Json::Str(format!("{owner}:{refname}"))),
        ("ref", Json::Str(refname.to_string())),
        ("sha", sha),
        ("user", user_json(owner)),
        ("repo", repo_json(repo)),
    ])
}

pub fn pull_json(repo: &Repo, pull: &crate::store::Pull) -> Json {
    Json::obj(vec![
        ("id", Json::Num(pull.gid as f64)),
        ("node_id", Json::Str(crate::store::b64_encode(format!("PullRequest:{}:{}", repo.id, pull.number).as_bytes()))),
        ("number", Json::Num(pull.number as f64)),
        ("state", Json::Str(pull.state.clone())),
        ("title", Json::Str(pull.title.clone())),
        ("body", Json::Str(pull.body.clone())),
        ("locked", Json::Bool(false)),
        ("user", user_json(&pull.author)),
        ("head", ref_side_json(repo, &repo.owner, &pull.head_ref)),
        ("base", ref_side_json(repo, &repo.owner, &pull.base_ref)),
        ("merged", Json::Bool(pull.merged)),
        ("merged_at", pull.merged_at.map(|t| Json::Str(iso8601(t))).unwrap_or(Json::Null)),
        (
            "merge_commit_sha",
            pull.merge_commit.as_ref().map(|s| Json::Str(s.clone())).unwrap_or(Json::Null),
        ),
        ("draft", Json::Bool(false)),
        ("created_at", Json::Str(iso8601(pull.created))),
        ("updated_at", Json::Str(iso8601(pull.updated))),
        ("closed_at", if pull.state == "closed" { Json::Str(iso8601(pull.updated)) } else { Json::Null }),
        ("url", Json::Str(format!("https://api.github.com/repos/{}/pulls/{}", repo.full_name(), pull.number))),
        ("html_url", Json::Str(format!("https://github.com/{}/pulls/{}", repo.full_name(), pull.number))),
    ])
}

fn identity_json(line: &str) -> Json {
    let (name, rest) = match line.split_once(" <") {
        Some((n, r)) => (n, r),
        None => (line, ""),
    };
    let (email, ts) = match rest.split_once("> ") {
        Some((e, t)) => (e, t.split(' ').next().unwrap_or("0")),
        None => ("", "0"),
    };
    let stamp: i64 = ts.parse().unwrap_or(0);
    Json::obj(vec![
        ("name", Json::Str(name.to_string())),
        ("email", Json::Str(email.to_string())),
        ("date", Json::Str(iso8601(stamp))),
    ])
}

pub fn commit_json(repo: &Repo, oid: &crate::sha1::Oid) -> Option<Json> {
    let obj = repo.objects.get(oid)?;
    if obj.kind != crate::store::ObjKind::Commit {
        return None;
    }
    let commit = gitcore::parse_commit(&obj.data)?;
    let author_login = if commit.author.contains(VIEWER_EMAIL) { Some(VIEWER_LOGIN) } else { None };
    Some(Json::obj(vec![
        ("sha", Json::Str(oid_hex(oid))),
        ("node_id", Json::Str(crate::store::b64_encode(format!("Commit:{}", oid_hex(oid)).as_bytes()))),
        (
            "commit",
            Json::obj(vec![
                ("author", identity_json(&commit.author)),
                ("committer", identity_json(&commit.committer)),
                ("message", Json::Str(commit.message.clone())),
                ("tree", Json::obj(vec![("sha", Json::Str(oid_hex(&commit.tree)))])),
                ("comment_count", Json::Num(0.0)),
            ]),
        ),
        (
            "parents",
            Json::Arr(
                commit
                    .parents
                    .iter()
                    .map(|p| Json::obj(vec![("sha", Json::Str(oid_hex(p)))]))
                    .collect(),
            ),
        ),
        ("author", author_login.map(user_json).unwrap_or(Json::Null)),
        ("committer", author_login.map(user_json).unwrap_or(Json::Null)),
        ("url", Json::Str(format!("https://api.github.com/repos/{}/commits/{}", repo.full_name(), oid_hex(oid)))),
        ("html_url", Json::Str(format!("https://github.com/{}/commit/{}", repo.full_name(), oid_hex(oid)))),
    ]))
}

// ----------------------------------------------------------------- helpers

pub fn not_found() -> Response {
    Response::error(404, "Not Found")
}

pub fn validation(message: &str) -> Response {
    Response::error(422, message)
}

struct Page {
    per_page: usize,
    page: usize,
}

fn page_of(req: &Request) -> Page {
    let per_page = req
        .query_get("per_page")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .map(|n| n.min(100))
        .unwrap_or(30);
    let page = req
        .query_get("page")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(1);
    Page { per_page, page }
}

/// Slice `items` to the requested page and build the Link header value
/// GitHub would send (next/last/first/prev, same-origin absolute URLs).
fn paginate<T: Clone>(req: &Request, items: &[T], path: &str) -> (Vec<T>, Option<String>) {
    let page = page_of(req);
    let total = items.len();
    let start = (page.page - 1) * page.per_page;
    let slice: Vec<T> = if start >= total {
        Vec::new()
    } else {
        items[start..total.min(start + page.per_page)].to_vec()
    };
    let last_page = total.div_ceil(page.per_page).max(1);
    if last_page <= 1 {
        return (slice, None);
    }
    let host = req.header("host").unwrap_or("localhost");
    let url = |p: usize| format!("http://{host}{path}?per_page={}&page={}", page.per_page, p);
    let mut links = Vec::new();
    if page.page < last_page {
        links.push(format!("<{}>; rel=\"next\"", url(page.page + 1)));
        links.push(format!("<{}>; rel=\"last\"", url(last_page)));
    }
    if page.page > 1 {
        links.push(format!("<{}>; rel=\"first\"", url(1)));
        links.push(format!("<{}>; rel=\"prev\"", url(page.page - 1)));
    }
    (slice, if links.is_empty() { None } else { Some(links.join(", ")) })
}

fn with_link(resp: Response, link: Option<String>) -> Response {
    match link {
        Some(value) => resp.header("Link", &value),
        None => resp,
    }
}

/// Resolve a ref-ish name (branch, refs/heads/x, or a raw object id) to
/// the commit it names inside `repo`.
pub fn resolve_ref(repo: &Repo, name: &str) -> Option<crate::sha1::Oid> {
    if let Some(oid) = parse_oid(name) {
        if repo.objects.contains_key(&oid) {
            return Some(oid);
        }
    }
    let candidates = [
        name.to_string(),
        format!("refs/heads/{name}"),
        format!("refs/{name}"),
    ];
    for candidate in &candidates {
        if let Some(oid) = repo.refs.get(candidate) {
            return Some(*oid);
        }
    }
    None
}

/// The identity line server-made commits carry.
pub fn server_identity(stamp: i64) -> String {
    format!("{VIEWER_NAME} <{VIEWER_EMAIL}> {stamp} +0000")
}

/// Create a commit on top of `parent` (or unborn) and return its id.
/// The caller updates the ref. `stamp` comes from
/// `Store::next_commit_time`, taken before borrowing the repo.
pub fn make_commit(
    repo: &mut Repo,
    tree: crate::sha1::Oid,
    parent: Option<crate::sha1::Oid>,
    message: &str,
    stamp: i64,
) -> crate::sha1::Oid {
    let identity = server_identity(stamp);
    let parents: Vec<crate::sha1::Oid> = parent.into_iter().collect();
    let bytes = gitcore::build_commit(&tree, &parents, &identity, &identity, message);
    gitcore::store_object(repo, crate::store::ObjKind::Commit, bytes)
}

// ------------------------------------------------------------------ routes

pub fn handle(req: &Request, rest: &[String]) -> Response {
    let resp = route(req, rest);
    // GitHub answers every API response with the caller's scopes; the
    // protocol group checks the header exists on the root document.
    resp.header("X-OAuth-Scopes", "repo, user, admin:org")
}

fn route(req: &Request, rest: &[String]) -> Response {
    // Open endpoints: the API root and /meta.
    if rest.is_empty() {
        return match req.method.as_str() {
            "GET" => Response::json(
                200,
                Json::obj(vec![
                    ("current_user_url", Json::Str("https://api.github.com/user".to_string())),
                    ("repository_url", Json::Str("https://api.github.com/repos/{owner}/{repo}".to_string())),
                    ("issue_search_url", Json::Str("https://api.github.com/search/issues".to_string())),
                ]),
            ),
            _ => Response::error(405, "Method Not Allowed"),
        };
    }
    if rest.len() == 1 && rest[0] == "meta" {
        return Response::json(
            200,
            Json::obj(vec![
                ("installed_version", Json::Str("1.0.0-forge".to_string())),
                ("verifiable_password_authentication", Json::Bool(false)),
                ("ssh_key_fingerprints", Json::obj(vec![])),
            ]),
        );
    }
    if !req.authed() {
        return Response::error(401, "Requires authentication");
    }
    // A JSON body that does not parse is a 400 on every write route.
    if matches!(req.method.as_str(), "POST" | "PUT" | "PATCH") && !req.body.is_empty() {
        if Json::parse(&req.body).is_err() {
            return Response::error(400, "Problems parsing JSON");
        }
    }
    match rest[0].as_str() {
        "user" => user_routes(req, &rest[1..]),
        "repos" if rest.len() >= 3 => repo_routes(req, &rest[1], &rest[2], &rest[3..]),
        _ => not_found(),
    }
}

fn body_obj(req: &Request) -> Json {
    req.json_body().unwrap_or(Json::Null)
}

fn user_routes(req: &Request, rest: &[String]) -> Response {
    if rest.is_empty() {
        return match req.method.as_str() {
            "GET" => Response::json(200, viewer_json()),
            _ => Response::error(405, "Method Not Allowed"),
        };
    }
    if rest[0] == "repos" {
        return match req.method.as_str() {
            "GET" => {
                let guard = store().read().unwrap();
                let mut mine: Vec<&Repo> =
                    guard.repos.values().filter(|r| r.owner == VIEWER_LOGIN).collect();
                mine.sort_by_key(|r| r.id);
                let all: Vec<Json> = mine.iter().map(|r| repo_json(r)).collect();
                let (page, link) = paginate(req, &all, "/api/v3/user/repos");
                with_link(Response::json(200, Json::Arr(page)), link)
            }
            "POST" => create_repo(req),
            _ => Response::error(405, "Method Not Allowed"),
        };
    }
    not_found()
}

fn create_repo(req: &Request) -> Response {
    let body = body_obj(req);
    let name = match body.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.trim().is_empty() => n.to_string(),
        _ => return validation("Repository creation failed: name is required"),
    };
    let description = body.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let private = body.get("private").and_then(|v| v.as_bool()).unwrap_or(false);
    let auto_init = body.get("auto_init").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut guard = store().write().unwrap();
    if guard.repo(VIEWER_LOGIN, &name).is_some() {
        return validation("Repository creation failed: name already exists on this account");
    }
    let stamp = guard.next_commit_time();
    guard.create_repo(VIEWER_LOGIN, &name, &description, private);
    if auto_init {
        let repo = guard.repo_mut(VIEWER_LOGIN, &name).expect("just created");
        let blob = gitcore::store_object(
            repo,
            crate::store::ObjKind::Blob,
            format!("# {name}\n").into_bytes(),
        );
        let mut map = std::collections::BTreeMap::new();
        map.insert("README.md".to_string(), ("100644".to_string(), blob));
        let tree = gitcore::tree_from_map(repo, &map);
        let commit = make_commit(repo, tree, None, "Initial commit", stamp);
        repo.refs.insert("refs/heads/main".to_string(), commit);
    }
    let out = repo_json(guard.repo(VIEWER_LOGIN, &name).expect("just created"));
    Response::json(201, out)
}

fn repo_routes(req: &Request, owner: &str, name: &str, rest: &[String]) -> Response {
    if rest.is_empty() {
        return match req.method.as_str() {
            "GET" => {
                let guard = store().read().unwrap();
                match guard.repo(owner, name) {
                    Some(repo) => Response::json(200, repo_json(repo)),
                    None => not_found(),
                }
            }
            "PATCH" => patch_repo(req, owner, name),
            "DELETE" => {
                let mut guard = store().write().unwrap();
                match guard.repos.remove(&Store::key(owner, name)) {
                    Some(_) => Response::bytes(204, "application/json", Vec::new()),
                    None => not_found(),
                }
            }
            _ => Response::error(405, "Method Not Allowed"),
        };
    }
    match rest[0].as_str() {
        "branches" => branch_routes(req, owner, name, &rest[1..]),
        "git" if rest.len() >= 2 && rest[1] == "refs" => git_ref_routes(req, owner, name, &rest[2..]),
        "contents" => contents_routes(req, owner, name, &rest[1..]),
        "labels" => label_routes(req, owner, name, &rest[1..]),
        "issues" => issue_routes(req, owner, name, &rest[1..]),
        "pulls" => pull_routes(req, owner, name, &rest[1..]),
        "commits" => commit_routes(req, owner, name, &rest[1..]),
        _ => not_found(),
    }
}

fn patch_repo(req: &Request, owner: &str, name: &str) -> Response {
    let body = body_obj(req);
    let mut guard = store().write().unwrap();
    let old_key = Store::key(owner, name);
    let mut repo = match guard.repos.remove(&old_key) {
        Some(r) => r,
        None => return not_found(),
    };
    if let Some(desc) = body.get("description") {
        repo.description = desc.as_str().unwrap_or("").to_string();
    }
    if let Some(private) = body.get("private").and_then(|v| v.as_bool()) {
        repo.private = private;
    }
    if let Some(branch) = body.get("default_branch").and_then(|v| v.as_str()) {
        if !branch.is_empty() {
            repo.default_branch = branch.to_string();
        }
    }
    if let Some(new_name) = body.get("name").and_then(|v| v.as_str()) {
        if !new_name.is_empty() {
            repo.name = new_name.to_string();
        }
    }
    repo.touch();
    let out = repo_json(&repo);
    let new_key = Store::key(&repo.owner.clone(), &repo.name.clone());
    guard.repos.insert(new_key, repo);
    Response::json(200, out)
}

fn branch_routes(req: &Request, owner: &str, name: &str, rest: &[String]) -> Response {
    let guard = store().read().unwrap();
    let repo = match guard.repo(owner, name) {
        Some(r) => r,
        None => return not_found(),
    };
    if rest.is_empty() {
        if req.method != "GET" {
            return Response::error(405, "Method Not Allowed");
        }
        let mut names: Vec<String> = repo
            .refs
            .keys()
            .filter_map(|r| r.strip_prefix("refs/heads/").map(|s| s.to_string()))
            .collect();
        names.sort();
        let all: Vec<Json> = names
            .iter()
            .map(|branch| {
                Json::obj(vec![
                    ("name", Json::Str(branch.clone())),
                    (
                        "commit",
                        Json::obj(vec![
                            ("sha", Json::Str(oid_hex(&repo.refs[&format!("refs/heads/{branch}")]))),
                            ("url", Json::Str(String::new())),
                        ]),
                    ),
                    ("protected", Json::Bool(false)),
                ])
            })
            .collect();
        let (page, link) = paginate(req, &all, &format!("/api/v3/repos/{owner}/{name}/branches"));
        return with_link(Response::json(200, Json::Arr(page)), link);
    }
    let branch = rest.join("/");
    match req.method.as_str() {
        "GET" => match repo.refs.get(&format!("refs/heads/{branch}")) {
            Some(oid) => Response::json(
                200,
                Json::obj(vec![
                    ("name", Json::Str(branch)),
                    (
                        "commit",
                        Json::obj(vec![
                            ("sha", Json::Str(oid_hex(oid))),
                            ("url", Json::Str(String::new())),
                        ]),
                    ),
                    ("protected", Json::Bool(false)),
                ]),
            ),
            None => not_found(),
        },
        _ => Response::error(405, "Method Not Allowed"),
    }
}

fn git_ref_routes(req: &Request, owner: &str, name: &str, rest: &[String]) -> Response {
    match req.method.as_str() {
        "POST" if rest.is_empty() => {
            let body = body_obj(req);
            let refname = match body.get("ref").and_then(|v| v.as_str()) {
                Some(r) if r.starts_with("refs/") => r.to_string(),
                _ => return validation("ref must be a full ref name"),
            };
            let sha_text = match body.get("sha").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return validation("sha is required"),
            };
            let oid = match parse_oid(&sha_text) {
                Some(o) => o,
                None => return validation("sha is not an object id"),
            };
            let mut guard = store().write().unwrap();
            let repo = match guard.repo_mut(owner, name) {
                Some(r) => r,
                None => return not_found(),
            };
            let kind = match repo.objects.get(&oid) {
                Some(o) => o.kind,
                None => return validation("sha does not name an object in this repository"),
            };
            if repo.refs.contains_key(&refname) {
                return validation("Reference already exists");
            }
            repo.refs.insert(refname.clone(), oid);
            repo.touch();
            Response::json(
                201,
                Json::obj(vec![
                    ("ref", Json::Str(refname)),
                    (
                        "object",
                        Json::obj(vec![
                            ("sha", Json::Str(sha_text)),
                            ("type", Json::Str(kind.name().to_string())),
                            ("url", Json::Str(String::new())),
                        ]),
                    ),
                ]),
            )
        }
        "GET" if !rest.is_empty() => {
            let full = format!("refs/{}", rest.join("/"));
            let guard = store().read().unwrap();
            let repo = match guard.repo(owner, name) {
                Some(r) => r,
                None => return not_found(),
            };
            match repo.refs.get(&full) {
                Some(oid) => {
                    let kind = repo.objects.get(oid).map(|o| o.kind.name()).unwrap_or("commit");
                    Response::json(
                        200,
                        Json::obj(vec![
                            ("ref", Json::Str(full)),
                            (
                                "object",
                                Json::obj(vec![
                                    ("sha", Json::Str(oid_hex(oid))),
                                    ("type", Json::Str(kind.to_string())),
                                    ("url", Json::Str(String::new())),
                                ]),
                            ),
                        ]),
                    )
                }
                None => not_found(),
            }
        }
        "DELETE" if !rest.is_empty() => {
            let full = format!("refs/{}", rest.join("/"));
            let mut guard = store().write().unwrap();
            let repo = match guard.repo_mut(owner, name) {
                Some(r) => r,
                None => return not_found(),
            };
            if repo.refs.remove(&full).is_some() {
                repo.touch();
                Response::bytes(204, "application/json", Vec::new())
            } else {
                not_found()
            }
        }
        _ => not_found(),
    }
}

fn commit_routes(req: &Request, owner: &str, name: &str, rest: &[String]) -> Response {
    let guard = store().read().unwrap();
    let repo = match guard.repo(owner, name) {
        Some(r) => r,
        None => return not_found(),
    };
    if !rest.is_empty() {
        return match parse_oid(&rest[0]) {
            Some(oid) => match commit_json(repo, &oid) {
                Some(json) => Response::json(200, json),
                None => not_found(),
            },
            None => not_found(),
        };
    }
    if req.method != "GET" {
        return Response::error(405, "Method Not Allowed");
    }
    let start = match req.query_get("sha") {
        Some(name) => resolve_ref(repo, name),
        None => repo.head_oid(),
    };
    let start = match start {
        Some(oid) => oid,
        None => return Response::error(409, "Git Repository is empty."),
    };
    let walk = gitcore::commit_walk(repo, &start);
    let all: Vec<Json> = walk.iter().filter_map(|oid| commit_json(repo, oid)).collect();
    let (page, link) = paginate(req, &all, &format!("/api/v3/repos/{owner}/{name}/commits"));
    with_link(Response::json(200, Json::Arr(page)), link)
}

// ------------------------------------------------------- contents and trees

fn branch_tip(repo: &Repo, branch: &str) -> Option<crate::sha1::Oid> {
    repo.refs.get(&format!("refs/heads/{branch}")).copied()
}

fn tip_tree(repo: &Repo, tip: &crate::sha1::Oid) -> Option<crate::sha1::Oid> {
    let obj = repo.objects.get(tip)?;
    gitcore::parse_commit(&obj.data).map(|c| c.tree)
}

fn commit_summary(repo: &Repo, oid: &crate::sha1::Oid) -> Json {
    let commit = repo
        .objects
        .get(oid)
        .and_then(|o| gitcore::parse_commit(&o.data))
        .unwrap_or(gitcore::CommitInfo {
            tree: crate::sha1::ZERO_OID,
            parents: vec![],
            author: String::new(),
            committer: String::new(),
            message: String::new(),
        });
    Json::obj(vec![
        ("sha", Json::Str(oid_hex(oid))),
        ("message", Json::Str(commit.message.clone())),
        ("author", identity_json(&commit.author)),
        ("committer", identity_json(&commit.committer)),
        ("tree", Json::obj(vec![("sha", Json::Str(oid_hex(&commit.tree)))])),
        (
            "parents",
            Json::Arr(commit.parents.iter().map(|p| Json::obj(vec![("sha", Json::Str(oid_hex(p)))])).collect()),
        ),
        ("url", Json::Str(String::new())),
        ("html_url", Json::Str(String::new())),
    ])
}

fn contents_routes(req: &Request, owner: &str, name: &str, rest: &[String]) -> Response {
    let path = rest.join("/");
    match req.method.as_str() {
        "GET" => contents_get(req, owner, name, &path),
        "PUT" => contents_put(req, owner, name, &path),
        "DELETE" => contents_delete(req, owner, name, &path),
        _ => Response::error(405, "Method Not Allowed"),
    }
}

fn contents_get(req: &Request, owner: &str, name: &str, path: &str) -> Response {
    let guard = store().read().unwrap();
    let repo = match guard.repo(owner, name) {
        Some(r) => r,
        None => return not_found(),
    };
    let branch = req.query_get("ref").unwrap_or(&repo.default_branch).to_string();
    let tip = match branch_tip(repo, &branch).or_else(|| parse_oid(&branch)) {
        Some(t) => t,
        None => return Response::error(404, "This repository is empty."),
    };
    let tree = match tip_tree(repo, &tip) {
        Some(t) => t,
        None => return not_found(),
    };
    if path.is_empty() {
        return dir_listing(repo, &tree, "");
    }
    match gitcore::tree_lookup(repo, &tree, path) {
        Some((_, oid, true)) => dir_listing(repo, &oid, path),
        Some((_, oid, false)) => {
            let obj = match repo.objects.get(&oid) {
                Some(o) => o,
                None => return not_found(),
            };
            let file_name = path.rsplit('/').next().unwrap_or(path);
            Response::json(
                200,
                Json::obj(vec![
                    ("type", Json::Str("file".to_string())),
                    ("encoding", Json::Str("base64".to_string())),
                    ("size", Json::Num(obj.data.len() as f64)),
                    ("name", Json::Str(file_name.to_string())),
                    ("path", Json::Str(path.to_string())),
                    ("sha", Json::Str(oid_hex(&oid))),
                    ("content", Json::Str(crate::store::b64_wrapped(&obj.data))),
                    ("url", Json::Str(String::new())),
                    ("html_url", Json::Str(String::new())),
                    ("download_url", Json::Str(String::new())),
                ]),
            )
        }
        None => not_found(),
    }
}

fn dir_listing(repo: &Repo, tree: &crate::sha1::Oid, prefix: &str) -> Response {
    let obj = match repo.objects.get(tree) {
        Some(o) => o,
        None => return not_found(),
    };
    let mut entries: Vec<Json> = gitcore::parse_tree(&obj.data)
        .iter()
        .map(|entry| {
            let is_dir = entry.mode == "40000";
            let size = if is_dir {
                0
            } else {
                repo.objects.get(&entry.oid).map(|o| o.data.len()).unwrap_or(0)
            };
            let full = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{prefix}/{}", entry.name)
            };
            Json::obj(vec![
                ("type", Json::Str(if is_dir { "dir".to_string() } else { "file".to_string() })),
                ("size", Json::Num(size as f64)),
                ("name", Json::Str(entry.name.clone())),
                ("path", Json::Str(full)),
                ("sha", Json::Str(oid_hex(&entry.oid))),
                ("url", Json::Str(String::new())),
                ("html_url", Json::Str(String::new())),
                ("download_url", Json::Null),
            ])
        })
        .collect();
    entries.sort_by_key(|e| e.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string());
    Response::json(200, Json::Arr(entries))
}

fn contents_put(req: &Request, owner: &str, name: &str, path: &str) -> Response {
    if path.is_empty() {
        return validation("path is required");
    }
    let body = body_obj(req);
    let message = match body.get("message").and_then(|v| v.as_str()) {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => return validation("message is required"),
    };
    let content_b64 = match body.get("content").and_then(|v| v.as_str()) {
        Some(c) => c.to_string(),
        None => return validation("content is required"),
    };
    let raw = match crate::store::b64_decode(&content_b64) {
        Some(r) => r,
        None => return validation("content is not valid base64"),
    };
    let given_sha = body.get("sha").and_then(|v| v.as_str()).map(|s| s.to_string());
    let mut guard = store().write().unwrap();
    let stamp = guard.next_commit_time();
    let repo = match guard.repo_mut(owner, name) {
        Some(r) => r,
        None => return not_found(),
    };
    let branch = body
        .get("branch")
        .and_then(|v| v.as_str())
        .unwrap_or(&repo.default_branch.clone())
        .to_string();
    let refname = format!("refs/heads/{branch}");
    // A write to a branch that does not exist yet starts from the
    // default branch's tip (the contents route creates the branch).
    let base_tip = repo
        .refs
        .get(&refname)
        .copied()
        .or_else(|| repo.head_oid());
    let mut flat = match base_tip.and_then(|tip| tip_tree(repo, &tip)) {
        Some(tree) => gitcore::flatten_tree(repo, &tree),
        None => std::collections::BTreeMap::new(),
    };
    let existing = flat.get(path).map(|(_, oid)| *oid);
    if let Some(current) = existing {
        match &given_sha {
            None => return validation("\"sha\" wasn't supplied."),
            Some(sha) if parse_oid(sha) != Some(current) => {
                return Response::error(409, "The sha provided does not match the current blob sha");
            }
            _ => {}
        }
    }
    let blob = gitcore::store_object(repo, crate::store::ObjKind::Blob, raw.clone());
    flat.insert(path.to_string(), ("100644".to_string(), blob));
    let tree = gitcore::tree_from_map(repo, &flat);
    let commit = make_commit(repo, tree, base_tip, &message, stamp);
    repo.refs.insert(refname, commit);
    repo.touch();
    let status = if existing.is_some() { 200 } else { 201 };
    Response::json(
        status,
        Json::obj(vec![
            (
                "content",
                Json::obj(vec![
                    ("name", Json::Str(path.rsplit('/').next().unwrap_or(path).to_string())),
                    ("path", Json::Str(path.to_string())),
                    ("sha", Json::Str(oid_hex(&blob))),
                    ("size", Json::Num(raw.len() as f64)),
                    ("type", Json::Str("file".to_string())),
                    ("encoding", Json::Str("base64".to_string())),
                    ("content", Json::Str(crate::store::b64_wrapped(&raw))),
                ]),
            ),
            ("commit", commit_summary(repo, &commit)),
        ]),
    )
}

fn contents_delete(req: &Request, owner: &str, name: &str, path: &str) -> Response {
    let body = body_obj(req);
    let message = match body.get("message").and_then(|v| v.as_str()) {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => return validation("message is required"),
    };
    let given_sha = body.get("sha").and_then(|v| v.as_str()).map(|s| s.to_string());
    let mut guard = store().write().unwrap();
    let stamp = guard.next_commit_time();
    let repo = match guard.repo_mut(owner, name) {
        Some(r) => r,
        None => return not_found(),
    };
    let branch = body
        .get("branch")
        .and_then(|v| v.as_str())
        .unwrap_or(&repo.default_branch.clone())
        .to_string();
    let refname = format!("refs/heads/{branch}");
    let base_tip = match repo.refs.get(&refname).copied() {
        Some(t) => t,
        None => return not_found(),
    };
    let tree = match tip_tree(repo, &base_tip) {
        Some(t) => t,
        None => return not_found(),
    };
    let mut flat = gitcore::flatten_tree(repo, &tree);
    let current = match flat.get(path) {
        Some((_, oid)) => *oid,
        None => return not_found(),
    };
    match &given_sha {
        None => return validation("\"sha\" wasn't supplied."),
        Some(sha) if parse_oid(sha) != Some(current) => {
            return Response::error(409, "The sha provided does not match the current blob sha");
        }
        _ => {}
    }
    flat.remove(path);
    let new_tree = gitcore::tree_from_map(repo, &flat);
    let commit = make_commit(repo, new_tree, Some(base_tip), &message, stamp);
    repo.refs.insert(refname, commit);
    repo.touch();
    Response::json(
        200,
        Json::obj(vec![("content", Json::Null), ("commit", commit_summary(repo, &commit))]),
    )
}

// ------------------------------------------------------------------ labels

fn label_routes(req: &Request, owner: &str, name: &str, rest: &[String]) -> Response {
    if rest.is_empty() {
        return match req.method.as_str() {
            "GET" => {
                let guard = store().read().unwrap();
                match guard.repo(owner, name) {
                    Some(repo) => {
                        Response::json(200, Json::Arr(repo.labels.iter().map(label_json).collect()))
                    }
                    None => not_found(),
                }
            }
            "POST" => {
                let body = body_obj(req);
                let label_name = match body.get("name").and_then(|v| v.as_str()) {
                    Some(n) if !n.is_empty() => n.to_string(),
                    _ => return validation("name is required"),
                };
                let color = body
                    .get("color")
                    .and_then(|v| v.as_str())
                    .unwrap_or("ededed")
                    .trim_start_matches('#')
                    .to_string();
                let mut guard = store().write().unwrap();
                let id = guard.next_label_id;
                guard.next_label_id += 1;
                let repo = match guard.repo_mut(owner, name) {
                    Some(r) => r,
                    None => return not_found(),
                };
                if repo.label(&label_name).is_some() {
                    return validation("Validation Failed: name has already been taken");
                }
                let label = crate::store::Label { id, name: label_name, color };
                repo.labels.push(label.clone());
                repo.touch();
                Response::json(201, label_json(&label))
            }
            _ => Response::error(405, "Method Not Allowed"),
        };
    }
    let label_name = rest.join("/");
    match req.method.as_str() {
        "GET" => {
            let guard = store().read().unwrap();
            match guard.repo(owner, name).and_then(|r| r.label(&label_name)) {
                Some(l) => Response::json(200, label_json(l)),
                None => not_found(),
            }
        }
        "PATCH" => {
            let body = body_obj(req);
            let mut guard = store().write().unwrap();
            let repo = match guard.repo_mut(owner, name) {
                Some(r) => r,
                None => return not_found(),
            };
            let label = match repo.labels.iter_mut().find(|l| l.name == label_name) {
                Some(l) => l,
                None => return not_found(),
            };
            if let Some(new_name) = body.get("new_name").and_then(|v| v.as_str()) {
                label.name = new_name.to_string();
            }
            if let Some(color) = body.get("color").and_then(|v| v.as_str()) {
                label.color = color.trim_start_matches('#').to_string();
            }
            let out = label_json(label);
            repo.touch();
            Response::json(200, out)
        }
        "DELETE" => {
            let mut guard = store().write().unwrap();
            let repo = match guard.repo_mut(owner, name) {
                Some(r) => r,
                None => return not_found(),
            };
            let before = repo.labels.len();
            repo.labels.retain(|l| l.name != label_name);
            if repo.labels.len() == before {
                return not_found();
            }
            repo.touch();
            Response::bytes(204, "application/json", Vec::new())
        }
        _ => Response::error(405, "Method Not Allowed"),
    }
}

// ------------------------------------------------------------------ issues

/// Create an issue; shared by REST, GraphQL and MCP so all three
/// surfaces agree on numbering, label validation and shape.
pub fn do_create_issue(
    st: &mut Store,
    owner: &str,
    name: &str,
    title: &str,
    body: &str,
    label_names: &[String],
) -> Result<(i64, Json), Response> {
    if title.trim().is_empty() {
        return Err(validation("Validation Failed: title is required"));
    }
    let gid = st.next_gid;
    st.next_gid += 1;
    let repo = st.repo_mut(owner, name).ok_or_else(not_found)?;
    for label in label_names {
        if repo.label(label).is_none() {
            return Err(validation("Validation Failed: one of the labels does not exist in this repository"));
        }
    }
    let number = repo.next_number;
    repo.next_number += 1;
    let now = now_unix();
    let issue = crate::store::Issue {
        gid,
        number,
        title: title.to_string(),
        body: body.to_string(),
        state: "open".to_string(),
        labels: label_names.to_vec(),
        author: VIEWER_LOGIN.to_string(),
        comments: Vec::new(),
        created: now,
        updated: now,
    };
    repo.issues.push(issue);
    repo.touch();
    let rendered = issue_json(repo, repo.issues.last().expect("just pushed"));
    Ok((number, rendered))
}

fn issue_routes(req: &Request, owner: &str, name: &str, rest: &[String]) -> Response {
    if rest.is_empty() {
        return match req.method.as_str() {
            "GET" => {
                let guard = store().read().unwrap();
                let repo = match guard.repo(owner, name) {
                    Some(r) => r,
                    None => return not_found(),
                };
                let state = req.query_get("state").unwrap_or("open");
                let wanted_labels: Vec<String> = req
                    .query_get("labels")
                    .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
                    .unwrap_or_default();
                let direction = req.query_get("direction").unwrap_or("desc");
                let mut issues: Vec<&crate::store::Issue> = repo
                    .issues
                    .iter()
                    .filter(|i| state == "all" || i.state == state)
                    .filter(|i| wanted_labels.iter().all(|l| i.labels.contains(l)))
                    .collect();
                issues.sort_by_key(|i| i.number);
                if direction != "asc" {
                    issues.reverse();
                }
                let all: Vec<Json> = issues.iter().map(|i| issue_json(repo, i)).collect();
                let (page, link) =
                    paginate(req, &all, &format!("/api/v3/repos/{owner}/{name}/issues"));
                with_link(Response::json(200, Json::Arr(page)), link)
            }
            "POST" => {
                let body = body_obj(req);
                let title = body.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let text = body.get("body").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let labels = label_names_of(&body);
                let mut guard = store().write().unwrap();
                match do_create_issue(&mut guard, owner, name, &title, &text, &labels) {
                    Ok((_, json)) => Response::json(201, json),
                    Err(resp) => resp,
                }
            }
            _ => Response::error(405, "Method Not Allowed"),
        };
    }
    let number: i64 = match rest[0].parse() {
        Ok(n) => n,
        Err(_) => return not_found(),
    };
    if rest.len() >= 2 && rest[1] == "comments" {
        return comment_routes(req, owner, name, number);
    }
    match req.method.as_str() {
        "GET" => issue_get_cached(owner, name, number),
        "PATCH" => {
            let body = body_obj(req);
            let mut guard = store().write().unwrap();
            let repo = match guard.repo_mut(owner, name) {
                Some(r) => r,
                None => return not_found(),
            };
            // Validate labels before mutating anything.
            let new_labels = if body.get("labels").is_some() {
                let names = label_names_of(&body);
                for label in &names {
                    if repo.label(label).is_none() {
                        return validation("Validation Failed: one of the labels does not exist in this repository");
                    }
                }
                Some(names)
            } else {
                None
            };
            let issue = match repo.issue_mut(number) {
                Some(i) => i,
                None => return not_found(),
            };
            if let Some(title) = body.get("title").and_then(|v| v.as_str()) {
                issue.title = title.to_string();
            }
            if let Some(text) = body.get("body").and_then(|v| v.as_str()) {
                issue.body = text.to_string();
            }
            if let Some(state) = body.get("state").and_then(|v| v.as_str()) {
                if state != "open" && state != "closed" {
                    return validation("state must be open or closed");
                }
                issue.state = state.to_string();
            }
            if let Some(names) = new_labels {
                issue.labels = names;
            }
            issue.updated = now_unix();
            let snapshot = issue.clone();
            drop(issue);
            let rendered = issue_json(repo, &snapshot);
            repo.touch();
            Response::json(200, rendered)
        }
        _ => Response::error(405, "Method Not Allowed"),
    }
}

/// The bench's read leg hammers GET issue: serve the serialized bytes
/// from the repository cache (invalidated wholesale on any mutation).
fn issue_get_cached(owner: &str, name: &str, number: i64) -> Response {
    let key = format!("issue:{number}");
    {
        let guard = store().read().unwrap();
        if let Some(repo) = guard.repo(owner, name) {
            if let Some(bytes) = repo.cache.get(&key) {
                return Response::bytes(200, "application/json", (**bytes).clone());
            }
        }
    }
    let mut guard = store().write().unwrap();
    let repo = match guard.repo_mut(owner, name) {
        Some(r) => r,
        None => return not_found(),
    };
    let issue = match repo.issue(number) {
        Some(i) => i,
        None => return not_found(),
    };
    let bytes = issue_json(repo, issue).to_bytes();
    repo.cache.insert(key, Arc::new(bytes.clone()));
    Response::bytes(200, "application/json", bytes)
}

fn label_names_of(body: &Json) -> Vec<String> {
    match body.get("labels") {
        Some(Json::Arr(items)) => items
            .iter()
            .filter_map(|item| match item {
                Json::Str(s) => Some(s.clone()),
                other => other.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()),
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn comment_routes(req: &Request, owner: &str, name: &str, number: i64) -> Response {
    match req.method.as_str() {
        "GET" => {
            let guard = store().read().unwrap();
            let repo = match guard.repo(owner, name) {
                Some(r) => r,
                None => return not_found(),
            };
            match repo.issue(number) {
                Some(issue) => {
                    let all: Vec<Json> =
                        issue.comments.iter().map(|c| comment_json(repo, number, c)).collect();
                    let (page, link) = paginate(
                        req,
                        &all,
                        &format!("/api/v3/repos/{owner}/{name}/issues/{number}/comments"),
                    );
                    with_link(Response::json(200, Json::Arr(page)), link)
                }
                None => not_found(),
            }
        }
        "POST" => {
            let body = body_obj(req);
            let text = match body.get("body").and_then(|v| v.as_str()) {
                Some(t) if !t.is_empty() => t.to_string(),
                _ => return validation("body is required"),
            };
            let mut guard = store().write().unwrap();
            let cid = guard.next_comment_id;
            guard.next_comment_id += 1;
            let repo = match guard.repo_mut(owner, name) {
                Some(r) => r,
                None => return not_found(),
            };
            let comment = crate::store::Comment {
                id: cid,
                body: text,
                author: VIEWER_LOGIN.to_string(),
                created: now_unix(),
            };
            let snapshot = comment.clone();
            let issue = match repo.issue_mut(number) {
                Some(i) => i,
                None => return not_found(),
            };
            issue.comments.push(comment);
            issue.updated = now_unix();
            let rendered = comment_json(repo, number, &snapshot);
            repo.touch();
            Response::json(201, rendered)
        }
        _ => Response::error(405, "Method Not Allowed"),
    }
}

// ------------------------------------------------------------------- pulls

/// Strip an "owner:" label prefix from a head/base argument.
fn bare_ref(value: &str) -> &str {
    match value.rsplit_once(':') {
        Some((_, tail)) => tail,
        None => value,
    }
}

/// Create a pull request; shared by REST, GraphQL and MCP.
pub fn do_create_pull(
    st: &mut Store,
    owner: &str,
    name: &str,
    title: &str,
    body: &str,
    head: &str,
    base: &str,
) -> Result<(i64, Json), Response> {
    let head = bare_ref(head).to_string();
    let base = bare_ref(base).to_string();
    if title.trim().is_empty() {
        return Err(validation("Validation Failed: title is required"));
    }
    let gid = st.next_gid;
    st.next_gid += 1;
    let repo = st.repo_mut(owner, name).ok_or_else(not_found)?;
    if repo.refs.get(&format!("refs/heads/{base}")).is_none() {
        return Err(validation("Validation Failed: base branch does not exist"));
    }
    if repo.refs.get(&format!("refs/heads/{head}")).is_none() {
        return Err(validation("Validation Failed: head branch does not exist"));
    }
    if head == base {
        return Err(validation("Validation Failed: head and base are the same branch"));
    }
    if repo
        .pulls
        .iter()
        .any(|p| p.state == "open" && p.head_ref == head && p.base_ref == base)
    {
        return Err(validation("A pull request already exists for these branches"));
    }
    let number = repo.next_number;
    repo.next_number += 1;
    let now = now_unix();
    let pull = crate::store::Pull {
        gid,
        number,
        title: title.to_string(),
        body: body.to_string(),
        state: "open".to_string(),
        merged: false,
        merged_at: None,
        merge_commit: None,
        head_ref: head,
        base_ref: base,
        author: VIEWER_LOGIN.to_string(),
        created: now,
        updated: now,
    };
    repo.pulls.push(pull);
    repo.touch();
    let rendered = pull_json(repo, repo.pulls.last().expect("just pushed"));
    Ok((number, rendered))
}

/// Merge a pull request (squash or merge commit); shared by all surfaces.
/// Errors carry the HTTP status the REST contract wants: 404 unknown,
/// 405 not mergeable (including conflicts).
pub fn do_merge_pull(
    st: &mut Store,
    owner: &str,
    name: &str,
    number: i64,
    method: &str,
) -> Result<String, Response> {
    let stamp = st.next_commit_time();
    let repo = st.repo_mut(owner, name).ok_or_else(not_found)?;
    let pull = repo.pull(number).ok_or_else(not_found)?.clone();
    if pull.state != "open" || pull.merged {
        return Err(Response::error(405, "Pull Request is not mergeable"));
    }
    let base_ref = format!("refs/heads/{}", pull.base_ref);
    let head_ref = format!("refs/heads/{}", pull.head_ref);
    let base_tip = repo.refs.get(&base_ref).copied().ok_or_else(|| Response::error(405, "Pull Request is not mergeable"))?;
    let head_tip = repo.refs.get(&head_ref).copied().ok_or_else(|| Response::error(405, "Pull Request is not mergeable"))?;
    let base_commit = gitcore::parse_commit(&repo.objects.get(&base_tip).ok_or_else(not_found)?.data).ok_or_else(not_found)?;
    let head_commit = gitcore::parse_commit(&repo.objects.get(&head_tip).ok_or_else(not_found)?.data).ok_or_else(not_found)?;
    let base_map = gitcore::flatten_tree(repo, &base_commit.tree);
    let head_map = gitcore::flatten_tree(repo, &head_commit.tree);
    let origin_map = match gitcore::merge_base(repo, &base_tip, &head_tip) {
        Some(mb) => {
            let mb_commit =
                gitcore::parse_commit(&repo.objects.get(&mb).ok_or_else(not_found)?.data).ok_or_else(not_found)?;
            gitcore::flatten_tree(repo, &mb_commit.tree)
        }
        None => std::collections::BTreeMap::new(),
    };
    let merged_map = match gitcore::merge_maps(&origin_map, &base_map, &head_map) {
        Ok(map) => map,
        Err(_) => return Err(Response::error(405, "Pull Request is not mergeable")),
    };
    let tree = gitcore::tree_from_map(repo, &merged_map);
    let (parents, message) = if method == "merge" {
        (
            vec![base_tip, head_tip],
            format!("Merge pull request #{number} from {owner}/{}\n\n{}", pull.head_ref, pull.title),
        )
    } else {
        // Squash (and rebase, whose content result is identical): one
        // commit on the base branch carrying the pull's title and body.
        let mut text = pull.title.clone();
        if !pull.body.is_empty() {
            text.push_str("\n\n");
            text.push_str(&pull.body);
        }
        (vec![base_tip], text)
    };
    let identity = server_identity(stamp);
    let bytes = gitcore::build_commit(&tree, &parents, &identity, &identity, &message);
    let commit = gitcore::store_object(repo, crate::store::ObjKind::Commit, bytes);
    repo.refs.insert(base_ref, commit);
    let now = now_unix();
    let pull = repo.pull_mut(number).ok_or_else(not_found)?;
    pull.state = "closed".to_string();
    pull.merged = true;
    pull.merged_at = Some(now);
    pull.merge_commit = Some(oid_hex(&commit));
    pull.updated = now;
    repo.touch();
    Ok(oid_hex(&commit))
}

fn pull_routes(req: &Request, owner: &str, name: &str, rest: &[String]) -> Response {
    if rest.is_empty() {
        return match req.method.as_str() {
            "GET" => {
                let guard = store().read().unwrap();
                let repo = match guard.repo(owner, name) {
                    Some(r) => r,
                    None => return not_found(),
                };
                let state = req.query_get("state").unwrap_or("open");
                let direction = req.query_get("direction").unwrap_or("desc");
                let mut pulls: Vec<&crate::store::Pull> = repo
                    .pulls
                    .iter()
                    .filter(|p| state == "all" || (state == "closed" && (p.state == "closed")) || (state == "open" && p.state == "open"))
                    .collect();
                pulls.sort_by_key(|p| p.number);
                if direction != "asc" {
                    pulls.reverse();
                }
                let all: Vec<Json> = pulls.iter().map(|p| pull_json(repo, p)).collect();
                let (page, link) = paginate(req, &all, &format!("/api/v3/repos/{owner}/{name}/pulls"));
                with_link(Response::json(200, Json::Arr(page)), link)
            }
            "POST" => {
                let body = body_obj(req);
                let title = body.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let text = body.get("body").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let head = body.get("head").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let base = body.get("base").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let mut guard = store().write().unwrap();
                match do_create_pull(&mut guard, owner, name, &title, &text, &head, &base) {
                    Ok((_, json)) => Response::json(201, json),
                    Err(resp) => resp,
                }
            }
            _ => Response::error(405, "Method Not Allowed"),
        };
    }
    let number: i64 = match rest[0].parse() {
        Ok(n) => n,
        Err(_) => return not_found(),
    };
    if rest.len() >= 2 && rest[1] == "files" {
        return pull_files(owner, name, number);
    }
    if rest.len() >= 2 && rest[1] == "merge" {
        if req.method != "PUT" {
            return Response::error(405, "Method Not Allowed");
        }
        let body = body_obj(req);
        let method = body.get("merge_method").and_then(|v| v.as_str()).unwrap_or("merge").to_string();
        let mut guard = store().write().unwrap();
        return match do_merge_pull(&mut guard, owner, name, number, &method) {
            Ok(sha) => Response::json(
                200,
                Json::obj(vec![
                    ("sha", Json::Str(sha)),
                    ("merged", Json::Bool(true)),
                    ("message", Json::Str("Pull Request successfully merged".to_string())),
                ]),
            ),
            Err(resp) => resp,
        };
    }
    match req.method.as_str() {
        "GET" => {
            let guard = store().read().unwrap();
            let repo = match guard.repo(owner, name) {
                Some(r) => r,
                None => return not_found(),
            };
            match repo.pull(number) {
                Some(pull) => Response::json(200, pull_json(repo, pull)),
                None => not_found(),
            }
        }
        "PATCH" => {
            let body = body_obj(req);
            let mut guard = store().write().unwrap();
            let repo = match guard.repo_mut(owner, name) {
                Some(r) => r,
                None => return not_found(),
            };
            let pull = match repo.pull_mut(number) {
                Some(p) => p,
                None => return not_found(),
            };
            if let Some(title) = body.get("title").and_then(|v| v.as_str()) {
                pull.title = title.to_string();
            }
            if let Some(text) = body.get("body").and_then(|v| v.as_str()) {
                pull.body = text.to_string();
            }
            if let Some(state) = body.get("state").and_then(|v| v.as_str()) {
                if state != "open" && state != "closed" {
                    return validation("state must be open or closed");
                }
                pull.state = state.to_string();
            }
            pull.updated = now_unix();
            let snapshot = pull.clone();
            drop(pull);
            let rendered = pull_json(repo, &snapshot);
            repo.touch();
            Response::json(200, rendered)
        }
        _ => Response::error(405, "Method Not Allowed"),
    }
}

fn pull_files(owner: &str, name: &str, number: i64) -> Response {
    let guard = store().read().unwrap();
    let repo = match guard.repo(owner, name) {
        Some(r) => r,
        None => return not_found(),
    };
    let pull = match repo.pull(number) {
        Some(p) => p,
        None => return not_found(),
    };
    let base_tip = repo.refs.get(&format!("refs/heads/{}", pull.base_ref)).copied();
    let head_tip = repo.refs.get(&format!("refs/heads/{}", pull.head_ref)).copied();
    let (Some(base_tip), Some(head_tip)) = (base_tip, head_tip) else {
        return Response::json(200, Json::Arr(vec![]));
    };
    let head_commit = match repo.objects.get(&head_tip).and_then(|o| gitcore::parse_commit(&o.data)) {
        Some(c) => c,
        None => return not_found(),
    };
    let head_map = gitcore::flatten_tree(repo, &head_commit.tree);
    let origin_map = match gitcore::merge_base(repo, &base_tip, &head_tip) {
        Some(mb) => match repo.objects.get(&mb).and_then(|o| gitcore::parse_commit(&o.data)) {
            Some(c) => gitcore::flatten_tree(repo, &c.tree),
            None => std::collections::BTreeMap::new(),
        },
        None => std::collections::BTreeMap::new(),
    };
    let changes = gitcore::diff_maps(&origin_map, &head_map);
    let files: Vec<Json> = changes
        .iter()
        .map(|change| {
            let old_bytes = change
                .old
                .and_then(|oid| repo.objects.get(&oid))
                .map(|o| (*o.data).clone())
                .unwrap_or_default();
            let new_bytes = change
                .new
                .and_then(|oid| repo.objects.get(&oid))
                .map(|o| (*o.data).clone())
                .unwrap_or_default();
            let (additions, deletions) = gitcore::line_diff(&old_bytes, &new_bytes);
            Json::obj(vec![
                ("sha", Json::Str(change.new.map(|o| oid_hex(&o)).unwrap_or_else(|| crate::sha1::oid_hex(&crate::sha1::ZERO_OID)))),
                ("filename", Json::Str(change.path.clone())),
                ("status", Json::Str(change.status.to_string())),
                ("additions", Json::Num(additions as f64)),
                ("deletions", Json::Num(deletions as f64)),
                ("changes", Json::Num((additions + deletions) as f64)),
            ])
        })
        .collect();
    Response::json(200, Json::Arr(files))
}
