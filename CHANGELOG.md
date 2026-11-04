# Changelog

## 0.1.0 (unreleased)

First public release. Alcuin is a Rust port of a maintained fork of [Oracle](https://github.com/steipete/oracle); this repository starts a fresh history and carries no upstream changelog.

- CLI `alcuin` and stdio MCP server `alcuin-mcp` for Windows and Linux.
- ChatGPT browser runs in Temporary Chat with model and thinking-tier verification before dispatch, durable sessions and recovery that never resends an uncertain prompt.
- OpenAI and Azure OpenAI Responses API runs, with background mode.
- Optional Grok browser fallback in Private Chat after a qualifying ChatGPT failure.
- Remote bridge (`serve` and client) for running the browser on another machine.
- Compatible with Oracle's `ORACLE_*` settings and `~/.oracle` session storage.
- `serve` prints its access token only when it generates one, adds `--token-file`, compares tokens without early exit, refuses an empty token and answers HTTP 413 `payload_too_large` for run requests over 1,432,704,342 bytes (Oracle's limit).
- File selection fails closed: `.gitignore` rules apply from the root of the repository holding each selected path, a `.gitignore` that cannot be read or parsed is an error, files read into an API prompt or a browser text bundle must be UTF-8 text rather than being garbled (browser uploads and ZIP bundles still send the original bytes), and credential and private-key files are refused by name and skipped in expansion.
- `serve` binds 127.0.0.1 by default; pass `--host` (for example `--host 0.0.0.0`) to accept other machines. It previously bound 0.0.0.0.
- MCP `consult` and `project_sources` read files only inside the server's working directory: absolute paths, `..` and links that lead outside are refused before anything is read, unless `ORACLE_MCP_ALLOW_EXTERNAL_INPUT=1` is set.
- MCP `sessions` and `wait` see only sessions started in or below the server's working directory, so one project's agent no longer reads another project's prompts, logs and requests; `ORACLE_MCP_ALL_SESSIONS=1` restores every project's sessions.
- The MCP server advertises only what it runs: the `chatgpt_image` tool and `consult`'s `models`, `generateImage`, `outputPath` and Deep Research options are gone, with `ORACLE_MCP_ALLOW_EXTERNAL_OUTPUT` (images and Deep Research were refused before anything was sent, and `models` resolved to the one permitted GPT-6 API model), and `project_sources` says that `add` only previews. Every tool carries MCP annotations: `consult` and `project_sources` reach ChatGPT (`openWorldHint`), `sessions` and `wait` only read local sessions (`readOnlyHint`).
- Every MCP tool refuses input keys its schema does not declare, and each input schema says so with `additionalProperties: false`. `project_sources` and `sessions` previously ignored undeclared keys, so a key such as `maxFileSizeBytes` reached the run.
- File sections in rendered prompts and bundles keep every line that ended, dropping only the final line ending, so a file ending in a whitespace-only line (as a diff can) arrives whole.
- The answer wait keeps polling for up to 10 command budgets when a page read after the send times out or throws, instead of failing the run while ChatGPT is still answering. A privacy refusal or a closed connection still ends it at once.
- A tab retirement that has reached its outcome always records it: a slow disk write at the end of the 5 s budget no longer reports a false cleanup failure and leaves the record undecided, with the tab already closed.
- Alcuin closes its ChatGPT tab once the answer is saved even when the tab's live text has drifted from the saved answer; identity, ownership and completion are still checked, and the mismatch is logged. It also closes the tabs that failed runs created: only a page that still carries the run's own claim, on the same browser instance, once the run and any parent consultation have ended and its controller has exited or is the current process. Recovery then reports the tab as closed after the run failed.
- A first ChatGPT attempt that failed before anything was sent (a closed CDP connection, a command queue timeout or an unconfirmed picker cleanup), and whose dispatch record is absent or never started, is retried once in a fresh tab. A failed run's tab stays open for 60 minutes when its prompt may have been sent or its answer would not load, so the answer can still be read; a run that cannot reach the browser says that nothing was sent; and a configured `remoteChromeLauncher` starts once when a loopback `remoteChrome` endpoint refuses connections.
- An answer wait reached just after the bounded original-target observation ended reports the answer deadline, as it does just before: a stall there no longer replaces that failure with an untyped error that withholds the Grok fallback.
- A browser slot held by an exited controller whose tab allocation failed before it recorded a target is freed. Nothing can have been sent through such a lease, yet it kept its slot for good, so later runs timed out waiting for a free slot.
- `bridge host` installs its SIGTERM and Ctrl+C handlers before the host can answer in the foreground, or before it reports ready in the background, so a stop signal sent at that point shuts it down cleanly. Such a signal previously could kill the process before shutdown.
- A turn holding only ChatGPT's terminal 'Thinking failed' state ends the run as a generation failure, with or without a Retry control, instead of being saved as the answer. An answer that quotes the phrase is still an answer. Reattach and harvest now treat any failed turn as no answer; they previously saved its text.
- A stray login link no longer fails a signed-in page in the login check or `browser-health` once the session and the app both prove sign-in. An auth page, a signed-out session or a login link with weaker evidence still fails.
- Warning codes joined by separators, such as `rate_limit_exceeded` and `cloudflare_challenge`, are collected and classified like their spaced wording.
- A failed ChatGPT page load is retried once in the same tab before the run reports it; nothing has been sent at that point.
- Licensed under FSL-1.1-ALv2; each version also becomes available under Apache-2.0 two years after its release.
