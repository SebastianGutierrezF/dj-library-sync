# Apple Developer setup

One membership unlocks two unrelated things. Do them in either order.

**A. MusicKit** — lets the token service sign Apple Music developer tokens.
**B. Developer ID** — lets macOS builds be signed and notarised, so Gatekeeper
stops calling the app damaged.

> Nothing in here should be pasted into a chat, a commit, or an issue. The `.p8`
> and the `.p12` are the two files that matter; everything else can be
> regenerated, and those two cannot.

---

## A. MusicKit key

At [developer.apple.com/account](https://developer.apple.com/account) →
**Certificates, Identifiers & Profiles**.

1. **Identifiers → + → Media IDs.** Register one. This is the MusicKit
   identifier the key will be bound to. Give it a description and a reverse-DNS
   identifier such as `media.com.djlibrarysync`.
2. **Keys → + →** name it, tick **MusicKit**, then **Configure** and select the
   Media ID from step 1. Continue → Register.
3. **Download the `.p8`.** Apple allows this exactly once. If it is lost the key
   must be revoked and replaced.
4. Note the **Key ID** on the key's page — 10 characters.
5. Note the **Team ID** from **Membership** — 10 characters.

### Where those go

Render dashboard → the `dj-library-sync-service` service → Environment:

| Variable | Value |
|---|---|
| `APPLE_TEAM_ID` | the 10-character Team ID |
| `APPLE_KEY_ID` | the 10-character Key ID |
| `APPLE_PRIVATE_KEY` | the entire contents of the `.p8`, including the BEGIN/END lines |

Render accepts real newlines in a value, so paste the file as-is. The service
also accepts `\n` escapes if a host mangles them.

Then check `https://<your-service>.onrender.com/readyz` — `appleCredentials`
should turn `ok`.

---

## B. Developer ID signing and notarisation

This is what stops macOS telling testers the download is damaged.

1. **Create a signing request.** Keychain Access → menu **Keychain Access →
   Certificate Assistant → Request a Certificate From a Certificate Authority**.
   Enter your email, leave CA Email blank, choose **Saved to disk**. This
   produces a `.certSigningRequest`.
2. **Certificates → + → Developer ID Application.** Upload the request, download
   the resulting `.cer`, and double-click it to install into your login keychain.
3. **Export it with its private key.** In Keychain Access, find
   *Developer ID Application: …*, expand it so both the certificate and the key
   are selected, right-click → **Export 2 items** → `.p12`, and set a password.
   That password is `APPLE_CERTIFICATE_PASSWORD`.
4. **Base64 the `.p12`:**
   ```bash
   base64 -i ~/Downloads/certificate.p12 | pbcopy
   ```
   That is `APPLE_CERTIFICATE`, now on your clipboard.
5. **Find the signing identity string:**
   ```bash
   security find-identity -v -p codesigning
   ```
   Copy the full name, e.g. `Developer ID Application: Your Name (AB12CD34EF)`.
   That is `APPLE_SIGNING_IDENTITY`.
6. **Create an app-specific password** at
   [appleid.apple.com](https://appleid.apple.com) → Sign-In and Security →
   App-Specific Passwords. Notarisation will not accept your normal password.
   That is `APPLE_PASSWORD`.

### Where those go

GitHub → the `dj-library-sync` repo → Settings → Secrets and variables →
Actions. Six secrets, matching what `.github/workflows/release.yml` already
reads:

| Secret | From |
|---|---|
| `APPLE_CERTIFICATE` | step 4 |
| `APPLE_CERTIFICATE_PASSWORD` | step 3 |
| `APPLE_SIGNING_IDENTITY` | step 5 |
| `APPLE_ID` | your Apple ID email |
| `APPLE_PASSWORD` | step 6 |
| `APPLE_TEAM_ID` | Membership page — the same one as part A |

Or from a terminal, which keeps the values out of a browser's history. Each
command prompts for the value:

```bash
gh secret set APPLE_CERTIFICATE --repo SebastianGutierrezF/dj-library-sync
```

Once all six exist, re-run the Release workflow — or tag `v0.1.1` — and the
macOS artefacts come out signed and notarised. Notarisation adds a few minutes
while Apple's service processes the upload.

---

## Verifying the MusicKit key without exposing it

`npm run verify:apple` in the token service repo reads a `.p8` path, signs a
token, and reports whether Apple accepts it. It prints no key material.
