# Backlog

What is deliberately not done at v0.1.0, and why. Ordered by what blocks what.

## Built but never run against Apple

The adapter, the licence flow and the sync path are written and tested against
fixtures. **No call has ever been made to Apple's real API**, and the hosted
MusicKit page has never been loaded in a browser. Every field name in
`apple.rs` came from documentation rather than an observed response;
`durationInMillis` was already silently parsing as zero before a test caught
it, and the ones that survived are simply the ones no test disproved.

The go-live sequence, and the failure modes to expect at each step, are in
[go-live.md](go-live.md).

## Needed before anyone can pay

- **Rate limiting on `/api/trial` and `/api/activate`.** Nothing stops a script
  claiming trials for invented device ids, and each one writes a credit grant
  into unified-pay's ledger. This is now the most important open item: it is
  abuse of the billing system, not just of the product.
- **No buy button.** `POST /api/checkout` exists and works; the landing page
  still says "Coming soon" and nothing calls it. Waiting on Apple being proven
  and on prices being set.
- **No account page.** A seat cannot be freed without editing the database, so
  a user who reinstalls three times is stuck.
- **Licence recovery.** Only the hash is stored, so a lost key cannot be
  reissued. Deliberate — it is what avoids storing keys in the clear or running
  email infrastructure — but it needs an answer before real customers exist.
- **unified-pay grants a credit per dollar on the first payment only**, on top
  of the plan grant, so month one differs from every month after. Either price
  so the two agree, or add a per-tenant flag there to skip it.

## Settled

- **Where `userId` comes from** — a licence key issued at purchase, exchanged
  for a short-lived activation token the machine uses thereafter. The client no
  longer names itself; the identity is read out of a signature this service
  produced. Design and rationale in the service repo's
  `docs/billing-identity.md`.
- **The Apple Music adapter**, the licence flow in the desktop app, and the
  platform-agnostic sync path. See "Built but never run against Apple" above.
- **Signed and notarised macOS builds** — shipping since v0.1.1.

## Correctness and coverage

- **macOS 12 refuses the signed installer.** Finder shows the prohibitory badge
  and refuses to launch, although the binary runs fine when invoked directly
  and reports `minos 10.13`, a valid signature and a notarised Gatekeeper
  verdict. Untested hypothesis: pinning `MACOSX_DEPLOYMENT_TARGET` in the
  release workflow, since the runner builds against the macOS 26 SDK.
- **`RELEASE_REPO` still names `dj-library-sync`.** The repository was renamed
  to `synccrate`; downloads work only because GitHub redirects.

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
