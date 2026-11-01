//! The structural check of a pushed report (spec §7.3): every bound of §6, so that a pod that
//! can push cannot make the controller store more than a sidecar could have recorded.

use super::report::Report;

/// Why a pushed report is refused. `reason` is the fixed string logged and returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Violation {
    Malformed,
    UnknownField,
    Version,
    PushLimit,
    Samples,
    Hosts,
    HostLength,
    Status,
    Bucket,
    SchemaNodes,
    SchemaLevels,
    SchemaCut,
    NameLength,
    Params,
    VolatileFields,
    Sketch,
    PathLength,
    PathSegments,
    MediaTypes,
    BodySize,
    BodyMedia,
}

impl Violation {
    pub fn reason(self) -> &'static str {
        unimplemented!()
    }

    /// `507` instead of `400`.
    pub fn is_push_limit(self) -> bool {
        unimplemented!()
    }
}

impl Report {
    /// Checks the report against §6; `max_push_endpoints` is `MIMIC_STORE_MAX_PUSH_ENDPOINTS`.
    pub fn check(&self, _max_push_endpoints: usize) -> Result<(), Violation> {
        unimplemented!()
    }

    /// Parses a pushed body and checks it. Never panics, whatever the input.
    pub fn parse_checked(_json: &str, _max_push_endpoints: usize) -> Result<Report, Violation> {
        unimplemented!()
    }
}
