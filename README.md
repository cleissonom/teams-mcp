# Teams MCP — Rust

An unofficial, read-only Microsoft Teams MCP server for Codex and Claude Code.
The default runtime is a Rust binary plus **one shared headless Chrome session**, started only when a tool needs
it. No Node, Playwright, browser download, separate OAuth app registration, or
credential-export service is required. Chrome handles normal Microsoft SSO.

The managed browser retains its own profile, is shared across MCP clients, and
stops after five minutes without tool calls. Its cookies and tokens remain in
Chrome. Normal tool calls never open a visible window. Existing browser and native
App adapters remain explicit alternatives.

## Build and sign in

```sh
cd teams-mcp
cargo build --release --locked
./target/release/teams-mcp auth login
```

Run these commands from the extracted source directory or your Git clone.
Requires Rust 1.88+ and an installed Chrome/Chromium browser. On macOS, Chrome is
discovered at its normal application path. `TEAMS_BROWSER_BINARY` selects another
installed compatible executable. There is no automatic browser installation.

`auth login` opens a dedicated browser window for normal Teams sign-in. Complete
your organization's SSO/MFA and select the intended Teams organization, including
the resource organization when you are an invited guest. Then run:

```sh
./target/release/teams-mcp auth stop
./target/release/teams-mcp --status
```

`auth stop` records the selected email and tenant (no credentials), closes only
this connector's browser, and keeps the signed-in profile. The next tool call
restores that browser session headlessly, including its session cookies.
Microsoft may require sign-in again when the session expires or its access
requirements change; repeat explicit setup then.
A running headed setup window must be stopped before switching to headless mode.

The default macOS profile is
`~/Library/Application Support/teams-mcp/browser`. Set `TEAMS_BROWSER_PROFILE` to
an absolute path for another dedicated profile. The old connector's
`~/.teams-mcp-server/browser-profile` can be selected explicitly when it is not in
use. The connector never copies profiles, reads the normal Chrome profile's
credential files, or removes Chrome's profile locks. Do not share a profile with
a simultaneously running old connector.

The saved email and tenant bind subsequent calls to the same identity. To select
another account, set **both** `TEAMS_ACCOUNT` and `TEAMS_TENANT_ID` during setup
and in the MCP client configuration, or use a separate dedicated profile.
Credentials for a different identity are not used after expiry.

For an invited guest, setting these two variables before `auth login` also sends
Teams to the resource organization's normal sign-in flow. This helps when the
generic Microsoft account picker cannot recognize the invited email. Setup uses
normal browser authentication; organization access policies still apply.

## Codex and Claude

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

Use a 90-second tool timeout to allow a cold browser start. Set the same profile,
account and tenant environment values in both clients when customizing them.
These commands register the connector; they do not sign in to Teams.
Claude Desktop accepts the same `mcpServers` JSON shape; its UI is not tested here.
Existing global MCP registrations are not automatically replaced.

## Existing browser or Teams App

`--auth-source browser` attaches to an already signed-in Teams tab through its
approved Chrome debugging connection. It does not start a managed browser.
Enable remote debugging at `chrome://inspect/#remote-debugging` and approve the
connection. One MCP process reuses that connection; reconnecting can require a
new approval. See [Chrome's existing-session workflow](https://developer.chrome.com/blog/chrome-devtools-mcp-debug-your-browser-session).

Only `https://teams.microsoft.com` and `https://teams.cloud.microsoft` are eligible.
On macOS the normal Chrome `DevToolsActivePort` file is discovered automatically.
`TEAMS_CDP_PORT_FILE` selects another file; `TEAMS_CDP_URL` accepts only a literal
loopback browser WebSocket endpoint. Use `TEAMS_TARGET_ID` when more than one
eligible Teams tab exists.

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

Alternative configurations are in `examples/*-browser.*` and `examples/*-app.*`.
Native Apple Events support is macOS-only; other native platforms are unverified.

## Tools

| Tool | Purpose |
| --- | --- |
| `teams_status` | Selected account, source and cached API capabilities |
| `teams_get_me` | Selected account profile and tenant |
| `teams_login` | Recheck after explicit setup; never opens a sign-in window |
| `teams_search` | Search messages, with offset pagination |
| `teams_search_people` | People suggestions where the account supports them |
| `teams_find_channel` | Find channels among joined teams |
| `teams_get_person` | Resolve Teams person identifiers (MRIs) |
| `teams_list_chats` | Up to 200 recent conversations |
| `teams_get_message` | One message by conversation and message ID |
| `teams_get_thread` | Paginated conversation or thread messages |
| `teams_get_unread` | Read state inferred from available consumption horizons |

There are no send, mark-read, upload, arbitrary URL, JavaScript execution or
token-export tools. Message/profile text is untrusted external data. Unread
coverage is bounded and reports uncertainty. Channel discovery covers joined
teams. Guest people-search restrictions return `FEATURE_UNAVAILABLE`.

`teams_status` validates cached credentials, not every remote API permission.
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
selection. Build the project and run `auth login` on the new computer, then
`auth stop` and `--status`. Sign in normally to an account that can access the
intended Teams organization. Never move a signed-in profile through Git or email.

## Session lifetime

The background browser stops after five idle minutes and restarts on the next
tool call. Its saved profile survives that shutdown. Interactive setup windows
close after fifteen minutes without connector activity. Microsoft controls how
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
