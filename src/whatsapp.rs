use crate::{Chat, Message, PollDraft, SourceScope};
use anyhow::{Context, Result, bail};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{
    Arc, Mutex, RwLock,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::mpsc;
use whatsapp_rust::download::DownloadWriter;
use whatsapp_rust::features::PollVoteCiphertext;
use whatsapp_rust::prelude::{Bot, BotHandle, Client, Jid, MessageBuilderExt, MessageExt, wa};
use whatsapp_rust::store::SqliteStore;
use whatsapp_rust::types::events as wa_events;
use whatsapp_rust::wacore::history_sync::{HistorySyncStream, MAX_DECOMPRESSED};
use whatsapp_rust::wacore::poll::compute_option_hash;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Qr(String),
    Connected {
        owner_jid: String,
    },
    Chat(Chat),
    Message(Message),
    VoiceNote {
        chat_id: String,
        message_id: String,
        timestamp: i64,
        audio: Vec<u8>,
    },
    PollUpdate(PollUpdate),
    Disconnected(String),
    Error(String),
}

const MAX_VOICE_NOTE_BYTES: u64 = 16 * 1024 * 1024;

/// Encrypted live vote data or the option hashes already present in history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollUpdate {
    pub chat_id: String,
    pub update_id: String,
    pub poll_id: String,
    pub sender_id: String,
    pub from_me: bool,
    pub timestamp: i64,
    pub enc_payload: Option<Vec<u8>>,
    pub enc_iv: Option<Vec<u8>>,
    pub selected_hashes: Option<Vec<Vec<u8>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedPoll {
    pub message_id: String,
    pub secret: Vec<u8>,
    pub creator_jid: String,
}

pub struct WhatsApp {
    client: Arc<Client>,
    _bot: Arc<Mutex<Option<BotHandle>>>,
    scope: Arc<RwLock<SourceScope>>,
}

impl WhatsApp {
    pub async fn connect(
        session_path: &Path,
        expected_owner: Option<&str>,
        scope: SourceScope,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>)> {
        let expected_owner = expected_owner.map(normalize_jid).transpose()?;
        if let Some(parent) = session_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create session directory {}", parent.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
        }

        let store = SqliteStore::new(&session_path.to_string_lossy())
            .await
            .context("open WhatsApp session database")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(session_path, std::fs::Permissions::from_mode(0o600))?;
        }

        let (raw_tx, mut raw_rx) = mpsc::unbounded_channel();
        let bot = Bot::builder()
            .with_backend(store)
            .with_event_handler(Bridge(raw_tx))
            .build()
            .await
            .context("build WhatsApp client")?;
        let handle = bot.spawn();
        let client = handle.client();
        let bot_handle = Arc::new(Mutex::new(Some(handle)));
        let scope = Arc::new(RwLock::new(scope));
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let task_client = client.clone();
        let task_scope = scope.clone();
        let task_bot_handle = bot_handle.clone();

        tokio::spawn(async move {
            let announced = AtomicBool::new(false);
            while let Some(event) = raw_rx.recv().await {
                if !announced.load(Ordering::Relaxed)
                    && matches!(
                        event.as_ref(),
                        wa_events::Event::Messages(_) | wa_events::Event::HistorySync(_)
                    )
                {
                    continue;
                }
                let result = match event.as_ref() {
                    wa_events::Event::PairingQrCode(qr) => {
                        let _ = event_tx.send(Event::Qr(qr.code.clone()));
                        None
                    }
                    wa_events::Event::PairSuccess(pair) => {
                        let owner = pair.id.to_non_ad_string();
                        if expected_owner
                            .as_deref()
                            .is_some_and(|expected| expected != owner.as_str())
                        {
                            let _ = event_tx.send(Event::Error(
                                "The linked WhatsApp account does not match the configured owner"
                                    .into(),
                            ));
                            stop_bot(&task_bot_handle);
                            break;
                        }
                        if !announced.swap(true, Ordering::Relaxed) {
                            let _ = event_tx.send(Event::Connected { owner_jid: owner });
                        }
                        None
                    }
                    wa_events::Event::Connected(_) => {
                        match task_client.pn().map(|jid| jid.to_non_ad_string()) {
                            Some(owner) => {
                                if expected_owner
                                    .as_deref()
                                    .is_some_and(|expected| expected != owner.as_str())
                                {
                                    let _ = event_tx.send(Event::Error(
                                        "The linked WhatsApp account does not match the configured owner".into(),
                                    ));
                                    stop_bot(&task_bot_handle);
                                    break;
                                }
                                if !announced.swap(true, Ordering::Relaxed) {
                                    let _ = event_tx.send(Event::Connected { owner_jid: owner });
                                }
                                None
                            }
                            None => Some(Event::Error(
                                "WhatsApp connected without an account identity".into(),
                            )),
                        }
                    }
                    wa_events::Event::Messages(batch) => {
                        for inbound in batch.iter() {
                            let info = &inbound.info;
                            let chat_id = info.source.chat.to_non_ad_string();
                            let name = if info.source.is_group {
                                chat_id.clone()
                            } else if !info.push_name.is_empty() {
                                info.push_name.to_string()
                            } else {
                                chat_id.clone()
                            };
                            let _ = event_tx.send(Event::Chat(Chat {
                                is_group: info.source.is_group,
                                id: chat_id.clone(),
                                name,
                            }));
                            if !task_scope
                                .read()
                                .map(|scope| scope.allows(&chat_id))
                                .unwrap_or(false)
                            {
                                continue;
                            }

                            if let Some(text) = inbound
                                .message
                                .text_content()
                                .filter(|text| !text.is_empty())
                            {
                                let sender_id = if info.source.is_from_me {
                                    task_client
                                        .pn()
                                        .map(|jid| jid.to_non_ad_string())
                                        .unwrap_or_else(|| info.source.sender.to_non_ad_string())
                                } else {
                                    info.source.sender.to_non_ad_string()
                                };
                                let _ = event_tx.send(Event::Message(Message {
                                    chat_id: chat_id.clone(),
                                    id: info.id.as_str().to_owned(),
                                    sender_id,
                                    sender_name: if info.push_name.is_empty() {
                                        info.source.sender.to_non_ad_string()
                                    } else {
                                        info.push_name.to_string()
                                    },
                                    timestamp: info.timestamp.timestamp(),
                                    text: text.to_owned(),
                                    is_live: true,
                                }));
                            }
                            if info.source.is_from_me
                                && let Some(audio) =
                                    inbound.message.get_base_message().audio_message.as_option()
                                && let Some(declared_size) = voice_note_size(audio)
                                && let Ok(writer) = task_client
                                    .download_to_writer(
                                        audio,
                                        BoundedVoiceWriter::new(declared_size),
                                    )
                                    .await
                            {
                                let audio = writer.into_inner();
                                if audio.len() as u64 <= MAX_VOICE_NOTE_BYTES {
                                    let _ = event_tx.send(Event::VoiceNote {
                                        chat_id: chat_id.clone(),
                                        message_id: info.id.as_str().to_owned(),
                                        timestamp: info.timestamp.timestamp(),
                                        audio,
                                    });
                                }
                            }
                            if let Some(update) = inbound
                                .message
                                .get_base_message()
                                .poll_update_message
                                .as_option()
                                && let Some(poll_key) = update.poll_creation_message_key.as_option()
                                && let Some(poll_id) =
                                    poll_key.id.as_deref().filter(|id| !id.is_empty())
                            {
                                let vote = update.vote.as_option();
                                let _ = event_tx.send(Event::PollUpdate(PollUpdate {
                                    chat_id,
                                    update_id: info.id.as_str().to_owned(),
                                    poll_id: poll_id.to_owned(),
                                    sender_id: if info.source.is_from_me {
                                        task_client
                                            .pn()
                                            .map(|jid| jid.to_non_ad_string())
                                            .unwrap_or_else(|| {
                                                info.source.sender.to_non_ad_string()
                                            })
                                    } else {
                                        info.source.sender.to_non_ad_string()
                                    },
                                    from_me: info.source.is_from_me,
                                    timestamp: info.timestamp.timestamp_millis(),
                                    enc_payload: vote.and_then(|vote| vote.enc_payload.clone()),
                                    enc_iv: vote.and_then(|vote| vote.enc_iv.clone()),
                                    selected_hashes: None,
                                }));
                            }
                        }
                        None
                    }
                    wa_events::Event::HistorySync(lazy) => {
                        let bytes = lazy.compressed_bytes().clone();
                        let owner = task_client.pn().map(|jid| jid.to_non_ad_string());
                        match tokio::task::spawn_blocking(move || {
                            parse_history(&bytes, owner.as_deref())
                        })
                        .await
                        {
                            Ok(Ok(events)) => {
                                for event in events {
                                    let chat_id = match &event {
                                        Event::Message(message) => Some(message.chat_id.as_str()),
                                        Event::PollUpdate(update) => Some(update.chat_id.as_str()),
                                        _ => None,
                                    };
                                    if chat_id.is_some_and(|chat_id| {
                                        !task_scope
                                            .read()
                                            .map(|scope| scope.allows(chat_id))
                                            .unwrap_or(false)
                                    }) {
                                        continue;
                                    }
                                    let _ = event_tx.send(event);
                                }
                                None
                            }
                            Ok(Err(error)) => Some(Event::Error(format!(
                                "Could not read WhatsApp history: {error}"
                            ))),
                            Err(_) => Some(Event::Error(
                                "WhatsApp history parsing worker failed".into(),
                            )),
                        }
                    }
                    wa_events::Event::Disconnected(_) => {
                        Some(Event::Disconnected("WhatsApp disconnected".into()))
                    }
                    wa_events::Event::LoggedOut(_) => {
                        Some(Event::Disconnected("WhatsApp was unlinked".into()))
                    }
                    _ => None,
                };
                if let Some(event) = result {
                    let _ = event_tx.send(event);
                }
            }
        });

        Ok((
            Self {
                client,
                _bot: bot_handle,
                scope,
            },
            event_rx,
        ))
    }

    pub fn set_scope(&self, scope: SourceScope) {
        if let Ok(mut current) = self.scope.write() {
            *current = scope;
        }
    }

    pub async fn send_text(&self, chat_id: &str, text: &str) -> Result<String> {
        let jid: Jid = chat_id.parse().context("invalid WhatsApp chat id")?;
        self.client
            .send_message(jid, wa::Message::text(text))
            .await
            .map(|sent| sent.message_id)
            .context("send WhatsApp message")
    }

    pub async fn create_poll(&self, chat_id: &str, draft: &PollDraft) -> Result<CreatedPoll> {
        if draft.options.len() < 2 || draft.options.len() > 12 {
            bail!("WhatsApp polls need 2 to 12 options");
        }
        let jid: Jid = chat_id.parse().context("invalid WhatsApp chat id")?;
        let creator_jid = self
            .client
            .pn()
            .context("WhatsApp account is not connected")?
            .to_non_ad_string();
        let (sent, secret) = self
            .client
            .polls()
            .create(
                jid,
                &draft.question,
                &draft.options,
                if draft.multiple {
                    draft.options.len() as u32
                } else {
                    1
                },
            )
            .await
            .context("send WhatsApp poll")?;
        Ok(CreatedPoll {
            message_id: sent.message_id,
            secret,
            creator_jid,
        })
    }

    pub async fn decrypt_poll_vote(
        &self,
        update: &PollUpdate,
        secret: &[u8],
        creator_jid: &str,
    ) -> Result<Vec<Vec<u8>>> {
        let (Some(enc_payload), Some(enc_iv)) = (&update.enc_payload, &update.enc_iv) else {
            return Ok(update.selected_hashes.clone().unwrap_or_default());
        };
        let creator: Jid = creator_jid.parse().context("invalid poll creator JID")?;
        let voter: Jid = update.sender_id.parse().context("invalid poll voter JID")?;
        self.client
            .polls()
            .decrypt_vote(
                PollVoteCiphertext {
                    enc_payload,
                    enc_iv,
                },
                secret,
                &update.poll_id,
                &creator,
                &voter,
            )
            .await
            .context("decrypt WhatsApp poll vote")
    }
}

pub fn poll_choices(options: &[String], selected_hashes: &[Vec<u8>]) -> Option<Vec<usize>> {
    let mut choices = Vec::with_capacity(selected_hashes.len());
    for hash in selected_hashes {
        let index = options
            .iter()
            .position(|option| compute_option_hash(option).as_slice() == hash)?;
        if !choices.contains(&index) {
            choices.push(index);
        }
    }
    choices.sort_unstable();
    Some(choices)
}

fn voice_note_size(audio: &wa::message::AudioMessage) -> Option<usize> {
    if !audio.ptt.unwrap_or(false) {
        return None;
    }
    let length = audio.file_length?;
    if length > MAX_VOICE_NOTE_BYTES {
        return None;
    }
    usize::try_from(length).ok()
}

struct BoundedVoiceWriter {
    bytes: Vec<u8>,
    position: u64,
    limit: usize,
}

impl BoundedVoiceWriter {
    fn new(capacity: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity),
            position: 0,
            limit: capacity,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for BoundedVoiceWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let start = usize::try_from(self.position).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "voice note offset overflow")
        })?;
        let end = start.checked_add(bytes.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "voice note size overflow")
        })?;
        if end > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "voice note exceeds size limit",
            ));
        }
        if start > self.bytes.len() {
            self.bytes.resize(start, 0);
        }
        if end > self.bytes.len() {
            self.bytes.resize(end, 0);
        }
        self.bytes[start..end].copy_from_slice(bytes);
        self.position = end as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for BoundedVoiceWriter {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
            SeekFrom::End(offset) => self.bytes.len() as i128 + i128::from(offset),
        };
        if !(0..=self.limit as i128).contains(&position) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "voice note seek exceeds size limit",
            ));
        }
        self.position = position as u64;
        Ok(self.position)
    }
}

impl DownloadWriter for BoundedVoiceWriter {
    fn truncate(&mut self, length: u64) -> io::Result<()> {
        let length = usize::try_from(length)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "voice note size overflow"))?;
        if length > self.bytes.len() || length > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot extend voice note while truncating",
            ));
        }
        self.bytes.truncate(length);
        self.position = self.position.min(length as u64);
        Ok(())
    }
}

struct Bridge(mpsc::UnboundedSender<Arc<wa_events::Event>>);

fn stop_bot(bot: &Arc<Mutex<Option<BotHandle>>>) {
    let handle = bot.lock().ok().and_then(|mut handle| handle.take());
    if let Some(handle) = handle {
        tokio::spawn(handle.shutdown());
    }
}

impl wa_events::EventHandler for Bridge {
    fn handle_event(&self, event: Arc<wa_events::Event>) {
        let _ = self.0.send(event);
    }
}

fn normalize_jid(value: &str) -> Result<String> {
    let jid: Jid = value.parse().context("invalid owner JID")?;
    Ok(jid.to_non_ad_string())
}

fn parse_history(compressed: &[u8], owner_jid: Option<&str>) -> Result<Vec<Event>> {
    let mut stream = HistorySyncStream::new(compressed, MAX_DECOMPRESSED);
    let mut events = Vec::new();
    while let Some(conversation) = stream.next_conversation()? {
        let chat_id = conversation.id.clone();
        let is_group = chat_id.ends_with("@g.us");
        events.push(Event::Chat(Chat {
            id: chat_id.clone(),
            name: if let Some(name) = conversation
                .display_name
                .as_deref()
                .filter(|name| !name.is_empty())
            {
                name.to_owned()
            } else if let Some(name) = conversation.name.as_deref().filter(|name| !name.is_empty())
            {
                name.to_owned()
            } else {
                chat_id.clone()
            },
            is_group,
        }));
        let mut rows = Vec::new();
        for entry in conversation.messages {
            let Some(info) = entry.message.as_option() else {
                continue;
            };
            let Some(key) = info.key.as_option() else {
                continue;
            };
            let Some(poll_id) = key.id.as_deref().filter(|id| !id.is_empty()) else {
                continue;
            };
            if let Some(body) = info.message.as_option() {
                let sender_id = if key.from_me.unwrap_or(false) {
                    owner_jid.map(str::to_owned).unwrap_or_else(|| {
                        key.participant
                            .clone()
                            .or_else(|| key.remote_jid.clone())
                            .unwrap_or_default()
                    })
                } else {
                    info.participant
                        .clone()
                        .or_else(|| key.participant.clone())
                        .or_else(|| key.remote_jid.clone())
                        .unwrap_or_default()
                };
                if let Some(text) = body.text_content().filter(|text| !text.is_empty()) {
                    rows.push((
                        info.message_timestamp.unwrap_or_default() as i64,
                        poll_id.to_owned(),
                        Event::Message(Message {
                            chat_id: chat_id.clone(),
                            id: poll_id.to_owned(),
                            sender_id: normalize_jid_or_raw(&sender_id),
                            sender_name: if info.push_name.as_deref().unwrap_or_default().is_empty()
                            {
                                sender_id.clone()
                            } else {
                                info.push_name.as_deref().unwrap_or_default().to_owned()
                            },
                            timestamp: info.message_timestamp.unwrap_or_default() as i64,
                            text: text.to_owned(),
                            is_live: false,
                        }),
                    ));
                }
                for update in &info.poll_updates {
                    let Some(update_key) = update.poll_update_message_key.as_option() else {
                        continue;
                    };
                    let Some(update_id) = update_key.id.as_deref().filter(|id| !id.is_empty())
                    else {
                        continue;
                    };
                    let Some(vote) = update.vote.as_option() else {
                        continue;
                    };
                    let from_me = update_key.from_me.unwrap_or(false);
                    let voter = if from_me {
                        owner_jid
                            .map(str::to_owned)
                            .or_else(|| update_key.participant.clone())
                            .or_else(|| update_key.remote_jid.clone())
                    } else {
                        update_key
                            .participant
                            .clone()
                            .or_else(|| update_key.remote_jid.clone())
                    };
                    let Some(sender_id) = voter else { continue };
                    rows.push((
                        update
                            .sender_timestamp_ms
                            .or(update.server_timestamp_ms)
                            .unwrap_or_default()
                            / 1000,
                        update_id.to_owned(),
                        Event::PollUpdate(PollUpdate {
                            chat_id: chat_id.clone(),
                            update_id: update_id.to_owned(),
                            poll_id: poll_id.to_owned(),
                            sender_id: normalize_jid_or_raw(&sender_id),
                            from_me,
                            timestamp: update
                                .sender_timestamp_ms
                                .or(update.server_timestamp_ms)
                                .unwrap_or_default(),
                            enc_payload: None,
                            enc_iv: None,
                            selected_hashes: Some(vote.selected_options.clone()),
                        }),
                    ));
                }
            }
        }
        rows.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        events.extend(rows.into_iter().map(|(_, _, event)| event));
    }
    let _ = stream.remainder()?;
    Ok(events)
}

fn normalize_jid_or_raw(value: &str) -> String {
    value
        .parse::<Jid>()
        .map(|jid| jid.to_non_ad_string())
        .unwrap_or_else(|_| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        BoundedVoiceWriter, MAX_VOICE_NOTE_BYTES, parse_history, poll_choices, voice_note_size,
    };
    use std::io::Write;

    #[test]
    fn voice_note_gate_requires_ptt_and_a_declared_size_within_limit() {
        use whatsapp_rust::prelude::wa;

        let audio = |ptt, file_length| wa::message::AudioMessage {
            ptt,
            file_length,
            ..Default::default()
        };
        assert_eq!(
            voice_note_size(&audio(Some(true), Some(MAX_VOICE_NOTE_BYTES))),
            Some(16 * 1024 * 1024)
        );
        assert_eq!(
            voice_note_size(&audio(Some(true), Some(MAX_VOICE_NOTE_BYTES + 1))),
            None
        );
        assert_eq!(voice_note_size(&audio(Some(false), Some(10))), None);
        assert_eq!(voice_note_size(&audio(None, Some(10))), None);
        assert_eq!(voice_note_size(&audio(Some(true), None)), None);
    }

    #[test]
    fn voice_note_writer_rejects_bytes_beyond_the_hard_cap() {
        let mut writer = BoundedVoiceWriter::new(4);
        writer.write_all(&[1, 2, 3, 4]).unwrap();
        assert!(writer.write_all(&[5]).is_err());
        assert_eq!(writer.into_inner(), [1, 2, 3, 4]);
    }

    #[test]
    fn poll_choices_uses_upstream_option_hashes_and_rejects_unknown_values() {
        let options = vec!["write tests".to_owned(), "review diff".to_owned()];
        let hashes = vec![
            whatsapp_rust::wacore::poll::compute_option_hash("review diff").to_vec(),
            whatsapp_rust::wacore::poll::compute_option_hash("write tests").to_vec(),
            whatsapp_rust::wacore::poll::compute_option_hash("write tests").to_vec(),
        ];
        assert_eq!(poll_choices(&options, &hashes), Some(vec![0, 1]));
        assert_eq!(poll_choices(&options, &[vec![0; 32]]), None);
    }

    #[test]
    fn malformed_history_is_rejected() {
        assert!(parse_history(&[], None).is_err());
    }
}
