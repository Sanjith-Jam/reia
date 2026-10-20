# Reia

Reia is a local Gemma assistant for WhatsApp. It indexes available text from
your chats and groups into an encrypted SQLite search database, answers your
questions from those sources, turns a request into a native WhatsApp todo
poll, and can run an explicitly requested coding task in a registered
workspace.

WhatsApp still needs an internet connection. Chat indexing and inference stay
on your computer. The only model endpoint Reia accepts is Ollama on loopback;
it has no cloud fallback.

## Setup

Install [Rust](https://www.rust-lang.org/tools/install), [Ollama](https://ollama.com/download),
and [Hermes Agent](https://github.com/NousResearch/hermes-agent). Start the
local Ollama service and download Gemma:

```sh
ollama pull gemma4:e2b
cargo run -- setup
cargo run -- doctor
cargo run -- run
```

On the first run, scan the displayed QR code from WhatsApp's **Linked
devices** screen. Reia saves that account as the only account allowed to
issue commands. Wait for WhatsApp to provide its available history and chat
list before using memory search.

By default, Reia indexes every chat and group the linked device provides. To
choose specific conversations, run `cargo run -- sources select`; it shows
the discovered chats and groups. Changing to a smaller selection deletes
indexed messages from conversations you removed. `cargo run -- sources all`
restores all-chat scope for future syncs. `cargo run -- sources status`
shows the current selection.

Register a local repository before sending it coding work:

```sh
cargo run -- workspace add repo /path/to/repository
cargo run -- workspace list
```

## WhatsApp commands

Commands only run when sent by the linked account owner. Other participants'
messages can be indexed as context, but they cannot trigger Reia. Reia does
not respond to ordinary messages.

- `!reia ask <question>` answers from indexed chat text and saved notes. In
  your WhatsApp self-chat it can search the configured source scope; in a
  group or another chat it only searches that conversation and excludes
  private saved notes.
- `!reia todo <request>` creates a multi-select WhatsApp poll in the current
  conversation. The local `!reia todos` summary marks options from the owner's
  latest selections; other participants' votes remain visible in WhatsApp.
- `!reia note <text>` and `!reia forget <note-id>` manage private saved notes.
- `!reia @repo <coding task>` starts Hermes in the selected repository. Coding
  commands are accepted only in your WhatsApp self-chat.

## Privacy and limits

The knowledge and todo databases use SQLCipher with a random key stored in the
operating-system keyring. WhatsApp's linked-device session is stored locally
with user-only file permissions. Personal answers run through a Hermes profile
with no tools. Coding uses a separate Hermes profile with file and terminal
tools and has the operating-system permissions of the Reia process. Only use
coding mode with repositories you trust.

Reia indexes text messages and poll votes, not attachments, voice notes, or
images. WhatsApp controls which history it supplies, so it may not provide
every older message when a device links. Local inference does not make the
WhatsApp connection work offline.

## Hacktoberfest

Built for the [Hacktoberfest Open-Source AI Challenge, Week 3: Slow and
Steady](https://dev.to/challenges/hacktoberfest-week3-2026-10-19).
