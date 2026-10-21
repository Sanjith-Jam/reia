use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const CHAT_COMPLETIONS_URL: &str = "https://api.paperinstruments.com/v1/chat/completions";
const MODEL: &str = "ultramarine-1-flash";
const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_COMPLETION_TOKENS: u32 = 1024;

#[derive(Serialize)]
struct Request<'a> {
    model: &'static str,
    messages: [Message<'a>; 1],
    reasoning_effort: &'static str,
    max_completion_tokens: u32,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Deserialize)]
struct Completion {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: AssistantMessage,
}

#[derive(Deserialize)]
struct AssistantMessage {
    content: Option<String>,
}

/// Sends one prompt to Ultramarine and returns the assistant's text response.
pub async fn complete(client: &reqwest::Client, api_key: &str, prompt: &str) -> Result<String> {
    complete_at(client, CHAT_COMPLETIONS_URL, api_key, prompt).await
}

async fn complete_at(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: &str,
    prompt: &str,
) -> Result<String> {
    anyhow::ensure!(!api_key.is_empty(), "Paper Instruments API key is empty");
    anyhow::ensure!(!prompt.trim().is_empty(), "Ultramarine prompt is empty");
    anyhow::ensure!(
        prompt.len() <= MAX_PROMPT_BYTES,
        "Ultramarine prompt exceeds 64 KiB"
    );

    let response = client
        .post(endpoint)
        .bearer_auth(api_key)
        .json(&Request {
            model: MODEL,
            messages: [Message {
                role: "user",
                content: prompt,
            }],
            reasoning_effort: "low",
            max_completion_tokens: MAX_COMPLETION_TOKENS,
        })
        .send()
        .await
        .context("send request to Paper Instruments")?;
    let status = response.status();
    if !status.is_success() {
        bail!("Paper Instruments chat completion failed with HTTP {status}");
    }
    if response
        .content_length()
        .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
    {
        bail!("Ultramarine response exceeded 256 KiB");
    }

    let mut response = response;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("read Paper Instruments response")?
    {
        anyhow::ensure!(
            body.len().saturating_add(chunk.len()) <= MAX_RESPONSE_BYTES,
            "Ultramarine response exceeded 256 KiB"
        );
        body.extend_from_slice(&chunk);
    }

    let completion: Completion =
        serde_json::from_slice(&body).context("parse Paper Instruments chat completion")?;
    let answer = completion
        .choices
        .first()
        .and_then(|choice| choice.message.content.as_deref())
        .map(str::trim)
        .filter(|answer| !answer.is_empty())
        .context("Paper Instruments returned no assistant text")?;
    Ok(answer.to_owned())
}

#[cfg(test)]
mod tests {
    use super::complete_at;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn sends_openai_compatible_chat_request_and_returns_text() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            let header_end = loop {
                let count = socket.read(&mut chunk).await.unwrap();
                assert_ne!(count, 0, "client closed before sending headers");
                request.extend_from_slice(&chunk[..count]);
                if let Some(index) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
            let content_length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .unwrap()
                .trim()
                .parse::<usize>()
                .unwrap();
            while request.len() - header_end < content_length {
                let count = socket.read(&mut chunk).await.unwrap();
                assert_ne!(count, 0, "client closed before sending JSON body");
                request.extend_from_slice(&chunk[..count]);
            }

            let raw = String::from_utf8_lossy(&request);
            assert!(headers.contains("post /v1/chat/completions http/1.1"));
            assert!(headers.contains("authorization: bearer test-secret"));
            let body = &raw[header_end..header_end + content_length];
            let body: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(body["model"], "ultramarine-1-flash");
            assert_eq!(body["messages"][0]["role"], "user");
            assert_eq!(body["messages"][0]["content"], "hello");
            assert_eq!(body["reasoning_effort"], "low");
            assert_eq!(body["max_completion_tokens"], 1024);

            let body = r#"{"choices":[{"message":{"role":"assistant","content":"hi there"}}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let answer = complete_at(
            &reqwest::Client::new(),
            &format!("http://{address}/v1/chat/completions"),
            "test-secret",
            "hello",
        )
        .await
        .unwrap();

        assert_eq!(answer, "hi there");
        server.await.unwrap();
    }
}
