//! OpenAPI 3.1 documents from a stored report (spec §7.6). Pure: a function of the checked
//! report, the namespace, the deployment and the `:meta` version.

use serde_json::Value;

use super::report::Report;

/// `openapi` of every document.
pub const OPENAPI_VERSION: &str = "3.1.1";

/// The port of an inbound document id (`inbound-<port>`, decimal, no leading zero, 1-65535).
pub fn parse_id(_id: &str) -> Option<u16> {
    todo!()
}

/// `{"documents": [{"id", "direction", "port", "endpoints"}]}`, sorted by id.
pub fn index(_report: &Report) -> Value {
    todo!()
}

/// `{"endpoints": [...]}` in the order of the report's endpoints.
pub fn endpoint_list(_report: &Report) -> Value {
    todo!()
}

/// The document `id`, or `None` when the report holds no such scope.
pub fn document(
    _report: &Report,
    _namespace: &str,
    _deployment: &str,
    _version: u64,
    _id: &str,
) -> Option<Value> {
    todo!()
}
