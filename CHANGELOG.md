# Changelog

## 0.1.0 (unreleased)

First public release. Alcuin is a Rust port of a maintained fork of [Oracle](https://github.com/steipete/oracle); this repository starts a fresh history and carries no upstream changelog.

- CLI `alcuin` and stdio MCP server `alcuin-mcp` for Windows and Linux.
- ChatGPT browser runs in Temporary Chat with model and thinking-tier verification before dispatch, durable sessions and recovery that never resends an uncertain prompt.
- OpenAI and Azure OpenAI Responses API runs, with background mode.
- Optional Grok browser fallback in Private Chat after a qualifying ChatGPT failure.
- Remote bridge (`serve` and client) for running the browser on another machine.
- Compatible with Oracle's `ORACLE_*` settings and `~/.oracle` session storage.
- Licensed under FSL-1.1-ALv2; each version also becomes available under Apache-2.0 two years after its release.
