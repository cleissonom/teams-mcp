# Security

Teams MCP runs locally and reads data available to the selected authenticated
Teams account. Use the latest revision; there is no long-term support branch or
guaranteed response SLA.

## Report a vulnerability privately

Use the repository's **Security → Report a vulnerability** option when enabled.
If that option is unavailable, contact a repository maintainer through a private
channel listed on their public profile before sharing details. Do not open a
public issue containing credentials, account data or an exploit against a live
organization.

Include the affected version, operating system, authentication source, a minimal
synthetic reproduction, expected behavior and impact. Do not send cookies,
tokens, signed-in profiles, Keychain data, message bodies or unredacted logs.

## Authentication and data boundaries

- Credentials remain in the selected Chrome profile or native Teams session.
  The default lease transport holds the selected account's short-lived access tokens and Skype token in the MCP
  process's memory only: never refresh tokens or cookies, never on disk, in logs,
  in errors or in tool output. They are sent only to the allowlisted Teams endpoints.
  With `TEAMS_TRANSPORT=page` the Rust process receives normalized results and
  sanitized errors, not tokens.
- The managed profile is sensitive local data. Keep it outside the repository
  with owner-only access. Do not sync or email it. On a new computer, sign in
  normally using `teams-mcp auth login`.
- The connector remembers the selected email and tenant in a private metadata
  file. These are identity metadata, not credentials, and are not release files.
- MCP initialization and tool discovery do not require authentication. Normal
  calls never open visible sign-in windows. Session renewal still depends on
  Microsoft; expiry can require explicit interactive sign-in.
- CDP endpoints are restricted to literal loopback addresses. Do not expose or
  forward the browser's debugging port. CDP grants powerful access to that local
  browser; this is not an isolation boundary against other software running as
  the same OS user.
- The bridge function is installed once per page context and called with its
  input as an argument. Its caches (decoded claims, decrypted entries, the Skype
  token) live in a private closure reachable only through the connector's DevTools
  handle, never in page globals or on disk.
- Idle managed pages are frozen and `auth compact` clears only Teams' Service
  Worker cache; neither touches cookies or credential storage. The managed browser
  never disables Safe Browsing, extensions or security component updates.
- Requests use fixed read endpoints and exact Teams origins. There are no send,
  mark-read, upload, arbitrary-URL, arbitrary-script or token-export MCP tools.
- Message and profile content is untrusted data, never instructions for an AI
  client. MCP consumers must preserve that distinction.

The connector does not bypass organization access policies, App automation
restrictions, browser security settings or authentication challenges. Private
Teams APIs and encrypted-cache formats can change without notice.

## Release hygiene

Source archives are built from `release-files.txt`. Build artifacts, `.private/`,
browser profiles, local MCP configuration and environment files are excluded.
Review the extracted archive and run a secret scanner before distribution.
Automated scans cannot guarantee that every kind of sensitive data is detected.
