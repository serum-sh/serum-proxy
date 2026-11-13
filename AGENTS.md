# AGENTS.md

This file provides guidance to coding agents when working with code in this repository.

This repository is the open-source, customer-deployed proxy for serum.

## Workflow

- When asked for a commit message: Keep it short and simple, don't use filler words, mention important points (and not every small detail). The commit details should be a bullet list, one line per bullet.

## Absolute Rules

- Never read a `.env*` file. Ask for explicit permission before you read any other file that starts with ".", except the tracked repo config: `.gitignore`, `.dockerignore` and `.cargo/config.toml`.

## Style and Naming Rules

- Never use horizontal ellipsis `…` in code, docstrings, comments or error messages. Use three dots instead e.g. `...`.
- Never use en-dash `–` or em-dash `—` in code, docstrings, comments or error messages. Use best fitting of: a hyphen `-`, colon `:` or semi-colon `;`.
- Never use double backtick denoting variables e.g. ``type`` in docstrings, use single e.g. `type`.
- Don't refer to the service as SaaS. Just call it "serum" or "the serum service", always lowercase, including at the start of a sentence or in a heading.
- When writing comments and docstrings: 1) optimize for 100 column width 2) do not get wordy, communicate all information concisely.
- To silence a lint, use `#[expect(<lint>, reason = "...")]`, not `#[allow]`.

## Writing and Wording Rules

Always output in ASD-STE100 Simplified Technical English.

## Project Context

- `CONTEXT.md` is the glossary. Use its terms.
- `docs/protocol.md` is the wire contract. A change to the envelope (`src/envelope.rs`) needs a change to `docs/protocol-vectors.json`. serum keeps an identical copy of that file and compares the SHA-256, so a change to the vectors also needs a change in serum.

## Development

- Everything MUST run inside a container. Run cargo with the Makefile targets (`make check`, `make test [name]`, `make fmt`, `make lint`); they run cargo in the toolchain image. `docker compose up proxy` runs the proxy for local development.
- After a Rust change, run `make fmt-check`, `make lint` and `make test`. All must pass.
