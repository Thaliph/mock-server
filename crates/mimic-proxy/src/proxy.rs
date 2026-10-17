//! Traitement du trafic entrant et sortant : relais, capture et mock (spec §5).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use http::{Method, Request, Response, StatusCode, Uri};
use http_body_util::{BodyExt, Empty, Full, Limited};
use hyper::body::Incoming;
use mimic_core::mock::{MockDecision, MockRequest, MockResponse};
use mimic_core::model::{Body, Direction, Exchange, Header};
use tokio::sync::oneshot;

use crate::capture::{Captured, TeeBody, encode};
use crate::config::{Mode, Unmatched};
use crate::metrics::{
    AuthorityLabel, DurationLabels, MockLabels, ReasonLabel, RequestLabels, UpstreamErrorLabels,
};
use crate::state::{AppState, BoxError, ProxyBody};
use crate::store::{Added, Origin};

/// Headers propres à une connexion, jamais relayés.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "transfer-encoding",
    "upgrade",
];

/// Informations sur la connexion acceptée.
#[derive(Debug, Clone)]
pub struct ConnInfo {
    pub listen: SocketAddr,
    pub peer: SocketAddr,
    /// Destination d'origine avant redirection iptables, si différente de l'écoute.
    pub original_dst: Option<SocketAddr>,
    /// `hôte:port` d'une connexion TLS interceptée et déchiffrée.
    pub tls_authority: Option<String>,
}

pub fn boxed<B>(body: B) -> ProxyBody
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<BoxError>,
{
    body.map_err(Into::into).boxed_unsync()
}

fn full(data: impl Into<Bytes>) -> ProxyBody {
    boxed(Full::new(data.into()))
}

fn text(status: StatusCode, msg: &str) -> Response<ProxyBody> {
    let mut r = Response::new(full(msg.to_string()));
    *r.status_mut() = status;
    r.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    r
}

/// Trafic reçu par l'application.
pub async fn inbound(
    state: Arc<AppState>,
    info: ConnInfo,
    req: Request<Incoming>,
) -> Response<ProxyBody> {
    let target = state.config.inbound.upstream.or_else(|| {
        info.original_dst
            .map(|d| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), d.port()))
    });
    let Some(target) = target else {
        return text(
            StatusCode::BAD_GATEWAY,
            "mimic: aucune destination pour le trafic entrant",
        );
    };
    let authority = host_header(req.headers())
        .map(|h| normalize_authority(&h, "http"))
        .unwrap_or_else(|| target.to_string());
    let record = state.mode() != Mode::Passthrough;
    let (parts, body) = req.into_parts();
    forward(
        state,
        Direction::Inbound,
        parts,
        boxed(body),
        record,
        Target {
            scheme: "http".into(),
            authority,
            connect: target.to_string(),
        },
    )
    .await
}

/// Trafic émis par l'application : proxy explicite (`HTTP_PROXY`) ou redirection iptables.
pub async fn outbound(
    state: Arc<AppState>,
    info: ConnInfo,
    req: Request<Incoming>,
) -> Response<ProxyBody> {
    if req.method() == Method::CONNECT {
        return connect(state, info, req);
    }
    let target = if let Some(a) = &info.tls_authority {
        Target {
            scheme: "https".into(),
            authority: a.clone(),
            connect: a.clone(),
        }
    } else if let Some(a) = req.uri().authority() {
        let scheme = req.uri().scheme_str().unwrap_or("http").to_string();
        let authority = normalize_authority(a.as_str(), &scheme);
        Target {
            scheme,
            connect: authority.clone(),
            authority,
        }
    } else if let Some(dst) = info.original_dst {
        let authority = host_header(req.headers())
            .map(|h| with_port(&h, dst.port()))
            .unwrap_or_else(|| dst.to_string());
        Target {
            scheme: "http".into(),
            authority,
            connect: dst.to_string(),
        }
    } else {
        return text(
            StatusCode::BAD_REQUEST,
            "mimic: requête sans destination (ni proxy explicite, ni redirection)",
        );
    };
    let (parts, body) = req.into_parts();
    match state.mode() {
        Mode::Passthrough => {
            forward(
                state,
                Direction::Outbound,
                parts,
                boxed(body),
                false,
                target,
            )
            .await
        }
        Mode::Record => forward(state, Direction::Outbound, parts, boxed(body), true, target).await,
        Mode::Mock | Mode::MockPartial => serve_mock(state, parts, body, target).await,
    }
}

/// Où envoyer la requête et sous quel nom l'enregistrer.
#[derive(Debug, Clone)]
struct Target {
    scheme: String,
    /// `hôte:port` enregistré et utilisé pour le matching.
    authority: String,
    /// Adresse de connexion effective.
    connect: String,
}

async fn forward(
    state: Arc<AppState>,
    direction: Direction,
    mut parts: http::request::Parts,
    body: ProxyBody,
    record: bool,
    target: Target,
) -> Response<ProxyBody> {
    let started = Instant::now();
    let started_ms = now_ms();
    let mode = state.mode();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let scheme = if target.scheme == "https" {
        "https"
    } else {
        "http"
    };
    let uri: Uri = match format!("{scheme}://{}{}", target.connect, path_and_query).parse() {
        Ok(u) => u,
        Err(_) => return text(StatusCode::BAD_REQUEST, "mimic: URI invalide"),
    };
    let original_uri = std::mem::replace(&mut parts.uri, uri);
    strip_hop_by_hop(&mut parts.headers);
    if !parts.headers.contains_key(http::header::HOST)
        && let Ok(v) = HeaderValue::from_str(&target.authority)
    {
        parts.headers.insert(http::header::HOST, v);
    }
    // HTTP/1.1 vers l'amont : le client choisit le protocole selon la connexion.
    parts.version = http::Version::HTTP_11;
    let req_headers = to_headers(&parts.headers);
    let method = parts.method.to_string();

    let limit = state.config.capture.max_body_bytes;
    let (body, req_capture) = if record {
        let (tee, rx) = TeeBody::new(body, limit);
        (boxed(tee), Some(rx))
    } else {
        (body, None)
    };

    let authority_label = state.metrics.authority(&target.authority);
    let response = match state.client.request(Request::from_parts(parts, body)).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(authority = %target.authority, error = %e, "échec vers l'amont");
            state
                .metrics
                .upstream_errors
                .get_or_create(&UpstreamErrorLabels {
                    direction: dir_str(direction).into(),
                    authority: authority_label.clone(),
                    kind: if e.is_connect() { "connect" } else { "request" }.into(),
                })
                .inc();
            count_request(&state, direction, mode, &authority_label, &method, 502);
            return text(StatusCode::BAD_GATEWAY, "mimic: amont injoignable");
        }
    };
    let ttfb = started.elapsed();
    let (mut resp_parts, resp_body) = response.into_parts();
    strip_hop_by_hop(&mut resp_parts.headers);
    let status = resp_parts.status.as_u16();
    count_request(&state, direction, mode, &authority_label, &method, status);

    let Some(req_capture) = req_capture else {
        observe_duration(&state, direction, mode, &authority_label, ttfb);
        return Response::from_parts(resp_parts, boxed(resp_body));
    };
    let (tee, resp_capture) = TeeBody::new(resp_body, limit);
    let pending = Pending {
        direction,
        mode,
        started,
        started_ms,
        ttfb,
        scheme: target.scheme,
        authority: target.authority,
        authority_label,
        method,
        uri: original_uri,
        req_headers,
        status,
        resp_headers: to_headers(&resp_parts.headers),
    };
    tokio::spawn(finalize(state, pending, req_capture, resp_capture));
    Response::from_parts(resp_parts, boxed(tee))
}

/// Ce qui est connu d'un échange avant la fin des bodies.
struct Pending {
    direction: Direction,
    mode: Mode,
    started: Instant,
    started_ms: u64,
    ttfb: Duration,
    scheme: String,
    authority: String,
    authority_label: String,
    method: String,
    uri: Uri,
    req_headers: Vec<Header>,
    status: u16,
    resp_headers: Vec<Header>,
}

async fn finalize(
    state: Arc<AppState>,
    p: Pending,
    req: oneshot::Receiver<Captured>,
    resp: oneshot::Receiver<Captured>,
) {
    let (Ok(req), Ok(resp)) = (req.await, resp.await) else {
        return;
    };
    let total = resp.ended.saturating_duration_since(p.started);
    observe_duration(&state, p.direction, p.mode, &p.authority_label, total);
    let capture = &state.config.capture;
    let req_body = req.into_body(
        capture.bodies,
        capture.max_body_bytes,
        find(&p.req_headers, "content-encoding"),
    );
    let resp_body = resp.into_body(
        capture.bodies,
        capture.max_body_bytes,
        find(&p.resp_headers, "content-encoding"),
    );
    for b in [&req_body, &resp_body] {
        if matches!(b, Body::Truncated { .. }) {
            dropped(&state, "truncated");
        }
    }
    let mut exchange = Exchange {
        id: format!("{}-{}", state.config.instance, ulid::Ulid::generate()),
        direction: p.direction,
        started_at_ms: p.started_ms,
        scheme: p.scheme,
        authority: p.authority,
        method: p.method,
        path: p.uri.path().to_string(),
        query: parse_query(p.uri.query()),
        trace_id: find(&p.req_headers, "traceparent")
            .and_then(|t| t.split('-').nth(1))
            .map(String::from),
        req_headers: p.req_headers,
        req_body,
        status: p.status,
        resp_headers: p.resp_headers,
        resp_body,
        ttfb_us: p.ttfb.as_micros() as u64,
        total_us: total.as_micros() as u64,
        redactions: vec![],
    };
    state.redactor.redact(&mut exchange);
    if state.store.add(exchange, Origin::Local) == Added::StoredWithoutBodies {
        dropped(&state, "sample_limit");
    }
    state.refresh_store_metrics();
}

async fn serve_mock(
    state: Arc<AppState>,
    parts: http::request::Parts,
    body: Incoming,
    target: Target,
) -> Response<ProxyBody> {
    let started = Instant::now();
    let mode = state.mode();
    let limit = state.config.capture.max_body_bytes;
    let body = match Limited::new(body, limit).collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => {
            return text(
                StatusCode::PAYLOAD_TOO_LARGE,
                "mimic: body trop gros pour le mode mock",
            );
        }
    };
    let request = MockRequest {
        authority: target.authority.clone(),
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        query: parse_query(parts.uri.query()),
        headers: to_headers(&parts.headers),
        body: body.to_vec(),
    };
    let decision = state.with_engine(|engine| engine.respond(&request));
    let authority_label = state.metrics.authority(&target.authority);
    let method = request.method.clone();
    match decision {
        MockDecision::Matched(r) => {
            mock_metric(&state, &authority_label, r.kind.as_str());
            let latency = Duration::from_micros(r.latency_us);
            state
                .metrics
                .mock_latency
                .get_or_create(&AuthorityLabel {
                    authority: authority_label.clone(),
                })
                .observe(latency.as_secs_f64());
            tokio::time::sleep(latency).await;
            let response = mock_response(&state, r);
            count_request(
                &state,
                Direction::Outbound,
                mode,
                &authority_label,
                &method,
                response.status().as_u16(),
            );
            observe_duration(
                &state,
                Direction::Outbound,
                mode,
                &authority_label,
                started.elapsed(),
            );
            response
        }
        MockDecision::Unmatched => {
            mock_metric(&state, &authority_label, "unmatched");
            let passthrough =
                mode == Mode::MockPartial || state.config.mock.unmatched == Unmatched::Passthrough;
            if passthrough {
                return forward(state, Direction::Outbound, parts, full(body), true, target).await;
            }
            count_request(
                &state,
                Direction::Outbound,
                mode,
                &authority_label,
                &method,
                599,
            );
            let mut r = text(
                StatusCode::from_u16(599).expect("599 est un code valide"),
                "mimic: aucun enregistrement ne correspond à cette requête",
            );
            r.headers_mut()
                .insert("x-mimic-unmatched", HeaderValue::from_static("true"));
            r
        }
    }
}

fn mock_response(state: &AppState, r: MockResponse) -> Response<ProxyBody> {
    let mut headers = HeaderMap::new();
    for h in &r.headers {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(h.name.as_bytes()),
            HeaderValue::from_str(&h.value),
        ) {
            headers.append(n, v);
        }
    }
    let data = match &r.body {
        Body::Captured { data } => data.clone(),
        _ => vec![],
    };
    let encoding = headers
        .get(http::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let data = match encoding {
        Some(enc) if !data.is_empty() => match encode(&enc, &data) {
            Some(encoded) => encoded,
            None => {
                headers.remove(http::header::CONTENT_ENCODING);
                data
            }
        },
        _ => data,
    };
    if state.config.mock.debug_header
        && let Ok(v) = HeaderValue::from_str(&format!("{};score={:.2}", r.kind.as_str(), r.score))
    {
        headers.insert("x-mimic-match", v);
    }
    let mut response = Response::new(full(data));
    *response.status_mut() = StatusCode::from_u16(r.status).unwrap_or(StatusCode::OK);
    *response.headers_mut() = headers;
    response
}

/// Tunnel `CONNECT` : intercepté si l'hôte est dans `tls.intercept`, sinon relayé à
/// l'aveugle, ou refusé en mock.
fn connect(state: Arc<AppState>, info: ConnInfo, req: Request<Incoming>) -> Response<ProxyBody> {
    let Some(authority) = req
        .uri()
        .authority()
        .map(|a| normalize_authority(a.as_str(), "https"))
    else {
        return text(StatusCode::BAD_REQUEST, "mimic: CONNECT sans destination");
    };
    let host = req.uri().host().unwrap_or_default().to_string();
    let mode = state.mode();
    let label = state.metrics.authority(&authority);
    if state.intercepts(&host) {
        count_request(&state, Direction::Outbound, mode, &label, "CONNECT", 200);
        tokio::spawn(async move {
            match hyper::upgrade::on(req).await {
                Ok(upgraded) => {
                    let io = hyper_util::rt::TokioIo::new(upgraded);
                    crate::tls_serve(state, io, info, authority).await;
                }
                Err(e) => tracing::debug!(error = %e, "upgrade CONNECT échoué"),
            }
        });
        return Response::new(boxed(Empty::<Bytes>::new()));
    }
    if mode == Mode::Mock {
        mock_metric(&state, &label, "unmatched");
        count_request(&state, Direction::Outbound, mode, &label, "CONNECT", 403);
        return text(
            StatusCode::FORBIDDEN,
            "mimic: tunnel refusé en mode mock (TLS non intercepté)",
        );
    }
    count_request(&state, Direction::Outbound, mode, &label, "CONNECT", 200);
    tokio::spawn(async move {
        let upgraded = match hyper::upgrade::on(req).await {
            Ok(u) => u,
            Err(e) => {
                tracing::debug!(error = %e, "upgrade CONNECT échoué");
                return;
            }
        };
        match tokio::net::TcpStream::connect(&authority).await {
            Ok(mut upstream) => {
                let mut client = hyper_util::rt::TokioIo::new(upgraded);
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            }
            Err(e) => {
                tracing::warn!(%authority, error = %e, "tunnel vers l'amont impossible");
                state
                    .metrics
                    .upstream_errors
                    .get_or_create(&UpstreamErrorLabels {
                        direction: "outbound".into(),
                        authority: label,
                        kind: "connect".into(),
                    })
                    .inc();
            }
        }
    });
    Response::new(boxed(Empty::<Bytes>::new()))
}

/// Relais TCP opaque d'une connexion redirigée non HTTP (spec §3.1).
pub async fn tunnel(
    state: Arc<AppState>,
    mut stream: tokio::net::TcpStream,
    original_dst: SocketAddr,
    protocol: crate::net::Protocol,
    inbound: bool,
) {
    let direction = if inbound { "inbound" } else { "outbound" };
    let count = |result: &str| {
        state
            .metrics
            .passthrough
            .get_or_create(&crate::metrics::PassthroughLabels {
                direction: direction.into(),
                kind: protocol.as_str().into(),
                result: result.into(),
            })
            .inc();
    };
    // En mock, aucun appel chiffré ne doit atteindre une dépendance (O3).
    if !inbound && protocol == crate::net::Protocol::Tls && state.mode() == Mode::Mock {
        count("refused");
        return;
    }
    let target = if inbound {
        state.config.inbound.upstream.unwrap_or_else(|| {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), original_dst.port())
        })
    } else {
        original_dst
    };
    match tokio::net::TcpStream::connect(target).await {
        Ok(mut upstream) => {
            count("relayed");
            let _ = upstream.set_nodelay(true);
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
        }
        Err(e) => {
            tracing::warn!(%target, error = %e, "tunnel impossible");
            count("error");
        }
    }
}

fn count_request(
    state: &AppState,
    direction: Direction,
    mode: Mode,
    authority: &str,
    method: &str,
    status: u16,
) {
    state
        .metrics
        .requests
        .get_or_create(&RequestLabels {
            direction: dir_str(direction).into(),
            mode: mode.as_str().into(),
            authority: authority.into(),
            method: method.into(),
            status: status.to_string(),
        })
        .inc();
}

fn observe_duration(
    state: &AppState,
    direction: Direction,
    mode: Mode,
    authority: &str,
    d: Duration,
) {
    state
        .metrics
        .request_duration
        .get_or_create(&DurationLabels {
            direction: dir_str(direction).into(),
            mode: mode.as_str().into(),
            authority: authority.into(),
        })
        .observe(d.as_secs_f64());
}

fn mock_metric(state: &AppState, authority: &str, kind: &str) {
    state
        .metrics
        .mock_responses
        .get_or_create(&MockLabels {
            authority: authority.into(),
            r#match: kind.into(),
        })
        .inc();
}

fn dropped(state: &AppState, reason: &str) {
    state
        .metrics
        .capture_dropped
        .get_or_create(&ReasonLabel {
            reason: reason.into(),
        })
        .inc();
}

pub fn dir_str(d: Direction) -> &'static str {
    match d {
        Direction::Inbound => "inbound",
        Direction::Outbound => "outbound",
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for name in HOP_BY_HOP {
        headers.remove(*name);
    }
}

fn to_headers(headers: &HeaderMap) -> Vec<Header> {
    headers
        .iter()
        .map(|(n, v)| Header::new(n.as_str(), String::from_utf8_lossy(v.as_bytes())))
        .collect()
}

fn find<'a>(headers: &'a [Header], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name == name)
        .map(|h| h.value.as_str())
}

fn host_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get(http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(String::from)
}

/// `hôte[:port]` → `hôte:port`, avec le port par défaut du schéma.
pub fn normalize_authority(authority: &str, scheme: &str) -> String {
    let default = if scheme == "https" { 443 } else { 80 };
    with_port(authority, default)
}

fn with_port(authority: &str, port: u16) -> String {
    let has_port = match authority.rsplit_once(':') {
        // IPv6 entre crochets : le port suit le crochet fermant.
        Some((host, p)) => {
            !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_digit())
                && (!host.contains(':') || host.ends_with(']'))
        }
        None => false,
    };
    if has_port {
        authority.to_ascii_lowercase()
    } else {
        format!("{}:{port}", authority.to_ascii_lowercase())
    }
}

/// Query décodée (`%XX` et `+`), dans l'ordre d'origine.
pub fn parse_query(query: Option<&str>) -> Vec<(String, String)> {
    query
        .unwrap_or_default()
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = |c: u8| (c as char).to_digit(16);
                match (
                    b.get(i + 1).copied().and_then(hex),
                    b.get(i + 2).copied().and_then(hex),
                ) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorities() {
        assert_eq!(
            normalize_authority("Api.Stripe.com", "https"),
            "api.stripe.com:443"
        );
        assert_eq!(
            normalize_authority("10.0.0.1:8080", "http"),
            "10.0.0.1:8080"
        );
        assert_eq!(normalize_authority("[::1]:81", "http"), "[::1]:81");
        assert_eq!(normalize_authority("[::1]", "http"), "[::1]:80");
    }

    #[test]
    fn query_decoding() {
        assert_eq!(
            parse_query(Some("q=a%20b&x=1+2&flag&bad=%zz")),
            vec![
                ("q".into(), "a b".into()),
                ("x".into(), "1 2".into()),
                ("flag".into(), String::new()),
                ("bad".into(), "%zz".into()),
            ]
        );
    }
}
