# Apple Music go-live

The adapter, the licence flow and the sync path are written and tested against
fixtures. **No call has ever been made to Apple's real API**, and the hosted
MusicKit page has never been loaded in a browser. This is the sequence that
finds out what that cost.

Phases are numbered because the ordering is real, not cosmetic: the smoke test
cannot run before the database wires up.

## 1 — Secrets and the database

*Render dashboard, `dj-library-sync-service`.*

- `ACTIVATION_SECRET` — `openssl rand -base64 48`. Rotating it later invalidates
  every activation token in the field, so set it once.
- `UNIPAY_CALLBACK_SECRET` — `openssl rand -hex 24`. The only thing standing
  between the payment callback and anyone minting themselves a licence.
- Create the Postgres. `render.yaml` declares `dj-library-sync-db` and
  `DATABASE_URL` wires itself from it.
- **Leave `PRICE_ID_PACK` and `PRICE_ID_UNLIMITED` unset.** `/api/plans` then
  reports `purchasable: false` and `/api/checkout` answers 503. Nothing should
  be for sale until the rest of this passes.

## 2 — unified-pay

- Rotate the Neon password (`neondb_owner`) and update `DATABASE_URL` on the
  Render unipay service.
- Merge `fix/credits-concurrency` and deploy.
- **Do not re-run the migration.** It is already applied to the branch Render
  actually uses; the default `production` branch is empty and unmigrated, so
  never promote from it.
- `curl $UNIPAY/health` → `{"status":"ok","service":"unified-pay"}`

Order matters and is already satisfied: the migration ran first, so
`ON CONFLICT (credits_tenant_user_key)` has an index to target. Deploying the
code against an unmigrated database breaks every credits call.

## 3 — Deploy the token service

Merge `feat/activation-identity`, then read `/readyz`. **Four** checks must all
say `ok`:

```
appleCredentials  billing  activation  licenceStore
```

`activation` and `licenceStore` are new; if only two appear, the old build is
still running. If `licenceStore` says "in-memory", the Postgres did not wire up
— stop, because trials will reset on every deploy and the smoke test below will
lie to you.

Set **`PUBLIC_ORIGIN`** to the origin this service is actually served from, no
trailing slash. It is load-bearing in three places: the developer token's
`origin` claim, the checkout success and cancel URLs, and the payment callback
URL.

Left unset it falls back to `http://localhost:8787`, and Apple then refuses a
developer token whose origin does not match the page it was served to. MusicKit
reports that as **"Storefront Country Code error"** — a message naming nothing
to do with the cause. `/readyz` checks for it now; it is the fifth check.

Also point `RELEASE_REPO` at `synccrate`. The repository was renamed; downloads
work today only because GitHub redirects.

## 4 — Prove the identity layer from a terminal

Before the app is involved, so that when something misbehaves later you already
know this half was sound.

| Request | Expect |
| --- | --- |
| `POST /api/trial {"deviceId":"smoke-test-0001"}` | `plan trial`, `credits 25`, `granted true` |
| the same call again | `granted false`, credits still 25 |
| `POST /api/usage` with `tracks: 25`, then one more | 402, not 500 |
| `GET /api/entitlement` with `Authorization: Bearer sebastian` | **401** |

The second row is the grant-once guard. If credits read 50, the claim and the
grant have come apart and every device can farm the trial. The last row is the
identity fix; anything other than 401 means it did not deploy.

## 5 — Connect Apple Music

*The riskiest phase.*

Run locally (`npm run tauri dev`) rather than from the released build. Then
Services → Apple Music → Start free trial → Sign in to Apple Music.

The browser opens `/auth/apple`, which must load MusicKit, run
`music.authorize()`, and redirect to `127.0.0.1:8889/callback`. Three things
that have never run together.

"Apple Music connected" appears only when both credentials are present — a
licence *and* a Music User Token. Either alone cannot reach the API.

## 6 — The first real push

Use a two or three track folder. Every track costs a credit and there are 25; a
wrong field name is just as visible on two.

1. Switch the header target to Apple Music and match.
2. **Check the durations read real lengths and not `0:00` before pushing.**
   `durationInMillis` parsed as zero once already, and duration carries 0.24 of
   the match score — every result would look like a duration mismatch and the
   matcher would take the blame.
3. Push one track, then open Apple Music and confirm the playlist and the track.
4. Push the same track again → `added 0, skipped 1`, and still **one** playlist.
   A second playlist means the writability fix regressed.
5. Confirm the credit count dropped by exactly one. Usage is reported after the
   sync log is written, so a failure logs to the console rather than failing the
   push — a silent console and an unchanged count means it never arrived.

## What will probably break

Ordered by likelihood, so you recognise it rather than debug it from scratch.

1. **The MusicKit page has never been loaded.** MusicKit may fail to initialise,
   Apple may reject the `origin` claim, or the redirect may never fire.
2. **More Apple field names may be wrong.** Every field came from documentation
   rather than an observed response. The ones that survived are simply the ones
   no test disproved.
3. **403 on every library call** means the Apple ID has no active Apple Music
   subscription. Confusing, because catalogue search keeps working without one —
   matching looks healthy right up until the push.
4. **The storefront lookup runs first** and every catalogue URL is scoped to it,
   so if `/v1/me/storefront` fails, search fails with it and the error will look
   like a search problem.

## Starting over

Nothing here is destructive. Clearing the licence and the Apple token from
Keychain Access — both under `dj-library-sync` — puts the app back to the start.
The trial is keyed by the device id in `config.json`; delete that field to claim
a fresh one.
