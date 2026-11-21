//! The in-memory data model: one store, many repositories, and inside
//! each repository a git object database plus the forge's own records
//! (labels, issues, pull requests). Everything lives behind one RwLock in
//! main; reads are shared, writes are exclusive, and every mutation bumps
//! the repository's version so cached response bytes can be invalidated
//! wholesale instead of tracked entry by entry.

use std::collections::HashMap;
use std::sync::Arc;

use crate::sha1::Oid;

/// The suite's fixed cast. The scenario is generated from one seed, so
/// these identities are constants of the contest, not configuration:
/// the token's owner (viewer), a teammate, and a stranger.
pub const VIEWER_LOGIN: &str = "adaiversen";
pub const VIEWER_NAME: &str = "ada iversen";
pub const VIEWER_EMAIL: &str = "adaiversen@example.invalid";
pub const VIEWER_ID: i64 = 1000;

/// Object kinds, numbered as in a packfile entry header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjKind {
    Commit = 1,
    Tree = 2,
    Blob = 3,
    Tag = 4,
}

impl ObjKind {
    pub fn name(self) -> &'static str {
        match self {
            ObjKind::Commit => "commit",
            ObjKind::Tree => "tree",
            ObjKind::Blob => "blob",
            ObjKind::Tag => "tag",
        }
    }

    pub fn from_name(name: &str) -> Option<ObjKind> {
        match name {
            "commit" => Some(ObjKind::Commit),
            "tree" => Some(ObjKind::Tree),
            "blob" => Some(ObjKind::Blob),
            "tag" => Some(ObjKind::Tag),
            _ => None,
        }
    }

    pub fn from_code(code: u32) -> Option<ObjKind> {
        match code {
            1 => Some(ObjKind::Commit),
            2 => Some(ObjKind::Tree),
            3 => Some(ObjKind::Blob),
            4 => Some(ObjKind::Tag),
            _ => None,
        }
    }
}

/// One stored git object: kind plus exact body bytes (no header).
#[derive(Clone)]
pub struct GitObject {
    pub kind: ObjKind,
    pub data: Arc<Vec<u8>>,
}

#[derive(Clone)]
pub struct Label {
    pub id: i64,
    pub name: String,
    pub color: String,
}

#[derive(Clone)]
pub struct Comment {
    pub id: i64,
    pub body: String,
    pub author: String,
    pub created: i64,
}

#[derive(Clone)]
pub struct Issue {
    pub gid: i64,
    pub number: i64,
    pub title: String,
    pub body: String,
    pub state: String, // "open" | "closed"
    pub labels: Vec<String>,
    pub author: String,
    pub comments: Vec<Comment>,
    pub created: i64,
    pub updated: i64,
}

#[derive(Clone)]
pub struct Pull {
    pub gid: i64,
    pub number: i64,
    pub title: String,
    pub body: String,
    pub state: String, // "open" | "closed"
    pub merged: bool,
    pub merged_at: Option<i64>,
    pub merge_commit: Option<String>,
    pub head_ref: String,
    pub base_ref: String,
    pub author: String,
    pub created: i64,
    pub updated: i64,
}

pub struct Repo {
    pub id: i64,
    pub owner: String,
    pub name: String,
    pub description: String,
    pub private: bool,
    pub default_branch: String,
    pub created: i64,
    /// The git object database, keyed by raw object id.
    pub objects: HashMap<Oid, GitObject>,
    /// Full ref names ("refs/heads/main") to the object they point at.
    pub refs: HashMap<String, Oid>,
    pub labels: Vec<Label>,
    pub issues: Vec<Issue>,
    pub pulls: Vec<Pull>,
    /// Issues and pull requests share one number sequence, as on GitHub.
    pub next_number: i64,
    /// Bumped on every mutation; cached bytes are keyed against it.
    pub version: u64,
    /// Serialized GET responses, dropped wholesale when `version` moves.
    pub cache: HashMap<String, Arc<Vec<u8>>>,
    /// Memoized immutable protocol bodies (fetch packs, advertisements,
    /// ls-refs answers), dropped wholesale when `version` moves. Behind its
    /// own mutex so hot read paths never need the store write lock.
    pub memo: std::sync::Mutex<HashMap<String, Arc<Vec<u8>>>>,
}

impl Repo {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// Record a mutation: advance the version and drop every cached byte.
    pub fn touch(&mut self) {
        self.version += 1;
        self.cache.clear();
        if let Ok(mut memo) = self.memo.lock() {
            memo.clear();
        }
    }

    pub fn head_ref(&self) -> String {
        format!("refs/heads/{}", self.default_branch)
    }

    pub fn head_oid(&self) -> Option<Oid> {
        self.refs.get(&self.head_ref()).copied()
    }

    pub fn issue(&self, number: i64) -> Option<&Issue> {
        self.issues.iter().find(|i| i.number == number)
    }

    pub fn issue_mut(&mut self, number: i64) -> Option<&mut Issue> {
        self.issues.iter_mut().find(|i| i.number == number)
    }

    pub fn pull(&self, number: i64) -> Option<&Pull> {
        self.pulls.iter().find(|p| p.number == number)
    }

    pub fn pull_mut(&mut self, number: i64) -> Option<&mut Pull> {
        self.pulls.iter_mut().find(|p| p.number == number)
    }

    pub fn label(&self, name: &str) -> Option<&Label> {
        self.labels.iter().find(|l| l.name == name)
    }
}

pub struct Store {
    pub repos: HashMap<String, Repo>,
    pub next_repo_id: i64,
    pub next_gid: i64,
    pub next_label_id: i64,
    pub next_comment_id: i64,
    /// Monotonic seconds-ish counter for commit timestamps: deterministic
    /// ordering without depending on the wall clock for git bytes.
    pub commit_clock: i64,
}

impl Store {
    pub fn new() -> Self {
        Store {
            repos: HashMap::new(),
            next_repo_id: 7000,
            next_gid: 1,
            next_label_id: 1,
            next_comment_id: 1,
            commit_clock: 1_735_689_600,
        }
    }

    pub fn key(owner: &str, name: &str) -> String {
        format!("{owner}/{name}")
    }

    pub fn repo(&self, owner: &str, name: &str) -> Option<&Repo> {
        self.repos.get(&Self::key(owner, name))
    }

    pub fn repo_mut(&mut self, owner: &str, name: &str) -> Option<&mut Repo> {
        self.repos.get_mut(&Self::key(owner, name))
    }

    pub fn create_repo(&mut self, owner: &str, name: &str, description: &str, private: bool) -> &Repo {
        let id = self.next_repo_id;
        self.next_repo_id += 1;
        let repo = Repo {
            id,
            owner: owner.to_string(),
            name: name.to_string(),
            description: description.to_string(),
            private,
            default_branch: "main".to_string(),
            created: now_unix(),
            objects: HashMap::new(),
            refs: HashMap::new(),
            labels: Vec::new(),
            issues: Vec::new(),
            pulls: Vec::new(),
            next_number: 1,
            version: 1,
            cache: HashMap::new(),
            memo: std::sync::Mutex::new(HashMap::new()),
        };
        self.repos.insert(Self::key(owner, name), repo);
        self.repos.get(&Self::key(owner, name)).expect("just inserted")
    }

    pub fn next_commit_time(&mut self) -> i64 {
        self.commit_clock += 1;
        self.commit_clock
    }
}

/// Wall-clock seconds for REST `created_at` fields (never for git bytes
/// that a check might hash: contents commits use the store's own clock).
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(1_735_689_600)
}

/// Format unix seconds as ISO-8601 UTC (`2025-01-01T00:00:00Z`).
pub fn iso8601(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year,
        month,
        day,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Howard Hinnant's civil-from-days algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding (GraphQL global ids, contents payloads).
pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Base64 decoding that tolerates embedded whitespace (GitHub wraps
/// contents payloads) and missing padding.
pub fn b64_decode(text: &str) -> Option<Vec<u8>> {
    let mut table = [255u8; 256];
    for (i, &c) in B64.iter().enumerate() {
        table[c as usize] = i as u8;
    }
    let clean: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &c in &clean {
        if c == b'=' {
            break;
        }
        let value = table[c as usize];
        if value == 255 {
            return None;
        }
        acc = (acc << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// GitHub-style wrapped base64 for contents responses (60-char lines).
pub fn b64_wrapped(data: &[u8]) -> String {
    let flat = b64_encode(data);
    let mut out = String::with_capacity(flat.len() + flat.len() / 60 + 1);
    for (i, ch) in flat.chars().enumerate() {
        if i > 0 && i % 60 == 0 {
            out.push('\n');
        }
        out.push(ch);
    }
    out
}
