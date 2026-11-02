//! Drives the mirror request: streams events, and re-posts the PURLs still
//! without a result whenever the stream ends early.

use crate::client::{Client, EventStream};
use crate::state::Conn;
use nfi::http::{HttpError, is_unauthorized};
use nfi_auth_client::Credentials;
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;
use yoink_api::Event;

/// Reconnect delays: exponential from [`Backoff::FIRST`], capped at
/// [`Backoff::MAX`]; give up after [`Backoff::GIVE_UP`] without progress.
#[derive(Debug, Default)]
pub struct Backoff {
    failing_since: Option<Instant>,
    attempt: u32,
}

impl Backoff {
    pub const FIRST: Duration = Duration::from_millis(500);
    pub const MAX: Duration = Duration::from_secs(30);
    pub const GIVE_UP: Duration = Duration::from_secs(300);

    /// Progress was made: the next failure starts over.
    pub fn success(&mut self) {
        *self = Self::default();
    }

    /// Record a failure; the delay before retrying, or `None` to give up.
    pub fn failure(&mut self, now: Instant) -> Option<Duration> {
        let since = *self.failing_since.get_or_insert(now);
        if now.duration_since(since) >= Self::GIVE_UP {
            return None;
        }
        let d = Self::FIRST
            .saturating_mul(1 << self.attempt.min(16))
            .min(Self::MAX);
        self.attempt += 1;
        Some(d)
    }
}

pub enum Msg {
    Event(Event),
    Conn(Conn),
}

/// Whether a failed request should not be retried.
pub fn is_fatal(e: &anyhow::Error) -> bool {
    e.downcast_ref::<HttpError>()
        .is_some_and(|h| (400..500).contains(&h.status) && !matches!(h.status, 408 | 429))
}

/// Mirror `purls`, sending events and connection changes to `tx` until every
/// PURL has a result, the failure is fatal, or `tx` is closed. `first` is the
/// result of an initial request already made. Each reconnect gets a token
/// from `creds` without prompting (a login may have been refreshed meanwhile),
/// keeping the previous one if that fails.
pub async fn drive(
    mut client: Client,
    purls: Vec<String>,
    first: Option<anyhow::Result<EventStream>>,
    creds: Credentials,
    tx: UnboundedSender<Msg>,
) {
    use futures::StreamExt;
    let mut remaining = purls;
    let mut done = HashSet::new();
    let mut backoff = Backoff::default();
    let mut next = first;
    loop {
        remaining.retain(|p| !done.contains(p));
        if remaining.is_empty() {
            let _ = tx.send(Msg::Conn(Conn::Done));
            return;
        }
        let r = match next.take() {
            Some(r) => r,
            None => {
                if let Ok(t) = creds.token(false).await {
                    client = client.with_token(t);
                }
                client
                    .mirror(&remaining)
                    .await
                    .map_err(|e| match is_unauthorized(&e) {
                        true => e.context(creds.hint()),
                        false => e,
                    })
            }
        };
        let error = match r {
            Err(e) if is_fatal(&e) => {
                let _ = tx.send(Msg::Conn(Conn::Failed(format!("{e:#}"))));
                return;
            }
            Err(e) => format!("{e:#}"),
            Ok(mut s) => {
                if tx.send(Msg::Conn(Conn::Connected)).is_err() {
                    return;
                }
                let mut error = "the stream ended before every result".to_string();
                while let Some(ev) = s.next().await {
                    match ev {
                        Ok(ev) => {
                            match &ev {
                                Event::Phases { .. } => {}
                                Event::Result { purl, .. } => {
                                    done.insert(purl.clone());
                                    backoff.success();
                                }
                                _ => backoff.success(),
                            }
                            if tx.send(Msg::Event(ev)).is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            error = format!("{e:#}");
                            break;
                        }
                    }
                }
                remaining.retain(|p| !done.contains(p));
                if remaining.is_empty() {
                    continue;
                }
                error
            }
        };
        let Some(delay) = backoff.failure(Instant::now()) else {
            let _ = tx.send(Msg::Conn(Conn::Failed(format!(
                "gave up after {} of failures: {error}",
                crate::format::duration(Backoff::GIVE_UP)
            ))));
            return;
        };
        let at = Instant::now() + delay;
        if tx.send(Msg::Conn(Conn::Reconnecting { at, error })).is_err() {
            return;
        }
        tokio::time::sleep(delay).await;
        if tx.is_closed() {
            return;
        }
        let _ = tx.send(Msg::Conn(Conn::Connecting));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        assert_eq!(b.failure(t0), Some(Duration::from_millis(500)));
        assert_eq!(
            b.failure(t0 + Duration::from_secs(1)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            b.failure(t0 + Duration::from_secs(2)),
            Some(Duration::from_secs(2))
        );
        for _ in 0..10 {
            b.failure(t0 + Duration::from_secs(3));
        }
        assert_eq!(b.failure(t0 + Duration::from_secs(4)), Some(Backoff::MAX));
        assert_eq!(b.failure(t0 + Backoff::GIVE_UP), None);
        b.success();
        assert_eq!(b.failure(t0 + Backoff::GIVE_UP), Some(Backoff::FIRST));
    }

    #[test]
    fn fatal_errors() {
        let e = |status| {
            anyhow::Error::new(HttpError {
                status,
                message: "x".into(),
            })
        };
        assert!(is_fatal(&e(401)));
        assert!(is_fatal(&e(403)));
        assert!(!is_fatal(&e(429)));
        assert!(!is_fatal(&e(503)));
        assert!(!is_fatal(&anyhow::anyhow!("connection refused")));
    }
}
