//! Git object machinery over the store: commit/tree/tag parsing and
//! building, tree flatten/rebuild for the contents API and merges,
//! reachability walks (full, depth-limited for shallow fetches, ancestry
//! for fast-forward checks and merge bases), three-way tree merges for
//! squash-merging pull requests, and packfile read/write.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crate::sha1::{object_id, oid_hex, Oid, ZERO_OID};
use crate::store::{GitObject, ObjKind, Repo};
use crate::zlib;

// ------------------------------------------------------------ object basics

/// Insert an object and return its id. Idempotent: the id is the hash.
pub fn store_object(repo: &mut Repo, kind: ObjKind, data: Vec<u8>) -> Oid {
    let oid = object_id(kind.name(), &data);
    repo.objects.entry(oid).or_insert_with(|| GitObject { kind, data: Arc::new(data) });
    oid
}

pub struct CommitInfo {
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub author: String,
    pub committer: String,
    pub message: String,
}

fn hex20(bytes: &[u8]) -> Option<Oid> {
    if bytes.len() != 40 {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    crate::sha1::parse_oid(text)
}

pub fn parse_commit(data: &[u8]) -> Option<CommitInfo> {
    let split = find_double_newline(data);
    let (head, message) = match split {
        Some(at) => (&data[..at], &data[at + 2..]),
        None => (data, &[][..]),
    };
    let mut tree = None;
    let mut parents = Vec::new();
    let mut author = String::new();
    let mut committer = String::new();
    for line in head.split(|&b| b == b'\n') {
        if let Some(rest) = line.strip_prefix(b"tree ".as_slice()) {
            tree = hex20(rest);
        } else if let Some(rest) = line.strip_prefix(b"parent ".as_slice()) {
            parents.push(hex20(rest)?);
        } else if let Some(rest) = line.strip_prefix(b"author ".as_slice()) {
            author = String::from_utf8_lossy(rest).into_owned();
        } else if let Some(rest) = line.strip_prefix(b"committer ".as_slice()) {
            committer = String::from_utf8_lossy(rest).into_owned();
        }
    }
    Some(CommitInfo {
        tree: tree?,
        parents,
        author,
        committer,
        message: String::from_utf8_lossy(message).into_owned(),
    })
}

fn find_double_newline(data: &[u8]) -> Option<usize> {
    data.windows(2).position(|w| w == b"\n\n")
}

/// The timestamp inside an identity line ("Name <mail> 1700000000 +0000").
pub fn identity_timestamp(line: &str) -> i64 {
    let mut parts = line.rsplitn(3, ' ');
    let _tz = parts.next();
    match parts.next().and_then(|p| p.parse::<i64>().ok()) {
        Some(v) => v,
        None => 0,
    }
}

pub fn build_commit(tree: &Oid, parents: &[Oid], author_line: &str, committer_line: &str, message: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(b"tree ");
    out.extend_from_slice(oid_hex(tree).as_bytes());
    out.push(b'\n');
    for parent in parents {
        out.extend_from_slice(b"parent ");
        out.extend_from_slice(oid_hex(parent).as_bytes());
        out.push(b'\n');
    }
    out.extend_from_slice(b"author ");
    out.extend_from_slice(author_line.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(b"committer ");
    out.extend_from_slice(committer_line.as_bytes());
    out.push(b'\n');
    out.push(b'\n');
    out.extend_from_slice(message.as_bytes());
    out
}

pub struct TagInfo {
    pub object: Oid,
    pub kind: String,
    pub tag: String,
}

pub fn parse_tag(data: &[u8]) -> Option<TagInfo> {
    let split = find_double_newline(data).unwrap_or(data.len());
    let mut object = None;
    let mut kind = String::new();
    let mut tag = String::new();
    for line in data[..split].split(|&b| b == b'\n') {
        if let Some(rest) = line.strip_prefix(b"object ".as_slice()) {
            object = hex20(rest);
        } else if let Some(rest) = line.strip_prefix(b"type ".as_slice()) {
            kind = String::from_utf8_lossy(rest).into_owned();
        } else if let Some(rest) = line.strip_prefix(b"tag ".as_slice()) {
            tag = String::from_utf8_lossy(rest).into_owned();
        }
    }
    Some(TagInfo { object: object?, kind, tag })
}

/// Peel a tag chain down to the first non-tag object.
pub fn peel_to_commit(repo: &Repo, mut oid: Oid) -> Oid {
    loop {
        match repo.objects.get(&oid) {
            Some(obj) if obj.kind == ObjKind::Tag => match parse_tag(&obj.data) {
                Some(tag) => oid = tag.object,
                None => return oid,
            },
            _ => return oid,
        }
    }
}

// ------------------------------------------------------------------- trees

#[derive(Clone, Debug)]
pub struct TreeEntry {
    pub mode: String,
    pub name: String,
    pub oid: Oid,
}

pub fn parse_tree(data: &[u8]) -> Vec<TreeEntry> {
    let mut entries = Vec::new();
    let mut at = 0usize;
    while at < data.len() {
        let space = match data[at..].iter().position(|&b| b == b' ') {
            Some(p) => at + p,
            None => break,
        };
        let mode = String::from_utf8_lossy(&data[at..space]).into_owned();
        let nul = match data[space + 1..].iter().position(|&b| b == 0) {
            Some(p) => space + 1 + p,
            None => break,
        };
        let name = String::from_utf8_lossy(&data[space + 1..nul]).into_owned();
        if nul + 21 > data.len() {
            break;
        }
        let mut oid = [0u8; 20];
        oid.copy_from_slice(&data[nul + 1..nul + 21]);
        entries.push(TreeEntry { mode, name, oid });
        at = nul + 21;
    }
    entries
}

/// Git's tree order: by name, directories as if suffixed with '/'.
fn tree_sort_key(entry: &TreeEntry) -> Vec<u8> {
    let mut key = entry.name.clone().into_bytes();
    if entry.mode == "40000" {
        key.push(b'/');
    }
    key
}

pub fn build_tree(entries: &[TreeEntry]) -> Vec<u8> {
    let mut sorted: Vec<&TreeEntry> = entries.iter().collect();
    sorted.sort_by_key(|e| tree_sort_key(e));
    let mut out = Vec::new();
    for entry in sorted {
        out.extend_from_slice(entry.mode.as_bytes());
        out.push(b' ');
        out.extend_from_slice(entry.name.as_bytes());
        out.push(0);
        out.extend_from_slice(&entry.oid);
    }
    out
}

/// Every file (blob) under a tree, keyed by slash-joined path.
pub fn flatten_tree(repo: &Repo, tree: &Oid) -> BTreeMap<String, (String, Oid)> {
    let mut out = BTreeMap::new();
    flatten_into(repo, tree, "", &mut out);
    out
}

fn flatten_into(repo: &Repo, tree: &Oid, prefix: &str, out: &mut BTreeMap<String, (String, Oid)>) {
    let obj = match repo.objects.get(tree) {
        Some(o) if o.kind == ObjKind::Tree => o,
        _ => return,
    };
    for entry in parse_tree(&obj.data) {
        let path = if prefix.is_empty() { entry.name.clone() } else { format!("{prefix}/{}", entry.name) };
        if entry.mode == "40000" {
            flatten_into(repo, &entry.oid, &path, out);
        } else {
            out.insert(path, (entry.mode.clone(), entry.oid));
        }
    }
}

/// Build (and store) nested trees for a flattened file map; returns the
/// root tree id. An empty map yields the canonical empty tree.
pub fn tree_from_map(repo: &mut Repo, map: &BTreeMap<String, (String, Oid)>) -> Oid {
    let items: Vec<(String, String, Oid)> =
        map.iter().map(|(p, (m, o))| (p.clone(), m.clone(), *o)).collect();
    build_subtree(repo, &items)
}

fn build_subtree(repo: &mut Repo, items: &[(String, String, Oid)]) -> Oid {
    let mut files: Vec<TreeEntry> = Vec::new();
    let mut dirs: BTreeMap<String, Vec<(String, String, Oid)>> = BTreeMap::new();
    for (path, mode, oid) in items {
        match path.split_once('/') {
            None => files.push(TreeEntry { mode: mode.clone(), name: path.clone(), oid: *oid }),
            Some((head, tail)) => {
                dirs.entry(head.to_string())
                    .or_default()
                    .push((tail.to_string(), mode.clone(), *oid));
            }
        }
    }
    let mut entries = files;
    for (name, sub) in dirs {
        let oid = build_subtree(repo, &sub);
        entries.push(TreeEntry { mode: "40000".to_string(), name, oid });
    }
    let bytes = build_tree(&entries);
    store_object(repo, ObjKind::Tree, bytes)
}

/// Look up one path inside a tree; returns (mode, oid, is_tree).
pub fn tree_lookup(repo: &Repo, tree: &Oid, path: &str) -> Option<(String, Oid, bool)> {
    let mut current = *tree;
    let segments: Vec<&str> = path.split('/').collect();
    for (index, segment) in segments.iter().enumerate() {
        let obj = repo.objects.get(&current)?;
        if obj.kind != ObjKind::Tree {
            return None;
        }
        let entry = parse_tree(&obj.data).into_iter().find(|e| &e.name == segment)?;
        if index + 1 == segments.len() {
            let is_tree = entry.mode == "40000";
            return Some((entry.mode, entry.oid, is_tree));
        }
        if entry.mode != "40000" {
            return None;
        }
        current = entry.oid;
    }
    None
}

// -------------------------------------------------------------- reachability

fn walk_tree_objects(repo: &Repo, tree: &Oid, set: &mut HashSet<Oid>) {
    if !set.insert(*tree) {
        return;
    }
    if let Some(obj) = repo.objects.get(tree) {
        if obj.kind == ObjKind::Tree {
            for entry in parse_tree(&obj.data) {
                if entry.mode == "40000" {
                    walk_tree_objects(repo, &entry.oid, set);
                } else if entry.mode != "160000" {
                    set.insert(entry.oid);
                }
            }
        }
    }
}

/// The full object closure of the given tips (tags peeled through, since
/// a tag's target is part of its reachability).
pub fn reachable(repo: &Repo, tips: &[Oid]) -> HashSet<Oid> {
    let mut set = HashSet::new();
    let mut queue: Vec<Oid> = tips.to_vec();
    while let Some(oid) = queue.pop() {
        if !set.insert(oid) {
            continue;
        }
        match repo.objects.get(&oid) {
            Some(obj) if obj.kind == ObjKind::Commit => {
                if let Some(commit) = parse_commit(&obj.data) {
                    walk_tree_objects(repo, &commit.tree, &mut set);
                    queue.extend(commit.parents.iter().copied());
                }
            }
            Some(obj) if obj.kind == ObjKind::Tag => {
                if let Some(tag) = parse_tag(&obj.data) {
                    queue.push(tag.object);
                }
            }
            _ => {}
        }
    }
    set
}

/// Depth-limited closure for shallow fetches: commits up to `depth`
/// generations from the tips (tips are generation 1), with the trees and
/// blobs of every included commit. Returns the set plus the boundary
/// commits (included commits whose parents were cut off).
pub fn reachable_depth(repo: &Repo, tips: &[Oid], depth: usize) -> (HashSet<Oid>, Vec<Oid>) {
    let mut set = HashSet::new();
    let mut boundary: Vec<Oid> = Vec::new();
    let mut seen_commits: HashMap<Oid, usize> = HashMap::new();
    let mut queue: VecDeque<(Oid, usize)> = tips.iter().map(|t| (*t, 1usize)).collect();
    while let Some((oid, gen)) = queue.pop_front() {
        if gen > depth {
            continue;
        }
        match repo.objects.get(&oid) {
            Some(obj) if obj.kind == ObjKind::Tag => {
                set.insert(oid);
                if let Some(tag) = parse_tag(&obj.data) {
                    queue.push_back((tag.object, gen));
                }
                continue;
            }
            Some(obj) if obj.kind == ObjKind::Commit => {
                if let Some(&existing) = seen_commits.get(&oid) {
                    if existing <= gen {
                        continue;
                    }
                }
                seen_commits.insert(oid, gen);
                set.insert(oid);
                if let Some(commit) = parse_commit(&obj.data) {
                    walk_tree_objects(repo, &commit.tree, &mut set);
                    if gen == depth {
                        if !commit.parents.is_empty() {
                            boundary.push(oid);
                        }
                    } else {
                        for parent in &commit.parents {
                            queue.push_back((*parent, gen + 1));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    (set, boundary)
}

/// The objects a client already holds: the closure of its `have` tips,
/// not descending past its declared shallow boundary commits.
pub fn held_set(repo: &Repo, haves: &[Oid], shallow: &[Oid]) -> HashSet<Oid> {
    let shallow_set: HashSet<Oid> = shallow.iter().copied().collect();
    let mut set = HashSet::new();
    let mut queue: Vec<Oid> = haves.to_vec();
    while let Some(oid) = queue.pop() {
        if !set.insert(oid) {
            continue;
        }
        match repo.objects.get(&oid) {
            Some(obj) if obj.kind == ObjKind::Commit => {
                if let Some(commit) = parse_commit(&obj.data) {
                    walk_tree_objects(repo, &commit.tree, &mut set);
                    if !shallow_set.contains(&oid) {
                        queue.extend(commit.parents.iter().copied());
                    }
                }
            }
            Some(obj) if obj.kind == ObjKind::Tag => {
                if let Some(tag) = parse_tag(&obj.data) {
                    queue.push(tag.object);
                }
            }
            _ => {}
        }
    }
    set
}

/// Is `ancestor` reachable from `descendant` by parent links alone?
pub fn is_ancestor(repo: &Repo, ancestor: &Oid, descendant: &Oid) -> bool {
    if ancestor == descendant {
        return true;
    }
    let mut seen = HashSet::new();
    let mut queue = vec![*descendant];
    while let Some(oid) = queue.pop() {
        if !seen.insert(oid) {
            continue;
        }
        if let Some(obj) = repo.objects.get(&oid) {
            if obj.kind == ObjKind::Commit {
                if let Some(commit) = parse_commit(&obj.data) {
                    for parent in &commit.parents {
                        if parent == ancestor {
                            return true;
                        }
                        queue.push(*parent);
                    }
                }
            }
        }
    }
    false
}

/// All ancestors of a commit (itself included), parents only.
pub fn ancestors(repo: &Repo, tip: &Oid) -> HashSet<Oid> {
    let mut set = HashSet::new();
    let mut queue = vec![*tip];
    while let Some(oid) = queue.pop() {
        if !set.insert(oid) {
            continue;
        }
        if let Some(obj) = repo.objects.get(&oid) {
            if obj.kind == ObjKind::Commit {
                if let Some(commit) = parse_commit(&obj.data) {
                    queue.extend(commit.parents.iter().copied());
                }
            }
        }
    }
    set
}

/// A best common ancestor of two commits (breadth-first from `b` into
/// `a`'s ancestor set; ties resolve toward the newest generation).
pub fn merge_base(repo: &Repo, a: &Oid, b: &Oid) -> Option<Oid> {
    let a_ancestors = ancestors(repo, a);
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([*b]);
    while let Some(oid) = queue.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        if a_ancestors.contains(&oid) {
            return Some(oid);
        }
        if let Some(obj) = repo.objects.get(&oid) {
            if obj.kind == ObjKind::Commit {
                if let Some(commit) = parse_commit(&obj.data) {
                    queue.extend(commit.parents.iter().copied());
                }
            }
        }
    }
    None
}

/// Commits reachable from `tip`, newest first (by commit timestamp, then
/// by discovery order), for the REST commits listing.
pub fn commit_walk(repo: &Repo, tip: &Oid) -> Vec<Oid> {
    let mut all = Vec::new();
    let mut seen = HashSet::new();
    let mut queue = vec![*tip];
    while let Some(oid) = queue.pop() {
        if !seen.insert(oid) {
            continue;
        }
        all.push(oid);
        if let Some(obj) = repo.objects.get(&oid) {
            if obj.kind == ObjKind::Commit {
                if let Some(commit) = parse_commit(&obj.data) {
                    queue.extend(commit.parents.iter().copied());
                }
            }
        }
    }
    let mut stamped: Vec<(i64, usize, Oid)> = all
        .iter()
        .enumerate()
        .map(|(index, oid)| {
            let ts = repo
                .objects
                .get(oid)
                .and_then(|o| parse_commit(&o.data))
                .map(|c| identity_timestamp(&c.committer))
                .unwrap_or(0);
            (ts, index, *oid)
        })
        .collect();
    stamped.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    stamped.into_iter().map(|(_, _, oid)| oid).collect()
}

// ------------------------------------------------------------------- merge

pub struct FileChange {
    pub path: String,
    pub status: &'static str, // "added" | "modified" | "removed"
    pub old: Option<Oid>,
    pub new: Option<Oid>,
}

/// Path-level differences between two flattened trees.
pub fn diff_maps(old: &BTreeMap<String, (String, Oid)>, new: &BTreeMap<String, (String, Oid)>) -> Vec<FileChange> {
    let mut changes = Vec::new();
    for (path, (_, oid)) in new {
        match old.get(path) {
            None => changes.push(FileChange { path: path.clone(), status: "added", old: None, new: Some(*oid) }),
            Some((_, old_oid)) if old_oid != oid => {
                changes.push(FileChange { path: path.clone(), status: "modified", old: Some(*old_oid), new: Some(*oid) })
            }
            _ => {}
        }
    }
    for (path, (_, oid)) in old {
        if !new.contains_key(path) {
            changes.push(FileChange { path: path.clone(), status: "removed", old: Some(*oid), new: None });
        }
    }
    changes
}

/// Three-way merge of flattened trees. `ours` and `theirs` both descend
/// from `base`. Returns the merged map, or the list of conflicted paths.
pub fn merge_maps(
    base: &BTreeMap<String, (String, Oid)>,
    ours: &BTreeMap<String, (String, Oid)>,
    theirs: &BTreeMap<String, (String, Oid)>,
) -> Result<BTreeMap<String, (String, Oid)>, Vec<String>> {
    let mut merged = BTreeMap::new();
    let mut conflicts = Vec::new();
    let mut paths: HashSet<&String> = HashSet::new();
    paths.extend(base.keys());
    paths.extend(ours.keys());
    paths.extend(theirs.keys());
    for path in paths {
        let b = base.get(path);
        let o = ours.get(path);
        let t = theirs.get(path);
        let pick = if o == t {
            o
        } else if b == o {
            t
        } else if b == t {
            o
        } else {
            conflicts.push(path.clone());
            continue;
        };
        if let Some((mode, oid)) = pick {
            merged.insert(path.clone(), (mode.clone(), *oid));
        }
    }
    if conflicts.is_empty() {
        Ok(merged)
    } else {
        Err(conflicts)
    }
}

/// Line-based diff counts (LCS over lines; falls back to a coarse count
/// for very large files so a pathological blob cannot stall a request).
pub fn line_diff(old: &[u8], new: &[u8]) -> (usize, usize) {
    let old_text = String::from_utf8_lossy(old);
    let new_text = String::from_utf8_lossy(new);
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    if old_lines.is_empty() {
        return (new_lines.len(), 0);
    }
    if new_lines.is_empty() {
        return (0, old_lines.len());
    }
    if old_lines.len() > 4000 || new_lines.len() > 4000 {
        return (new_lines.len(), old_lines.len());
    }
    // Classic LCS dynamic program over the shorter axis.
    let (short, long) = if old_lines.len() <= new_lines.len() {
        (&old_lines, &new_lines)
    } else {
        (&new_lines, &old_lines)
    };
    let mut prev = vec![0usize; short.len() + 1];
    let mut cur = vec![0usize; short.len() + 1];
    for long_line in long.iter() {
        for (j, short_line) in short.iter().enumerate() {
            cur[j + 1] = if long_line == short_line {
                prev[j] + 1
            } else {
                prev[j + 1].max(cur[j])
            };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let common = prev[short.len()];
    (new_lines.len() - common, old_lines.len() - common)
}

// -------------------------------------------------------------------- pack

/// Serialize objects into a version-2 packfile (no deltas: plain entries
/// in the order given, each zlib-deflated whole).
pub fn write_pack(repo: &Repo, oids: &[Oid]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1024);
    out.extend_from_slice(b"PACK");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(oids.len() as u32).to_be_bytes());
    for oid in oids {
        let obj = match repo.objects.get(oid) {
            Some(o) => o,
            None => continue,
        };
        let code = obj.kind as u32;
        let mut size = obj.data.len() as u64;
        let mut first = (code << 4) | (size & 0x0f) as u32;
        size >>= 4;
        while size > 0 {
            first |= 0x80;
            out.push(first as u8);
            first = (size & 0x7f) as u32;
            size >>= 7;
        }
        out.push(first as u8);
        out.extend_from_slice(&zlib::deflate(&obj.data));
    }
    let trailer = crate::sha1::sha1(&out);
    out.extend_from_slice(&trailer);
    out
}

/// Parse a packfile into (id, kind, body) triples, resolving OFS_DELTA
/// and REF_DELTA entries against earlier entries or, for thin packs,
/// objects the repository already holds.
pub fn read_pack(
    repo: &Repo,
    data: &[u8],
) -> Result<Vec<(Oid, ObjKind, Vec<u8>)>, String> {
    if data.len() < 12 + 20 || &data[0..4] != b"PACK" {
        return Err("not a packfile".to_string());
    }
    let count = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;
    let mut pos = 12usize;
    // Entry start offsets let OFS_DELTA find its base.
    let mut by_offset: HashMap<usize, (ObjKind, Vec<u8>)> = HashMap::new();
    let mut results: Vec<(Oid, ObjKind, Vec<u8>)> = Vec::with_capacity(count);
    for _ in 0..count {
        let entry_start = pos;
        if pos >= data.len() {
            return Err("pack truncated".to_string());
        }
        let mut byte = data[pos] as u64;
        pos += 1;
        let code = ((byte >> 4) & 0x7) as u32;
        let mut size = byte & 0x0f;
        let mut shift = 4u32;
        while byte & 0x80 != 0 {
            if pos >= data.len() {
                return Err("pack truncated in header".to_string());
            }
            byte = data[pos] as u64;
            pos += 1;
            size |= (byte & 0x7f) << shift;
            shift += 7;
        }
        let (kind, body) = match code {
            6 => {
                // OFS_DELTA: a negative offset back to the base entry.
                if pos >= data.len() {
                    return Err("pack truncated in ofs-delta".to_string());
                }
                let mut byte = data[pos] as u64;
                pos += 1;
                let mut offset = byte & 0x7f;
                while byte & 0x80 != 0 {
                    if pos >= data.len() {
                        return Err("pack truncated in ofs-delta".to_string());
                    }
                    byte = data[pos] as u64;
                    pos += 1;
                    offset = ((offset + 1) << 7) | (byte & 0x7f);
                }
                let base_at = entry_start
                    .checked_sub(offset as usize)
                    .ok_or("ofs-delta points outside the pack")?;
                let (base_kind, base_body) = by_offset
                    .get(&base_at)
                    .cloned()
                    .ok_or("ofs-delta base not yet read")?;
                let (delta, used) = zlib::inflate(&data[pos..]).ok_or("bad delta stream")?;
                pos += used;
                let body = apply_delta(&base_body, &delta).ok_or("delta does not apply")?;
                (base_kind, body)
            }
            7 => {
                // REF_DELTA: base named by id, in this pack or the store.
                if pos + 20 > data.len() {
                    return Err("pack truncated in ref-delta".to_string());
                }
                let mut base_oid = [0u8; 20];
                base_oid.copy_from_slice(&data[pos..pos + 20]);
                pos += 20;
                let (delta, used) = zlib::inflate(&data[pos..]).ok_or("bad delta stream")?;
                pos += used;
                // The base may be an entry read earlier in this pack
                // (found by recomputing ids) or an object the repository
                // already holds (a thin pack).
                let mut found = None;
                for (k, b) in by_offset.values() {
                    if object_id(k.name(), b) == base_oid {
                        found = Some((*k, b.clone()));
                        break;
                    }
                }
                let (base_kind, base_body) = match found {
                    Some(v) => v,
                    None => {
                        let stored = repo.objects.get(&base_oid).ok_or("ref-delta base unknown")?;
                        (stored.kind, (*stored.data).clone())
                    }
                };
                let body = apply_delta(&base_body, &delta).ok_or("delta does not apply")?;
                (base_kind, body)
            }
            _ => {
                let kind = ObjKind::from_code(code).ok_or("unknown pack entry type")?;
                let (body, used) = zlib::inflate(&data[pos..]).ok_or("bad object stream")?;
                pos += used;
                if body.len() as u64 != size {
                    return Err("pack entry size mismatch".to_string());
                }
                (kind, body)
            }
        };
        by_offset.insert(entry_start, (kind, body.clone()));
        let oid = object_id(kind.name(), &body);
        results.push((oid, kind, body));
    }
    Ok(results)
}

/// Apply a git delta (source-size, target-size, copy/insert opcodes).
pub fn apply_delta(base: &[u8], delta: &[u8]) -> Option<Vec<u8>> {
    let mut pos = 0usize;
    let read_varint = |pos: &mut usize| -> Option<usize> {
        let mut value = 0usize;
        let mut shift = 0u32;
        loop {
            if *pos >= delta.len() {
                return None;
            }
            let byte = delta[*pos];
            *pos += 1;
            value |= ((byte & 0x7f) as usize) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
        }
        Some(value)
    };
    let source_size = read_varint(&mut pos)?;
    if source_size != base.len() {
        return None;
    }
    let target_size = read_varint(&mut pos)?;
    let mut out = Vec::with_capacity(target_size);
    while pos < delta.len() {
        let op = delta[pos];
        pos += 1;
        if op & 0x80 != 0 {
            let mut offset = 0usize;
            let mut size = 0usize;
            for i in 0..4 {
                if op & (1 << i) != 0 {
                    if pos >= delta.len() {
                        return None;
                    }
                    offset |= (delta[pos] as usize) << (8 * i);
                    pos += 1;
                }
            }
            for i in 0..2 {
                if op & (0x10 << i) != 0 {
                    if pos >= delta.len() {
                        return None;
                    }
                    size |= (delta[pos] as usize) << (8 * i);
                    pos += 1;
                }
            }
            if size == 0 {
                size = 0x10000;
            }
            if offset + size > base.len() {
                return None;
            }
            out.extend_from_slice(&base[offset..offset + size]);
        } else if op != 0 {
            let size = op as usize;
            if pos + size > delta.len() {
                return None;
            }
            out.extend_from_slice(&delta[pos..pos + size]);
            pos += size;
        } else {
            return None;
        }
    }
    if out.len() != target_size {
        return None;
    }
    Some(out)
}

// --------------------------------------------------------------- pkt-lines

/// One pkt-line: 4 hex digits of total length, then the payload.
pub fn pkt_line(payload: &[u8]) -> Vec<u8> {
    let total = payload.len() + 4;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(format!("{total:04x}").as_bytes());
    out.extend_from_slice(payload);
    out
}

pub const PKT_FLUSH: &[u8] = b"0000";
pub const PKT_DELIM: &[u8] = b"0001";

/// Wrap bytes in sideband band-1 pkt-lines (payload chunks of at most
/// 65515 bytes so each framed packet stays under the 65520 maximum).
pub fn sideband_wrap(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 60000 * 5 + 5);
    for chunk in data.chunks(65515) {
        let mut payload = Vec::with_capacity(chunk.len() + 1);
        payload.push(1u8);
        payload.extend_from_slice(chunk);
        out.extend_from_slice(&pkt_line(&payload));
    }
    out
}

/// Parse a pkt-line stream into payloads and control markers. Stops at
/// the first malformed frame and reports what it managed to read.
pub enum Pkt {
    Data(Vec<u8>),
    Flush,
    Delim,
}

pub fn parse_pkts(data: &[u8]) -> Vec<Pkt> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 4 <= data.len() {
        let header = match std::str::from_utf8(&data[pos..pos + 4]) {
            Ok(h) => h,
            Err(_) => break,
        };
        let len = match usize::from_str_radix(header, 16) {
            Ok(v) => v,
            Err(_) => break,
        };
        match len {
            0 => {
                out.push(Pkt::Flush);
                pos += 4;
            }
            1 => {
                out.push(Pkt::Delim);
                pos += 4;
            }
            2 => {
                pos += 4;
            }
            3 => break,
            _ => {
                if pos + len > data.len() {
                    break;
                }
                out.push(Pkt::Data(data[pos + 4..pos + len].to_vec()));
                pos += len;
            }
        }
    }
    out
}

/// The offset just past the pkt-line stream in a receive-pack body
/// (where the packfile begins).
pub fn pkt_stream_end(data: &[u8]) -> Option<usize> {
    let mut pos = 0usize;
    loop {
        if pos + 4 > data.len() {
            return None;
        }
        let header = std::str::from_utf8(&data[pos..pos + 4]).ok()?;
        let len = usize::from_str_radix(header, 16).ok()?;
        match len {
            0 => return Some(pos + 4),
            1 | 2 => pos += 4,
            3 => return None,
            _ => {
                if pos + len > data.len() {
                    return None;
                }
                pos += len;
            }
        }
    }
}

pub const ZERO_HEX: &str = "0000000000000000000000000000000000000000";

pub fn is_zero_oid(oid: &Oid) -> bool {
    *oid == ZERO_OID
}
