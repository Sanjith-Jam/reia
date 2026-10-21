use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client as HttpClient, Response, Url};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const API_URL: &str = "https://api.browserbase.com/v1/";
const MAX_QUERY_CHARS: usize = 200;
const MAX_SEARCH_RESULTS: usize = 5;
const MAX_FETCHES: usize = 3;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_PAGE_CHARS: usize = 8_000;

/// Bounded Search → Fetch client for Browserbase's official REST API.
pub struct BrowserbaseClient {
    http: HttpClient,
    base_url: Url,
    api_key: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResearchSource {
    pub title: String,
    pub url: String,
    pub description: Option<String>,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResearchReport {
    pub query: String,
    pub sources: Vec<ResearchSource>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchRequest<'a> {
    query: &'a str,
    num_results: usize,
}

#[derive(Deserialize)]
struct SearchResponse {
    results: Vec<SearchResult>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FetchRequest<'a> {
    url: &'a str,
    format: &'static str,
    allow_redirects: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FetchResponse {
    status_code: u16,
    content: String,
}

impl BrowserbaseClient {
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Self::with_base_url(api_key, API_URL)
    }

    fn with_base_url(api_key: impl Into<String>, base_url: &str) -> Result<Self> {
        let api_key = api_key.into();
        ensure!(
            !api_key.trim().is_empty() && api_key.len() <= 4096 && !api_key.contains(['\r', '\n']),
            "Browserbase API key is invalid"
        );
        let base_url = Url::parse(base_url).context("parse Browserbase API URL")?;
        ensure!(
            base_url.scheme() == "https"
                || (base_url.scheme() == "http" && base_url.host_str().is_some_and(is_loopback)),
            "Browserbase API URL must use HTTPS"
        );
        let http = HttpClient::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .context("create Browserbase HTTP client")?;
        Ok(Self {
            http,
            base_url,
            api_key,
        })
    }

    /// Search for a topic and fetch up to three ranked pages as Markdown.
    pub async fn research(&self, query: &str, fetch_count: usize) -> Result<ResearchReport> {
        let query = query.trim();
        ensure!(
            !query.is_empty() && query.chars().count() <= MAX_QUERY_CHARS,
            "Browserbase search query must contain 1 to {MAX_QUERY_CHARS} characters"
        );
        ensure!(
            (1..=MAX_FETCHES).contains(&fetch_count),
            "Browserbase fetch count must be between 1 and {MAX_FETCHES}"
        );

        let response = self
            .http
            .post(self.endpoint("search")?)
            .header("x-bb-api-key", &self.api_key)
            .json(&SearchRequest {
                query,
                num_results: MAX_SEARCH_RESULTS,
            })
            .send()
            .await
            .context("send Browserbase search request")?;
        let search: SearchResponse = read_json(response, "search").await?;
        ensure!(
            search.results.len() <= MAX_SEARCH_RESULTS,
            "Browserbase returned more search results than requested"
        );

        let mut sources = Vec::new();
        for result in search.results.into_iter().take(fetch_count) {
            let url = public_http_url(&result.url)?;
            let response = self
                .http
                .post(self.endpoint("fetch")?)
                .header("x-bb-api-key", &self.api_key)
                .json(&FetchRequest {
                    url: url.as_str(),
                    format: "markdown",
                    allow_redirects: false,
                })
                .send()
                .await
                .context("send Browserbase fetch request")?;
            let fetched: FetchResponse = read_json(response, "fetch").await?;
            ensure!(
                (200..300).contains(&fetched.status_code),
                "Browserbase fetch returned target HTTP {}",
                fetched.status_code
            );
            sources.push(ResearchSource {
                title: trim_chars(&result.title, 300),
                url: url.to_string(),
                description: result.description.map(|text| trim_chars(&text, 1_000)),
                content: trim_chars(&fetched.content, MAX_PAGE_CHARS),
            });
        }
        Ok(ResearchReport {
            query: trim_chars(query, MAX_QUERY_CHARS),
            sources,
        })
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        self.base_url
            .join(path)
            .context("build Browserbase API endpoint")
    }
}

async fn read_json<T: for<'de> Deserialize<'de>>(
    mut response: Response,
    endpoint: &str,
) -> Result<T> {
    let status = response.status();
    ensure!(
        status.is_success(),
        "Browserbase {endpoint} returned HTTP {status}"
    );
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        bail!("Browserbase {endpoint} response exceeded the size limit");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("read Browserbase {endpoint} response"))?
    {
        ensure!(
            body.len().saturating_add(chunk.len()) <= MAX_RESPONSE_BYTES,
            "Browserbase {endpoint} response exceeded the size limit"
        );
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).with_context(|| format!("parse Browserbase {endpoint} response"))
}

fn public_http_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).context("Browserbase search result has an invalid URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none(),
        "Browserbase search result URL must be public HTTP(S) without credentials"
    );
    let host = url.host_str().unwrap_or_default();
    ensure!(
        !host.eq_ignore_ascii_case("localhost")
            && !host.to_ascii_lowercase().ends_with(".localhost")
            && !host.eq_ignore_ascii_case("metadata.google.internal"),
        "Browserbase returned a non-public URL"
    );
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        let public = match ip {
            std::net::IpAddr::V4(ip) => {
                !(ip.is_private()
                    || ip.is_loopback()
                    || ip.is_link_local()
                    || ip.is_unspecified()
                    || ip.is_broadcast())
            }
            std::net::IpAddr::V6(ip) => {
                !(ip.is_loopback()
                    || ip.is_unique_local()
                    || ip.is_unicast_link_local()
                    || ip.is_unspecified()
                    || ip.is_multicast())
            }
        };
        ensure!(public, "Browserbase returned a non-public URL");
    }
    Ok(url)
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

fn trim_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::BrowserbaseClient;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn request(socket: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
        let mut bytes = Vec::new();
        let mut chunk = [0; 4096];
        let header_end = loop {
            let count = socket.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0);
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
        let length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .unwrap()
            .trim()
            .parse::<usize>()
            .unwrap();
        while bytes.len() - header_end < length {
            let count = socket.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0);
            bytes.extend_from_slice(&chunk[..count]);
        }
        (headers, bytes[header_end..header_end + length].to_vec())
    }

    async fn respond(socket: &mut tokio::net::TcpStream, body: &str) {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn research_searches_then_fetches_with_bounded_markdown_output() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut search, _) = listener.accept().await.unwrap();
            let (headers, body) = request(&mut search).await;
            assert!(headers.contains("x-bb-api-key: test-secret"));
            assert!(headers.starts_with("post /v1/search "));
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["query"], "rust async");
            assert_eq!(body["numResults"], 5);
            respond(
                &mut search,
                r#"{"results":[{"title":"Rust","url":"https://example.com/rust","description":"Docs"}]}"#,
            )
            .await;

            let (mut fetch, _) = listener.accept().await.unwrap();
            let (headers, body) = request(&mut fetch).await;
            assert!(headers.contains("x-bb-api-key: test-secret"));
            assert!(headers.starts_with("post /v1/fetch "));
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["url"], "https://example.com/rust");
            assert_eq!(body["format"], "markdown");
            assert_eq!(body["allowRedirects"], false);
            respond(
                &mut fetch,
                r##"{"statusCode":200,"content":"# Rust\nA language."}"##,
            )
            .await;
        });

        let client =
            BrowserbaseClient::with_base_url("test-secret", &format!("http://{address}/v1/"))
                .unwrap();
        let report = client.research("  rust async  ", 1).await.unwrap();
        assert_eq!(report.query, "rust async");
        assert_eq!(report.sources.len(), 1);
        assert_eq!(report.sources[0].content, "# Rust\nA language.");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_bad_queries_and_non_public_result_urls() {
        let client =
            BrowserbaseClient::with_base_url("test-secret", "http://127.0.0.1:1/v1/").unwrap();
        assert!(client.research("", 1).await.is_err());
        assert!(client.research("valid", 0).await.is_err());
        assert!(super::public_http_url("file:///etc/passwd").is_err());
        assert!(super::public_http_url("http://127.0.0.1/admin").is_err());
        assert!(super::public_http_url("http://user:pass@example.com/").is_err());
    }
}
