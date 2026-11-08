//! Load test runs in the store (spec §7.3 `:load:<uid>`, `:loads`, `loads:active`,
//! `loads:reserved`; §7.8 "Admission" and "End").

use std::collections::BTreeMap;

use mimic_core::load::summary::PodResults;
use serde::{Deserialize, Serialize};

use super::{ReadError, Store, StoreError};

/// Largest results post of one generator pod, in bytes (spec §7.8 "Load API").
pub const MAX_POST: usize = 256 << 10;
/// Largest stored summary of an ended run, in bytes.
pub const MAX_SUMMARY: usize = 192 << 10;

/// Limits the admission script enforces (spec §10.2).
#[derive(Debug, Clone, Copy)]
pub struct AdmitLimits {
    pub max_runs: u64,
    pub max_namespace_runs: u64,
    pub budget: u64,
}

/// A run to admit: everything the stored run holds while it is active.
pub struct Admit<'a> {
    pub namespace: &'a str,
    pub deployment: &'a str,
    pub uid: &'a str,
    /// The `MimicLoadTest` name and spec, no Secret value.
    pub spec: &'a str,
    pub plan: &'a str,
    pub ticket: &'a str,
    pub now_ms: u64,
    /// Start plus `activeDeadlineSeconds`.
    pub deadline_ms: u64,
    /// Plan bytes + pods x 256 KiB + 192 KiB.
    pub reserve: u64,
    pub limits: AdmitLimits,
}

/// Why a run is not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmitError {
    Busy,
    NamespaceBusy,
    ClusterBusy,
    StoreFull,
    Name,
    Unavailable,
}

/// How a run ended.
pub struct Ended<'a> {
    pub phase: &'a str,
    pub reason: &'a str,
    /// `status.results` as JSON, at most [`MAX_SUMMARY`] bytes.
    pub summary: &'a str,
    pub end_ms: u64,
}

/// What a generator pod posted, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredPost {
    #[serde(skip)]
    pub final_post: bool,
    pub reason: Option<String>,
    pub results: PodResults,
}

/// The plan and ticket of an active run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveRun {
    pub plan: String,
    pub ticket: String,
    pub stop: bool,
}

impl Store {
    pub async fn load_admit(&self, _admit: &Admit<'_>) -> Result<(), AdmitError> {
        todo!()
    }

    /// The plan, ticket and stop flag; `None` for an unknown or ended run.
    pub async fn load_run(
        &self,
        _namespace: &str,
        _deployment: &str,
        _uid: &str,
    ) -> Result<Option<ActiveRun>, ReadError> {
        todo!()
    }

    /// The stop flag of an active run; `None` for an unknown or ended one.
    pub async fn load_active(
        &self,
        _namespace: &str,
        _deployment: &str,
        _uid: &str,
    ) -> Result<Option<bool>, ReadError> {
        todo!()
    }

    /// Replaces the results of `pod`; the stop flag, or `None` for an unknown or ended run.
    pub async fn load_post(
        &self,
        _namespace: &str,
        _deployment: &str,
        _uid: &str,
        _pod: &str,
        _post: &StoredPost,
    ) -> Result<Option<bool>, StoreError> {
        todo!()
    }

    /// The posts of every pod of an active run.
    pub async fn load_posts(
        &self,
        _namespace: &str,
        _deployment: &str,
        _uid: &str,
    ) -> Result<BTreeMap<String, StoredPost>, ReadError> {
        todo!()
    }

    /// Sets the stop flag; the next generator poll stops.
    pub async fn load_stop(
        &self,
        _namespace: &str,
        _deployment: &str,
        _uid: &str,
    ) -> Result<(), StoreError> {
        todo!()
    }

    /// Ends the run (spec §7.8 "End"); false when it had ended already or is unknown.
    pub async fn load_end(
        &self,
        _namespace: &str,
        _deployment: &str,
        _uid: &str,
        _ended: &Ended<'_>,
    ) -> Result<bool, StoreError> {
        todo!()
    }

    /// The small fields and the summary of a run, whatever its state.
    pub async fn load_fields(
        &self,
        _namespace: &str,
        _deployment: &str,
        _uid: &str,
    ) -> Result<BTreeMap<String, String>, ReadError> {
        todo!()
    }
}
