use super::AgentConfig;
use anyhow::{Context, Result, bail};
use std::{fs, path::Path};

pub(super) fn validate_ollama_url(url: &str) -> Result<()> {
    let url = url.trim().trim_end_matches('/');
    let Some(rest) = url.strip_prefix("http://") else {
        bail!("Ollama must use a loopback http:// endpoint");
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    anyhow::ensure!(
        path.is_empty(),
        "Ollama URL must not include a path, query, or credentials"
    );
    anyhow::ensure!(
        !authority.contains(['@', '?', '#']),
        "Ollama URL must not include a path, query, or credentials"
    );
    let hostname = if let Some(bracketed) = authority.strip_prefix('[') {
        let (hostname, suffix) = bracketed
            .split_once(']')
            .context("invalid IPv6 loopback URL")?;
        validate_port_suffix(suffix)?;
        hostname
    } else {
        let (hostname, port) = authority.split_once(':').unwrap_or((authority, ""));
        if !port.is_empty() {
            anyhow::ensure!(
                port.bytes().all(|byte| byte.is_ascii_digit()),
                "invalid Ollama port"
            );
        } else if authority.contains(':') {
            bail!("invalid Ollama port");
        }
        hostname
    };
    anyhow::ensure!(
        matches!(hostname, "127.0.0.1" | "localhost" | "::1"),
        "Ollama must use a loopback http:// endpoint"
    );
    anyhow::ensure!(!hostname.is_empty(), "Ollama URL has no host");
    Ok(())
}

fn validate_port_suffix(suffix: &str) -> Result<()> {
    if suffix.is_empty() {
        return Ok(());
    }
    let port = suffix
        .strip_prefix(':')
        .context("invalid IPv6 loopback port")?;
    anyhow::ensure!(
        !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()),
        "invalid Ollama port"
    );
    Ok(())
}

pub(super) fn prepare(state_dir: &Path, config: &AgentConfig) -> Result<()> {
    for (name, tools) in [
        ("hermes-personal", "[]"),
        ("hermes-coding", "[file, terminal]"),
    ] {
        let home = state_dir.join(name);
        fs::create_dir_all(&home).with_context(|| format!("create {}", home.display()))?;
        private_dir(&home)?;
        let config_path = home.join("config.yaml");
        let yaml = hermes_config(config, tools);
        fs::write(&config_path, yaml)
            .with_context(|| format!("write {}", config_path.display()))?;
        private_file(&config_path)?;
    }
    Ok(())
}

fn hermes_config(config: &AgentConfig, tools: &str) -> String {
    let base_url = format!("{}/v1", config.ollama_url.trim().trim_end_matches('/'));
    format!(
        "model:\n  default: {}\n  provider: custom\n  base_url: {}\n  api_key: not-needed\nfallback_providers: []\nauxiliary: {}\nmcp_servers: {}\nplatform_toolsets:\n  acp: {}\nagent:\n  disabled_toolsets: [web, browser, skills, memory, delegation, code_execution, connections, tts, vision, computer_use, cronjob]\nmemory:\n  memory_enabled: false\n  user_profile_enabled: false\nplugins:\n  enabled: []\nterminal:\n  backend: local\n",
        serde_json::to_string(&config.model).expect("serializing a Rust string cannot fail"),
        serde_json::to_string(&base_url).expect("serializing a Rust string cannot fail"),
        "{}",
        "{}",
        tools,
    )
}

#[cfg(unix)]
fn private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn private_dir(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn private_file(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{hermes_config, validate_ollama_url};
    use crate::agent::AgentConfig;

    #[test]
    fn hermes_profiles_pin_local_model_and_separate_toolsets() {
        let config = AgentConfig {
            ollama_url: "http://127.0.0.1:11434".into(),
            model: "gemma4:e2b".into(),
        };
        let personal = hermes_config(&config, "[]");
        let coding = hermes_config(&config, "[file, terminal]");
        assert!(personal.contains("acp: []"));
        assert!(coding.contains("acp: [file, terminal]"));
        assert!(personal.contains("fallback_providers: []"));
        assert!(personal.contains("memory_enabled: false"));
        assert!(personal.contains("base_url: \"http://127.0.0.1:11434/v1\""));
    }

    #[test]
    fn ollama_endpoint_must_be_http_on_loopback() {
        assert!(validate_ollama_url("http://127.0.0.1:11434").is_ok());
        assert!(validate_ollama_url("http://[::1]:11434").is_ok());
        assert!(validate_ollama_url("http://localhost:11434").is_ok());
        assert!(validate_ollama_url("https://localhost:11434").is_err());
        assert!(validate_ollama_url("http://192.168.1.2:11434").is_err());
        assert!(validate_ollama_url("http://localhost.evil:11434").is_err());
        assert!(validate_ollama_url("http://127.0.0.1:11434.evil").is_err());
        assert!(validate_ollama_url("http://127.0.0.1@evil:11434").is_err());
    }
}
