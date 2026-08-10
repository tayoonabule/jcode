# Local changes in this checkout

This install carries local commits that are **not** in upstream
`1jehuang/jcode`. This file records what they are and how to keep them working,
because a future you (or agent) will otherwise wonder why this repo is ahead of
origin.

Last updated: 2026-08-10

## What was added

Support for **remote MCP servers** over Streamable HTTP with browser OAuth 2.0,
so hosted endpoints work without an `mcp-remote` stdio shim.

- `crates/jcode-base/src/mcp/http.rs` (new) — Streamable HTTP transport: one
  `POST` per JSON-RPC message, JSON and SSE response bodies, `mcp-session-id`.
- `crates/jcode-base/src/mcp/oauth.rs` (new) — protected-resource discovery from
  the `WWW-Authenticate` challenge, authorization-server metadata, dynamic client
  registration, loopback PKCE S256, refresh, and requesting the scopes a server
  advertises.
- `crates/jcode-app-core/src/update.rs` — updates rebase local commits onto
  upstream instead of failing `--ff-only`. Without this, **this very checkout
  could never update again**.
- `scripts/check_local_commits_rebase.sh` (new) — reports whether these commits
  still replay onto upstream, before an update needs it.
- Plus test files, and small edits to `mcp/client.rs`, `mcp/protocol.rs`,
  `mcp/mod.rs`, `tool/mcp.rs`.

## Configured servers

`~/.jcode/mcp.json` (outside this repo, survives updates):

```json
{
  "servers": {
    "granola": { "type": "http", "url": "https://mcp.granola.ai/mcp" },
    "semrush": { "type": "http", "url": "https://mcp.semrush.com/v2/mcp" },
    "atlassian": { "type": "http", "url": "https://mcp.atlassian.com/v1/mcp/authv2" },
    "gmail": { "type": "http", "url": "https://gmailmcp.googleapis.com/mcp/v1" },
    "google-drive": { "type": "http", "url": "https://drivemcp.googleapis.com/mcp/v1" },
    "google-workspace": { "type": "http", "url": "https://workspacemcp.googleapis.com/mcp/v1" },
    "twenty": {
      "type": "http",
      "url": "https://crm.drewl.com/mcp",
      "headers": { "Authorization": "Bearer ${TWENTY_API_KEY}" }
    }
  }
}
```

OAuth tokens live in `~/.jcode/mcp-auth/<server>.json`, mode 0600, with refresh
tokens. Deleting one forces a fresh browser sign-in for that server.

The HTTP transport retries once when a server returns either a normal OAuth
`401` challenge or an auth-like `403` body such as an unregistered-caller
message. A genuine permission `403` is surfaced unchanged instead of causing
an endless re-authentication loop.

Atlassian uses OAuth 2.1 dynamic registration. Google Gmail, Drive, and
Workspace Universal Search require a Google Cloud OAuth web client. The config
already contains Google's authorization endpoint, token endpoint, scopes, and
the fixed loopback callback `http://127.0.0.1:1455/callback`; add that callback
to the Google OAuth client and then add `clientId` and `clientSecret` to each
Google server's `oauth` block. Twenty uses the `TWENTY_API_KEY` environment
variable so its bearer token is never written here.

The newly added servers are intentionally `shared: false` until their
credentials are configured. They connect on demand and do not add idle memory
or open browser prompts during daemon startup.

### Acceptance notes

- The stored Google OAuth token was accepted by the Gmail REST API (`HTTP 200`)
  and returned the account's label list.
- The same token was accepted by the Google Drive REST API (`HTTP 200`) and the
  acceptance probe returned zero files.
- The real `google-workspace` MCP search path reached Google's gateway but was
  rejected with `The caller does not have permission`, so that remaining issue
  is provider-side rather than a local OAuth or transport failure.
- The Gmail MCP path returned the same provider permission error. Drive OAuth was
  then completed for `tayo@drewl.com`; the token is stored locally with mode 0600.
  The `drivemcp.googleapis.com` service is now enabled in the project, but a
  read-only Drive MCP probe still returns `The caller does not have permission`.
  The underlying Drive/Gmail APIs and MCP services are enabled, and Google
  Workspace Admin shows the OAuth client as Trusted. Google's documentation
  lists these MCP servers as Developer Preview features, so the remaining
  provider-side boundary may require enrolling `tayo@drewl.com` and project
  `drewl-366215` in the Workspace Developer Preview Program. This is not a
  local OAuth or transport failure.
- The real Twenty MCP read-only path completed with `HTTP 200` and zero matches
  using the local `twentykey.txt` only through `TWENTY_API_KEY`; the first
  uppercase comparator was rejected by the API validator and the lowercase retry
  succeeded. The key file was not copied into the repo or printed.

## Keeping updates working

Updates are expected to keep working: `jcode` rebases these commits onto
upstream automatically. Two things to know.

1. **Check before trusting it.** After an upstream release lands:

   ```sh
   ./scripts/check_local_commits_rebase.sh
   ```

   Exit 0 means the commits still replay cleanly. Exit 1 names the conflicting
   files. It uses a throwaway clone and never touches this checkout.

2. **If it ever conflicts**, resolve once:

   ```sh
   git pull --rebase
   ```

Conflict risk is low but real: 8 of the 13 changed files are new (no conflict
surface), and the 5 shared files were each touched once in upstream's last 60
commits. Replay onto `v0.74.0` and onto the current base was verified clean.

## History

All of this is deliberately **one commit** (`git log origin/master..HEAD` shows
a single entry). One commit replays onto a new upstream far more predictably
than a chain of ten, and it is easy to inspect or drop as a unit.

The pre-squash history is kept on the local branch `backup-before-squash`, in
case the individual steps are ever useful. It can be deleted once you are
confident: `git branch -D backup-before-squash`.

## Note on this repo

This checkout is **shallow** (`.git/shallow` exists). That is why clones of it
cannot push, which matters if you script anything around it.

Pushing to `origin` is denied (403). To upstream this work, fork and open a PR;
a prepared description sits in `.git/PR_MCP_HTTP.md`.

## Verification already done

- 11 injected faults (dropped bearer header, broken PKCE verifier, skipped
  persistence, no client-id reuse, non-loopback redirect, dropped `Accept`,
  dropped session id, ignored SSE content-type, per-server HTTP client, and two
  scope mutations) each fail the tests.
- Memory: an idle remote server costs ~2.6 KB. It was 199 KB before a single
  shared `reqwest::Client`; `mcp_http_memory` guards the regression.
- Live: Granola and Semrush both authenticate and return real data, including
  called concurrently from one session.
