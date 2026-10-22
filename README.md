# Reia

Reia is a local Gemma assistant for WhatsApp. It indexes available text from
your chats and groups into an encrypted SQLite search database, answers your
questions from those sources, turns a request into a native WhatsApp todo
poll, and can run an explicitly requested coding task in a registered
workspace.

WhatsApp still needs an internet connection. Reia keeps its searchable archive
and default Gemma inference on your computer. Once configured, sponsor services
are part of the normal workflow: questions use Paper Instruments for answers,
Backboard memory in your private self-chat, and Browserbase for current web
research. Owner voice notes are transcribed by ElevenLabs. Reia sends only the
question context needed for that request, not the full archive.
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
messages can be indexed as context, but they cannot trigger Reia. Reia does not
respond to ordinary messages.

- `!reia ask <question>` searches the configured local chat scope and saved
  notes. In your self-chat, configured Backboard memory joins the context;
  Browserbase is used automatically for current or web information. Paper
  Instruments answers when its key is configured, otherwise local Gemma answers.
  In a group, Reia only searches that chat and excludes private notes and
  Backboard memory.
- `!reia todo <request>` creates a multi-select WhatsApp poll in the current
  conversation. The local `!reia todos` summary marks options from the owner's
  latest selections; other participants' votes remain visible in WhatsApp.
- `!reia note <text>` and `!reia forget <note-id>` manage saved notes. In your
  self-chat, notes also sync to Backboard and are removed there when forgotten.
- `!reia @repo <coding task>` starts Hermes in the selected repository. Coding
  commands are accepted only in your WhatsApp self-chat.

When configured, Reia sends the owner's live WhatsApp voice notes (up to 16
MiB) from selected chats to ElevenLabs for transcription. The transcript is
indexed locally and may run a Reia command only if it begins with `!reia`.

## Privacy and limits

The knowledge and todo databases use SQLCipher with a random key stored in the
operating-system keyring. Sponsor API keys are stored in the same keyring,
never Reia's config or database. WhatsApp's linked-device session is stored
locally with user-only file permissions. `!reia ask` uses local Ollama and a
Hermes profile with no tools when Paper Instruments is not configured. Coding
uses a separate Hermes profile with file and terminal tools and has the
operating-system permissions of the Reia process. Only use coding mode with
repositories you trust.

`cargo run -- setup` opens hidden-input prompts for sponsor API keys and saves them in the OS keyring. Re-run setup to add or replace keys; pressing Enter keeps an existing key. Do not paste keys into WhatsApp or chat. Legacy environment variables remain supported for existing setups.

Configured providers receive only the content needed for the requested
operation: Paper Instruments receives the question and matching excerpts,
Browserbase receives current-information queries, Backboard receives notes
explicitly saved in your private self-chat and searches for relevant memories,
and ElevenLabs receives selected outgoing voice-note audio. Reia never uploads
the complete indexed archive automatically.

Reia indexes text messages and poll votes, not attachments or images. WhatsApp
controls which history it supplies, so it may not provide every older message
when a device links. Local inference does not make the WhatsApp connection
work offline.

## Hacktoberfest

Built for the [Hacktoberfest Open-Source AI Challenge, Week 3: Slow and
Steady](https://dev.to/challenges/hacktoberfest-week3-2026-10-19).
