# Contributing

Report bugs and request features in
[GitHub issues](https://github.com/EquityBot/Alcuin/issues). For a suspected
security vulnerability, follow [SECURITY.md](SECURITY.md) instead of opening an
issue.

## Pull requests

Propose changes as a pull request against `main`. A change lands on `main` only
when the `check (ubuntu-latest)` and `check (windows-latest)` checks from
[ci.yml](.github/workflows/ci.yml) and the `Attribution policy` check pass. The
attribution check fails a pull request whose title, commit trailers or
identities credit an AI tool.

Run what CI runs before opening one:

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

New functionality and bug fixes come with automated tests, in `tests/` or the
module's `#[cfg(test)]` tests, run by `cargo test --locked` in CI.

CI runs the offline tests only. Tests that need a signed-in browser or a live
provider are marked `#[ignore]` and run only on request, with your own
accounts.
