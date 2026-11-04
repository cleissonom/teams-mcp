# Verification record

2026-11-04, macOS. These are observed results on the owner's invited Teams
account, not a guarantee against future Teams changes or other tenant policies.
All connector operations exercised were read-only.

## Runtime verification and automated checks

| Command | Result |
| --- | --- |
| `cargo fmt --check` | Passed. |
| `cargo test --locked` | Passed: 71 Rust tests. |
| `cargo clippy --locked --all-targets -- -D warnings` | Passed. |
| `node --test tests/bridge.cjs` | Passed: 18 bridge tests. |
| `cargo build --release --locked` | Passed. |
| `python3 scripts/smoke-clients.py --protocol-only` | Passed in actual Codex and Claude Code. Both discovered the tools and returned the expected `AUTH_SETUP_REQUIRED` without launching Chrome. |
| `python3 scripts/smoke-clients.py` | Passed in actual Codex and Claude Code. Both completed authenticated headless status and a unique negative message search. |

Binary used for these live checks: `target/release/teams-mcp`,
**4,646,208 bytes** (4.43 MiB).
SHA-256: `dbca2c980820316cd53e72f1de00543485ca810e603130e839dbf8eda9c06886`.
All six example JSON/TOML configurations parsed successfully.

Tested versions: Rust 1.99.1, Codex CLI 0.166.0, Claude Code 2.1.318,
Google Chrome 156.0.8078.80. Node and Python are verification tools only;
normal MCP operation uses Rust and the installed Chrome binary.

## Public source archive

The public release uses the 48-file allowlist in `release-files.txt`. A fresh
extraction passed `cargo test --locked` (71 tests),
`cargo clippy --locked --all-targets -- -D warnings`,
`node --test tests/bridge.cjs` (18 tests),
`python3 -m unittest discover -s tests -p 'test_*.py'` (7 tests), and
`cargo build --release --locked`. `cargo fmt --check` also passed.

The packaging tests first reproduced missing implementation and unsafe artifact
acceptance, then passed with the release packager. Independent review verified
the manifest, archive CRC, source-byte equality, portable examples and upstream
license attribution. Gitleaks 8.30.1 reported no leaks in the extracted public
source. This is supporting evidence, not a guarantee that a scanner detects every
kind of sensitive data; signed-in profiles and private evidence are excluded by
the manifest.

GitHub Actions is configured for macOS and Linux, including source packaging and
secret scanning. Hosted CI has not run yet. The archive contains source only;
build and complete normal Teams sign-in on the destination computer.

## Live managed session

The owner completed normal interactive sign-in in the intended resource
organization. The bridge confirmed that the selected account and tenant matched
the intended identity and that search, chat and channel credentials were present.
`auth stop` saved only the validated email and tenant in a mode-0600 metadata
file, then closed the setup browser normally.

Headless restart retained the authenticated session. The first handoff exposed a
transient target-discovery failure; the final build waits within the existing
readiness deadline without attaching to foreign pages. The final client smoke
started from a stopped managed browser: Codex launched it headlessly and passed,
then Claude Code passed against the shared session. No new sign-in was required.

All eleven tools were exercised directly against the final release:

| Operations | Result |
| --- | --- |
| Status, own profile, login recheck | Passed; correct account and tenant. |
| Message search | Passed; unique query returned zero results. |
| Channel discovery | Passed; one matching joined channel returned. |
| Known-person lookup | Passed for the selected account. |
| Recent conversations | Passed; ten conversations returned. |
| Thread and message retrieval | Passed; three thread messages and one selected message read. |
| Unread state | Passed for the selected conversation; bounded coverage remains explicit. |
| People suggestions | Teams rejected this guest feature; the connector returned `FEATURE_UNAVAILABLE`. This is an account limitation, not a successful people search. |

Live checks printed booleans, counts, timings and sanitized error codes only.
Conversation IDs, message bodies and credentials were not included in reports.

## Regression evidence and review

Observed failures preceded the relevant fixes:

- The original App automation runner left stdin open and timed out on harmless
  `osascript` evaluation. The regression now completes.
- A native exception could leak an untrusted error marker, and an expired active
  identity could select another cached identity under a tenant-only constraint.
  Bridge regressions now reject both behaviors.
- Cancelled MCP work retained a pending CDP socket. Cancellation now closes it
  promptly and a subsequent request succeeds.
- Managed lifecycle tests exposed stale endpoint reuse, unsafe existing profile
  permissions, premature stop completion and abrupt startup termination that
  left a Chromium profile lock. Tests now cover graceful owned-child cleanup,
  concurrent clients, explicit headed setup and private profile handling.
- Saved account/tenant selectors initially were not bound into bridge calls.
  The regression now proves this binding only for managed Headless mode.
- Synthetic Chrome experiments established that normal restart discards session
  cookies; `--restore-last-session` retains them. Combining restore with a startup
  URL duplicates the page, so headless launch omits that URL.
- Early empty targets could create a duplicate restored page. Bootstrap now waits
  before its empty-browser fallback, and navigation checks the current origin
  atomically before touching a blank or Microsoft sign-in page.
- A real cold call returned `TEAMS_TAB_REQUIRED` before restoration finished.
  `cargo test --locked --test cdp readiness::headless_restoration_waits_through_transient_navigation_without_trusting_it -- --exact`
  reproduced the failure before the fix. Discovery now waits boundedly through
  transitional URLs; foreign pages are never attached to or evaluated.

Independent review confirmed the scoped fixes and reran six managed unit tests,
eleven managed integration tests, nineteen CDP tests and the cancellation test.
The reviewer also executed the actual navigation guard against nine synthetic
current-origin cases, including foreign redirects and misleading hosts.

## Resource use and remaining limits

After both client tests, process inspection found **one Rust supervisor and one
headless Chrome browser**, with ten Chrome processes including its helpers.
The supervisor used approximately 2.5 MiB RSS. Two Chrome process-tree samples
summed to approximately 688–941 MiB RSS; this double-counts shared pages and is
not a physical-memory benchmark. The Teams web application dominates resource
use. The connector shares Chrome between clients and stops it after five idle
minutes; no Node/Playwright process is part of its runtime.
The test browser was stopped after verification; its saved profile was retained
and its Chromium lock released. The next normal tool call can start it headlessly.

Existing-browser mode passed authenticated API/client checks in earlier builds.
Its approval-based Chrome connection can require consent after reconnecting;
the final successful client runs above use managed Headless mode.

Native App authentication remains blocked on Teams 26289.204.5281.6363:
Apple Events returned `APP_AUTOMATION_DISABLED`, and the approved debug-port
restart logged `shell_remote_debugging_port_blocked`. Native adapters have
synthetic coverage but no successful live App authentication. The managed
headless mode operates independently of whether the Teams App is open.

Claude Desktop UI, Windows, Linux, consumer accounts and national clouds were
not live-tested. Microsoft can require interactive sign-in again or deny access.
No send or mark-read operation was invoked, and no global MCP registration was
replaced. Runtime credentials remain in the dedicated browser profile outside
this repository.
