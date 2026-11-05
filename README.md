# Teams MCP — Rust

An unofficial, read-only Microsoft Teams MCP server for Codex and Claude Code.
The default source is **your existing browser's approved Teams session**. A Rust
binary mints short-lived access tokens from that session, then calls Teams over
HTTPS. Browser cookies and refresh tokens stay in the browser; access tokens live
only in the MCP process's memory. No Node, Playwright, browser download, separate
OAuth app registration, or credential-export service is required.

Managed headless Chrome and the native Teams App remain explicit alternatives.
Normal tool calls never open a visible sign-in window. `TEAMS_TRANSPORT=page` runs
each call inside the selected Teams page instead of using in-memory access tokens.

## Build and sign in

```sh
cd teams-mcp
cargo install --locked --path .   # or: cargo build --release --locked
teams-mcp auth login
teams-mcp --status
```

Run these commands from the extracted source directory or your Git clone.
`cargo install` places the binary at `~/.cargo/bin/teams-mcp`; tagged releases also
publish prebuilt macOS and Linux binaries. Building requires Rust 1.89+.

For the default existing-browser source, open Teams in your intended Chrome
session, enable remote debugging at `chrome://inspect/#remote-debugging`, and
approve the connector's connection. `auth login` explains the selected source's
setup and waits for a usable Teams session; complete sign-in and MFA in that
browser. It never starts managed Chrome or copies your browser profile.
See [authentication sources](#authentication-sources) for endpoint selection and
the managed headless and native App alternatives.

`auth status` reports local configuration and setup only. A profile or debugging
endpoint can exist while Microsoft requires sign-in again. `--status` and the
`teams_status` tool verify the selected session; individual API calls still check
their own permissions. Keep the same source, account and tenant configuration for
setup and MCP clients. Set **both** `TEAMS_ACCOUNT` and `TEAMS_TENANT_ID` to bind
calls to an intended identity, especially for invited guests.

Local status includes `authentication.sessionVerified: false` and a state such as
`unverified`, `sign_in_required` or `signing_in`. Managed `profileExists` and the
compatibility `setupComplete` field describe the profile, not the validity of its
Microsoft session. Browser and App status do not inspect a managed profile.
A successful `teams_status` reports `authentication.state: "verified"` and
`sessionVerified: true`; `apiAccess: "not_checked"` means individual remote API
permissions still need their actual reads.

## Codex and Claude Code

With no action argument the binary speaks MCP over stdin/stdout. Initialization
and tool discovery do not require Chrome, a network connection, or a signed-in
session. Authentication failures are structured tool errors.

Use [examples/codex.toml](examples/codex.toml) or
[examples/claude.json](examples/claude.json), replacing `/absolute/path/to/teams-mcp`
with the checkout path on your computer. From the project directory, register
Claude Code with:

```sh
claude mcp add --scope project teams_rust -- "$(pwd)/target/release/teams-mcp"
```

For Codex:

```sh
codex mcp add teams_rust -- "$(pwd)/target/release/teams-mcp"
```

Use a 90-second tool timeout to allow a cold session check. One MCP process keeps
one CDP connection and the installed bridge across calls, and concurrent tool calls
run in parallel. Set the same profile,
account and tenant environment values in both clients when customizing them.
These commands register the connector; they do not sign in to Teams.
Claude Desktop accepts the same `mcpServers` JSON shape; its UI is not tested here.
Existing global MCP registrations are not automatically replaced.

Both clients receive the same server instructions and structured authentication
recovery information. The intended recovery workflow is:

1. Recognize the authentication failure and explain which selected session needs
   sign-in. An access denial is a permission problem, not a reason to loop login.
2. When the owner has authorized interactive sign-in, run the recovery executable
   with its argument array and the MCP server's source and identity configuration.
   Otherwise obtain authorization before initiating sign-in. Credentials and MFA
   stay in the browser or App, never in chat or shell arguments.
3. Wait for setup to complete, call `teams_status` to verify the selected session,
   then retry the original read once. If verification or the retry fails, report
   the new error instead of restarting an authentication loop.

Authentication and access failures preserve `error.code` and `error.message`, with additional
`error.authentication` and `error.recovery` metadata. Recovery identifies the
selected `authSource`, whether interaction is required, a source-specific action,
the executable/argument array and nonsecret environment when a command is
appropriate, a `teams_status` verification call, and the allowed retry count.
Remove the command's `unsetEnv` variables before applying its `env` values so a
different terminal environment cannot override the selected session. Partial
results can include a `warnings` array with the same authentication and recovery
fields when a follow-up read failed; the available results remain intact.
Use these fields directly rather than building a shell command from message text.
Access denials request permission review and do not supply a login command or
permit automatic retries. Account-selection errors require explicit selection;
neither client should switch to another source, account or tenant to recover.

`teams_login` is a compatibility alias for a session recheck. It does not perform
interactive sign-in; repeating it cannot resolve a sign-in challenge. Use the
source-specific CLI setup command provided in recovery information.

This is a stdio server. Codex's `/mcp login` / `codex mcp login` handles OAuth for
HTTP servers and cannot start this connector's session setup; see the
[official Codex command documentation](https://learn.chatgpt.com/docs/developer-commands#codex-mcp).
For this connector in either Codex or Claude Code, use `teams-mcp auth login` with
the same authentication source and configuration as the MCP registration.

**Default change:** registrations without `--auth-source` or `TEAMS_AUTH_SOURCE`
now use the existing browser. To keep using a previously configured managed
profile, select `--auth-source headless` explicitly in both setup and client
configuration. See [Codex headless](examples/codex-headless.toml) and
[Claude Code headless](examples/claude-headless.json) examples.

## Authentication sources

`--auth-source browser` is the default. It attaches to a Teams tab through its
approved Chrome debugging connection. It does not start a managed browser.
Enable remote debugging at `chrome://inspect/#remote-debugging` and approve the
connection. One MCP process reuses that connection; reconnecting can require a
new approval. See [Chrome's existing-session workflow](https://developer.chrome.com/blog/chrome-devtools-mcp-debug-your-browser-session).

Only `https://teams.microsoft.com` and `https://teams.cloud.microsoft` are eligible.
On macOS the normal Chrome `DevToolsActivePort` file is discovered automatically.
`TEAMS_CDP_PORT_FILE` selects another file; `TEAMS_CDP_URL` accepts only a literal
loopback browser WebSocket endpoint. Use `TEAMS_TARGET_ID` when more than one
eligible Teams tab exists.

`--auth-source headless` uses one shared, managed Chrome profile across MCP clients.
It requires an installed Chrome, Edge, Chromium or Brave, discovered at its normal
location (Chrome first). `TEAMS_BROWSER=chrome|edge|chromium|brave` selects one;
`TEAMS_BROWSER_BINARY` selects another compatible executable. The connector does
not install a browser.

```sh
teams-mcp --auth-source headless auth login
teams-mcp --auth-source headless --status
```

Explicit managed login waits for active connector calls before transitioning its
owned background browser to a sign-in window. Complete normal SSO/MFA and select
the intended organization. On a terminal, press Enter to confirm the displayed
account and tenant; without a terminal, setup finishes when the selected identity
is ready. Setup saves only identity metadata and closes the owned window. Later
calls resume headlessly without a separate `auth stop` step.
`auth login --no-wait` retains the separate setup flow; finish it with
`teams-mcp --auth-source headless auth stop`.

The managed macOS profile defaults to
`~/Library/Application Support/teams-mcp/browser`. Set `TEAMS_BROWSER_PROFILE` to
an absolute path for a separate dedicated profile. The old connector's
`~/.teams-mcp-server/browser-profile` can be selected explicitly when it is not in
use. Profiles are never copied, their credential files are not read directly, and
Chromium Singleton locks are never removed. Do not share a profile with a
simultaneously running old connector.

Saved account and tenant bind subsequent managed calls to the same identity.
Select a different identity explicitly with **both** `TEAMS_ACCOUNT` and
`TEAMS_TENANT_ID`, or use another dedicated profile. For an invited guest, these
variables also guide managed setup to the resource organization's normal sign-in
flow. Authentication expiry never silently changes source, account or tenant.

`--auth-source app` selects the native Teams WebView. On macOS it uses Apple Events
and requires the App's **View → Developer → Allow JavaScript from Apple Events**
setting and OS Automation permission. Set `TEAMS_APP_WINDOW_ID` to an existing
numeric window ID when needed. Alternatively, `TEAMS_APP_CDP_URL` selects an
approved CDP endpoint exposed by Teams itself. App mode never falls back to Chrome.

**Native App authentication remains blocked on the tested Teams version
26289.204.5281.6363.** Its JavaScript setting was disabled; an approved restart
with the remote-debugging option logged `shell_remote_debugging_port_blocked`.
The adapter is covered synthetically but has no successful live App verification.
No App Keychain items, policy settings, signatures or executable patches are used.
The managed headless session works independently of whether the Teams App is open.

Source-specific configurations are in `examples/*-browser.*`,
`examples/*-headless.*` and `examples/*-app.*`.
Native Apple Events support is macOS-only; other native platforms are unverified.
Windows browser discovery is included and CI compiles it, but Windows is not live-qualified.

## Tools

| Tool | Purpose |
| --- | --- |
| `teams_status` | Verify the selected session; report account, source and cached API capabilities |
| `teams_get_me` | Selected account profile and tenant |
| `teams_login` | Compatibility session recheck; use explicit CLI setup for sign-in |
| `teams_search` | Search messages with Teams operators and `sender`/`after`/`before` filters; links and offset pagination |
| `teams_search_people` | People suggestions where the account supports them |
| `teams_find_channel` | Find channels among joined teams |
| `teams_list_teams` | Joined teams and their channels |
| `teams_get_person` | Resolve Teams person identifiers (MRIs) |
| `teams_get_frequent_contacts` | People you chat with most, from recent conversations |
| `teams_get_chat` | 1:1 chat ID for a person |
| `teams_list_chats` | Up to 200 recent conversations (default 20) |
| `teams_get_favorites` | Pinned conversations |
| `teams_get_message` | One message by conversation and message ID, with its link |
| `teams_open_link` | The message behind a Teams message link |
| `teams_get_thread` | Paginated messages (default 20), optional `since`/`until` window |
| `teams_get_shared_files` | Files and links shared in a page of messages |
| `teams_get_activity` | Activity feed: mentions, replies, reactions (`mentionsOnly`) |
| `teams_get_followed_threads` | Threads you follow |
| `teams_get_unread` | Read state from consumption horizons; `includeMessages` adds unread messages and counts |

Messages are compact: null fields are omitted, the sender is a name and MRI, and
reactions, mentions, files, subject, edits and quoted replies appear only when
present. `maxContentChars` truncates long bodies and marks them `truncated`.

There are no send, mark-read, upload, arbitrary URL, JavaScript execution or
token-export tools. Message/profile text is untrusted external data. Unread
coverage is bounded and reports uncertainty; empty conversations are counted
separately rather than reported as unknown. Channel discovery covers joined teams.
Guest people-search restrictions return `FEATURE_UNAVAILABLE`. Saved messages,
meetings/transcripts and mailbox search are not offered: their endpoints were
unavailable or unverifiable on the tested account (see the reverse-engineering notes).

`teams_status` verifies the selected session's cached credentials and reports
authentication separately from local setup. It does not verify every remote API permission.
Authenticated search/chat calls test actual access. Teams' private APIs and cache
formats can change. This project does not guarantee all account types, national
clouds or organization policies. Microsoft access denials remain access denials.

## Verification

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
node --test tests/bridge.cjs
python3 -m unittest discover -s tests -p 'test_*.py'
cargo build --release --locked
```

Tests use synthetic encrypted caches, local CDP fixtures and owned fake browser
processes. Node is needed only for bridge tests. The macOS process regression
runs the harmless expression `1 + 1`; it sends no Apple Events to Teams.

To exercise the installed Codex and Claude Code clients:

```sh
# Tool discovery and missing-auth errors, no browser or real Teams access:
python3 scripts/smoke-clients.py --protocol-only
# After setup: status and a unique negative search, metadata-only output:
python3 scripts/smoke-clients.py
```

The smoke script uses temporary client configurations and normal model access.
It does not change installed registrations, persist conversations, send messages,
or print Teams content. Python 3.11+ is needed only for this optional script.

## Moving to another computer

The source archive contains no browser profile, cookies, tokens or account
selection. Build the project, select an authentication source, then run `auth
login` and `--status` with that same source on the new computer. Sign in normally
to an account that can access the intended Teams organization. Never move a
signed-in profile through Git or email.

## Session lifetime and resource use

For explicit managed headless mode with the page transport, the Teams page is
frozen 15 seconds after the last tool call
and made active again by the next one (tens of milliseconds), and the background
browser stops after five idle minutes. With the default lease transport it stops 20
seconds after minting. Its saved profile survives that
shutdown. Interactive setup windows close after fifteen minutes without connector
activity.

| Variable | Default | Effect |
| --- | --- | --- |
| `TEAMS_AUTH_SOURCE` | `browser` | Selected session: `browser`, explicit `headless`, or `app` |
| `TEAMS_TRANSPORT` | `lease` | `page`: run every call inside a resident Teams page; see below |
| `TEAMS_HIDDEN_TAB` | off | Existing browser only: use a connector-owned hidden Teams target |
| `TEAMS_BROWSER_IDLE_SECS` | `20` (`300` with `page`) | Stop the managed browser after this idle time (10–86400) |
| `TEAMS_BROWSER_FREEZE_SECS` | `15` | Page transport: freeze the idle Teams page after this time; `0` disables |
| `TEAMS_REQUEST_TIMEOUT_SECS` | `65` | Upper bound for one tool call, including a cold start |
| `TEAMS_BROWSER_CACHE_LIMIT_MB` | unset | At shutdown, clear Teams' Service Worker cache above this size |
| `TEAMS_BROWSER_LEAN` | off | Experimental: no images and no Microsoft telemetry hosts in the hidden page |
| `TEAMS_PREWARM` | off | Check the selected session after MCP initialization; may start explicit managed mode |
| `TEAMS_MCP_LOG` | off | Timings and error codes (never content) on stderr and in a worker log |

### Lease transport (default) and page transport

The lease transport keeps the browser for authentication only. The selected
session mints the account's short-lived access tokens (about 80–90 minutes); the MCP
process then calls Teams over HTTPS with the same endpoint allowlist, exact origins,
2 MiB response limit, no redirects and retries as the in-page bridge. The managed
headless browser stops 20 seconds after minting and starts again only when a needed
token is within five minutes of expiry or is rejected. Refresh tokens and cookies
never leave the browser; access tokens stay in the MCP process's memory and are never
written to disk, logged or returned to clients. In a 10-minute agent session this
averaged 80 MiB instead of about 1 GiB with the page transport, with the same warm
call latency. `TEAMS_TRANSPORT=page` restores the page transport if a tenant rejects
direct API calls or you prefer that no token leaves the browser.

With the default `--auth-source browser`, the lease is minted from your own approved Chrome, so no
managed browser runs at all. By default it uses an already open, signed-in Teams tab.
With `TEAMS_HIDDEN_TAB=1` the connector instead creates its own hidden Teams target
(not shown in the tab strip) that shares your browser's normal sign-in, mints the
lease and closes it; it never uses your visible tabs or sign-in pages. If that browser
is not signed in to Teams, calls return `BROWSER_SIGNIN_REQUIRED`. Renewal repeats
roughly once per token lifetime, and Chrome asks for debugging approval once per MCP
process.

Teams' Service Worker cache can grow to hundreds of megabytes. `teams-mcp
--auth-source headless auth compact` clears only that managed cache through Chrome;
sign-in data stays, and the next
cold start downloads Teams' files again. The managed browser disables Chrome Sync,
translation, media routing and optimization-model downloads. Safe Browsing,
extensions and certificate/component security updates are left enabled. Microsoft controls how
long authentication remains valid; saving cookies does not extend that lifetime.
For guest accounts using an emailed one-time code, Microsoft documents a
[24-hour session](https://learn.microsoft.com/en-us/entra/external-id/one-time-passcode#user-experience-for-one-time-passcode-guest-users).
Other SSO methods follow the organization's session policy.

## Contributing, security and license

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development workflow and source
archive commands, and [SECURITY.md](SECURITY.md) for the authentication boundary
and private vulnerability reporting. Contributions are licensed under the
[MIT License](LICENSE). [NOTICE](NOTICE) retains upstream attribution.

This project is not affiliated with or endorsed by Microsoft, OpenAI or Anthropic.
Its private Teams API integration is experimental and can break when Teams changes.
macOS has been live-tested; Linux and Windows have not been live-qualified.

See [verification results](docs/verification.md) for actual test outcomes and
limits, and [reverse-engineering findings](docs/reverse-engineering.md) for the
installed connector's behavior and evidence provenance.
