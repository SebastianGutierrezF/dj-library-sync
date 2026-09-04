# DJ Library Sync

Watches a downloads folder, matches new tracks against Spotify, and pushes them
to a playlist. This repo currently covers **Phase 0** (scaffold, folder watcher,
tag extraction) and **Phase 0.5** (headless matcher + match-rate report).

## Why the CLI exists

The matcher is the make-or-break piece, so it is built headless and measurable
first. Before writing any more UI, run `djls match` against your real downloads
folder. That number — what percentage of your library actually resolves on
Spotify — should decide how much further this project is worth taking.

## Layout

```
crates/djls-core/    tags, normalization, matching, Spotify client, folder watcher
crates/djls-cli/     `djls` — headless scan / match / watch
src-tauri/           Tauri v2 desktop app (Phase 0 shell)
src/                 React frontend
```

All logic lives in `djls-core`. The app and the CLI are both thin shells over
it, so anything the matcher learns is shared by both.

## Setup

Rust and Node are both required.

```bash
cd ~/dj-library-sync && cargo build
```

Run the tests with `cargo test -p djls-core -p djls-cli` rather than a bare
`cargo test` — the workspace includes the Tauri app, and linking it makes a
full-workspace test run take minutes for no benefit.

```bash
npm install --cache ~/.npm-cache-cc
```

## The CLI

```bash
cargo run -p djls-cli -- scan ~/Downloads/Beatport
```

Reads tags and shows how each title parses — artist, base title, version
descriptor, length, and whether an ISRC is present. No network.

```bash
cargo run -p djls-cli -- match ~/Downloads/Beatport --csv report.csv
```

Matches against Spotify and prints the hit rate: auto-push / needs review /
no match, broken down by match method, with the reasons tracks missed.

Needs credentials — copy `.env.example` to `.env` and fill in a client ID and
secret from https://developer.spotify.com/dashboard. This command uses the
Client Credentials flow, so no redirect URI and no user account are involved.

Useful flags: `--limit 100` for a fast read on a huge folder, `--market GB` to
count only what is actually available in your country, `--verbose` for
per-track output.

```bash
cargo run -p djls-cli -- watch ~/Downloads/Beatport
cargo run -p djls-cli -- parse "Grey (Adam Beyer's Extended Remix)"
```

## The app

```bash
npm run tauri dev
```

Pick a folder; it scans what is there and then watches for new arrivals. Files
are only read once their size has held steady, so partial downloads are never
parsed.

## Matching design

Four signals, weighted: artist `0.32`, base title `0.30`, duration `0.24`, mix
agreement `0.14`. Thresholds live in `Thresholds` in `matcher.rs`.

Two decisions worth knowing about:

**Mix descriptors are parsed into a field, never stripped.** `Track (Extended
Mix)` and `Track (Radio Edit)` are different recordings. Stripping the
descriptor makes a 3-minute radio edit match a 7-minute extended mix at 100%
confidence, with no signal that anything is wrong. Instead the title splits into
`base` + `kind` + `remixer`, and the descriptor is scored separately.

**Duration is a first-class signal.** It is what resolves extended-vs-original
when the strings are identical, and it is why an unlabelled Spotify candidate
whose length lands within 2s of the local file can still be auto-pushed. A
different remixer is a hard reject regardless of string similarity.

Verdicts are `auto` (push it), `review` (park it — never block the batch), and
`no_match` (the future AcoustID queue).

## Not yet built

Phase 1 onward: OAuth (PKCE, `127.0.0.1` loopback — Spotify rejects
`localhost`), SQLite state, the review screen, playlist push, notifications.

Two things to settle before Phase 1:

- **Spotify app quota.** New apps are limited to a small allowlist of users.
  Shipping to anyone else needs an extended-quota request that Spotify reviews.
  Irrelevant for personal use; decisive if this becomes a product.
- **Refresh-token rotation.** Spotify's PKCE flow can return a new refresh token
  on every refresh. Persist it each time or auth dies silently after a while.
