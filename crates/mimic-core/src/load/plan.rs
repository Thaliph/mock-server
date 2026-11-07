//! The load plan (spec §7.8 "Plan"): which inbound endpoints are loaded, with which samples.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::discovery::report::Report;

/// A credential the test supplies: the request field it replaces and the Secret key that holds
/// the value. The plan holds these two names, never a value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialField {
    pub field: String,
    pub key: String,
}

/// What of the `MimicLoadTest` spec and of the controller's limits the plan depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// `endpoints.include`: `<METHOD> <template>` entries; `None` keeps every endpoint.
    pub include: Option<BTreeSet<String>>,
    /// `endpoints.exclude`, which wins over `include`.
    pub exclude: BTreeSet<String>,
    pub credentials: Vec<CredentialField>,
    /// `MIMIC_LOAD_MAX_ENDPOINTS`.
    pub max_endpoints: usize,
    /// `MIMIC_LOAD_MAX_PLAN`, in bytes of the serialised plan.
    pub max_plan: usize,
}

/// A request body as the sample holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "lowercase")]
pub enum Body {
    /// The canonical text of a JSON value.
    Json { media: String, text: String },
    /// The pairs of a urlencoded form, in order.
    Form {
        media: String,
        pairs: Vec<(String, String)>,
    },
}

/// One redacted request of an endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sample {
    pub hash: u64,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Option<Body>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub method: String,
    pub port: u16,
    pub template: String,
    /// The first host of the endpoint in byte order.
    pub host: Option<String>,
    /// Exchanges recorded: the weight of the endpoint in a draw.
    pub weight: u64,
    /// The recorder's volatile request fields (`query/…`, `request.header/…`, `request.body/…`).
    pub volatile: Vec<String>,
    pub samples: Vec<Sample>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub credentials: Vec<CredentialField>,
    pub endpoints: Vec<Endpoint>,
}

/// A plan and why the other endpoints are not in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Planned {
    pub plan: Plan,
    /// Endpoints left out, by reason (`status.skipped`).
    pub skipped: BTreeMap<&'static str, u64>,
}

/// The plan of a stored inbound report.
pub fn plan(_report: &Report, _spec: &Spec) -> Planned {
    todo!()
}

impl Plan {
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Reads a plan of at most `max_plan` bytes.
    pub fn from_json(_json: &str, _max_plan: usize) -> Result<Self, String> {
        todo!()
    }
}
