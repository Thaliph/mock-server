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
    pub fn validate(&self) -> Result<(), String> {
        todo!()
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
pub fn observe(_prev: Option<Observed>, _lease: Option<&Lease>, _now_ms: u64) -> Option<Observed> {
    todo!()
}

/// Décision de la réplique `me` sur le bail relu.
pub fn decide(
    _me: &str,
    _lease: Option<&Lease>,
    _observed: Option<&Observed>,
    _timings: &Timings,
    _now_ms: u64,
) -> Decision {
    todo!()
}

/// Vrai tant que l'active peut se tenir pour active : son dernier renouvellement réussi a
/// commencé à `last_renew_start_ms`.
pub fn still_leading(_last_renew_start_ms: u64, _now_ms: u64, _timings: &Timings) -> bool {
    todo!()
}

/// Bail rendu par son détenteur à l'arrêt : contenu à écrire et version attendue. `None` si
/// `me` ne le détient pas.
pub fn release(_me: &str, _lease: &Lease) -> Option<(Record, String)> {
    todo!()
}
