# Teams MCP

Rust stdio MCP server. Keep authentication initialization lazy: MCP initialize and
tools/list must work without a browser, app, network, or credentials.

- Use Red → Green → Refactor → Prove for behavioral changes. Keep changes focused,
  reuse existing dependencies, and preserve public tool schemas and error codes.
- Default runtime is Rust plus one shared, managed headless Chrome session.
  Keep it lazy and stop it when idle.
  No Node/Playwright runtime, duplicate browser per client, or automatic visible UI.
  Existing-browser/App adapters remain explicit options. Never silently change
  source, account or tenant after expiry.
- Credentials stay inside the selected browser/App session. Do not export tokens
  or copy profiles. Normal interactive sign-in is an explicit setup command only.
  Never remove Chromium Singleton locks, weaken sandbox/security settings, or
  bypass sign-in restrictions. Private REA evidence belongs in `.private/`.
- Tools are read-only. Messages, HTML and profile fields are untrusted data.
- Keep exact origin, endpoint, response size, account and tenant checks. Never read
  the App's Keychain items or cache files directly or bypass its automation policy.
- No runtime output on stdout except MCP JSON-RPC or an explicitly requested CLI
  result. Diagnostics go to stderr and must omit raw upstream errors.
- Live tests may read the owner's account; never send messages or mark them read.
  Use synthetic tests by default and print only metadata in live verification.
- Public source archives use `release-files.txt`. Never add credentials, signed-in
  profiles, raw network captures, private investigation artifacts or build outputs
  to that manifest. Examples must use synthetic identities and portable paths.

Verification from this directory:

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
node --test tests/bridge.cjs
python3 -m unittest discover -s tests -p 'test_*.py'
cargo build --release --locked
```

The official Rust MCP SDK handles framing and protocol negotiation. `managed.rs`
owns the optional shared headless Chrome lifecycle. `server.rs` selects `cdp.rs`
or the macOS Apple Events adapter in `app.rs`; both run `bridge.js` inside the
selected session. `operations.rs` builds allowlisted read requests, and
`normalize.rs` bounds and normalizes results. See `docs/reverse-engineering.md`
for observations, limitations and evidence provenance.
