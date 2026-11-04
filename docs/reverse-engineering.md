# Investigation and reconstruction

Inspected on 2026-11-03/04. These findings describe the installed versions; Teams'
private APIs and cache formats are not a supported Microsoft integration contract.

## Installed connector

The installed `~/.local/bin/teams-mcp` wrapper runs the npm package
`msteams-mcp@0.30.2`, using Node/NVM and a separate Playwright browser profile in
`~/.teams-mcp-server/browser-profile`. Its saved browser state is independent of
the owner's normal browser and native Teams app.

REA analyzed 20 selected shipped source files (19 JavaScript modules parsed, zero
parse failures; 167 other files excluded). Artifact SHA-256:
`0124317b453123abcdda5ed2bab23798946b635ca2401f1febfc4283e8826533`.
Summary evidence: `ev_b3bad9fada33dfc92aeee045f5209241b0198c7d4d5a3d275dceb3e6692b6669`;
parent: `ev_567a969b1dc032ed79e11b8f9d07fa21b405ed11ccfcf78de5410f708a93bc94`.
This is bounded static analysis, not proof that every package path was executed.
Full local evidence is ignored at `.private/rea-evidence.json`.

The installed token extraction expects plaintext MSAL cache entries. Current
Teams sessions also use encrypted entries. A live status check found saved state
but unavailable direct API capabilities. Historical Claude Code logs showed
successful status calls followed by `auth_expired` and browser-login timeouts.
**The observed Claude failure is authentication/session handling; those logs do
not establish a broken MCP handshake.** Private transcripts are not copied here.

Focused follow-up of that same artifact established its HTTP/browser split:

- `dist/auth/token-refresh-http.js:94` selects saved refresh credentials, a client
  ID and tenant realm; `:139` submits a refresh-token grant and updates the saved
  Substrate, Spaces and CSA credentials. These are the existing session's client
  credentials, not an independent permission grant for a new application.
- `dist/auth/token-refresh.js:68` tries HTTP renewal first, then starts headless
  Chrome when that fails or no usable Substrate token is available. Explicit login
  can open a visible browser when interaction is required.
- `dist/browser/context.js:134` launches system Chrome/Edge with the dedicated
  persistent profile through Playwright. `:184` saves state and closes it. There
  is no idle browser pool in the examined paths. Its force-removal of a Chromium
  profile lock after a launch error is deliberately not reproduced.

These are static observations of selected shipped files; they do not establish
successful renewal under every tenant policy or compatibility with every account.

## Managed headless reconstruction

The user explicitly permitted a headless browser after trying a pure-HTTP design.
The final runtime therefore uses the existing isolated-world bridge with a small
Rust Chrome supervisor. It removes Playwright/Node and the separate OAuth/token
store experiment. Chrome owns the persistent sign-in and normal MSAL renewal;
credentials are not exported to Rust, a plaintext cache or an MCP client.

One dedicated profile and one managed Chrome instance are shared across Codex and
Claude, with inter-process startup locking and idle shutdown. Initial sign-in is
an explicit headed setup command. Existing browser and App sessions remain
separate, explicitly selected transports. No automatic source/account fallback
occurs. This lifecycle is a reconstruction choice, not a claim that the installed
package uses an identical supervisor.

[Chrome documents](https://developer.chrome.com/docs/automation-and-testing/headless)
that its unified headless mode runs the browser without displaying platform
windows. The connector uses the already installed binary and its normal sandbox.

The earlier device-code experiment was denied with AADSTS53003 (Conditional
Access) and an unregistered-device diagnostic. No credential was issued or saved.
The device-code path is removed; normal browser sign-in still has to satisfy
Microsoft's access checks. Request/correlation IDs and other personal diagnostics
are intentionally not retained here.

Public Teams web bundles corroborate normal MSAL silent acquisition with
interactive fallback and tenant-specific authorities. REA parent
`ev_55145099eabc21f25b3e1eb0dc6b47b8eea9d082aabbc6e3dedcc2e3132f1135`
analyzed three selected scripts, zero parse failures; two other files were
unselected. The semantic graph is partial. Root SHA-256:
`e668549edb0242351c7f805c915e8d902664f119c92d02cd62905046f75a1001`.
This establishes shipped behavior, not live authentication success.

Two restart failures were isolated during the managed-browser checks:

- A generic Microsoft account picker did not recognize the invited email. The
  same browser reached the expected email-OTP flow when Teams received the
  resource tenant and login hint. The public `async-entry` bundle reads the
  `tenantid` and `login_hint` query values (observed byte offset 4,705,474 in the
  retained 8 MiB partial download, SHA-256
  `27e4e52f694457a756f3ce366818c53d2339aa19e8e5109777d130808f918276`).
  This is a direct observation of retained bytes, not a complete bundle analysis.
  The connector now remembers only the selected email and tenant after setup and
  binds subsequent bridge calls to them.
- An isolated Chrome 156.0.8078.80 experiment with a synthetic localhost cookie
  showed that a normal clean restart discarded the session cookie, while
  `--restore-last-session` preserved it across repeated restarts. Adding an
  explicit startup URL alongside that flag produced two tabs; restoration without
  a URL produced one. Managed headless startup therefore restores the session
  without appending a duplicate Teams tab. No cookie lifetime, credential,
  browser policy or real account data was changed by that experiment.

These fixes preserve normal browser behavior; they do not extend a Microsoft
session past its own expiry or remove an interactive sign-in requirement.

## Browser adapter

The new adapter attaches to an explicitly approved, existing Chrome/Chromium CDP
session. It selects an exact Teams origin and runs its fixed JavaScript bridge in
an isolated world. Tokens and HTTP authorization stay in the page. The MCP client
receives only normalized tool results and sanitized errors.

Observed formats:

- MSAL entries: HKDF-SHA256 with nonce salt and client-ID context, then AES-GCM.
  The cache encryption cookie belongs to the same origin/session.
- Teams resource cache: AES-256-CBC with an exported browser key and per-entry IV.
- Teams discovery provides regional chat, channel and profile service roots.
- Chat/profile requests exchange the existing Spaces access token at Teams'
  authz service for a Skype token. No refresh token is read or exported.
- Substrate search requires correlation IDs. The observed guest account cannot
  use the people-suggestions provider; it receives `FEATURE_UNAVAILABLE`.

Sources: [MSAL cache documentation](https://github.com/AzureAD/microsoft-authentication-library-for-js/blob/dev/lib/msal-browser/docs/caching.md),
[MSAL browser crypto implementation](https://github.com/AzureAD/microsoft-authentication-library-for-js/blob/dev/lib/msal-browser/src/crypto/BrowserCrypto.ts),
[Chrome's existing-session debugging workflow](https://developer.chrome.com/blog/chrome-devtools-mcp-debug-your-browser-session).
Chrome's approval mode requires consent for each new debugging connection. Reusing
a connected socket preserves the approved session for subsequent tool calls.

The installed browser's loaded Teams bundles corroborated the CBC resource-cache
format and native encryption-key bridge. These are public shipped assets fetched
without cookies; retained only under `.private/web-assets/`.

## Native Teams app

Target: `/Applications/Microsoft Teams.app`, bundle `com.microsoft.teams2`, version
`26289.204.5281.6363`. The actual running WebView profile is
`EBWebView/WV2Profile_tfw`, not the nearly empty `Default` profile.

Schema-only investigation found encrypted resource tokens, empty plaintext token
fields, and no browser-style exported key. The active identity is recorded in
`tmp.auth.v1.GLOBAL.User.User`; it takes precedence over other clients' MSAL state.
Expired credentials never cause a switch to another cached identity, including
when only a tenant constraint is supplied. A different identity requires both an
explicit account and tenant. The earlier cache-reader prototype was removed:
the final connector performs no direct cache-file or Keychain reads.

Native static analysis located the actual encryption path:
`acquireEncryptionKey` → `AuthEncryptionManager::GetEncryptionKeyForBridgeId` →
`TeamsCloudManager::GetCloudOriginForBridgeId` → `CredentialVault::Get` →
`SecItemCopyMatching`. The matching setter uses `SecItemAdd`/`SecItemUpdate`.
Static disassembly established a generic-password Keychain query using cloud
origin as `kSecAttrAccount`, `kSecUseDataProtectionKeychain=true`, and
`kSecReturnData=true`. Metadata-only probes for the observed origin returned
`-25300` (not found), including a separate legacy-query probe. This does not
establish that a standalone connector can access the App's item. No key was
returned or saved, and no process was attached or memory read.

The shipped `Resources/client-bindings/bundle.js` exposes
`nativeModules.Auth.acquireEncryptionKey`, which invokes
`auth.acquireEncryptionKey` and returns a `result` field. The final design calls
this bridge inside the App, where the key and HTTPS credentials remain. It can
run through macOS Apple Events or an explicitly configured App CDP endpoint.
Synthetic tests verify this contract; they do not establish live App access.

Runtime checks after explicit owner approval established the current blocker:

- **Apple Events:** the WebView's JavaScript setting exists but its menu item is
  disabled. Normal UI activation did not enable it. The working Rust adapter,
  directed at an existing numeric window ID, returned `APP_AUTOMATION_DISABLED`.
- **App CDP:** Teams was gracefully restarted with its built-in
  `--remote-debugging-port=9223` argument. The new main process contained that
  argument, but no endpoint appeared. Its new startup log contained
  `shell_remote_debugging_port_blocked` in one failure record (the event name
  appears twice within that record). Static code in
  `ReportRemoteDebuggingPortBlocked` connects that event to a startup port
  request; it does not establish the underlying blocking predicate. The absence
  of a listener alone would not have established App-side blocking.

These observations block live native authentication on the tested installation.
They do not prove that every Teams build rejects automation, identify an
administrator setting that permits it, or justify bypassing the restriction.
No preferences, enterprise policy, executable signatures or Keychain permissions
were changed. The App was left running with its existing signed-in profile.

Native executable arm64 SHA-256:
`8b533bee873352a22aceee2874849b1903b265f5edb9bbf645be50e7a9665006`.
WebView framework evidence `ev_c784f91fc0f7eb1a39297bd0ce881bc4f58d5924212b76fdae52a5e0dbca8f29`
proves an additional-browser-arguments selector, not a supported environment flag.
Bounded misses for guessed flags are not proof that all debug options are absent.

Native REA exports are retained under `.private/native-rea.json` and
`.private/native-auth-rea.json`. The exact Keychain query and startup-gate findings
also use focused static LLDB disassembly, distinguished from runtime observations
above. Private logs and cache content are not copied into this report.

## Read-only endpoint observations (2026-11-04)

A metadata-only probe ran inside the managed page's isolated world with the
existing bridge's token handling. It returned key names, types, counts and HTTP
status codes only; no values, tokens or message content were retained.

- The Skype token exchange (`authsvc.teams.microsoft.com/v1.0/authz`) returns
  `tokens.expiresIn` (2,203 seconds observed) and `regionGtms`, whose
  `chatServiceAfd`, `chatSvcAggAfd` and `middleTier` values equal the cached
  discovery roots. The bridge reuses the token until shortly before expiry and uses
  those roots only as a validated fallback.
- System conversations exist for `48:notifications` (activity feed:
  `reaction`, `mention`, `reactionInChat`, `mentionInChat`, `replyToReply`),
  `48:mentions`, `48:threads` (followed threads), `48:calllogs`, `48:notes`,
  `48:annotations` and `48:tagmentions`. Their messages carry an `activity`
  property with source thread, message, reply-chain, sender and preview fields.
  `48:saved` returned HTTP 404, so saved messages are not offered.
- CSA `/api/v1/teams/users/me/conversationFolders` returns folders including
  `Favorites` with conversation IDs.
- Chat messages accept a `startTime` (epoch milliseconds) filter: all returned
  messages were at or after it and matched the unfiltered count in that window.
  Message properties include `emotions`, `mentions`, `files`, `links`, `cards`,
  `qtdMsgs`, `subject` and `edittime`, some as JSON strings.
- `/v1/threads/{id}/consumptionhorizons` returned HTTP 403 for a channel and an
  empty list for meeting and 1:1 chats, so it cannot resolve unknown read
  positions. 16 of 23 previously unknown conversations were empty conversations.
- Middle-tier `calendarView` paths returned HTTP 400/404 and `meetingInfo` held
  only an RSVP status, so meetings are not offered. The Exchange search source
  returned zero results for the guest account and is not offered.
- Substrate search honours `from:` and `sent>=`/`sent<=` filters (result totals
  changed accordingly).
- In the headless page, `document.visibilityState` was `visible` and
  `document.hasFocus()` was `true`. Freezing the idle page
  (`Page.setWebLifecycleState`) cut its CPU from about 2–5% to 0.5%; the next call
  made it active again in tens of milliseconds.

## Token lifetimes and renewal (2026-11-04)

A metadata-only probe read token expiry claims inside the managed page; no token
values were returned. Access tokens for chat (Spaces), search (Substrate) and
channels (CSA) had 85, 78 and 92 minute lifetimes without long-lived (CAE) claims.
The Skype token's `expiresIn` matched the remaining Spaces token lifetime. The single
MSAL refresh token carried an expiry with about 16 hours remaining, consistent with
the 24-hour limit for browser (SPA) refresh tokens, so exporting it could not remove
the browser anyway.

Teams did not renew still-valid tokens at boot: a later cold start held the same
tokens. After the search token had expired, the next cold start obtained a new one
within about two seconds. The lease transport therefore uses the same five-minute
margin as MSAL's renewal window and, when a needed token is inside it, re-reads the
session for up to 30 seconds while Teams renews it.

## Design and protocol

The reconstruction uses [the official Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk)
and [newline-delimited stdio transport](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports).
Initialization and tool discovery do not access Teams. Authentication failures
are tool errors, so clients can still discover and diagnose the connector.

There are no message-send, mark-read, file-upload, arbitrary URL, arbitrary
JavaScript, or token-export tools. Channel discovery covers joined teams;
pagination and unread uncertainty are explicit in normalized results.

The installed package is MIT-licensed by Rob MacDonald (`m0nkmaster/msteams-mcp`).
Its shipped contracts informed this reconstruction; this project uses its own
Rust implementation and browser bridge. See `NOTICE` for attribution.

## Acceptance and verification

| Requirement | Evidence and result |
| --- | --- |
| Explain installed MCP/Claude failure | Bounded REA analysis plus historical client errors: authentication state failed; the handshake worked. |
| Rust project in the requested directory | Release binary built from this repository; no Node dependency at runtime. |
| Headless normal operation | Authenticated restart and actual API calls passed with the managed browser hidden; details in `verification.md`. |
| Reuse browser authentication | Live Teams API checks passed without a second login or token export. |
| Codex and Claude Code compatibility | Final release passed both actual clients: unauthenticated protocol/error checks and authenticated headless status/search. |
| Reuse native App authentication | Implemented and covered with synthetic tests; live access blocked by both App automation interfaces. Requirement remains unverified. |
| Do not contact other people | Only allowlisted read operations are exposed or exercised; no send or mark-read tool exists. |

Independent review found and reproduced an AppleScript stdin/EOF timeout, a
native-error redaction gap, a tenant-only account-selection gap, and an MCP
cancellation gap that retained a pending connection until timeout. Focused tests
failed before each fix and passed afterward. Authentication remains lazy, so
missing or blocked sessions do not prevent MCP initialization and tool discovery.
