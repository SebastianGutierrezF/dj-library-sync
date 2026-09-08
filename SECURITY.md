# Security

## Reporting

Please report vulnerabilities privately through GitHub's "Report a
vulnerability" flow rather than opening a public issue.

## How credentials are handled

- **OAuth tokens** are stored in the operating system credential store —
  Keychain on macOS, Credential Manager on Windows, Secret Service on Linux —
  never in a file.
- **No client secret is used or requested.** Authentication is Authorization
  Code with PKCE, which exists so that desktop applications don't need one. A
  secret stored on a user's machine is not a secret.
- **Refresh tokens rotate.** A refresh that returns a new refresh token
  persists it immediately; dropping a rotated token is a common way for auth to
  fail silently days later.
- **The redirect listener** binds `127.0.0.1` only, accepts a single callback,
  and rejects a response whose `state` does not match the one it generated.
- **The client ID is not a secret** under PKCE, which is why the app can ask
  users to paste one and why it may be shipped in a build.

## What the app can reach

It reads audio files in the folder you point it at, and it can create and add
to playlists on a connected account. It does not delete playlists, remove
tracks, or read anything from your library beyond the playlists you own.
