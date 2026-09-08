# Contributing

## Getting set up

```bash
cargo build
npm install
cargo test -p djls-core -p djls-cli
```

Run the tests that way rather than a bare `cargo test` — the workspace includes
the Tauri app, and linking it adds minutes for no benefit.

For anything that talks to Spotify, copy `.env.example` to `.env` and follow
the setup steps in the README. `djls scan` and `djls parse` need no network and
no credentials, so most matcher work can be done offline.

## Where things live

| Path | What it does |
|---|---|
| `crates/djls-core/src/normalize.rs` | Title parsing, mix descriptors, string similarity |
| `crates/djls-core/src/matcher.rs` | Candidate scoring and verdicts |
| `crates/djls-core/src/tags.rs` | Reading tags off disk (MP3, AIFF, WAV, FLAC, M4A) |
| `crates/djls-core/src/watcher.rs` | Debounced folder watching |
| `crates/djls-core/src/spotify.rs` | Spotify client |
| `crates/djls-core/src/platform.rs` | The `MusicPlatform` trait |
| `crates/djls-core/src/db.rs` | SQLite state |
| `crates/djls-cli/` | `djls` — also how the matcher is measured |
| `src-tauri/`, `src/` | Desktop app |

## Adding a streaming service

Implement `MusicPlatform`. Nothing above the client is platform-specific, so a
new service should not require touching the matcher — if it does, that is worth
discussing in the PR, because it probably means the trait is wrong.

Set `CredentialModel` honestly:

- `Hosted` — one developer account can serve every user, so sign-in is one
  click.
- `UserProvided` — the platform caps how many users one app may serve, so each
  user registers their own. Spotify is this, and it is why its setup is longer.

Then add a `PlatformInfo` const and list it in `PlatformInfo::ALL`; the connect
screen picks it up automatically.

## Working on the matcher

The matcher is the part most worth getting right and the easiest to break
subtly, because a bad match is silent — it looks exactly like a good one until
you're at the gym listening to a radio edit.

Two rules that are easy to violate with good intentions:

**Don't strip mix descriptors to normalize a title.** It looks like sensible
cleanup and it is the single most damaging thing you can do here: it makes
different recordings compare equal at full confidence. Parse them into a field
instead.

**Don't cache a failed API call as a result.** A network error is not evidence
that a track is absent from a catalogue. Conflating the two turns an outage
into a confidently wrong report.

Every behaviour above is pinned by a test in `matcher.rs`. If you change
scoring, run the suite and expect some of them to fail — then decide which is
wrong, the code or the expectation. Both happen; say which in the PR.

## Testing against real files

`djls scan <folder>` and `djls parse "<title>"` need no credentials and are the
fastest way to check parsing behaviour:

```bash
djls parse "Body (Adam Beyer's Extended Remix)"
```

For matcher work, `djls match <folder> --csv report.csv` writes per-signal
scores and the runner-up candidate for every track, which is usually enough to
see why a decision went the way it did.

## Pull requests

- Keep the test suite green, and add a test for behaviour you are relying on.
- Explain *why* in the commit message. What changed is in the diff; the reason
  is not.
- If you found something surprising about a platform's API, write it down in
  the README. Several days of this project were spent rediscovering things that
  were documented somewhere unobvious.
