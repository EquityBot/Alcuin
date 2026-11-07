---
name: alcuin
description: Get a second opinion from ChatGPT or the OpenAI API with the Alcuin CLI. Use when asked to consult another model, or to check a plan, diff or stubborn bug independently, and `alcuin` is installed.
---

# Alcuin

Alcuin sends a prompt and the files you choose to a model and saves the answer as a session. Its answer is advice: check each point against the source before acting on it.

## Before you send

Everything you attach goes to the provider. Show the user the provider and the files, and get their approval before you send. Never attach secrets. Alcuin refuses a run whose files or prompt match a credential pattern: remove the value, and pass `--allow-secret-file <path>` only for a test fixture the user has confirmed is fabricated. Preview what would go:

```sh
alcuin --dry-run summary --files-report -p "Review this module" --file src/lib.rs
```

The model sees only the prompt and the attachments. Give it the goal, what you have ruled out and the one decision you need, and ask for evidence for each claim.

## Run it

ChatGPT in the browser needs one sign-in, which the user does by hand in the window this opens:

```sh
alcuin --engine browser --browser-manual-login --browser-keep-browser -p "HI"
```

Then:

```sh
alcuin --engine browser --slug "review of parser" -p "Review this module" --file src/lib.rs
```

For the API, with `OPENAI_API_KEY` set, use `--engine api`. Azure OpenAI uses `AZURE_OPENAI_API_KEY`, `AZURE_OPENAI_ENDPOINT` and `AZURE_OPENAI_DEPLOYMENT`.

A run can take many minutes. Start it in the background if you can, and wait for it to finish.

## Read the answer

```sh
alcuin status                  # recent sessions
alcuin session <id> --render   # a saved answer
```

`--followup <id>` continues a saved conversation.

Before acting on evidence the answer cites, run `alcuin ground <id>`. It exits 1 when a cited line runs past a sent file's end, a cited file was not sent or a quote appears in no sent file; check each flagged item against the source. When you report the answer, give a verdict and a reason for each finding you dismiss, not only for those you accept.

If a run fails after sending, don't send the prompt again: the answer may still arrive. Run `alcuin session <id> --harvest --write-output answer.md` to read it from the conversation. If that shows an error rather than an answer, resend only with the user's approval.

Run `alcuin --help --verbose` for every option.
