# DJ Library Sync

Watches the folder your music downloads land in, works out which tracks exist
on a streaming service, and adds them to a playlist — so new records are on
your phone before you've had time to sit down and listen to them.

Built for DJs who buy from Beatport and similar stores, where a week's
downloads pile up unheard because curating them takes an evening you don't
have.

```
31 tracks in ~/Downloads/Beatport

  auto-push        29    93.5%
  needs review      1     3.2%
  no match          1     3.2%
```

## What makes it work

Matching a local file to a streaming catalogue sounds trivial and isn't. Three
things do most of the heavy lifting:

**Mix descriptors are parsed, never stripped.** `Track (Extended Mix)` and
`Track (Radio Edit)` are different recordings. A matcher that strips the
descriptor to "normalize" the title will match a 3-minute radio edit to your
7-minute extended mix at 100% confidence, with nothing to warn you. Titles are
split into a base name, a version kind, and a remixer, and each is scored
separately.

**Duration is a first-class signal.** It is what separates an extended mix from
a radio edit when the strings are identical, and it is why an unlabelled
candidate whose length lands within two seconds can still be trusted.

**Not-found is a real answer.** Roughly a third of a typical Beatport folder
exists on Spotify only as a shorter cut — the extended mix was never published.
That is a decision, not an error, so it is a setting rather than a silent
substitution.

Verdicts are `auto` (push it), `review` (park it — it never blocks the batch),
and `no_match` (logged for future audio fingerprinting).

## Install

Requires [Rust](https://rustup.rs) and [Node](https://nodejs.org) 18+.

```bash
git clone https://github.com/<owner>/dj-library-sync
cd dj-library-sync
cargo build --release
npm install
```

## Connecting Spotify

Spotify limits any one developer app to **five users**, and extended access is
open only to organisations with 250k+ monthly actives. There is no way to ship
a shared Spotify app, so this one runs on an app *you* own. It is free and
takes a couple of minutes.

1. Create an app at [developer.spotify.com/dashboard](https://developer.spotify.com/dashboard)
2. Add `http://127.0.0.1:8888/callback` as a Redirect URI — it must be the
   `127.0.0.1` form, Spotify rejects `localhost`
3. Under Settings → User Management, add your own Spotify account
4. Copy the Client ID into the app, or into `.env` for the CLI

You need Spotify **Premium**: development-mode apps stop working without it.

Only the Client ID is ever requested. This uses PKCE, which exists so that
desktop apps don't need a client secret — and a secret stored on a user's
machine isn't secret anyway.

## Using it

The desktop app watches a folder and gives you a dashboard, a review list and a
playlist picker:

```bash
npm run tauri dev
```

Everything is also available headless:

```bash
djls match ~/Downloads/Beatport --csv report.csv   # hit rate + per-track report
djls push  ~/Downloads/Beatport --accept-shorter   # add confident matches
djls misses                                        # tracks found nowhere
djls stats                                         # what the database holds
```

`--accept-shorter` pushes the shorter cut when the extended mix isn't
published. On a real 31-track library that moved auto-push from 56% to 85%,
because it collapses eight separate judgement calls into one preference.

## How it fits together

```
crates/djls-core/    tags, normalization, matching, database, watcher, platforms
crates/djls-cli/     djls — headless matcher, and how the matcher gets measured
src-tauri/ + src/    Tauri v2 desktop app
```

All logic lives in `djls-core`. The CLI and the app are thin shells over it, so
anything the matcher learns is shared by both — and the matcher can be measured
against a real library without any UI existing.

Local state lives in SQLite, so a second run over the same folder costs no API
calls. Identity is not the file path: Rekordbox and Serato rewrite tags on
import and DJs move files between folders, so a moved file keeps its history
while a genuinely re-tagged one is matched again.

## Adding a streaming service

Implement `MusicPlatform` in `crates/djls-core/src/platform.rs`. Nothing above
the client is platform-specific, so a new service does not touch the matcher.

`CredentialModel` records whether a platform can be served from one developer
account (`Hosted`, one-click sign-in) or requires each user to register their
own (`UserProvided`, as Spotify does). See [CONTRIBUTING.md](CONTRIBUTING.md).

## Spotify's February 2026 Development Mode changes

Development Mode lost a lot of surface area, and this app targets the reduced
set. The
[migration guide](https://developer.spotify.com/documentation/web-api/tutorials/february-2026-migration-guide)
is authoritative; what affects this project:

| Changed | Used here as |
|---|---|
| `POST /users/{id}/playlists` removed | `POST /me/playlists` |
| `POST\|GET /playlists/{id}/tracks` | `/playlists/{id}/items` |
| playlist `tracks` field renamed | `items` (entries carry `item`, not `track`) |
| search `limit` max 50 → **10** | clamped; widened with extra queries |
| `GET /me` drops `country`, `product` | not relied on |

**A removed endpoint returns a bare `403 Forbidden`** — not a scope error, and
nothing a dashboard setting can fix. `"Insufficient client scope"` is the
genuinely scope-related 403. The two look alike and need opposite fixes; this
cost a full debugging session to learn.

## Status

Working: folder watching, tag extraction, matching, review, Spotify auth and
playlist push, local state.

Not built yet: Apple Music and TIDAL adapters, audio fingerprinting for the
no-match queue, background sync with notifications.

## Licence

MIT — see [LICENSE](LICENSE).
