//! Git smart-HTTP: ref advertisements (v0 and v2), the v2 ls-refs and
//! fetch commands, and receive-pack with sideband report-status.
//!
//! The shapes here are dictated by the suite's own git client: v2 is
//! answered when the request carries `Git-Protocol: version=2`, v0
//! otherwise; receive-pack is always v0. Fetch packs are assembled from
//! the object cache and memoized per repository state, so the bench's
//! fifty concurrent clones of one tip share a single pack build.

use std::collections::HashSet;

use crate::gitcore::{self, Pkt};
use crate::sha1::{oid_hex, parse_oid, Oid, ZERO_OID};
use crate::store::{ObjKind, Repo};
use crate::{store, Request, Response};

const UPLOAD_CAPS_V0: &str = "multi_ack_detailed side-band-64k thin-pack ofs-delta shallow deepen-since deepen-not agent=forge/1.0";
const RECEIVE_CAPS_V0: &str = "report-status side-band-64k delete-refs ofs-delta agent=forge/1.0";

pub fn handle(req: &Request, owner: &str, name: &str, tail: &[String]) -> Response {
    if tail.len() == 2 && tail[0] == "info" && tail[1] == "refs" {
        return advertise(req, owner, name);
    }
    if tail.len() == 1 && tail[0] == "git-upload-pack" && req.method == "POST" {
        return upload_pack(req, owner, name);
    }
    if tail.len() == 1 && tail[0] == "git-receive-pack" && req.method == "POST" {
        return receive_pack(req, owner, name);
    }
    Response::error(404, "Not Found")
}

fn repo_exists(owner: &str, name: &str) -> bool {
    store().read().unwrap().repo(owner, name).is_some()
}

// ---------------------------------------------------------- advertisements

fn advertise(req: &Request, owner: &str, name: &str) -> Response {
    let service = req.query_get("service").unwrap_or("git-upload-pack").to_string();
    if service != "git-upload-pack" && service != "git-receive-pack" {
        return Response::error(400, "Unknown service");
    }
    let guard = store().read().unwrap();
    let repo = match guard.repo(owner, name) {
        Some(r) => r,
        None => return Response::error(404, "Repository not found"),
    };
    let wants_v2 = req
        .header("git-protocol")
        .map(|v| v.split(',').any(|p| p.trim() == "version=2"))
        .unwrap_or(false);
    let content_type = format!("application/x-{service}-advertisement");
    if wants_v2 && service == "git-upload-pack" {
        let mut body = Vec::new();
        body.extend_from_slice(&gitcore::pkt_line(b"# service=git-upload-pack\n"));
        body.extend_from_slice(gitcore::PKT_FLUSH);
        body.extend_from_slice(&gitcore::pkt_line(b"version 2\n"));
        body.extend_from_slice(&gitcore::pkt_line(b"agent=forge/1.0\n"));
        body.extend_from_slice(&gitcore::pkt_line(b"ls-refs=unborn\n"));
        body.extend_from_slice(&gitcore::pkt_line(b"fetch=shallow wait-for-done\n"));
        body.extend_from_slice(&gitcore::pkt_line(b"object-format=sha1\n"));
        body.extend_from_slice(gitcore::PKT_FLUSH);
        return Response::bytes(200, &content_type, body);
    }
    // v0: preamble, then ref lines; capabilities ride the first line.
    let caps = if service == "git-upload-pack" {
        format!("{UPLOAD_CAPS_V0} symref=HEAD:refs/heads/{}", repo.default_branch)
    } else {
        RECEIVE_CAPS_V0.to_string()
    };
    let mut body = Vec::new();
    body.extend_from_slice(&gitcore::pkt_line(format!("# service={service}\n").as_bytes()));
    body.extend_from_slice(gitcore::PKT_FLUSH);
    let mut lines: Vec<String> = Vec::new();
    let head_oid = repo.head_oid();
    match head_oid {
        Some(oid) => lines.push(format!("{} HEAD", oid_hex(&oid))),
        None => lines.push(format!("{} capabilities^{{}}", oid_hex(&ZERO_OID))),
    }
    let mut ref_names: Vec<&String> = repo.refs.keys().collect();
    ref_names.sort();
    for refname in ref_names {
        let oid = repo.refs[refname];
        lines.push(format!("{} {}", oid_hex(&oid), refname));
        if let Some(obj) = repo.objects.get(&oid) {
            if obj.kind == ObjKind::Tag {
                let peeled = gitcore::peel_to_commit(repo, oid);
                if peeled != oid {
                    lines.push(format!("{} {}^{{}}", oid_hex(&peeled), refname));
                }
            }
        }
    }
    for (index, line) in lines.iter().enumerate() {
        if index == 0 {
            body.extend_from_slice(&gitcore::pkt_line(format!("{line}\0{caps}\n").as_bytes()));
        } else {
            body.extend_from_slice(&gitcore::pkt_line(format!("{line}\n").as_bytes()));
        }
    }
    body.extend_from_slice(gitcore::PKT_FLUSH);
    Response::bytes(200, &content_type, body)
}

// --------------------------------------------------------------- ls-refs

struct RefLine {
    oid: Oid,
    name: String,
    symref: Option<String>,
    peeled: Option<Oid>,
    unborn: bool,
}

fn collect_refs(repo: &Repo, peel: bool, symrefs: bool) -> Vec<RefLine> {
    let mut out = Vec::new();
    let head_target = repo.head_ref();
    match repo.head_oid() {
        Some(oid) => out.push(RefLine {
            oid,
            name: "HEAD".to_string(),
            symref: if symrefs { Some(head_target) } else { None },
            peeled: None,
            unborn: false,
        }),
        None => out.push(RefLine {
            oid: ZERO_OID,
            name: "HEAD".to_string(),
            symref: if symrefs { Some(head_target) } else { None },
            peeled: None,
            unborn: true,
        }),
    }
    let mut ref_names: Vec<&String> = repo.refs.keys().collect();
    ref_names.sort();
    for refname in ref_names {
        let oid = repo.refs[refname];
        let peeled = if peel {
            match repo.objects.get(&oid) {
                Some(obj) if obj.kind == ObjKind::Tag => {
                    let target = gitcore::peel_to_commit(repo, oid);
                    if target != oid {
                        Some(target)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        } else {
            None
        };
        out.push(RefLine { oid, name: refname.clone(), symref: None, peeled, unborn: false });
    }
    out
}

fn ls_refs(repo: &Repo, args: &[String]) -> Vec<u8> {
    let peel = args.iter().any(|a| a == "peel");
    let symrefs = args.iter().any(|a| a == "symrefs");
    let unborn_ok = args.iter().any(|a| a == "unborn");
    let prefixes: Vec<&str> = args
        .iter()
        .filter_map(|a| a.strip_prefix("ref-prefix "))
        .collect();
    let mut body = Vec::new();
    for line in collect_refs(repo, peel, symrefs) {
        if !prefixes.is_empty() && !prefixes.iter().any(|p| line.name.starts_with(p)) {
            continue;
        }
        if line.unborn && !unborn_ok {
            continue;
        }
        let mut text = if line.unborn {
            format!("unborn {}", line.name)
        } else {
            format!("{} {}", oid_hex(&line.oid), line.name)
        };
        if let Some(target) = &line.symref {
            text.push_str(&format!(" symref-target:{target}"));
        }
        if let Some(peeled) = &line.peeled {
            text.push_str(&format!(" peeled:{}", oid_hex(peeled)));
        }
        text.push('\n');
        body.extend_from_slice(&gitcore::pkt_line(text.as_bytes()));
    }
    body.extend_from_slice(gitcore::PKT_FLUSH);
    body
}

// ------------------------------------------------------------------ fetch

fn fetch_response(repo: &Repo, args: &[String]) -> Vec<u8> {
    let mut wants: Vec<Oid> = Vec::new();
    let mut haves: Vec<Oid> = Vec::new();
    let mut shallow: Vec<Oid> = Vec::new();
    let mut depth: Option<usize> = None;
    let mut done = false;
    for arg in args {
        if let Some(v) = arg.strip_prefix("want ") {
            if let Some(oid) = parse_oid(v.trim()) {
                wants.push(oid);
            }
        } else if let Some(v) = arg.strip_prefix("have ") {
            if let Some(oid) = parse_oid(v.trim()) {
                haves.push(oid);
            }
        } else if let Some(v) = arg.strip_prefix("shallow ") {
            if let Some(oid) = parse_oid(v.trim()) {
                shallow.push(oid);
            }
        } else if let Some(v) = arg.strip_prefix("deepen ") {
            depth = v.trim().parse::<usize>().ok();
        } else if arg == "done" {
            done = true;
        }
    }
    if !done {
        // Negotiation stays open: acknowledge and wait for the next round.
        let mut body = Vec::new();
        body.extend_from_slice(&gitcore::pkt_line(b"acknowledgments\n"));
        body.extend_from_slice(&gitcore::pkt_line(b"NAK\n"));
        body.extend_from_slice(gitcore::PKT_DELIM);
        return body;
    }
    let held = gitcore::held_set(repo, &haves, &shallow);
    let (mut send, shallow_lines) = match depth {
        Some(d) => {
            let (set, boundary) = gitcore::reachable_depth(repo, &wants, d);
            let mut lines = Vec::new();
            for oid in &boundary {
                lines.push(format!("shallow {}", oid_hex(oid)));
            }
            for oid in &shallow {
                if set.contains(oid) && !boundary.contains(oid) {
                    lines.push(format!("unshallow {}", oid_hex(oid)));
                }
            }
            (set, lines)
        }
        None => (gitcore::reachable(repo, &wants), Vec::new()),
    };
    for oid in &held {
        send.remove(oid);
    }
    // Order: commits, trees, blobs, tags; ids sorted inside each class so
    // the pack is a pure function of the object set (cache-friendly).
    let mut ordered: Vec<Oid> = send.iter().copied().collect();
    ordered.sort_by_key(|oid| {
        let rank = match repo.objects.get(oid).map(|o| o.kind) {
            Some(ObjKind::Commit) => 0,
            Some(ObjKind::Tree) => 1,
            Some(ObjKind::Blob) => 2,
            _ => 3,
        };
        (rank, oid_hex(oid))
    });
    let pack = gitcore::write_pack(repo, &ordered);
    let mut body = Vec::new();
    if !shallow_lines.is_empty() {
        body.extend_from_slice(&gitcore::pkt_line(b"shallow-info\n"));
        for line in &shallow_lines {
            body.extend_from_slice(&gitcore::pkt_line(format!("{line}\n").as_bytes()));
        }
        body.extend_from_slice(gitcore::PKT_DELIM);
    }
    body.extend_from_slice(&gitcore::pkt_line(b"packfile\n"));
    body.extend_from_slice(&gitcore::sideband_wrap(&pack));
    body.extend_from_slice(gitcore::PKT_FLUSH);
    body
}

fn upload_pack(req: &Request, owner: &str, name: &str) -> Response {
    if !repo_exists(owner, name) {
        return Response::error(404, "Repository not found");
    }
    let packets = gitcore::parse_pkts(&req.body);
    let mut lines: Vec<String> = Vec::new();
    for packet in &packets {
        match packet {
            Pkt::Data(data) => lines.push(String::from_utf8_lossy(data).into_owned()),
            Pkt::Delim | Pkt::Flush => {}
        }
    }
    let command = lines
        .first()
        .map(|l| l.trim_end().to_string())
        .unwrap_or_default();
    let args: Vec<String> = lines
        .iter()
        .skip(1)
        .filter(|l| !l.starts_with("agent=") && !l.starts_with("object-format=") && !l.starts_with("session-id="))
        .map(|l| l.trim_end().to_string())
        .collect();
    let guard = store().read().unwrap();
    let repo = match guard.repo(owner, name) {
        Some(r) => r,
        None => return Response::error(404, "Repository not found"),
    };
    match command.as_str() {
        "command=ls-refs" => {
            // Memoized under the repo's own mutex: the answer depends only
            // on the argument set and the (mutation-invalidated) ref state.
            let key = format!("lsrefs|{}", args.join("|"));
            if let Ok(memo) = repo.memo.lock() {
                if let Some(hit) = memo.get(&key) {
                    return Response::bytes(200, "application/x-git-upload-pack-result", (**hit).clone());
                }
            }
            let body = ls_refs(repo, &args);
            if let Ok(mut memo) = repo.memo.lock() {
                memo.insert(key, std::sync::Arc::new(body.clone()));
            }
            Response::bytes(200, "application/x-git-upload-pack-result", body)
        }
        "command=fetch" => {
            // The pack memo is keyed by the full argument set; a hit skips
            // the closure walk and the pack assembly entirely. The memo has
            // its own mutex, so a storm of concurrent clones never queues
            // behind the store write lock.
            let key = format!("fetch|{}", args.join("|"));
            if let Ok(memo) = repo.memo.lock() {
                if let Some(pack) = memo.get(&key) {
                    return Response::bytes(200, "application/x-git-upload-pack-result", (**pack).clone());
                }
            }
            let body = fetch_response(repo, &args);
            if let Ok(mut memo) = repo.memo.lock() {
                memo.insert(key, std::sync::Arc::new(body.clone()));
            }
            Response::bytes(200, "application/x-git-upload-pack-result", body)
        }
        _ => Response::error(400, "Unknown upload-pack command"),
    }
}

// ------------------------------------------------------------ receive-pack

struct PushCommand {
    old: Oid,
    new: Oid,
    refname: String,
}

fn receive_pack(req: &Request, owner: &str, name: &str) -> Response {
    let mut guard = store().write().unwrap();
    let repo = match guard.repo_mut(owner, name) {
        Some(r) => r,
        None => return Response::error(404, "Repository not found"),
    };
    let stream_end = match gitcore::pkt_stream_end(&req.body) {
        Some(end) => end,
        None => return Response::error(400, "Malformed receive-pack request"),
    };
    let packets = gitcore::parse_pkts(&req.body[..stream_end]);
    let mut commands: Vec<PushCommand> = Vec::new();
    let mut sideband = false;
    let mut report_status = false;
    for (index, packet) in packets.iter().enumerate() {
        let Pkt::Data(data) = packet else { continue };
        let mut text = String::from_utf8_lossy(data).into_owned();
        if index == 0 {
            if let Some(nul) = text.find('\0') {
                let caps = text[nul + 1..].to_string();
                for cap in caps.split(' ') {
                    let cap = cap.trim_end_matches('\n');
                    if cap == "side-band-64k" {
                        sideband = true;
                    }
                    if cap == "report-status" {
                        report_status = true;
                    }
                }
                text.truncate(nul);
            }
        }
        let text = text.trim_end();
        let parts: Vec<&str> = text.split(' ').collect();
        if parts.len() == 3 {
            if let (Some(old), Some(new)) = (parse_oid(parts[0]), parse_oid(parts[1])) {
                commands.push(PushCommand { old, new, refname: parts[2].to_string() });
            }
        }
    }
    let pack_bytes = &req.body[stream_end..];
    let mut unpack_ok = true;
    let mut unpack_detail = String::new();
    if !pack_bytes.is_empty() {
        match gitcore::read_pack(repo, pack_bytes) {
            Ok(objects) => {
                for (oid, kind, data) in objects {
                    repo.objects
                        .entry(oid)
                        .or_insert_with(|| crate::store::GitObject { kind, data: std::sync::Arc::new(data) });
                }
            }
            Err(err) => {
                unpack_ok = false;
                unpack_detail = err;
            }
        }
    }
    let mut report_lines: Vec<String> = Vec::new();
    if unpack_ok {
        report_lines.push("unpack ok".to_string());
    } else {
        report_lines.push(format!("unpack error: {unpack_detail}"));
    }
    let mut changed = false;
    for command in &commands {
        if !unpack_ok {
            report_lines.push(format!("ng {} unpacker error", command.refname));
            continue;
        }
        let current = repo.refs.get(&command.refname).copied().unwrap_or(ZERO_OID);
        let verdict: Result<(), &'static str> = if command.old != current {
            Err("stale info")
        } else if command.new == ZERO_OID {
            Ok(()) // delete
        } else {
            match repo.objects.get(&command.new) {
                None => Err("unknown object"),
                Some(obj) => {
                    let new_commit = gitcore::peel_to_commit(repo, command.new);
                    if repo.objects.get(&new_commit).map(|o| o.kind) != Some(ObjKind::Commit) {
                        Err("object is not a commit")
                    } else if command.refname.starts_with("refs/heads/")
                        && obj.kind != ObjKind::Commit
                    {
                        Err("object is not a commit")
                    } else if command.old != ZERO_OID {
                        let old_commit = gitcore::peel_to_commit(repo, command.old);
                        if gitcore::is_ancestor(repo, &old_commit, &new_commit) {
                            Ok(())
                        } else {
                            Err("non-fast-forward")
                        }
                    } else {
                        Ok(())
                    }
                }
            }
        };
        match verdict {
            Ok(()) => {
                if command.new == ZERO_OID {
                    repo.refs.remove(&command.refname);
                } else {
                    repo.refs.insert(command.refname.clone(), command.new);
                }
                changed = true;
                report_lines.push(format!("ok {}", command.refname));
            }
            Err(reason) => {
                report_lines.push(format!("ng {} {}", command.refname, reason));
            }
        }
    }
    if changed {
        repo.touch();
    }
    let mut report = Vec::new();
    for line in &report_lines {
        report.extend_from_slice(&gitcore::pkt_line(format!("{line}\n").as_bytes()));
    }
    report.extend_from_slice(gitcore::PKT_FLUSH);
    let body = if sideband && report_status {
        let mut wrapped = gitcore::sideband_wrap(&report);
        wrapped.extend_from_slice(gitcore::PKT_FLUSH);
        wrapped
    } else {
        report
    };
    Response::bytes(200, "application/x-git-receive-pack-result", body)
}
