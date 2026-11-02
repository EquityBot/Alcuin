# Contributing

Report bugs and request features in
[GitHub issues](https://github.com/EquityBot/Alcuin/issues). For a suspected
security vulnerability, follow [SECURITY.md](SECURITY.md) instead of opening an
issue.

## Pull requests

Propose changes as a pull request against `main`. A change lands on `main` only
when the `check (ubuntu-latest)`, `check (windows-latest)` and `native-chrome`
checks from [ci.yml](.github/workflows/ci.yml), the `audit` check from
[audit.yml](.github/workflows/audit.yml) and the `Attribution policy` check
pass. The
attribution check fails a pull request whose title, commit trailers or
identities credit an AI tool.

Run what CI runs before opening one:

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo audit --deny warnings
```

New functionality and bug fixes come with automated tests, in `tests/` or the
module's `#[cfg(test)]` tests, run by `cargo test --locked` in CI.

Changes to the remote JSON canonical form or the notification preview should also
survive the fuzz targets in `fuzz/`, which CI runs for 5 minutes each every
Monday. They need a nightly toolchain and `cargo install cargo-fuzz`; run one
with `cargo +nightly fuzz run remote_json` (or `preview`). A crash leaves its
input in `fuzz/artifacts/`; turn it into a test before fixing it.

CI runs the offline tests only. Tests that need a signed-in browser or a live
provider are marked `#[ignore]` and run only on request, with your own
accounts. One ignored test needs only an installed Chrome: CI's `native-chrome`
job runs it, and
`cargo test --locked native_temporary_chat_predicate_and_retained_send_guard -- --ignored`
runs it locally.
