# Backlog

What is deliberately not done at v0.1.0, and why. Ordered by what blocks what.

## Blocked on the Apple Developer Program

Enrollment is pending; one membership unlocks both items.

- **Apple Music adapter** — implement `MusicPlatform` for Apple Music. The auth
  design is settled in [apple-music-auth.md](apple-music-auth.md); the adapter
  itself is unwritten. Note Apple's catalogue is per *storefront*, which must be
  resolved per user rather than assumed — it is mandatory where Spotify's
  `market` is optional.
- **Signed and notarised macOS builds.** Until then Gatekeeper tells testers the
  app is damaged. The release workflow already reads six `APPLE_*` secrets; they
  just do not exist yet.
- **Apple credentials on the token service** — `APPLE_TEAM_ID`, `APPLE_KEY_ID`,
  `APPLE_PRIVATE_KEY`. `/readyz` reports them red until set.

## Needed before anyone can pay

- **Decide where `userId` comes from.** The token service trusts whatever the
  desktop app sends. It needs to be stable and unguessable — a licence key
  issued at purchase, not an email or an install id — because anyone who knows
  a `userId` can spend that balance. This is the single most important open
  decision.
- **The desktop app does not talk to the token service at all.** No licence
  storage, no `/api/developer-token` call, no usage reporting. Nothing is wired.
- **No way to buy credits.** unified-pay can take a payment and top up a
  balance; nothing in this product initiates that.
- **Rate limiting on the token service.** A leaked `userId` can currently
  request tokens without limit.

## Correctness and coverage

- **Cached rows cannot be reviewed.** Candidates are not persisted, so a row
  restored from the database offers no alternatives and the review UI shows
  nothing to choose between. `--rescan` is the workaround. Either persist the
  top few candidates or re-query on demand when a row is expanded.
- **Windows is untested.** CI compiles it; the app has never been run there. The
  folder watcher and the keychain backend are the likely places to break.
- **The no-match queue does nothing yet.** `djls misses` lists tracks nothing was
  found for. Audio fingerprinting (AcoustID) was always the intended answer and
  is not started.
- **Auto-match on arrival is manual.** The watcher now surfaces new downloads and
  offers a button. The stated goal was for tracks to appear without anyone
  opening anything, which means matching on a debounce and a native notification
  when it finishes.

## Product and distribution

- **No auto-update.** Testers will have to download new builds by hand. Tauri has
  an updater; it needs signing keys and a release feed.
- **Spotify setup is five manual steps** and each is a place to drop out. For the
  first testing round, consider shipping *your* client ID and adding testers to
  the app's User Management instead — five slots, and it removes the setup
  entirely for them. It does not scale past five, which is the whole reason the
  bring-your-own flow exists, but it makes the first round much easier.
- **The landing page's download link 404s** until a release exists and the repo
  is public.
- **Actions minutes.** macOS runners bill at a multiplier on private repos, and a
  release builds two macOS targets plus Windows and Linux. Making the repo public
  removes the cost entirely.

## Smaller things worth doing

- Persist the chosen candidate per track, so an override survives a restart.
- `same_recording` compares raw names for the runner-up ambiguity check but
  parsed base titles elsewhere; unify on the parsed form.
- The CSV report writes per-signal scores but the desktop app never shows them —
  useful when a verdict looks wrong.
- `PlatformInfo::TIDAL` is listed as coming soon, but third-party playlist
  *writes* were never confirmed available. Verify before promising it.
- The service picker offers no way to disconnect an account; only the CLI has
  `djls logout`.
