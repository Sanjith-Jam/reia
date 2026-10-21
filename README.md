# Reia

Reia is a local Gemma assistant for WhatsApp. It indexes available text from
your chats and groups into an encrypted SQLite search database, answers your
questions from those sources, turns a request into a native WhatsApp todo
poll, and can run an explicitly requested coding task in a registered
workspace.

WhatsApp still needs an internet connection. Chat indexing and the default
Gemma inference stay on your computer. Optional, explicit commands can send
their inputs to Browserbase, Backboard, Paper Instruments, or ElevenLabs when
the matching API keys are configured. Reia never sends every indexed message
to those services automatically.

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
- `!reia paper <question>` sends the question and matching local sources to
  Paper Instruments' Ultramarine model. It needs `PAPERINSTRUMENTS_API_KEY`.
- `!reia web <question>` searches and fetches up to three public pages with
  Browserbase, then sends those pages and the question to Ultramarine. It needs
  `BROWSERBASE_API_KEY` and `PAPERINSTRUMENTS_API_KEY`.
- `!reia cloud-note <text>` explicitly saves the supplied note to the
  configured Backboard assistant. `!reia cloud <question>` combines up to five
  matching Backboard memories with the scoped local sources and sends them to
  Ultramarine. Both commands work only in your WhatsApp self-chat and require
  `BACKBOARD_API_KEY`, `BACKBOARD_ASSISTANT_ID`, and
  `PAPERINSTRUMENTS_API_KEY`.
- `!reia todo <request>` creates a multi-select WhatsApp poll in the current
  conversation. The local `!reia todos` summary marks options from the owner's
  latest selections; other participants' votes remain visible in WhatsApp.
- `!reia note <text>` and `!reia forget <note-id>` manage private saved notes.
- `!reia @repo <coding task>` starts Hermes in the selected repository. Coding
  commands are accepted only in your WhatsApp self-chat.

If `ELEVENLABS_API_KEY` is set, Reia sends the owner's live WhatsApp voice
notes (up to 16 MiB) from selected chats to ElevenLabs for transcription. The
transcript is indexed locally and may run a Reia command only if it begins
with `!reia`.

## Privacy and limits

The knowledge and todo databases use SQLCipher with a random key stored in the
operating-system keyring. WhatsApp's linked-device session is stored locally
with user-only file permissions. `!reia ask` runs through local Ollama and a
Hermes profile with no tools. Coding uses a separate Hermes profile with file
and terminal tools and has the operating-system permissions of the Reia
process. Only use coding mode with repositories you trust.

Sponsor features are opt-in by command and key: `paper` and `web` send the
question plus selected source text or fetched pages to Paper Instruments;
`web` also sends the search query to Browserbase; `cloud` sends the query and
matching sources to Paper Instruments and searches Backboard; `cloud-note`
sends only its text to Backboard; voice notes send audio to ElevenLabs. Set
keys in the environment before starting Reia. Sponsor API keys are never
written into Reia's config or database.

For example, export keys before launching Reia (replace the placeholders;
never commit real keys):

```sh
export PAPERINSTRUMENTS_API_KEY=...
export BROWSERBASE_API_KEY=...
export BACKBOARD_API_KEY=...
export BACKBOARD_ASSISTANT_ID=...
export ELEVENLABS_API_KEY=...
cargo run -- run
```

Reia indexes text messages and poll votes, not attachments or images. Voice
notes are transcribed only when the ElevenLabs key is configured. WhatsApp
controls which history it supplies, so it may not provide every older message
when a device links. Local inference does not make the WhatsApp connection
work offline.

## Hacktoberfest

Built for the [Hacktoberfest Open-Source AI Challenge, Week 3: Slow and
Steady](https://dev.to/challenges/hacktoberfest-week3-2026-10-19).
