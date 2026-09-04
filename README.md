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
count only what is actually available in your country, `--accept-shorter` /
`--reject-shorter` to set the shorter-cut policy, `--verbose` for per-track
output.

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

**Shorter cuts.** The most common real outcome is that Spotify has the right
song but only a shorter cut of it — the extended mix was never published.
`ShorterVersionPolicy` decides what happens then: `Review` (default), `Accept`
(push the shorter version), or `Reject` (treat as no match). It only applies
when artist and title both score above the auto thresholds and the candidate is
at least 20s shorter, so it reframes a length disagreement and never rescues a
doubtful identity.

## Measured on a real library

27 tracks from a Beatport downloads folder, 0 search errors:

| | default | `--accept-shorter` |
|---|---|---|
| auto-push | 15 (56%) | 23 (85%) |
| needs review | 11 (41%) | 3 (11%) |
| no match | 1 (4%) | 1 (4%) |

96% of the library exists on Spotify, but only 67% of matches agree on length
within 5s. Of 6 local extended mixes, only 2 had their extended cut published;
the other 4 were verified by hand to be genuinely absent, not a search failure.
So availability is not the constraint — version fidelity is, and the whole
review queue collapses to one repeated question: accept the shorter cut or not.

## Not yet built

Phase 1 onward: OAuth (PKCE, `127.0.0.1` loopback — Spotify rejects
`localhost`), SQLite state, the review screen, playlist push, notifications.

Known API constraint: a development-mode app is capped at `limit=10` on
search and returns `400 Invalid limit` above it — which fails the whole query,
not just the excess. The client clamps to 10; widen the candidate pool with
extra queries (or `offset` paging, which is not restricted) instead.

Two things to settle before Phase 1:

- **Spotify app quota.** New apps are limited to a small allowlist of users.
  Shipping to anyone else needs an extended-quota request that Spotify reviews.
  Irrelevant for personal use; decisive if this becomes a product.
- **Refresh-token rotation.** Spotify's PKCE flow can return a new refresh token
  on every refresh. Persist it each time or auth dies silently after a while.
