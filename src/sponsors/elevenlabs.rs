use anyhow::{Context, Result, bail};
use reqwest::multipart::{Form, Part};
use serde::Deserialize;

const TRANSCRIPTION_URL: &str = "https://api.elevenlabs.io/v1/speech-to-text";
const MAX_AUDIO_BYTES: usize = 16 * 1024 * 1024;

#[derive(Deserialize)]
struct Transcript {
    text: String,
}

/// Transcribes one audio file with ElevenLabs Scribe v2.
pub async fn transcribe(
    client: &reqwest::Client,
    api_key: &str,
    filename: &str,
    mime_type: &str,
    audio: &[u8],
) -> Result<String> {
    transcribe_at(
        client,
        TRANSCRIPTION_URL,
        api_key,
        filename,
        mime_type,
        audio,
    )
    .await
}

async fn transcribe_at(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: &str,
    filename: &str,
    mime_type: &str,
    audio: &[u8],
) -> Result<String> {
    anyhow::ensure!(!api_key.is_empty(), "ElevenLabs API key is empty");
    anyhow::ensure!(!filename.is_empty(), "audio filename is empty");
    anyhow::ensure!(!audio.is_empty(), "audio file is empty");
    anyhow::ensure!(
        audio.len() <= MAX_AUDIO_BYTES,
        "audio exceeds the 16 MiB transcription limit"
    );

    let file = Part::bytes(audio.to_vec())
        .file_name(filename.to_owned())
        .mime_str(mime_type)
        .context("invalid audio MIME type")?;
    let form = Form::new().text("model_id", "scribe_v2").part("file", file);

    let response = client
        .post(endpoint)
        .header("xi-api-key", api_key)
        .multipart(form)
        .send()
        .await
        .context("send transcription request to ElevenLabs")?;
    let status = response.status();
    if !status.is_success() {
        bail!("ElevenLabs transcription failed with HTTP {status}");
    }

    response
        .json::<Transcript>()
        .await
        .context("parse ElevenLabs transcription response")
        .map(|transcript| transcript.text)
}

#[cfg(test)]
mod tests {
    use super::transcribe_at;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn posts_audio_as_scribe_v2_multipart_and_returns_text() {
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
                assert_ne!(count, 0, "client closed before sending multipart body");
                request.extend_from_slice(&chunk[..count]);
            }

            let raw = String::from_utf8_lossy(&request).to_ascii_lowercase();
            assert!(raw.contains("xi-api-key: test-secret"));
            assert!(raw.contains("name=\"model_id\""));
            assert!(raw.contains("scribe_v2"));
            assert!(raw.contains("name=\"file\"; filename=\"voice.ogg\""));
            assert!(raw.contains("content-type: audio/ogg"));
            assert!(request.windows(8).any(|window| window == b"opusdata"));

            let body = r#"{"text":"turn on the lights"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let client = reqwest::Client::new();
        let transcript = transcribe_at(
            &client,
            &format!("http://{address}/v1/speech-to-text"),
            "test-secret",
            "voice.ogg",
            "audio/ogg",
            b"opusdata",
        )
        .await
        .unwrap();

        assert_eq!(transcript, "turn on the lights");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_audio_above_limit_before_network_request() {
        let client = reqwest::Client::new();
        let oversized = vec![0; 16 * 1024 * 1024 + 1];
        let error = transcribe_at(
            &client,
            "http://127.0.0.1:1/unused",
            "test-secret",
            "voice.ogg",
            "audio/ogg",
            &oversized,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("16 MiB"));
    }
}
