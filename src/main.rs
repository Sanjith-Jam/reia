use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reia::{
    agent::{AgentConfig, LocalAgent},
    config::Config,
    storage,
};
use std::{
    collections::BTreeSet,
    io::{self, Write},
    path::{Path, PathBuf},
};

#[derive(Parser)]
#[command(name = "reia", version, about = "Local Gemma assistant for WhatsApp")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Setup,
    Run,
    Doctor,
    Sources {
        #[command(subcommand)]
        command: Sources,
    },
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
}

#[derive(Subcommand)]
enum WorkspaceCommand {
    Add { alias: String, path: PathBuf },
    List,
    Remove { alias: String },
}

#[derive(Subcommand)]
enum Sources {
    All,
    Select { chats: Vec<String> },
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let data_dir = storage::data_dir()?;
    let config_path = storage::config_path()?;
    let mut config = load_config(&config_path)?;

    match cli.command {
        Command::Setup => {
            config.save(&config_path)?;
            println!(
                "Configuration saved. Run `reia run` and scan the WhatsApp QR code to link this device."
            );
        }
        Command::Run => reia::app::run(&data_dir, &config_path).await?,
        Command::Doctor => doctor(&config).await?,
        Command::Sources { command } => match command {
            Sources::All => {
                config.scope = reia::config::ScopeConfig::All;
                config.selected_chats.clear();
                config.save(&config_path)?;
                println!("Reia can read all chats and groups.");
            }
            Sources::Select { chats } => {
                let mut knowledge = open_knowledge(&data_dir)?;
                let available = knowledge.chats()?;
                let chats = if chats.is_empty() {
                    choose_chats(&available)?
                } else {
                    chats
                };
                anyhow::ensure!(!chats.is_empty(), "select at least one chat");
                let available_ids = available
                    .iter()
                    .map(|chat| chat.id.as_str())
                    .collect::<BTreeSet<_>>();
                for chat in &chats {
                    anyhow::ensure!(
                        available_ids.contains(chat.as_str()),
                        "unknown chat ID {chat}; use `reia sources select` to see available chats"
                    );
                }
                config.scope = reia::config::ScopeConfig::Selected;
                config.selected_chats = chats
                    .into_iter()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                config.save(&config_path)?;
                knowledge.revoke_chats(&config.selected_chats.iter().cloned().collect())?;
                println!(
                    "Saved {} selected conversations.",
                    config.selected_chats.len()
                );
            }
            Sources::Status => match config.scope {
                reia::config::ScopeConfig::All => {
                    println!("Scope: all chats and groups");
                    for chat in open_knowledge(&data_dir)?.chats()? {
                        println!(
                            "{}  {}{}",
                            chat.id,
                            chat.name,
                            if chat.is_group { " (group)" } else { "" }
                        );
                    }
                }
                reia::config::ScopeConfig::Selected => {
                    println!("Scope: {} selected chats", config.selected_chats.len());
                    let chats = open_knowledge(&data_dir)?.chats()?;
                    for chat in &config.selected_chats {
                        let name = chats
                            .iter()
                            .find(|row| row.id == *chat)
                            .map(|row| row.name.as_str())
                            .unwrap_or("unknown chat");
                        println!("{chat}  {name}");
                    }
                }
            },
        },
        Command::Workspace { command } => match command {
            WorkspaceCommand::Add { alias, path } => {
                anyhow::ensure!(
                    !alias.is_empty()
                        && alias
                            .chars()
                            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'),
                    "workspace alias may contain only letters, numbers, _ and -"
                );
                let path = path
                    .canonicalize()
                    .with_context(|| format!("open workspace {}", path.display()))?;
                anyhow::ensure!(path.is_dir(), "workspace path is not a directory");
                anyhow::ensure!(
                    !config
                        .workspaces
                        .iter()
                        .any(|workspace| workspace.alias == alias),
                    "workspace @{alias} already exists"
                );
                config.workspaces.push(reia::config::Workspace {
                    alias: alias.clone(),
                    path: path.clone(),
                });
                config.save(&config_path)?;
                println!("Added @{alias} at {}.", path.display());
            }
            WorkspaceCommand::List => {
                for workspace in &config.workspaces {
                    println!("@{}  {}", workspace.alias, workspace.path.display());
                }
                if config.workspaces.is_empty() {
                    println!(
                        "No workspaces configured. Add one with `reia workspace add repo /path/to/repo`."
                    );
                }
            }
            WorkspaceCommand::Remove { alias } => {
                let length = config.workspaces.len();
                config
                    .workspaces
                    .retain(|workspace| workspace.alias != alias);
                anyhow::ensure!(
                    config.workspaces.len() < length,
                    "workspace @{alias} does not exist"
                );
                config.save(&config_path)?;
                println!("Removed @{alias}.");
            }
        },
    }
    Ok(())
}

fn load_config(path: &std::path::Path) -> Result<Config> {
    match Config::load(path) {
        Ok(config) => Ok(config),
        Err(_error) if !path.exists() => Ok(Config::default()),
        Err(error) => Err(error).context("could not load Reia configuration"),
    }
}

fn open_knowledge(data_dir: &Path) -> Result<reia::knowledge::KnowledgeStore> {
    let key = storage::database_key()?;
    let path = data_dir.join("knowledge.db");
    let store = reia::knowledge::KnowledgeStore::open(&path, &key)?;
    storage::ensure_private_file(&path)?;
    Ok(store)
}

fn choose_chats(chats: &[reia::Chat]) -> Result<Vec<String>> {
    anyhow::ensure!(
        !chats.is_empty(),
        "no chats are indexed yet; run `reia run` and wait for WhatsApp history sync"
    );
    println!("Choose chats and groups Reia may use (comma-separated numbers):");
    for (index, chat) in chats.iter().enumerate() {
        println!(
            "{}. {}{}  [{}]",
            index + 1,
            chat.name,
            if chat.is_group { " (group)" } else { "" },
            chat.id
        );
    }
    print!("Selection: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let mut selected = BTreeSet::new();
    for token in line
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        let index = token
            .parse::<usize>()
            .context("enter chat numbers separated by commas")?;
        anyhow::ensure!(
            (1..=chats.len()).contains(&index),
            "chat number {index} is out of range"
        );
        selected.insert(chats[index - 1].id.clone());
    }
    Ok(selected.into_iter().collect())
}

async fn doctor(config: &Config) -> Result<()> {
    let agent = LocalAgent::new(
        AgentConfig {
            ollama_url: config.ollama_url.clone(),
            model: config.model.clone(),
        },
        storage::data_dir()?.join("agents"),
    )?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = client
        .get(format!(
            "{}/api/version",
            config.ollama_url.trim_end_matches('/')
        ))
        .send()
        .await
        .context("Ollama is not reachable on the configured local endpoint")?;
    anyhow::ensure!(
        response.status().is_success(),
        "Ollama returned {}",
        response.status()
    );
    let available = client
        .get(format!(
            "{}/api/tags",
            config.ollama_url.trim_end_matches('/')
        ))
        .send()
        .await?
        .error_for_status()?
        .json::<serde_json::Value>()
        .await?;
    let installed = available["models"].as_array().is_some_and(|models| {
        models.iter().any(|model| {
            model["name"].as_str().is_some_and(|name| {
                name == config.model || name.starts_with(&format!("{}:", config.model))
            })
        })
    });
    anyhow::ensure!(
        installed,
        "model {} is not installed; run `ollama pull {}`",
        config.model,
        config.model
    );
    agent.doctor()?;
    println!("Ollama is ready with {}.", config.model);
    println!("Hermes ACP is ready with separate personal and coding modes.");
    Ok(())
}
