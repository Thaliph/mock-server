//! Élection de la réplique active du contrôleur par un bail (spec §5.5.4).
//!
//! Fonctions pures : l'horloge est celle de la réplique, monotone, en millisecondes, passée en
//! paramètre. Les dates écrites dans le bail ne servent jamais à décider : un bail est expiré
//! quand la réplique l'a vu inchangé (même version) pendant sa durée, sur sa propre horloge.

/// Délais de l'élection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timings {
    /// Bail vu inchangé depuis ce délai : expiré.
    pub lease_duration_ms: u64,
    /// L'active se tient pour perdue si son dernier renouvellement réussi a commencé il y a
    /// ce délai ou plus.
    pub renew_deadline_ms: u64,
    /// Renouvellement par l'active, relecture par la passive.
    pub retry_period_ms: u64,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            lease_duration_ms: 15_000,
            renew_deadline_ms: 10_000,
            retry_period_ms: 2_000,
        }
    }
}

impl Timings {
    /// `retry_period` < `renew_deadline` < `lease_duration` : l'active cesse d'écrire avant
    /// qu'une autre puisse prendre le bail, et renouvelle plusieurs fois avant d'abandonner.
    pub fn validate(&self) -> Result<(), String> {
        if self.retry_period_ms == 0
            || self.retry_period_ms >= self.renew_deadline_ms
            || self.renew_deadline_ms >= self.lease_duration_ms
        {
            return Err(format!(
                "élection : il faut 0 < retry_period ({} ms) < renew_deadline ({} ms) < lease_duration ({} ms)",
                self.retry_period_ms, self.renew_deadline_ms, self.lease_duration_ms
            ));
        }
        Ok(())
    }
}

/// Contenu du bail.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Record {
    /// Détenteur ; `None` : bail rendu.
    pub holder: Option<String>,
    pub duration_ms: u64,
    /// Changements de détenteur.
    pub transitions: u32,
}

/// Bail tel que relu, avec sa version (`resourceVersion`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub record: Record,
    pub version: String,
}

/// Version du bail vue par la réplique, et depuis quand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub version: String,
    pub since_ms: u64,
}

/// Ce que la réplique fait du bail relu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Écrire ce contenu, à condition que la version soit encore `expected` (`None` :
    /// création). `acquire` : prise du bail (sinon, renouvellement par son détenteur).
    Write {
        record: Record,
        expected: Option<String>,
        acquire: bool,
    },
    /// Bail tenu par une autre réplique, encore valable.
    Wait { holder: Option<String> },
}

/// Met à jour l'observation : une nouvelle version repart de `now_ms`.
pub fn observe(prev: Option<Observed>, lease: Option<&Lease>, now_ms: u64) -> Option<Observed> {
    let lease = lease?;
    match prev {
        Some(o) if o.version == lease.version => Some(o),
        _ => Some(Observed {
            version: lease.version.clone(),
            since_ms: now_ms,
        }),
    }
}

/// Décision de la réplique `me` sur le bail relu.
pub fn decide(
    me: &str,
    lease: Option<&Lease>,
    observed: Option<&Observed>,
    timings: &Timings,
    now_ms: u64,
) -> Decision {
    let mine = |transitions| Record {
        holder: Some(me.to_string()),
        duration_ms: timings.lease_duration_ms,
        transitions,
    };
    let Some(lease) = lease else {
        return Decision::Write {
            record: mine(0),
            expected: None,
            acquire: true,
        };
    };
    let record = &lease.record;
    if record.holder.as_deref() == Some(me) {
        return Decision::Write {
            record: mine(record.transitions),
            expected: Some(lease.version.clone()),
            acquire: false,
        };
    }
    // Expiré : cette version-là, vue inchangée depuis la durée du bail, sur l'horloge locale.
    let expired = observed.is_some_and(|o| {
        o.version == lease.version && now_ms.saturating_sub(o.since_ms) >= record.duration_ms
    });
    if record.holder.is_none() || expired {
        return Decision::Write {
            record: mine(record.transitions.saturating_add(1)),
            expected: Some(lease.version.clone()),
            acquire: true,
        };
    }
    Decision::Wait {
        holder: record.holder.clone(),
    }
}

/// Vrai tant que l'active peut se tenir pour active : son dernier renouvellement réussi a
/// commencé à `last_renew_start_ms`.
pub fn still_leading(last_renew_start_ms: u64, now_ms: u64, timings: &Timings) -> bool {
    now_ms.saturating_sub(last_renew_start_ms) < timings.renew_deadline_ms
}

/// Bail rendu par son détenteur à l'arrêt : contenu à écrire et version attendue. `None` si
/// `me` ne le détient pas.
pub fn release(me: &str, lease: &Lease) -> Option<(Record, String)> {
    (lease.record.holder.as_deref() == Some(me)).then(|| {
        (
            Record {
                holder: None,
                duration_ms: RELEASED_MS,
                transitions: lease.record.transitions,
            },
            lease.version.clone(),
        )
    })
}

/// Durée écrite dans un bail rendu (sans détenteur, il est pris aussitôt de toute façon).
const RELEASED_MS: u64 = 1_000;
