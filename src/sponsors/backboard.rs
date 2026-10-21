use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client as HttpClient, Method, Response, Url};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use std::time::Duration;

const API_URL: &str = "https://app.backboard.io/api";
const MAX_BODY_BYTES: usize = 16 * 1024;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_SEARCH_RESULTS: u8 = 20;

/// Minimal client for Backboard's assistant-scoped memory API.
pub struct BackboardClient {
    http: HttpClient,
    base_url: Url,
    api_key: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Assistant {
    pub assistant_id: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Memory {
    #[serde(alias = "memory_id")]
    pub id: String,
    pub content: String,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub score: Option<f32>,
}

#[derive(Serialize)]
struct CreateAssistant<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_prompt: Option<&'a str>,
}

#[derive(Deserialize)]
struct CreateAssistantResponse {
    assistant_id: String,
    name: String,
}

#[derive(Serialize)]
struct AddMemory<'a> {
    content: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<&'a Map<String, Value>>,
}

#[derive(Deserialize)]
struct AddMemoryResponse {
    #[serde(alias = "id")]
    memory_id: String,
}

#[derive(Serialize)]
struct SearchMemories<'a> {
    query: &'a str,
    limit: u8,
}

#[derive(Deserialize)]
struct SearchMemoriesResponse {
    memories: Vec<Memory>,
}

impl BackboardClient {
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Self::with_base_url(api_key, API_URL)
    }

    fn with_base_url(api_key: impl Into<String>, base_url: &str) -> Result<Self> {
        let api_key = api_key.into();
        ensure!(!api_key.trim().is_empty(), "Backboard API key is empty");
        ensure!(
            api_key.len() <= 4096 && !api_key.contains(['\r', '\n']),
            "Backboard API key is invalid"
        );

        let base_url = Url::parse(base_url).context("parse Backboard API URL")?;
        ensure!(
            base_url.scheme() == "https"
                || (base_url.scheme() == "http" && base_url.host_str().is_some_and(is_loopback)),
            "Backboard API URL must use HTTPS"
        );
        let http = HttpClient::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .build()
            .context("create Backboard HTTP client")?;
        Ok(Self {
            http,
            base_url,
            api_key,
        })
    }

    pub async fn create_assistant(
        &self,
        name: &str,
        system_prompt: Option<&str>,
    ) -> Result<Assistant> {
        ensure!(
            !name.trim().is_empty() && name.chars().count() <= 255,
            "assistant name must contain 1 to 255 characters"
        );
        ensure!(
            system_prompt.is_none_or(|prompt| prompt.len() <= MAX_BODY_BYTES),
            "assistant prompt exceeds 16 KiB"
        );
        let response: CreateAssistantResponse = self
            .json_request(
                Method::POST,
                &["assistants"],
                &CreateAssistant {
                    name,
                    system_prompt,
                },
            )
            .await?;
        validate_uuid(&response.assistant_id, "assistant ID")?;
        ensure!(
            response.name == name,
            "Backboard returned a different assistant name"
        );
        Ok(Assistant {
            assistant_id: response.assistant_id,
            name: response.name,
        })
    }

    pub async fn add_memory(
        &self,
        assistant_id: &str,
        content: &str,
        metadata: Option<&Map<String, Value>>,
    ) -> Result<String> {
        validate_uuid(assistant_id, "assistant ID")?;
        validate_text(content, "memory")?;
        let response: AddMemoryResponse = self
            .json_request(
                Method::POST,
                &["assistants", assistant_id, "memories"],
                &AddMemory { content, metadata },
            )
            .await?;
        validate_id_segment(&response.memory_id, "memory ID")?;
        Ok(response.memory_id)
    }

    pub async fn search_memories(
        &self,
        assistant_id: &str,
        query: &str,
        limit: u8,
    ) -> Result<Vec<Memory>> {
        validate_uuid(assistant_id, "assistant ID")?;
        validate_text(query, "search query")?;
        ensure!(
            (1..=MAX_SEARCH_RESULTS).contains(&limit),
            "search result limit must be between 1 and {MAX_SEARCH_RESULTS}"
        );
        let response: SearchMemoriesResponse = self
            .json_request(
                Method::POST,
                &["assistants", assistant_id, "memories", "search"],
                &SearchMemories { query, limit },
            )
            .await?;
        ensure!(
            response.memories.len() <= usize::from(limit),
            "Backboard returned more memories than requested"
        );
        for memory in &response.memories {
            validate_id_segment(&memory.id, "memory ID")?;
            ensure!(
                memory.content.len() <= MAX_BODY_BYTES,
                "Backboard memory exceeds 16 KiB"
            );
            ensure!(
                memory.score.is_none_or(f32::is_finite),
                "Backboard returned an invalid relevance score"
            );
        }
        Ok(response.memories)
    }

    pub async fn delete_memory(&self, assistant_id: &str, memory_id: &str) -> Result<()> {
        validate_uuid(assistant_id, "assistant ID")?;
        validate_id_segment(memory_id, "memory ID")?;
        let response = self
            .http
            .delete(self.url(&["assistants", assistant_id, "memories", memory_id])?)
            .header("X-API-Key", &self.api_key)
            .send()
            .await
            .context("send Backboard delete-memory request")?;
        ensure!(
            response.status().is_success(),
            "Backboard delete-memory request returned HTTP {}",
            response.status()
        );
        read_limited(response).await?;
        Ok(())
    }

    async fn json_request<B: Serialize, T: DeserializeOwned>(
        &self,
        method: Method,
        path: &[&str],
        body: &B,
    ) -> Result<T> {
        let body = serde_json::to_vec(body).context("serialize Backboard API request")?;
        ensure!(
            body.len() <= MAX_BODY_BYTES,
            "Backboard request exceeds 16 KiB"
        );
        let response = self
            .http
            .request(method, self.url(path)?)
            .header("X-API-Key", &self.api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .context("send Backboard API request")?;
        ensure!(
            response.status().is_success(),
            "Backboard API request returned HTTP {}",
            response.status()
        );
        let bytes = read_limited(response).await?;
        serde_json::from_slice(&bytes).context("parse Backboard API response")
    }

    fn url(&self, path: &[&str]) -> Result<Url> {
        let mut url = self.base_url.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| anyhow::anyhow!("Backboard API URL cannot accept path segments"))?;
            segments.pop_if_empty();
            for segment in path {
                segments.push(segment);
            }
        }
        Ok(url)
    }
}

fn validate_text(value: &str, label: &str) -> Result<()> {
    ensure!(!value.trim().is_empty(), "{label} is empty");
    ensure!(value.len() <= MAX_BODY_BYTES, "{label} exceeds 16 KiB");
    Ok(())
}

fn validate_uuid(value: &str, label: &str) -> Result<()> {
    let valid = value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        });
    ensure!(valid, "invalid Backboard {label}");
    Ok(())
}

fn validate_id_segment(value: &str, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "invalid Backboard {label}"
    );
    Ok(())
}

fn is_loopback(host: &str) -> bool {
    host == "localhost" || host == "127.0.0.1" || host == "[::1]"
}

async fn read_limited(mut response: Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        bail!("Backboard response exceeds 1 MiB");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.context("read Backboard response")? {
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= MAX_RESPONSE_BYTES,
            "Backboard response exceeds 1 MiB"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::BackboardClient;
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    #[tokio::test]
    async fn authenticated_memory_lifecycle_uses_documented_endpoints() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let responses = [
                (
                    "POST",
                    "/api/assistants",
                    r#"{"assistant_id":"123e4567-e89b-12d3-a456-426614174000","name":"Reia"}"#,
                ),
                (
                    "POST",
                    "/api/assistants/123e4567-e89b-12d3-a456-426614174000/memories",
                    r#"{"memory_id":"123e4567-e89b-12d3-a456-426614174001"}"#,
                ),
                (
                    "POST",
                    "/api/assistants/123e4567-e89b-12d3-a456-426614174000/memories/search",
                    r#"{"memories":[{"id":"123e4567-e89b-12d3-a456-426614174001","content":"Use Rust","score":0.9,"metadata":{"source":"manual"}}]}"#,
                ),
                (
                    "DELETE",
                    "/api/assistants/123e4567-e89b-12d3-a456-426614174000/memories/123e4567-e89b-12d3-a456-426614174001",
                    r#"{"message":"deleted"}"#,
                ),
            ];
            let mut seen = Vec::new();
            for (method, path, response_body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let (headers, body) = read_request(&mut stream);
                let request_line = headers.lines().next().unwrap().to_owned();
                assert!(
                    headers
                        .to_ascii_lowercase()
                        .contains("x-api-key: test-secret")
                );
                assert!(request_line.starts_with(&format!("{method} {path} HTTP/1.1")));
                seen.push((request_line, body));
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                )
                .unwrap();
            }
            seen
        });

        let client =
            BackboardClient::with_base_url("test-secret", &format!("http://{address}/api"))
                .unwrap();
        let assistant = client.create_assistant("Reia", None).await.unwrap();
        let id = client
            .add_memory(
                &assistant.assistant_id,
                "Use Rust",
                Some(json!({"source":"manual"}).as_object().unwrap()),
            )
            .await
            .unwrap();
        let found = client
            .search_memories(&assistant.assistant_id, "language preference", 3)
            .await
            .unwrap();
        assert_eq!(found[0].id, id);
        assert_eq!(found[0].content, "Use Rust");
        client
            .delete_memory(&assistant.assistant_id, &id)
            .await
            .unwrap();

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[1].1).unwrap()["content"],
            "Use Rust"
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[2].1).unwrap()["limit"],
            3
        );
    }

    fn read_request(stream: &mut std::net::TcpStream) -> (String, Vec<u8>) {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 1];
        while !bytes.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut chunk).unwrap();
            bytes.push(chunk[0]);
        }
        let headers = String::from_utf8(bytes).unwrap();
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|length| length.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        let mut body = vec![0; content_length];
        stream.read_exact(&mut body).unwrap();
        (headers, body)
    }
}
