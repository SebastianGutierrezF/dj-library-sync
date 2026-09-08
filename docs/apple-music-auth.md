# Apple Music: how authentication has to work

Written before implementing the adapter, because the answer changes the shape
of the product rather than just the code.

## Two tokens, obtained very differently

Every Apple Music API request needs a **developer token**. Requests touching a
user's library also need a **Music User Token**.

### Developer token — straightforward

A JWT we can mint ourselves:

- header: `alg: ES256`, `kid` = 10-character Key ID
- claims: `iss` = 10-character Team ID, `iat`, `exp` (≤ 6 months out)
- signed with the MusicKit `.p8` private key
- optional `origin` claim restricting which web origins may use it

ES256 only — Apple returns 401 for anything else.

### Music User Token — the constraint

There is **no REST endpoint that issues one**. Apple documents exactly three
sources:

| Platform | How |
|---|---|
| iOS / macOS / tvOS / watchOS | MusicKit for Swift, automatic |
| Web | MusicKit on the Web (JS), automatic |
| Android | MusicKit for Android, manual retrieval |

A Rust desktop app is none of these. There is no supported path that avoids a
web context — this is not a matter of finding a better endpoint.

## What that means for a Tauri app

Three options, and only one is sound.

**Run MusicKit JS inside the app's own webview.** Tempting, since there is
already a webview. But the Tauri webview's origin is `tauri://localhost`, not
an `https://` origin, and Apple's authorization flow is a popup handing a token
back by `postMessage`. Relying on that working — across macOS and Windows
webviews, across storage partitioning changes — is betting the paid tier on
undocumented behaviour. If Apple tightens origin handling, the product breaks
with no recourse.

**Bridge to native MusicKit (Swift) on macOS.** Supported and clean, but
macOS-only, and it means Objective-C/Swift interop from Rust for one platform
while Windows still needs another answer.

**Do the authorization on a hosted https page, hand the token back to the
loopback listener.** ← this one.

## The chosen flow

```
desktop app                 system browser              our web page
     |                            |                          |
     |-- open auth URL ---------->|                          |
     |                            |-- loads MusicKit JS ---->|
     |                            |   music.authorize()      |
     |                            |<-- Music User Token -----|
     |<-- GET 127.0.0.1:8888/callback?music_user_token=... ---|
     |                                                        |
   store in OS keychain
```

Three things make this the right answer:

1. **It is the documented path.** MusicKit on the Web is a supported way to get
   the token; nothing here depends on undocumented webview behaviour.
2. **The loopback listener already exists.** `auth.rs` binds `127.0.0.1`,
   validates `state`, serves a completion page and shuts down — built and
   tested for Spotify's PKCE flow, and reusable as-is.
3. **The hosted page has to exist anyway.** The developer token must be signed
   with a private key that cannot ship in an open-source desktop app, so a
   server holding that key is already required for the hosted tier. The auth
   page is the same web property doing double duty.

Consent still happens in the user's real browser, where they may already be
signed in to Apple, and where credentials never touch a surface this app
controls. That matches the Spotify flow.

## Free and paid, without a second architecture

`CredentialModel` covers Apple Music the same way it covers Spotify:

- **`UserProvided` (free)** — the user has their own Apple Developer membership
  and supplies Team ID, Key ID and `.p8`. The app signs developer tokens
  locally. Costs us nothing; realistic only for people who already pay Apple
  $99/yr.
- **`Hosted` (paid)** — the app requests a short-lived developer token from our
  service, which signs with our key. This is also where metering belongs: every
  paid operation already passes through a server, so the count lives there
  rather than in a local SQLite file the user can edit.

A fork can copy every line of this repository and still not issue tokens,
because the private key is not in it.

## Consequences to plan for

- **A web property is a hard dependency of the paid tier**, not a nice-to-have.
  It signs developer tokens and hosts the authorization page.
- **Developer tokens should be short-lived** even though Apple permits six
  months. A leaked six-month token is a six-month problem.
- **Set the `origin` claim** on hosted tokens to our own domain, so a token
  lifted from a browser cannot be replayed from anywhere else.
- **The user needs an active Apple Music subscription**, exactly as Spotify's
  development mode needs Premium.
- **Storefronts are explicit.** Apple Music catalogue requests are per
  storefront (`/v1/catalog/{storefront}/...`), so the adapter must resolve the
  user's storefront rather than assuming one — the closest analogue to
  Spotify's `market`, but mandatory rather than optional.
