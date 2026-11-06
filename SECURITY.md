# Security

## What Alcuin can reach

Alcuin runs as a local CLI and a stdio MCP server under your account. It:

- drives a signed-in ChatGPT (and, if you enable the fallback, Grok) session in a Chrome profile it controls, and can attach to a Chrome you already run when you ask it to;
- can read Chrome cookies for chatgpt.com when you enable cookie sync;
- sends your prompt and the files you select to the provider you chose: OpenAI (ChatGPT or the API), Azure OpenAI, or xAI (grok.com, when the fallback is enabled);
- reads API keys from the environment for API runs;
- stores prompts, answers and session logs under `~/.oracle`.

API runs (`--engine api`) use OpenAI's Responses API, which keeps each response for at least 30 days unless a request sets `store` to false ([API reference](https://developers.openai.com/api/reference/resources/responses/methods/create)). Alcuin does not set it to false, because background runs (its default) and `--followup` both need the stored response.

Treat every file you attach as sent to that provider. Do not attach credentials, `.env` files or data you may not share. As a backstop, file selection refuses a named file that looks like a credential or private key (`.env` and `.env.*` other than example templates, `.npmrc`, `.pypirc`, `.netrc`, `.git-credentials`, `.htpasswd`, `id_rsa` and other SSH private keys, and `.pem`, `.key`, `.ppk`, `.p12`, `.pfx`, `.jks`, `.keystore` and `.kdbx` files), skips such files when expanding folders and globs, and applies `.gitignore` rules from the root of the repository holding each selected path. A text bundle (`--browser-bundle-format text`) refuses files that are not UTF-8; use `zip` to send their original bytes. The MCP server reads files only inside its working directory, refusing absolute paths, `..` and links that lead outside, unless `ORACLE_MCP_ALLOW_EXTERNAL_INPUT=1` is set. Its `sessions` and `wait` tools see only sessions started in or below that directory, unless `ORACLE_MCP_ALL_SESSIONS=1` is set. It is meant for a local, trusted caller; do not expose it or the remote bridge to untrusted networks.

Alcuin also reads every selected file, of any type, and the prompt, system prompt and browser follow-ups before it sends anything. A match for a credential pattern refuses the whole run, including a match found while expanding a folder. The patterns cover private key blocks, tokens with a published format from GitHub, GitLab, Anthropic, xAI, Google, Slack, Stripe, npm and PyPI, OpenAI admin keys, and an AWS access key ID followed by its secret. The refusal gives the source, line and pattern, and no part of the value. A selection over 1 GiB, anything but a regular file, and a file that changes after its scan are refused rather than sent unscanned. `--allow-secret-file <path>` sends files the run selected despite a match; it is for test fixtures, takes exact paths, applies to one run and is CLI only. Prompt text has no override.

This scan is a backstop with known gaps:

- OpenAI project and service account keys: OpenAI publishes no format for them, so no rule covers them, although they are this tool's most likely credential.
- Values with no fixed format, such as passwords and connection strings, GitLab tokens with a custom prefix, an AWS key ID alone, and an AWS secret that precedes its ID or stands alone.
- Compressed or encoded content, including ZIP, PDF, Office files, images and Base64, so an uploaded archive is scanned only as its stored bytes.
- UTF-7, EBCDIC and other encodings. UTF-16 and UTF-32 are handled by removing NUL bytes from the text, not by decoding it, so only characters in the ASCII range are seen.
- `alcuin serve` scans nothing it receives. The Alcuin client scans before it sends, but any other client that holds the token is not checked.

`alcuin serve` listens on 127.0.0.1 unless `--host` names another interface. It speaks plain HTTP, so its access token and attachments cross the network unencrypted; use an SSH tunnel or a trusted network for other machines. It prints its access token only when it generates one. Pass your own with `--token-file` rather than `--token`, which other users of the machine can see in the process list. Run requests are limited to 1,432,704,342 bytes (two attachment sets at the 512 MiB artifact limit); the host holds a whole request in memory, so admit only clients you trust.

## Reporting a vulnerability

Please report suspected vulnerabilities privately through GitHub's 'Report a vulnerability' button on this repository's Security tab, at https://github.com/yungSNAT/Alcuin/security/advisories/new. Do not open a public issue. Include the version (`alcuin --version`), platform and the smallest steps that show the problem. You will get an acknowledgement, and a fix or a decision, as soon as the maintainer can review it; there is no fixed response time.

## Supported versions

Only the latest release receives fixes.
