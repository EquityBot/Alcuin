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
- File selection fails closed: `.gitignore` rules apply from the repository root, a `.gitignore` that cannot be read or parsed is an error, files read into an API prompt must be UTF-8 text rather than being garbled (browser uploads still send the original bytes), and credential and private-key files are refused by name and skipped in expansion.
- Licensed under FSL-1.1-ALv2; each version also becomes available under Apache-2.0 two years after its release.
