# Changelog

## 0.1.0 (unreleased)

First public release. Industrious is a Rust port of Calvino, a maintained fork of [Oracle](https://github.com/steipete/oracle); this repository starts a fresh history and carries no upstream changelog.

- CLI `industrious` and stdio MCP server `industrious-mcp` for Windows and Linux.
- ChatGPT browser runs in Temporary Chat with model and thinking-tier verification before dispatch, durable sessions and recovery that never resends an uncertain prompt.
- OpenAI and Azure OpenAI Responses API runs, with background mode.
- Optional Grok browser fallback in Private Chat after a qualifying ChatGPT failure.
- Remote bridge (`serve` and client) for running the browser on another machine.
- Compatible with Oracle's `ORACLE_*` settings and `~/.oracle` session storage.
- `serve` prints its access token only when it generates one, adds `--token-file`, compares tokens without early exit, refuses an empty token and answers HTTP 413 `payload_too_large` for run requests over 1,432,704,342 bytes (Oracle's limit).
- File selection fails closed: `.gitignore` rules apply from the root of the repository holding each selected path, a `.gitignore` that cannot be read or parsed is an error, files read into an API prompt or a browser text bundle must be UTF-8 text rather than being garbled (browser uploads and ZIP bundles still send the original bytes), and credential and private-key files are refused by name and skipped in expansion.
- File sections in rendered prompts and bundles keep every line that ended, dropping only the final line ending, so a file ending in a whitespace-only line (as a diff can) arrives whole.
- A tab retirement that has reached its outcome always records it: a slow disk write at the end of the 5 s budget no longer reports a false cleanup failure and leaves the record undecided, with the tab already closed.
- An answer wait reached just after the bounded original-target observation ended reports the answer deadline, as it does just before: a stall there no longer replaces that failure with an untyped error that withholds the Grok fallback.
- Licensed under FSL-1.1-ALv2; each version also becomes available under Apache-2.0 two years after its release.
