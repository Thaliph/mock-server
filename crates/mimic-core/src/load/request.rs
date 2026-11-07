//! Request building (spec §7.8 "Requests"): one request from a plan, a seed and the time.

use std::collections::BTreeMap;
use std::net::IpAddr;

use super::plan::Plan;

/// What the caller supplies besides the plan.
#[derive(Debug, Clone)]
pub struct Context<'a> {
    /// The value of the `mimic-load-run` header.
    pub marker: &'a str,
    /// The address of the target pod: the `host` of an endpoint without host.
    pub ip: IpAddr,
    /// Unix time, in milliseconds, for the `date-time` and `date` volatile fields.
    pub now_ms: u64,
    /// The values of the credentials Secret, by key.
    pub secrets: &'a BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Index in `plan.endpoints`.
    pub endpoint: usize,
    /// Index in that endpoint's `samples`.
    pub sample: usize,
    pub method: String,
    /// Path and query.
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Why no request is built. Never carries a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The plan has no endpoint.
    Empty,
    /// A credential names a key the Secret does not hold.
    MissingSecret(String),
    /// A part of the request is not valid HTTP/1.1 (`what` names it).
    Invalid(&'static str),
    /// The request would carry the redaction marker.
    Redacted,
}

/// The request for `seed`: an endpoint drawn by weight, then one of its samples.
pub fn request(_plan: &Plan, _ctx: &Context<'_>, _seed: u64) -> Result<Request, Error> {
    todo!()
}
