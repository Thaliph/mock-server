//! Garde-fous sous charge (spec §9.3) : logs d'erreurs répétitives limités en débit, et
//! pause après une erreur d'`accept` au lieu d'une boucle à vide.

use std::future::Future;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Pause après une erreur d'`accept` (EMFILE…) avant de réessayer.
pub const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// Au plus une ligne par période pour un même log ; les lignes omises sont comptées et
/// annoncées par la ligne suivante.
pub struct LogLimiter {
    period: Duration,
    /// Dernière ligne écrite, lignes omises depuis.
    state: Mutex<(Option<Instant>, u64)>,
}

impl LogLimiter {
    pub const fn new(period: Duration) -> Self {
        Self {
            period,
            state: Mutex::new((None, 0)),
        }
    }

    /// Une ligne par seconde.
    pub const fn per_second() -> Self {
        Self::new(Duration::from_secs(1))
    }

    /// `Some(omises)` si une ligne peut être écrite à `now`, avec le nombre de lignes omises
    /// depuis la précédente ; `None` si elle doit être omise (elle est alors comptée).
    pub fn allow(&self, now: Instant) -> Option<u64> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (last, omitted) = &mut *state;
        if last.is_some_and(|t| now.saturating_duration_since(t) < self.period) {
            *omitted += 1;
            return None;
        }
        *last = Some(now);
        Some(std::mem::take(omitted))
    }
}

/// Attend la prochaine connexion. Après une erreur, l'écoute est suspendue
/// [`ACCEPT_BACKOFF`] puis reprise ; l'erreur est journalisée au plus une fois par seconde.
pub async fn accept_with_backoff<T, F, Fut>(mut accept: F, log: &LogLimiter) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::io::Result<T>>,
{
    loop {
        match accept().await {
            Ok(c) => return c,
            Err(e) => {
                if let Some(omitted) = log.allow(Instant::now()) {
                    tracing::warn!(error = %e, omitted, "accept échoué, écoute suspendue 50 ms");
                }
                tokio::time::sleep(ACCEPT_BACKOFF).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn prx_10_one_line_per_period_then_the_omitted_count() {
        let l = LogLimiter::per_second();
        let t0 = Instant::now();
        assert_eq!(l.allow(t0), Some(0));
        for i in 1..=999 {
            assert_eq!(l.allow(t0 + Duration::from_micros(i)), None);
        }
        assert_eq!(l.allow(t0 + Duration::from_millis(1000)), Some(999));
        assert_eq!(l.allow(t0 + Duration::from_millis(1500)), None);
        assert_eq!(l.allow(t0 + Duration::from_millis(2100)), Some(1));
    }

    #[tokio::test]
    async fn prx_09_accept_errors_pause_before_retrying() {
        let calls = AtomicUsize::new(0);
        let log = LogLimiter::per_second();
        let started = Instant::now();
        let got = accept_with_backoff(
            || {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n < 3 {
                        Err(std::io::Error::from_raw_os_error(24)) // EMFILE
                    } else {
                        Ok(n)
                    }
                }
            },
            &log,
        )
        .await;
        assert_eq!(got, 3);
        assert!(started.elapsed() >= ACCEPT_BACKOFF * 3);
        // Trois erreurs en 150 ms : une seule ligne écrite, deux omises.
        assert_eq!(log.allow(Instant::now()), None);
    }
}
