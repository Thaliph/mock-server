//! UI pages and static assets (spec §7.5), embedded in the binary: nothing is read from disk or
//! fetched at run time. The pages are the same bytes for every deployment; `mimic.js` fills them.

use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use mimic_core::discovery::openapi;

use crate::store::{ReadError, keys};

/// The pinned `swagger-ui-dist` version (`ui/package.json`; a test checks they agree).
const SWAGGER_VERSION: &str = "5.34.0";

const HTML: &str = "text/html; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const TEXT: &str = "text/plain; charset=utf-8";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

const PAGE: &[u8] = include_bytes!("../../ui/pages/page.html");
const OPENAPI_PAGE: &[u8] = include_bytes!("../../ui/pages/openapi.html");

/// A file of `swagger-ui/<version>/`, served immutable.
const VENDORED: [(&str, &str, &[u8]); 5] = [
    (
        "swagger-ui-bundle.js",
        JS,
        include_bytes!("../../ui/vendor/swagger-ui/swagger-ui-bundle.js"),
    ),
    (
        "swagger-ui.css",
        CSS,
        include_bytes!("../../ui/vendor/swagger-ui/swagger-ui.css"),
    ),
    (
        "LICENSE",
        TEXT,
        include_bytes!("../../ui/vendor/swagger-ui/LICENSE"),
    ),
    (
        "NOTICE",
        TEXT,
        include_bytes!("../../ui/vendor/swagger-ui/NOTICE"),
    ),
    (
        "swagger-ui-bundle.js.LICENSE.txt",
        TEXT,
        include_bytes!("../../ui/vendor/swagger-ui/swagger-ui-bundle.js.LICENSE.txt"),
    ),
];

fn served(content_type: &'static str, body: &'static [u8]) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

/// `/ui/` and the deployment page: static, so `Valkey` is not involved.
pub(super) async fn list_page() -> Response {
    served(HTML, PAGE)
}

pub(super) async fn workload_page(
    Path((namespace, deployment)): Path<(String, String)>,
) -> Response {
    if keys(&namespace, &deployment).is_err() {
        return ReadError::Name.into_response();
    }
    served(HTML, PAGE)
}

pub(super) async fn openapi_page(
    Path((namespace, deployment, id)): Path<(String, String, String)>,
) -> Response {
    if keys(&namespace, &deployment).is_err() || openapi::parse_document_id(&id).is_none() {
        return ReadError::Name.into_response();
    }
    served(HTML, OPENAPI_PAGE)
}

/// `/ui/assets/<path>`: `mimic.js`, `mimic.css` and `swagger-ui/<version>/<file>`; any other
/// path (a `..` included, it matches no name) is `404`.
pub(super) async fn asset(Path(path): Path<String>) -> Response {
    match path.as_str() {
        "mimic.js" => return served(JS, include_bytes!("../../ui/mimic.js")),
        "mimic.css" => return served(CSS, include_bytes!("../../ui/mimic.css")),
        _ => {}
    }
    let vendored = path
        .strip_prefix("swagger-ui/")
        .and_then(|rest| rest.strip_prefix(SWAGGER_VERSION))
        .and_then(|rest| rest.strip_prefix('/'))
        .and_then(|file| VENDORED.iter().find(|(name, ..)| *name == file));
    match vendored {
        Some((_, content_type, body)) => {
            let mut response = served(content_type, body);
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static(IMMUTABLE));
            response
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
