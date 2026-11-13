# AGENTS.md

This file provides guidance to coding agents when working with code in this repository.

This repository is the open-source, customer-deployed proxy for serum.

## Workflow

- When asked for a commit message: Keep it short and simple, don't use filler words, mention important points (and not every small detail). The commit details should be a bullet list, one line per bullet.

## Absolute Rules

- Never read any file starting with "." such as ".env" without first asking for explicit permission.

## Style or Naming Rules

- Never use horizontal ellipsis `…` in code, docstrings, comments or error messages, Use three dots instead e.g. `...`.
- Never use en-dash `–` or em-dash `—` in code, docstrings, comments or error messages. Use best fitting of: a hyphen `-`, colon `:` or semi-colon `;`.
- Never use double backtick denoting variables e.g. ``type`` in docstrings, use single e.g. `type`.
- Don't refer to the service as SaaS. Just call it "serum" or "the serum service", always lowercase, including at the start of a sentence or in a heading.
- When writing comments and docstrings: 1) optimize for 100 column width 2) do not get wordy, communicate all information concisely.

## Writing and Wording Rules

Always output in ASD-STE100 Simplified Technical English.

## Development

- Everything MUST run inside a container using docker-compose. This is especially important for package management, running management commands, one-off scripts and launching dev servers.
- After a Rust change, run `make fmt-check` and `make lint`. Both must pass.
