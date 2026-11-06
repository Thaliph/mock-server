//! UI-09 (pages and assets) and the page and asset parts of UI-08. No Valkey: the pages are
//! static, so the store points at a closed port.

use sha2::{Digest, Sha256};

use super::*;

const VENDOR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/ui/vendor/swagger-ui");
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const SWAGGER_ASSETS: [(&str, &str); 5] = [
    ("swagger-ui-bundle.js", "text/javascript; charset=utf-8"),
    ("swagger-ui.css", "text/css; charset=utf-8"),
    ("LICENSE", "text/plain; charset=utf-8"),
    ("NOTICE", "text/plain; charset=utf-8"),
    (
        "swagger-ui-bundle.js.LICENSE.txt",
        "text/plain; charset=utf-8",
    ),
];
const PAGES: [&str; 3] = [
    "/ui/",
    "/ui/workloads/shop/front",
    "/ui/workloads/shop/front/openapi/inbound-8080",
];

fn offline() -> Router {
    app(&store("redis://127.0.0.1:1")).0
}

fn text(answer: &Full) -> String {
    String::from_utf8(answer.body.clone()).expect("UTF-8")
}

fn swagger_version() -> String {
    let package: Value = serde_json::from_str(
        &std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/package.json")).unwrap(),
    )
    .unwrap();
    package["dependencies"]["swagger-ui-dist"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn ui_09_pages_are_html_without_inline_code_or_foreign_urls() {
    let app = offline();
    for path in PAGES {
        let page = fetch(&app, path).await;
        assert_eq!(page.status, 200, "{path}");
        assert_eq!(
            page.headers["content-type"], "text/html; charset=utf-8",
            "{path}"
        );
        assert_eq!(page.headers["cache-control"], "no-store", "{path}");
        let html = text(&page);
        assert!(
            !html.contains("http://") && !html.contains("https://"),
            "{path}"
        );
        assert!(!html.contains("style="), "{path}");
        assert!(!html.contains("<style"), "{path}");
        let mut scripts = 0;
        let mut rest = html.as_str();
        while let Some(at) = rest.find("<script") {
            let end = rest[at..].find('>').map(|e| at + e).expect("a closed tag");
            let tag = &rest[at..end];
            assert!(tag.contains("src=\"/ui/assets/"), "{path}: {tag}");
            let after = &rest[end + 1..];
            assert!(after.starts_with("</script>"), "{path}: script with a body");
            scripts += 1;
            rest = after;
        }
        assert!(scripts >= 1, "{path}: no script");
        for tag in html.split('<').skip(1) {
            let tag = tag.split('>').next().unwrap();
            for attr in tag.split_whitespace().skip(1) {
                assert!(
                    !attr.to_ascii_lowercase().starts_with("on"),
                    "{path}: {attr}"
                );
            }
            if tag.starts_with("link") {
                assert!(tag.contains("href=\"/ui/assets/"), "{path}: {tag}");
            }
        }
    }
    let page = text(&fetch(&app, PAGES[2]).await);
    assert!(page.contains("swagger-ui-bundle.js"));
    assert!(page.contains(&format!("/ui/assets/swagger-ui/{}/", swagger_version())));
}

#[tokio::test]
async fn ui_09_pages_are_the_same_bytes_for_every_deployment() {
    let app = offline();
    let a = fetch(&app, "/ui/workloads/shop/front").await.body;
    let b = fetch(&app, "/ui/workloads/other/back").await.body;
    assert_eq!(a, b);
}

#[tokio::test]
async fn ui_09_invalid_names_and_ids_are_400_and_head_is_served() {
    let app = offline();
    for path in [
        "/ui/workloads/Bad_Ns/front",
        "/ui/workloads/shop/Bad_Dep",
        "/ui/workloads/Bad_Ns/front/openapi/inbound-80",
        "/ui/workloads/shop/front/openapi/inbound-0",
        "/ui/workloads/shop/front/openapi/inbound-080",
        "/ui/workloads/shop/front/openapi/x",
    ] {
        assert_eq!(fetch(&app, path).await.status, 400, "{path}");
    }
    let local = Some("localhost:8080");
    let head = send(&app, Method::HEAD, "/ui/", "127.0.0.1", local).await;
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    assert_eq!(fetch(&app, "/ui").await.status, 404);
    let post = send(&app, Method::POST, "/ui/", "127.0.0.1", local).await;
    assert_eq!(post.status, 405);
}

#[tokio::test]
async fn ui_09_swagger_assets_are_the_vendored_bytes_with_fixed_types() {
    let app = offline();
    let version = swagger_version();
    let sums = std::fs::read_to_string(format!("{VENDOR}/SHA256SUMS")).unwrap();
    for (file, content_type) in SWAGGER_ASSETS {
        let answer = fetch(&app, &format!("/ui/assets/swagger-ui/{version}/{file}")).await;
        assert_eq!(answer.status, 200, "{file}");
        assert_eq!(answer.headers["content-type"], content_type, "{file}");
        assert_eq!(answer.headers["cache-control"], IMMUTABLE, "{file}");
        assert_eq!(
            answer.body,
            std::fs::read(format!("{VENDOR}/{file}")).unwrap(),
            "{file}"
        );
        let digest: String = Sha256::digest(&answer.body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(
            sums.lines().any(|l| l == format!("{digest}  {file}")),
            "{file}: not in SHA256SUMS"
        );
    }
}

#[tokio::test]
async fn ui_09_own_assets_and_unknown_paths() {
    let app = offline();
    let js = fetch(&app, "/ui/assets/mimic.js").await;
    assert_eq!(js.status, 200);
    assert_eq!(js.headers["content-type"], "text/javascript; charset=utf-8");
    assert_eq!(js.headers["cache-control"], "no-store");
    let css = fetch(&app, "/ui/assets/mimic.css").await;
    assert_eq!(css.status, 200);
    assert_eq!(css.headers["content-type"], "text/css; charset=utf-8");
    let version = swagger_version();
    for path in [
        "/ui/assets/".to_string(),
        "/ui/assets/nothing.js".to_string(),
        "/ui/assets/swagger-ui/0.0.1/swagger-ui.css".to_string(),
        format!("/ui/assets/swagger-ui/{version}/swagger-ui.css.map"),
        format!("/ui/assets/swagger-ui/{version}/"),
        format!("/ui/assets/swagger-ui/{version}/../../mimic.js"),
        "/ui/assets/%2e%2e/mimic.js".to_string(),
        "/ui/assets/mimic.js/".to_string(),
    ] {
        assert_eq!(fetch(&app, &path).await.status, 404, "{path}");
    }
}

#[tokio::test]
async fn ui_09_mimic_js_is_safe_and_sets_the_swagger_options() {
    let app = offline();
    let js = text(&fetch(&app, "/ui/assets/mimic.js").await);
    let css = text(&fetch(&app, "/ui/assets/mimic.css").await);
    for forbidden in [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
        "eval",
        "Function(",
        "http://",
        "https://",
    ] {
        assert!(!js.contains(forbidden), "mimic.js contains {forbidden}");
    }
    assert!(!css.contains("http://") && !css.contains("https://"));
    for required in [
        "supportedSubmitMethods: []",
        "tryItOutEnabled: false",
        "validatorUrl: null",
        "queryConfigEnabled: false",
        "withCredentials: false",
        "showExtensions: true",
        "deepLinking: true",
        "requestInterceptor",
        "BaseLayout",
        "/v1/workloads/",
    ] {
        assert!(js.contains(required), "mimic.js lacks {required}");
    }
    // Swagger UI keeps the original request when an interceptor returns a falsy value: the
    // interceptor must throw to refuse, and never return null or undefined.
    let start = js
        .find("function requestInterceptor")
        .expect("the interceptor");
    let body = &js[start..js[start..].find("\n  }\n").map(|e| start + e).unwrap()];
    assert!(body.matches("throw new Error").count() >= 2, "{body}");
    assert!(body.contains("catch (error) {\n      throw"), "{body}");
    assert!(
        !body.contains("return null") && !body.contains("return;"),
        "{body}"
    );
    assert!(!body.contains("undefined"), "{body}");
    assert!(js.contains("requestInterceptor: requestInterceptor"));
    assert!(js.contains("window.mimicRequestInterceptor = requestInterceptor"));
}

#[tokio::test]
async fn ui_08_pages_and_assets_carry_the_security_headers() {
    let store = store("redis://127.0.0.1:1");
    let version = swagger_version();
    for path in [
        "/ui/".to_string(),
        "/ui/workloads/Bad_Ns/front".to_string(),
        "/ui/assets/mimic.js".to_string(),
        "/ui/assets/nothing".to_string(),
        format!("/ui/assets/swagger-ui/{version}/swagger-ui.css"),
    ] {
        let response = raw(&store, Method::GET, &path, "127.0.0.1", Some("localhost")).await;
        let h = response.headers();
        let immutable = path.contains("swagger-ui/") && response.status() == 200;
        for (name, value) in [
            ("content-security-policy", CSP),
            ("x-content-type-options", "nosniff"),
            ("referrer-policy", "no-referrer"),
            ("x-frame-options", "DENY"),
            ("cross-origin-opener-policy", "same-origin"),
            ("cross-origin-resource-policy", "same-origin"),
            (
                "cache-control",
                if immutable { IMMUTABLE } else { "no-store" },
            ),
        ] {
            assert_eq!(
                h.get(name).and_then(|v| v.to_str().ok()),
                Some(value),
                "{path}: {name}"
            );
        }
        assert!(!h.keys().any(|k| k.as_str().starts_with("access-control-")));
    }
}
