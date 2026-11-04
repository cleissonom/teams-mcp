# Contributing

Bug reports, small fixes, documentation improvements and synthetic test cases are
welcome. Keep changes focused on the read-only Teams MCP contract.

## Development

Use Rust 1.88 or newer. Node.js 22+ is needed for JavaScript bridge tests; Python
3.11+ is needed for packaging tests and optional real-client smoke checks. Chrome
and a Teams account are needed only for explicitly requested live verification.

From the checkout root:

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
node --test tests/bridge.cjs
python3 -m unittest discover -s tests -p 'test_*.py'
cargo build --release --locked
```

The GitHub Actions workflow defines synthetic checks on macOS and Linux. These
checks must not need secrets, a signed-in browser, a paid model client or
organization access. Synthetic checks do not establish live Teams compatibility.

## Changes and pull requests

For behavioral changes, first add a focused failing regression test, implement
the smallest fix, then run the relevant checks. Existing tests can characterize
behavior-preserving refactors. Documentation-only changes do not need invented
tests. Describe the problem, resulting behavior, verification and known limits
in the pull request.

Preserve lazy MCP initialization, exact origin and endpoint checks, account and
tenant selection, bounded responses, cancellation, and sanitized errors. Keep
stdout reserved for MCP or explicitly requested CLI output. Do not add token
export, arbitrary JavaScript execution, send/mark-read operations, automatic
visible login windows or authentication-policy bypasses.

Use synthetic accounts, tokens and service responses in tests. Never attach real
Teams messages, profiles, cookies, access/refresh tokens, private keys, raw HAR
files or organization identifiers to an issue, commit, test fixture or pull
request. Report sensitive findings through [SECURITY.md](SECURITY.md).

Live verification is opt-in and must use an account you are authorized to access.
`scripts/smoke-clients.py` invokes installed model clients and can incur their
normal usage costs. It prints only metadata; it must not persist conversations or
send messages. Ordinary tests never invoke it automatically.

## Source archives and first publication

`release-files.txt` is the exact public source-file manifest. Update it when
adding a public file, and keep it aligned with the source allowlist in
`Cargo.toml`. To create a source-only ZIP:

```sh
python3 scripts/package-release.py
```

Before sharing or publishing, extract the ZIP to an empty directory, inspect its
contents, run the checks above from that copy, and scan it for secrets. With
[Gitleaks](https://github.com/gitleaks/gitleaks) installed:

```sh
gitleaks dir /path/to/extracted/teams-mcp --redact --no-banner
```

The manifest excludes local profiles, private investigation data, build outputs,
archives and credentials. A manifest and secret scanner reduce accidental
exposure; neither replaces review. Never publish `.private/` or a signed-in
browser profile, even from an earlier commit.

When creating the GitHub repository, inspect `git status` and the staged diff
before the first push. Enable private vulnerability reporting and secret
scanning/push protection where available. Repository creation, publication and
client registration are separate actions; the packaging script performs none of
them.

## License and attribution

By contributing, you agree that your contributions are distributed under this
project's [MIT License](LICENSE). Include attribution and license notices when
introducing third-party material; preserve the existing [NOTICE](NOTICE).
