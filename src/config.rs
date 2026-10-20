use crate::SourceScope;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub owner_jid: Option<String>,
    pub control_chat: Option<String>,
    pub scope: ScopeConfig,
    pub selected_chats: Vec<String>,
    pub ollama_url: String,
    pub model: String,
    pub workspaces: Vec<Workspace>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeConfig {
    #[default]
    All,
    Selected,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Workspace {
    pub alias: String,
    pub path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            owner_jid: None,
            control_chat: None,
            scope: ScopeConfig::All,
            selected_chats: Vec::new(),
            ollama_url: "http://127.0.0.1:11434".into(),
            model: "gemma4:e2b".into(),
            workspaces: Vec::new(),
        }
    }
}

impl Config {
    pub fn source_scope(&self) -> SourceScope {
        match self.scope {
            ScopeConfig::All => SourceScope::All,
            ScopeConfig::Selected => {
                SourceScope::Selected(self.selected_chats.iter().cloned().collect())
            }
        }
    }

    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }

    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}
