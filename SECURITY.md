# Security

## What Alcuin can reach

Alcuin runs as a local CLI and a stdio MCP server under your account. It:

- drives a signed-in ChatGPT (and, if you enable the fallback, Grok) session in a Chrome profile it controls, and can attach to a Chrome you already run when you ask it to;
- can read Chrome cookies for chatgpt.com when you enable cookie sync;
- sends your prompt and the files you select to the provider you chose: OpenAI (ChatGPT or the API), Azure OpenAI, or xAI (grok.com, when the fallback is enabled);
- reads API keys from the environment for API runs;
- stores prompts, answers and session logs under `~/.oracle`.

Treat every file you attach as sent to that provider. Do not attach credentials, `.env` files or data you may not share. The MCP server is meant for a local, trusted caller; do not expose it or the remote bridge to untrusted networks.

## Reporting a vulnerability

Please report suspected vulnerabilities privately through GitHub's 'Report a vulnerability' button on this repository's Security tab. Do not open a public issue. Include the version (`alcuin --version`), platform and the smallest steps that show the problem. You will get an acknowledgement, and a fix or a decision, as soon as the maintainer can review it; there is no fixed response time.

## Supported versions

Only the latest release receives fixes.
