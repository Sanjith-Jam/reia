use super::AgentConfig;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const PROMPT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub(super) fn doctor(hermes_home: &Path, _config: &AgentConfig) -> Result<()> {
    let output = hermes_command(hermes_home, hermes_home)
        .arg("acp")
        .arg("--check")
        .output()
        .context(
            "Hermes ACP is unavailable; install Hermes Agent and ensure `hermes` is on PATH",
        )?;
    anyhow::ensure!(
        output.status.success(),
        "Hermes ACP check failed (exit {})",
        output.status
    );
    Ok(())
}

pub(super) fn prompt(hermes_home: &Path, prompt: &str, workspace: Option<&Path>) -> Result<String> {
    let cwd = workspace.unwrap_or(hermes_home);
    let coding = workspace.is_some();
    let mut child = hermes_command(hermes_home, cwd)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start Hermes ACP; install Hermes Agent and ensure `hermes` is on PATH")?;

    let stdin = child
        .stdin
        .take()
        .context("Hermes ACP stdin was unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("Hermes ACP stdout was unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("Hermes ACP stderr was unavailable")?;
    let (line_tx, line_rx) = mpsc::sync_channel(32);
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if line_tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    thread::spawn(move || std::io::copy(&mut BufReader::new(stderr), &mut std::io::sink()).ok());

    let result = run_session(stdin, line_rx, cwd, prompt, coding);
    stop_child(&mut child);
    result
}

fn hermes_command(hermes_home: &Path, cwd: &Path) -> Command {
    let mut command = Command::new("hermes");
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HERMES_HOME", hermes_home)
        .env("HERMES_ACP_SKIP_CONFIGURED_MCP", "1")
        .env("HERMES_API_TIMEOUT", "1800")
        .current_dir(cwd);
    if let Some(home) = std::env::var_os("HOME") {
        command.env("HOME", home);
    }
    command
}

fn run_session(
    stdin: ChildStdin,
    lines: Receiver<String>,
    cwd: &Path,
    prompt: &str,
    coding: bool,
) -> Result<String> {
    let mut client = RpcClient {
        stdin,
        lines,
        next_id: 1,
        allow_permissions: coding,
    };
    client.request(
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": {},
            "clientInfo": {"name": "Reia", "version": env!("CARGO_PKG_VERSION")}
        }),
        STARTUP_TIMEOUT,
    )?;
    client.notify("notifications/initialized", json!({}))?;
    let session = client.request(
        "session/new",
        json!({"cwd": cwd, "mcpServers": []}),
        STARTUP_TIMEOUT,
    )?;
    let session_id = session
        .get("sessionId")
        .and_then(Value::as_str)
        .context("Hermes ACP did not return a session ID")?;

    if coding {
        client.request(
            "session/set_mode",
            json!({"sessionId": session_id, "modeId": "dont_ask"}),
            STARTUP_TIMEOUT,
        )?;
    }

    client.prompt(session_id, prompt)
}

struct RpcClient {
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
    allow_permissions: bool,
}

impl RpcClient {
    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}))?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            anyhow::ensure!(
                !remaining.is_zero(),
                "timed out waiting for Hermes ACP {method}"
            );
            let line = self
                .lines
                .recv_timeout(remaining)
                .map_err(|error| match error {
                    mpsc::RecvTimeoutError::Timeout => {
                        anyhow::anyhow!("timed out waiting for Hermes ACP {method}")
                    }
                    mpsc::RecvTimeoutError::Disconnected => {
                        anyhow::anyhow!("Hermes ACP exited during {method}")
                    }
                })?;
            let message: Value =
                serde_json::from_str(&line).context("Hermes ACP sent invalid JSON-RPC")?;
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                bail!(
                    "Hermes ACP {method} failed: {}",
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                );
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write(json!({"jsonrpc":"2.0", "method":method, "params":params}))
    }

    fn prompt(&mut self, session_id: &str, prompt: &str) -> Result<String> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({
            "jsonrpc":"2.0", "id":id, "method":"session/prompt",
            "params":{"sessionId":session_id, "prompt":[{"type":"text", "text":prompt}]}
        }))?;
        let deadline = std::time::Instant::now() + PROMPT_TIMEOUT;
        let mut answer = String::new();
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            anyhow::ensure!(!remaining.is_zero(), "Hermes response timed out");
            let line = self
                .lines
                .recv_timeout(remaining)
                .map_err(|error| match error {
                    mpsc::RecvTimeoutError::Timeout => anyhow::anyhow!("Hermes response timed out"),
                    mpsc::RecvTimeoutError::Disconnected => {
                        anyhow::anyhow!("Hermes ACP exited before completing the response")
                    }
                })?;
            let message: Value =
                serde_json::from_str(&line).context("Hermes ACP sent invalid JSON-RPC")?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    bail!(
                        "Hermes prompt failed: {}",
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error")
                    );
                }
                let stop_reason = message
                    .pointer("/result/stopReason")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                anyhow::ensure!(stop_reason != "cancelled", "Hermes response was cancelled");
                if answer.trim().is_empty() {
                    bail!("Hermes completed without an assistant response");
                }
                return Ok(answer.trim().to_owned());
            }
            if let Some(chunk) = assistant_chunk(&message) {
                answer.push_str(chunk);
                if answer.len() > 256_000 {
                    bail!("Hermes response exceeded 256 KB");
                }
            }
            if message.get("method").and_then(Value::as_str) == Some("session/request_permission") {
                self.write(permission_response(&message, self.allow_permissions))?;
            }
        }
    }

    fn write(&mut self, value: Value) -> Result<()> {
        serde_json::to_writer(&mut self.stdin, &value)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }
}

fn assistant_chunk(message: &Value) -> Option<&str> {
    if message.get("method")?.as_str()? != "session/update" {
        return None;
    }
    let params = message.get("params")?;
    if params.get("sessionUpdate")?.as_str()? != "agent_message_chunk" {
        return None;
    }
    params.pointer("/content/text").and_then(Value::as_str)
}

fn permission_response(message: &Value, allow: bool) -> Value {
    let outcome = if allow {
        message
            .pointer("/params/options")
            .and_then(Value::as_array)
            .and_then(|options| {
                options
                    .iter()
                    .find(|option| option.get("kind").and_then(Value::as_str) == Some("allow_once"))
                    .or_else(|| {
                        options.iter().find(|option| {
                            option
                                .get("kind")
                                .and_then(Value::as_str)
                                .is_some_and(|kind| kind.starts_with("allow"))
                        })
                    })
            })
            .and_then(|option| option.get("optionId"))
            .and_then(Value::as_str)
            .map(|option_id| json!({"outcome":"selected", "optionId":option_id}))
            .unwrap_or_else(|| json!({"outcome":"cancelled"}))
    } else {
        json!({"outcome":"cancelled"})
    };
    json!({"jsonrpc":"2.0", "id":message.get("id").cloned().unwrap_or(Value::Null), "result":{"outcome":outcome}})
}

fn stop_child(child: &mut Child) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::{assistant_chunk, permission_response};
    use serde_json::json;

    #[test]
    fn collects_only_assistant_text_chunks() {
        let message = json!({"method":"session/update", "params":{
            "sessionUpdate":"agent_message_chunk", "content":{"type":"text", "text":"hello"}
        }});
        assert_eq!(assistant_chunk(&message), Some("hello"));
        let tool = json!({"method":"session/update", "params":{
            "sessionUpdate":"tool_call", "content":{"type":"text", "text":"private args"}
        }});
        assert_eq!(assistant_chunk(&tool), None);
    }

    #[test]
    fn permission_requests_fail_closed_without_coding_authorization() {
        let request = json!({"id":7,"method":"session/request_permission","params":{"options":[
            {"kind":"allow_once","optionId":"once"}, {"kind":"reject_once","optionId":"deny"}
        ]}});
        assert_eq!(
            permission_response(&request, false)["result"]["outcome"]["outcome"],
            "cancelled"
        );
        assert_eq!(
            permission_response(&request, true)["result"]["outcome"]["optionId"],
            "once"
        );
    }
}
