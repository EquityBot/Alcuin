# Security

## What Industrious can reach

Industrious runs as a local CLI and a stdio MCP server under your account. It:

- drives a signed-in ChatGPT (and, if you enable the fallback, Grok) session in a Chrome profile it controls, and can attach to a Chrome you already run when you ask it to;
- can read Chrome cookies for chatgpt.com when you enable cookie sync;
- sends your prompt and the files you select to the provider you chose: OpenAI (ChatGPT or the API), Azure OpenAI, or xAI (grok.com, when the fallback is enabled);
- reads API keys from the environment for API runs;
- stores prompts, answers and session logs under `~/.oracle`.

Treat every file you attach as sent to that provider. Do not attach credentials, `.env` files or data you may not share. As a backstop, file selection refuses a named file that looks like a credential or private key (`.env` and `.env.*` other than example templates, `.npmrc`, `.pypirc`, `.netrc`, `.git-credentials`, `.htpasswd`, `id_rsa` and other SSH private keys, and `.pem`, `.key`, `.ppk`, `.p12`, `.pfx`, `.jks`, `.keystore` and `.kdbx` files), skips such files when expanding folders and globs, and applies `.gitignore` rules from the root of the repository holding each selected path. A text bundle (`--browser-bundle-format text`) refuses files that are not UTF-8; use `zip` to send their original bytes. It does not inspect file contents for secrets. The MCP server is meant for a local, trusted caller; do not expose it or the remote bridge to untrusted networks.

`industrious serve` prints its access token only when it generates one. Pass your own with `--token-file` rather than `--token`, which other users of the machine can see in the process list. Run requests are limited to 1,432,704,342 bytes (two attachment sets at the 512 MiB artifact limit); the host holds a whole request in memory, so admit only clients you trust.

## Reporting a vulnerability

Please report suspected vulnerabilities privately through GitHub's 'Report a vulnerability' button on this repository's Security tab. Do not open a public issue. Include the version (`industrious --version`), platform and the smallest steps that show the problem. You will get an acknowledgement, and a fix or a decision, as soon as the maintainer can review it; there is no fixed response time.

## Supported versions

Only the latest release receives fixes.
