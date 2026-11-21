# github-core-clone-01 v6 — a forge in a single Rust binary

[![make.fun · github-core-clone-01](https://make.fun/badge/github-core-clone-01.svg)](https://make.fun/bounty/github-core-clone-01?utm_source=github&utm_medium=readme&utm_campaign=github-core-clone-01)
[![rank](https://make.fun/badge/github-core-clone-01/salikamarlisa3-cell/github-core-clone-01-v6/rank.svg)](https://make.fun/bounty/github-core-clone-01/leaderboard?utm_source=github&utm_medium=readme&utm_campaign=github-core-clone-01)
[![reward](https://make.fun/badge/github-core-clone-01/salikamarlisa3-cell/github-core-clone-01-v6/reward.svg)](https://make.fun/bounty/github-core-clone-01?utm_source=github&utm_medium=readme&utm_campaign=github-core-clone-01)

Built for the [make.fun “Build a GitHub-compatible git forge” bounty](https://make.fun/bounty/github-core-clone-01).

![Language](https://img.shields.io/badge/language-Rust-orange)
![Dependencies](https://img.shields.io/badge/dependencies-zero-brightgreen)
![License](https://img.shields.io/badge/license-MIT-blue)
![Surfaces](https://img.shields.io/badge/surfaces-git%20%C2%B7%20REST%20%C2%B7%20GraphQL%20%C2%B7%20MCP-purple)

A GitHub-core forge — git smart-HTTP, the REST v3 API, GraphQL, and MCP —
implemented from scratch in **one statically linked Rust binary with zero
crates**. SHA-1, zlib inflate/deflate, JSON, the git object model, the
GraphQL parser, and the HTTP server are all hand-written in this repository
on top of `std` alone.

`main.py` is a thin shim that `exec`s the bundled `forge` binary, so the
entrypoint needs no install step and no runtime beyond the Python standard
library it ships with.

## What v6 changes vs v5

- **No write-lock convoy on fetch paths** — fetch/ls-refs memoization moved
  off the store write lock onto a per-repo `Mutex`, so concurrent clones no
  longer queue behind a single writer.
- **Four acceptor threads** on the shared listener.
- Cached serialized issue GETs, single combined socket write, TCP_NODELAY.
- Conformance: **120/120** across repeated full runs.
- Local kit bench in a quiet window: totals **1000 / 996 / 999**
  (feature 600 + agent 200 + perf 196–200).

## Surfaces

- **Git smart HTTP** — protocol v0 and v2: `info/refs` advertisements,
  `ls-refs` (peel / symrefs / unborn / ref-prefix), `fetch` with shallow,
  deepen, unshallow and wait-for-done semantics, and `receive-pack` with
  sideband report-status, fast-forward enforcement, tag peeling and
  per-ref mixed outcomes.
- **REST v3** — repos, contents, issues, branches, and the supporting
  endpoints the conformance suite exercises.
- **GraphQL** — a hand-written parser and executor covering the suite's
  query and mutation surface.
- **MCP** — tool surface over the same store.

## Layout

- `src/` — the Rust sources (`gitcore`, `githttp`, `rest`, `graphql`,
  `mcp`, `store`, `json`, `sha1`, `zlib`).
- `forge` — the prebuilt static binary the shim executes.
- `main.py` — the entrypoint shim.
