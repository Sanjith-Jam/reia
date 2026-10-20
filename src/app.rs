use crate::SourceScope;
use crate::{
    Message,
    agent::{AgentConfig, LocalAgent},
    config::Config,
    knowledge::KnowledgeStore,
    polls::TodoStore,
    storage,
    whatsapp::{Event, WhatsApp},
};
use anyhow::{Context, Result};
use qrcode::QrCode;
use std::{collections::BTreeSet, path::Path, sync::Arc};

const MAX_REPLY_CHARS: usize = 3500;

pub async fn run(state_dir: &Path, config_path: &Path) -> Result<()> {
    let mut config = if config_path.exists() {
        Config::load(config_path)?
    } else {
        Config::default()
    };
    let key = storage::database_key()?;
    let knowledge_path = state_dir.join("knowledge.db");
    let todo_path = state_dir.join("todos.db");
    let mut knowledge = KnowledgeStore::open(&knowledge_path, &key)?;
    let mut todos = TodoStore::open(&todo_path, &key)?;
    storage::ensure_private_file(&knowledge_path)?;
    storage::ensure_private_file(&todo_path)?;
    let agent = Arc::new(LocalAgent::new(
        AgentConfig {
            ollama_url: config.ollama_url.clone(),
            model: config.model.clone(),
        },
        state_dir.join("agents"),
    )?);
    let scope = config.source_scope();
    let session_path = state_dir.join("whatsapp.db");
    let (whatsapp, mut events) =
        WhatsApp::connect(&session_path, config.owner_jid.as_deref(), scope).await?;
    let whatsapp = Arc::new(whatsapp);
    println!("Reia is running. Send `!reia help` from the linked account to see commands.");

    while let Some(event) = events.recv().await {
        if let Ok(updated) = Config::load(config_path) {
            let scope_changed =
                config.scope != updated.scope || config.selected_chats != updated.selected_chats;
            if scope_changed {
                config.scope = updated.scope;
                config.selected_chats = updated.selected_chats;
                let allowed = config
                    .selected_chats
                    .iter()
                    .cloned()
                    .collect::<BTreeSet<_>>();
                if matches!(config.scope, crate::config::ScopeConfig::Selected) {
                    knowledge.revoke_chats(&allowed)?;
                }
                whatsapp.set_scope(config.source_scope());
            }
        }
        match event {
            Event::Qr(value) => print_qr(&value),
            Event::Connected { owner_jid } => {
                config.owner_jid = Some(owner_jid.clone());
                config.save(config_path)?;
                println!("Linked WhatsApp account {owner_jid}.");
            }
            Event::Chat(chat) => knowledge.record_chat(&chat)?,
            Event::Message(message) => {
                if !config.source_scope().allows(&message.chat_id) {
                    continue;
                }
                knowledge.ingest(&message)?;
                if may_run_command(&message, config.owner_jid.as_deref())
                    && let Err(error) = handle_command(
                        &message,
                        &config,
                        &mut knowledge,
                        &mut todos,
                        &whatsapp,
                        &agent,
                    )
                    .await
                {
                    eprintln!("Reia command failed: {error:#}");
                    reply(
                        &whatsapp,
                        &message.chat_id,
                        "Reia could not finish that command. Check the local terminal for details.",
                    )
                    .await?;
                }
            }
            Event::PollUpdate(update) => {
                if !config.source_scope().allows(&update.chat_id) {
                    continue;
                }
                if let Err(error) = todos.apply_vote(&whatsapp, &update).await {
                    eprintln!("Could not update a Reia todo vote: {error:#}");
                }
            }
            Event::Disconnected(reason) => {
                eprintln!("WhatsApp disconnected: {reason}");
                break;
            }
            Event::Error(error) => eprintln!("WhatsApp: {error}"),
        }
    }
    Ok(())
}

fn may_run_command(message: &Message, owner_jid: Option<&str>) -> bool {
    message.is_live && owner_jid == Some(message.sender_id.as_str())
}

async fn handle_command(
    message: &Message,
    config: &Config,
    knowledge: &mut KnowledgeStore,
    todos: &mut TodoStore,
    whatsapp: &WhatsApp,
    agent: &Arc<LocalAgent>,
) -> Result<()> {
    let Some(command) = message.text.trim().strip_prefix("!reia") else {
        return Ok(());
    };
    let command = command.trim();
    let is_private_control = config.owner_jid.as_deref() == Some(message.chat_id.as_str());
    if command.is_empty() || command == "help" {
        return reply(whatsapp, &message.chat_id,
            "Reia commands (owner only):\n!reia ask <question>\n!reia todo <request>\n!reia todos\n!reia note <text>\n!reia forget <note-id>\n!reia @<workspace> <coding task>\nNo other messages trigger replies.").await;
    }
    if let Some(question) = command.strip_prefix("ask ") {
        let hits = knowledge.search(
            question,
            &command_scope(config, &message.chat_id),
            8,
            is_private_control,
        )?;
        let agent = agent.clone();
        let question = question.to_owned();
        let answer = tokio::task::spawn_blocking(move || agent.answer(&question, &hits))
            .await
            .context("local answer worker stopped")??;
        return reply(whatsapp, &message.chat_id, &answer).await;
    }
    if let Some(request) = command.strip_prefix("todo ") {
        let request = request.trim();
        anyhow::ensure!(!request.is_empty(), "usage: !reia todo <request>");
        let hits = knowledge.search(
            request,
            &command_scope(config, &message.chat_id),
            8,
            is_private_control,
        )?;
        let agent = agent.clone();
        let request = request.to_owned();
        let draft = tokio::task::spawn_blocking(move || agent.draft_checklist(&request, &hits))
            .await
            .context("local todo worker stopped")??;
        let created = whatsapp.create_poll(&message.chat_id, &draft).await?;
        todos.record_poll(&message.chat_id, &draft, &created)?;
        return reply(whatsapp, &message.chat_id, "Todo poll created.").await;
    }
    if command == "todos" {
        let statuses = todos.latest(&message.chat_id)?;
        let text = if statuses.is_empty() {
            "There are no Reia todo polls in this chat yet.".to_owned()
        } else {
            statuses
                .iter()
                .map(|poll| {
                    let items = poll
                        .options
                        .iter()
                        .enumerate()
                        .map(|(index, option)| {
                            format!(
                                "{} {option} ({})",
                                if poll.selected_voters[index] > 0 {
                                    "✅"
                                } else {
                                    "▫️"
                                },
                                poll.selected_voters[index]
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    format!("{}\n{}", poll.question, items)
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        };
        return reply(whatsapp, &message.chat_id, &text).await;
    }
    if let Some(note) = command.strip_prefix("note ") {
        let note = note.trim();
        anyhow::ensure!(!note.is_empty(), "usage: !reia note <text>");
        let id = format!("n{}", rand::random::<u128>());
        knowledge.add_note(&id, note)?;
        return reply(
            whatsapp,
            &message.chat_id,
            &format!("Saved note {id}. Use `!reia forget {id}` to remove it."),
        )
        .await;
    }
    if let Some(note_id) = command.strip_prefix("forget ") {
        let note_id = note_id.trim();
        anyhow::ensure!(!note_id.is_empty(), "usage: !reia forget <note-id>");
        knowledge.forget_note(note_id)?;
        return reply(
            whatsapp,
            &message.chat_id,
            "Forgotten if that note existed.",
        )
        .await;
    }
    if let Some(coding) = command.strip_prefix('@') {
        anyhow::ensure!(
            is_private_control,
            "coding tasks can only be started from your WhatsApp self-chat"
        );
        let (alias, task) = coding
            .split_once(char::is_whitespace)
            .unwrap_or((coding, ""));
        let task = task.trim();
        anyhow::ensure!(!task.is_empty(), "usage: !reia @<workspace> <coding task>");
        let workspace = config
            .workspaces
            .iter()
            .find(|workspace| workspace.alias == alias)
            .with_context(|| format!("workspace @{alias} is not configured"))?
            .path
            .clone();
        reply(whatsapp, &message.chat_id, &format!("Starting the coding task in @{alias}. Reia has the host permissions of its user account.")).await?;
        let agent = agent.clone();
        let task = task.to_owned();
        let result = tokio::task::spawn_blocking(move || agent.code(&task, &workspace))
            .await
            .context("coding worker stopped")?;
        let response = match result {
            Ok(result) => result,
            Err(error) => format!("Coding task failed: {error:#}"),
        };
        return reply(whatsapp, &message.chat_id, &response).await;
    }
    reply(
        whatsapp,
        &message.chat_id,
        "Unknown command. Send `!reia help`.",
    )
    .await
}

fn command_scope(config: &Config, chat_id: &str) -> SourceScope {
    if config.owner_jid.as_deref() == Some(chat_id) {
        config.source_scope()
    } else {
        SourceScope::Selected(BTreeSet::from([chat_id.to_owned()]))
    }
}

async fn reply(whatsapp: &WhatsApp, chat_id: &str, text: &str) -> Result<()> {
    let text: String = text.chars().take(MAX_REPLY_CHARS).collect();
    whatsapp.send_text(chat_id, &text).await?;
    Ok(())
}

fn print_qr(value: &str) {
    match QrCode::new(value.as_bytes()) {
        Ok(code) => {
            println!("Scan this QR code in WhatsApp > Linked devices:");
            println!(
                "{}",
                code.render::<char>()
                    .quiet_zone(false)
                    .module_dimensions(2, 1)
                    .build()
            );
        }
        Err(_) => println!("WhatsApp QR code: {value}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{command_scope, may_run_command};
    use crate::Message;
    use crate::config::{Config, ScopeConfig};

    #[test]
    fn configured_sources_scope_controls_retrieval() {
        let mut config = Config::default();
        assert!(config.source_scope().allows("group:1"));
        config.scope = ScopeConfig::Selected;
        config.selected_chats = vec!["group:2".into()];
        let allowed = config.source_scope();
        assert!(!allowed.allows("group:1"));
        assert!(allowed.allows("group:2"));
        config.owner_jid = Some("owner@s.whatsapp.net".into());
        assert!(command_scope(&config, "owner@s.whatsapp.net").allows("group:2"));
        assert!(command_scope(&config, "group:2").allows("group:2"));
        assert!(!command_scope(&config, "group:2").allows("owner@s.whatsapp.net"));
        let old_owner_command = Message {
            chat_id: "owner@s.whatsapp.net".into(),
            id: "old".into(),
            sender_id: "owner@s.whatsapp.net".into(),
            sender_name: "Owner".into(),
            timestamp: 1,
            text: "!reia @repo change files".into(),
            is_live: false,
        };
        assert!(!may_run_command(
            &old_owner_command,
            config.owner_jid.as_deref()
        ));
        let live = Message {
            is_live: true,
            ..old_owner_command
        };
        assert!(may_run_command(&live, config.owner_jid.as_deref()));
    }
}
