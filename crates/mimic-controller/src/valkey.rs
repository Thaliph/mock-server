//! Connection to Valkey, the controller's only state (spec §7, FND-06).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mimic_runtime::config::{ConfigError, Env};
use redis::aio::MultiplexedConnection;

const CHECK_EVERY: Duration = Duration::from_secs(1);
const TIMEOUT: Duration = Duration::from_secs(2);

/// `MIMIC_VALKEY_URL` and `MIMIC_VALKEY_PASSWORD`, turned into a client. The password never
/// appears in the URL given by the operator, nor in any message.
pub fn client_from_env(env: &Env) -> Result<redis::Client, ConfigError> {
    const VAR: &str = "MIMIC_VALKEY_URL";
    let url = env.required(VAR)?;
    let Some((scheme, rest)) = url.split_once("://").filter(|(s, _)| *s == "redis") else {
        return Err(ConfigError::invalid(
            VAR,
            url,
            "expected redis://host:port[/db]",
        ));
    };
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.contains('@') {
        return Err(ConfigError::Rejected {
            var: VAR.into(),
            reason: "credentials are not allowed in the URL, set MIMIC_VALKEY_PASSWORD instead"
                .into(),
        });
    }
    if authority.is_empty() {
        return Err(ConfigError::invalid(VAR, url, "missing host"));
    }
    let full = match env.optional("MIMIC_VALKEY_PASSWORD") {
        Some(password) => format!("{scheme}://:{}@{rest}", percent_encode(password)),
        None => url.to_string(),
    };
    // The parse error may echo the URL, which now holds the password: keep only its kind.
    redis::Client::open(full).map_err(|e| ConfigError::invalid(VAR, url, format!("{:?}", e.kind())))
}

fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Pings Valkey every second and keeps the result for `/readyz`; logs each transition once.
pub struct Health {
    up: AtomicBool,
}

impl Health {
    pub fn is_up(&self) -> bool {
        self.up.load(Ordering::Relaxed)
    }

    pub fn spawn(client: redis::Client) -> Arc<Health> {
        let health = Arc::new(Health {
            up: AtomicBool::new(false),
        });
        let state = health.clone();
        tokio::spawn(async move {
            let mut conn: Option<MultiplexedConnection> = None;
            let mut reported: Option<bool> = None;
            loop {
                let result = ping(&client, &mut conn).await;
                let up = result.is_ok();
                state.up.store(up, Ordering::Relaxed);
                if reported != Some(up) {
                    match result {
                        Ok(()) => tracing::info!("valkey reachable"),
                        Err(error) => tracing::warn!(%error, "valkey unreachable"),
                    }
                    reported = Some(up);
                }
                tokio::time::sleep(CHECK_EVERY).await;
            }
        });
        health
    }
}

async fn ping(
    client: &redis::Client,
    conn: &mut Option<MultiplexedConnection>,
) -> Result<(), String> {
    if conn.is_none() {
        let fresh = tokio::time::timeout(TIMEOUT, client.get_multiplexed_async_connection())
            .await
            .map_err(|_| "connection timed out".to_string())?
            .map_err(|e| e.to_string())?;
        *conn = Some(fresh);
    }
    let Some(c) = conn.as_mut() else {
        unreachable!()
    };
    let reply = tokio::time::timeout(TIMEOUT, redis::cmd("PING").query_async::<String>(c)).await;
    match reply {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => {
            *conn = None;
            Err(e.to_string())
        }
        Err(_) => {
            *conn = None;
            Err("PING timed out".to_string())
        }
    }
}
