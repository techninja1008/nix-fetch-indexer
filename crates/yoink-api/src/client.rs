//! HTTP client for the yoink service.

use crate::{ErrorBody, Event, MIRROR_PATH, MirrorRequest};
use anyhow::{Context, Result};
use futures::stream::{BoxStream, StreamExt};
use std::time::Duration;

/// A non-success HTTP response.
#[derive(Debug, thiserror::Error)]
#[error("HTTP {status}: {message}")]
pub struct HttpError {
    pub status: u16,
    pub message: String,
}

/// The service sends a keep-alive comment every 15 s; a stream silent for
/// this long is considered dead.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

pub type EventStream = BoxStream<'static, Result<Event>>;

impl Client {
    pub fn new(base: &str, token: Option<String>) -> Result<Self> {
        Ok(Client {
            http: reqwest::Client::builder()
                .user_agent(concat!("yoink/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(30))
                .read_timeout(READ_TIMEOUT)
                .build()?,
            base: base.trim_end_matches('/').to_string(),
            token,
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn set_token(&mut self, token: Option<String>) {
        self.token = token;
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let r = self.http.request(method, format!("{}{path}", self.base));
        match &self.token {
            Some(t) => r.bearer_auth(t),
            None => r,
        }
    }

    async fn check(&self, resp: reqwest::Response) -> Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let bytes = resp.bytes().await.unwrap_or_default();
        let message = serde_json::from_slice::<ErrorBody>(&bytes)
            .map(|e| e.error)
            .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned());
        Err(HttpError {
            status: status.as_u16(),
            message,
        }
        .into())
    }

    /// GET a JSON document.
    pub async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let r = self
            .req(reqwest::Method::GET, path)
            .send()
            .await
            .with_context(|| format!("connecting to {}", self.base))?;
        Ok(self.check(r).await?.json().await?)
    }

    /// Start mirroring `purls` and stream the events. The stream ends when
    /// the service closes it (after the last result) or fails.
    pub async fn mirror(&self, purls: &[String]) -> Result<EventStream> {
        let r = self
            .req(reqwest::Method::POST, MIRROR_PATH)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&MirrorRequest {
                purls: purls.to_vec(),
            })
            .send()
            .await
            .with_context(|| format!("connecting to {}", self.base))?;
        let r = self.check(r).await?;
        let mut parser = SseParser::default();
        let s = r
            .bytes_stream()
            .map(move |chunk| -> Vec<Result<Event>> {
                match chunk {
                    Err(e) => vec![Err(anyhow::Error::new(e).context("reading the event stream"))],
                    Ok(b) => parser
                        .feed(&b)
                        .into_iter()
                        .filter_map(|(name, data)| {
                            Event::from_sse(&name, &data)
                                .map(|r| r.with_context(|| format!("decoding {name} event")))
                        })
                        .collect(),
                }
            })
            .flat_map(futures::stream::iter);
        Ok(s.boxed())
    }
}

/// Incremental Server-Sent Events parser.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: String,
    data: String,
}

impl SseParser {
    /// Feed bytes; returns the complete `(event name, data)` pairs.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<(String, String)> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=i).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            if line.is_empty() {
                if !self.data.is_empty() || !self.event.is_empty() {
                    let name = std::mem::take(&mut self.event);
                    let mut data = std::mem::take(&mut self.data);
                    if data.ends_with('\n') {
                        data.pop();
                    }
                    out.push((if name.is_empty() { "message".into() } else { name }, data));
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                None => (&*line, ""),
            };
            match field {
                "event" => self.event = value.to_string(),
                "data" => {
                    self.data.push_str(value);
                    self.data.push('\n');
                }
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sse() {
        let mut p = SseParser::default();
        assert!(p.feed(b": keep-alive\n\nevent: log\ndata: {\"a\"").is_empty());
        let r = p.feed(b":1}\r\n\r\nevent: x\ndata: a\ndata: b\n\n");
        assert_eq!(
            r,
            vec![("log".into(), "{\"a\":1}".into()), ("x".into(), "a\nb".into())]
        );
    }
}
