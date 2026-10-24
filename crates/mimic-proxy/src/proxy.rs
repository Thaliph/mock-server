//! Traitement du trafic entrant et sortant : relais, capture et mock (spec §5).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::header::{HeaderMap, HeaderValue};
use http::{Method, Request, Response, StatusCode, Uri};
use http_body_util::{BodyExt, Empty, Full, Limited};
use hyper::body::Incoming;
use hyper::upgrade::OnUpgrade;
use mimic_core::mock::{MockDecision, MockRequest, MockResponse};
use mimic_core::model::{Body, Direction, Exchange, Header};
use mimic_core::unmatched::{self, Reason};
use tokio::sync::oneshot;

use crate::capture::{Captured, TeeBody};
use crate::config::{InboundCapture, MockTcp, MockWebsocket, Mode, Unmatched};
use crate::engine::{Arrived, MockSet, Trigger};
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
    let target = match info.original_dst {
        Some(dst) => Some(state.inbound_targets.resolve(dst).await),
        None => state.config.inbound.upstream,
    };
    let Some(target) = target else {
        return text(
            StatusCode::BAD_GATEWAY,
            "mimic: aucune destination pour le trafic entrant",
        );
    };
    let authority = host_header(req.headers())
        .map(|h| normalize_authority(&h, "http"))
        .unwrap_or_else(|| target.to_string());
    // Capture de l'entrant réglable pendant un tir (spec §5.2) ; rien en passthrough.
    let record = if state.mode() == Mode::Passthrough {
        Record::No
    } else {
        match state.inbound_capture() {
            InboundCapture::Full => Record::Full,
            InboundCapture::Metadata => Record::Metadata,
            InboundCapture::Off => Record::Timed,
        }
    };
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
    let mode = state.mode();
    // Les trames ne sont pas enregistrées, il n'y a donc rien à rejouer : une poignée de main
    // WebSocket est relayée dans tous les modes, et enregistrée hors passthrough, sauf en
    // mock avec `mock.websocket: refuse` (spec §3.4).
    if is_websocket_upgrade(&parts) {
        if crate::engine::mock_mode(mode) && state.mock_websocket() == MockWebsocket::Refuse {
            return refuse_websocket(&state, mode, &parts, &target);
        }
        let record = if mode == Mode::Passthrough {
            Record::No
        } else {
            Record::Full
        };
        return forward(
            state,
            Direction::Outbound,
            parts,
            boxed(body),
            record,
            target,
        )
        .await;
    }
    match mode {
        Mode::Passthrough => {
            forward(
                state,
                Direction::Outbound,
                parts,
                boxed(body),
                Record::No,
                target,
            )
            .await
        }
        Mode::Record => {
            forward(
                state,
                Direction::Outbound,
                parts,
                boxed(body),
                Record::Full,
                target,
            )
            .await
        }
        Mode::Mock | Mode::MockPartial => serve_mock(state, parts, body, target).await,
    }
}

/// Ce que `forward` garde d'un échange (spec §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Record {
    /// Rien : métriques seules, durée jusqu'au premier octet (passthrough).
    No,
    /// Rien non plus, mais durée mesurée jusqu'au dernier octet (`capture.inbound: off`).
    Timed,
    /// Métadonnées seules, en mémoire, ni journalisées ni publiées.
    Metadata,
    /// Échange complet.
    Full,
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
    record: Record,
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
    // Côté appelant, la connexion qui basculera en WebSocket après le 101 ; sans elle (aucune
    // bascule possible), la requête est relayée comme une requête ordinaire.
    let client_upgrade = if is_websocket_upgrade(&parts) {
        parts.extensions.remove::<OnUpgrade>()
    } else {
        None
    };
    let websocket = client_upgrade.is_some();
    strip_hop_by_hop(&mut parts.headers, websocket);
    // HTTP/2 vers l'amont : en clair (h2c, connaissance préalable), ou en HTTPS pour gRPC,
    // qui l'exige ; une autre requête HTTP/2 déchiffrée part en HTTP/1.1, que toute
    // destination accepte (le client a négocié `h2` avec le serveur forgé, spec §4.2).
    let grpc = parts
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/grpc"));
    let h2 = parts.version == http::Version::HTTP_2 && (target.scheme != "https" || grpc);
    if h2 {
        // En HTTP/2, l'authority voyage dans `:authority` ; seul `te: trailers` est permis.
        parts.headers.remove(http::header::HOST);
        if parts
            .headers
            .get(http::header::TE)
            .is_some_and(|v| v != "trailers")
        {
            parts.headers.remove(http::header::TE);
        }
    } else {
        parts.version = http::Version::HTTP_11;
        if !parts.headers.contains_key(http::header::HOST)
            && let Ok(v) = HeaderValue::from_str(&target.authority)
        {
            parts.headers.insert(http::header::HOST, v);
        }
    }
    let req_headers = to_headers(&parts.headers);
    let method = parts.method.to_string();

    let metadata = record == Record::Metadata;
    // En métadonnées, le tee compte les octets sans en copier aucun.
    let limit = if metadata {
        0
    } else {
        state.config.capture.max_body_bytes
    };
    let (body, req_capture) = if matches!(record, Record::Full | Record::Metadata) {
        let (tee, rx) = TeeBody::with_budget(body, limit, state.inflight.clone());
        (boxed(tee), Some(rx))
    } else {
        (body, None)
    };

    let authority_label = state.metrics.authority(&target.authority);
    let client = if h2 { &state.client_h2 } else { &state.client };
    let response = match client.request(Request::from_parts(parts, body)).await {
        Ok(r) => r,
        Err(e) => {
            if let Some(omitted) = state.upstream_log.allow(Instant::now()) {
                tracing::warn!(authority = %target.authority, error = %e, omitted, "échec vers l'amont");
            }
            // L'application a pu changer d'adresse d'écoute : refaire le choix (spec §3.1).
            if direction == Direction::Inbound
                && e.is_connect()
                && let Ok(a) = target.connect.parse::<SocketAddr>()
            {
                state.inbound_targets.forget(a.port());
            }
            // Connexion non établie dans le délai : 504, compté à part (spec §3.5).
            let timed_out = e.is_connect() && crate::connect::is_timeout(&e);
            let kind = match (timed_out, e.is_connect()) {
                (true, _) => "connect_timeout",
                (false, true) => "connect",
                (false, false) => "request",
            };
            state
                .metrics
                .count_upstream_error(direction, authority_label, kind);
            if timed_out {
                count_request(&state, direction, mode, authority_label, &method, 504);
                return text(
                    StatusCode::GATEWAY_TIMEOUT,
                    "mimic: connexion vers l'amont non établie dans le délai (outbound.connect_timeout)",
                );
            }
            count_request(&state, direction, mode, authority_label, &method, 502);
            return text(StatusCode::BAD_GATEWAY, "mimic: amont injoignable");
        }
    };
    let ttfb = started.elapsed();
    let (mut resp_parts, resp_body) = response.into_parts();
    let switched = websocket && resp_parts.status == StatusCode::SWITCHING_PROTOCOLS;
    strip_hop_by_hop(&mut resp_parts.headers, switched);
    let status = resp_parts.status.as_u16();
    count_request(&state, direction, mode, authority_label, &method, status);

    if switched || (req_capture.is_none() && record != Record::Timed) {
        observe_duration(&state, direction, mode, authority_label, ttfb);
    }
    if record == Record::Timed && !switched {
        // Durée jusqu'au dernier octet, comme en capture complète, sans rien garder.
        let (tee, done) = TeeBody::new(resp_body, 0);
        tokio::spawn(async move {
            if let Ok(c) = done.await {
                let total = c.ended.saturating_duration_since(started);
                observe_duration(&state, direction, mode, authority_label, total);
            }
        });
        return Response::from_parts(resp_parts, boxed(tee));
    }
    let pending = req_capture.map(|rx| {
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
            metadata,
        };
        (pending, rx)
    });
    if switched {
        state.metrics.count_websocket(direction, "relayed");
        let upstream_upgrade = resp_parts.extensions.remove::<OnUpgrade>();
        // Seule la poignée de main est enregistrée : le body de requête (vide) n'est pas attendu.
        let pending = pending.map(|(p, _)| p);
        tokio::spawn(relay_websocket(
            state,
            client_upgrade,
            upstream_upgrade,
            pending,
        ));
        return Response::from_parts(resp_parts, boxed(Empty::<Bytes>::new()));
    }
    let Some((pending, req_capture)) = pending else {
        return Response::from_parts(resp_parts, boxed(resp_body));
    };
    let (tee, resp_capture) = TeeBody::with_budget(resp_body, limit, state.inflight.clone());
    tokio::spawn(finalize(state, pending, req_capture, resp_capture));
    Response::from_parts(resp_parts, boxed(tee))
}

/// Refus d'une poignée de main WebSocket sortante en mock (`mock.websocket: refuse`) : `503`
/// sans joindre la dépendance, rien d'enregistré, sans passer par le moteur (spec §3.4).
fn refuse_websocket(
    state: &AppState,
    mode: Mode,
    parts: &http::request::Parts,
    target: &Target,
) -> Response<ProxyBody> {
    let authority_label = state.metrics.authority(&target.authority);
    state
        .metrics
        .count_websocket(Direction::Outbound, "refused");
    count_request(
        state,
        Direction::Outbound,
        mode,
        authority_label,
        parts.method.as_str(),
        503,
    );
    let mut r = text(
        StatusCode::SERVICE_UNAVAILABLE,
        "mimic: WebSocket sortant refusé en mock (mock.websocket: refuse)",
    );
    r.headers_mut()
        .insert("x-mimic-websocket", HeaderValue::from_static("refused"));
    r
}

/// Poignée de main WebSocket en HTTP/1.1 : `Upgrade: websocket` et le jeton `upgrade` dans
/// `Connection` (spec §3.4).
fn is_websocket_upgrade(parts: &http::request::Parts) -> bool {
    parts.version == http::Version::HTTP_11
        && parts
            .headers
            .get(http::header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(',')
                    .any(|p| p.trim().eq_ignore_ascii_case("websocket"))
            })
        && connection_tokens(&parts.headers).any(|t| t == "upgrade")
}

/// Jetons du header `Connection`, en minuscules.
fn connection_tokens(headers: &HeaderMap) -> impl Iterator<Item = String> + '_ {
    headers
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
}

/// Copie les octets entre l'appelant et l'amont, sans interpréter les trames, jusqu'à ce que
/// les deux côtés aient fermé ; puis enregistre la poignée de main (spec §3.4).
async fn relay_websocket(
    state: Arc<AppState>,
    client: Option<OnUpgrade>,
    upstream: Option<OnUpgrade>,
    pending: Option<Pending>,
) {
    let (Some(client), Some(upstream)) = (client, upstream) else {
        tracing::debug!("bascule WebSocket impossible : connexion non basculable");
        return;
    };
    match tokio::join!(client, upstream) {
        (Ok(client), Ok(upstream)) => {
            let mut client = hyper_util::rt::TokioIo::new(client);
            let mut upstream = hyper_util::rt::TokioIo::new(upstream);
            if let Err(e) = tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
                tracing::debug!(error = %e, "relais WebSocket interrompu");
            }
        }
        (client, upstream) => {
            tracing::debug!(
                client = ?client.err(),
                upstream = ?upstream.err(),
                "bascule WebSocket échouée"
            );
        }
    }
    if let Some(p) = pending {
        record_websocket(&state, p, Instant::now());
    }
}

/// Échange d'une connexion WebSocket : la poignée de main, sans body ni trame ; `total` est la
/// durée de la connexion.
fn record_websocket(state: &AppState, mut p: Pending, ended: Instant) {
    let total = ended.saturating_duration_since(p.started);
    let metadata = p.metadata;
    if metadata {
        p.req_headers.clear();
        p.resp_headers.clear();
        p.uri = p.uri.path().parse().unwrap_or_default();
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
        trace_id: trace_id(&p.req_headers),
        req_headers: p.req_headers,
        req_body: Body::Empty,
        status: p.status,
        resp_headers: p.resp_headers,
        resp_body: Body::Empty,
        ttfb_us: p.ttfb.as_micros() as u64,
        total_us: total.as_micros() as u64,
        resp_trailers: vec![],
        redactions: vec![],
    };
    if metadata {
        state.store.add_ephemeral(exchange);
    } else {
        // Headers redactés avant d'entrer dans le store, comme pour tout échange (RED-04).
        if state.redact(&mut exchange) {
            state.store.add(exchange, Origin::Local);
        }
    }
}

/// Identifiant de trace W3C tiré de `traceparent`.
fn trace_id(headers: &[Header]) -> Option<String> {
    find(headers, "traceparent")
        .and_then(|t| t.split('-').nth(1))
        .map(String::from)
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
    authority_label: &'static str,
    method: String,
    uri: Uri,
    req_headers: Vec<Header>,
    status: u16,
    resp_headers: Vec<Header>,
    /// `capture.inbound: metadata` : ni query, ni headers, ni body (spec §5.2).
    metadata: bool,
}

async fn finalize(
    state: Arc<AppState>,
    p: Pending,
    req: oneshot::Receiver<Captured>,
    resp: oneshot::Receiver<Captured>,
) {
    let (Ok(req), Ok(mut resp)) = (req.await, resp.await) else {
        return;
    };
    let resp_trailers = std::mem::take(&mut resp.trailers);
    let total = resp.ended.saturating_duration_since(p.started);
    observe_duration(&state, p.direction, p.mode, p.authority_label, total);
    if p.metadata {
        record_metadata(&state, p, req, resp, total);
        return;
    }
    let capture = &state.config.capture;
    let id = format!("{}-{}", state.config.instance, ulid::Ulid::generate());
    let path = p.uri.path().to_string();
    // Réservoir de l'opération : un body qui ne serait pas gardé n'est ni décodé, ni parsé,
    // ni redacté (spec §5.2).
    let keep = capture.bodies
        && state
            .store
            .wants_bodies(p.direction, &p.authority, &p.method, &path, p.status, &id);
    if capture.bodies && !keep && (req.size > 0 || resp.size > 0) {
        dropped(&state, "sample_limit");
    }
    // Bodies que le budget en vol n'a pas laissé copier en entier (spec §5.2).
    let over_budget = [req.over_budget, resp.over_budget];
    let req_body = req.into_body(
        keep,
        capture.max_body_bytes,
        find(&p.req_headers, "content-encoding"),
    );
    let resp_body = resp.into_body(
        keep,
        capture.max_body_bytes,
        find(&p.resp_headers, "content-encoding"),
    );
    for (b, over_budget) in [&req_body, &resp_body].into_iter().zip(over_budget) {
        if matches!(b, Body::Truncated { .. }) {
            dropped(
                &state,
                if over_budget {
                    "inflight_budget"
                } else {
                    "truncated"
                },
            );
        }
    }
    let mut exchange = Exchange {
        id,
        direction: p.direction,
        started_at_ms: p.started_ms,
        scheme: p.scheme,
        authority: p.authority,
        method: p.method,
        path,
        query: parse_query(p.uri.query()),
        trace_id: trace_id(&p.req_headers),
        req_headers: p.req_headers,
        req_body,
        status: p.status,
        resp_headers: p.resp_headers,
        resp_body,
        ttfb_us: p.ttfb.as_micros() as u64,
        total_us: total.as_micros() as u64,
        resp_trailers,
        redactions: vec![],
    };
    if !state.redact(&mut exchange) {
        return;
    }
    if state.store.add(exchange, Origin::Local) == Added::StoredWithoutBodies {
        dropped(&state, "sample_limit");
    }
}

/// Échange réduit à ses métadonnées : rien n'est décodé, redacté, journalisé ni publié ; il
/// ne reste qu'en mémoire (spec §5.2).
fn record_metadata(state: &AppState, p: Pending, req: Captured, resp: Captured, total: Duration) {
    let exchange = Exchange {
        id: format!("{}-{}", state.config.instance, ulid::Ulid::generate()),
        direction: p.direction,
        started_at_ms: p.started_ms,
        scheme: p.scheme,
        authority: p.authority,
        method: p.method,
        path: p.uri.path().to_string(),
        query: vec![],
        trace_id: None,
        req_headers: vec![],
        req_body: req.into_body(false, 0, None),
        status: p.status,
        resp_headers: vec![],
        resp_body: resp.into_body(false, 0, None),
        ttfb_us: p.ttfb.as_micros() as u64,
        total_us: total.as_micros() as u64,
        resp_trailers: vec![],
        redactions: vec![],
    };
    state.store.add_ephemeral(exchange);
}

async fn serve_mock(
    state: Arc<AppState>,
    parts: http::request::Parts,
    body: Incoming,
    target: Target,
) -> Response<ProxyBody> {
    // Arrivée de la requête : la latence rejouée est comptée d'ici, comme la durée mesurée
    // en record (spec §5.3).
    let started = Instant::now();
    // Enregistrements sortants présents à l'arrivée : ceux qu'un moteur à jour doit connaître.
    let seen = state.store.outbound_version();
    let mode = state.mode();
    let limit = state.config.capture.max_body_bytes;
    let body = match Limited::new(body, limit).collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => {
            // Refus du mock, compté et relevé comme une requête sans correspondance (§5.3).
            let label = state.metrics.authority(&target.authority);
            mock_metric(&state, label, "too_large");
            count_request(
                &state,
                Direction::Outbound,
                mode,
                label,
                parts.method.as_str(),
                413,
            );
            refused(&state, Reason::TooLarge, &target.authority, &parts);
            return text(
                StatusCode::PAYLOAD_TOO_LARGE,
                "mimic: body trop gros pour le mode mock",
            );
        }
    };
    let mut request = MockRequest {
        authority: target.authority.clone(),
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        query: parse_query(parts.uri.query()),
        headers: to_headers(&parts.headers),
        body: body.to_vec(),
    };
    // Comparée aux enregistrements redactés : redactée de même (spec §7). La requête relayée
    // en mock-partial part des `parts` et du `body` d'origine.
    state.redact_mock_request(&mut request);
    // Moteur courant, lu sans verrou ; construit ici seulement s'il n'y en a pas encore.
    let mut set = match state.mock_engine() {
        Some(set) => set,
        None => state.fresh_engine(Trigger::Switch, seen).await,
    };
    let mut decision = set.engine.respond(&request);
    if state.is_stale(&set, seen) {
        if decision == MockDecision::Unmatched {
            // Le store a changé : l'enregistrement cherché vient peut-être d'arriver. Il est
            // cherché parmi les ajouts de son opération, sans reconstruire le moteur.
            match state.recent_engine(&set, &request) {
                Arrived::Nothing => {}
                Arrived::Engine(recent) => {
                    decision = recent.engine.respond(&request);
                    set = recent;
                }
                Arrived::Unknown => {
                    set = state.fresh_engine(Trigger::Unmatched, seen).await;
                    decision = set.engine.respond(&request);
                }
            }
        }
        state.schedule_refresh();
    }
    let authority_label = state.metrics.authority(&target.authority);
    let method = request.method;
    match decision {
        MockDecision::Matched(r) => {
            mock_metric(&state, authority_label, r.kind.as_str());
            let latency = Duration::from_micros(r.latency_us);
            state.metrics.observe_mock_latency(authority_label, latency);
            // Réponse prête avant l'attente : sa préparation est prise sur la latence.
            let response = mock_response(&state, &set, &r);
            count_request(
                &state,
                Direction::Outbound,
                mode,
                authority_label,
                &method,
                response.status().as_u16(),
            );
            crate::engine::sleep_until(started + latency).await;
            observe_duration(
                &state,
                Direction::Outbound,
                mode,
                authority_label,
                started.elapsed(),
            );
            response
        }
        MockDecision::Unmatched => {
            mock_metric(&state, authority_label, "unmatched");
            let passthrough =
                mode == Mode::MockPartial || state.config.mock.unmatched == Unmatched::Passthrough;
            if passthrough {
                return forward(
                    state,
                    Direction::Outbound,
                    parts,
                    full(body),
                    Record::Full,
                    target,
                )
                .await;
            }
            count_request(
                &state,
                Direction::Outbound,
                mode,
                authority_label,
                &method,
                599,
            );
            refused(&state, Reason::Unmatched, &target.authority, &parts);
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

/// Requête à laquelle le mock ne répond pas : relevée et journalisée par sa forme, sans
/// valeur (spec §5.3).
fn refused(state: &AppState, reason: Reason, authority: &str, parts: &http::request::Parts) {
    let u = unmatched::Unmatched::new(reason, authority, parts.method.as_str(), parts.uri.path());
    let trace = parts
        .headers
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
        .and_then(|t| t.split('-').nth(1))
        // Identifiant de trace W3C seulement : rien d'autre du header n'atteint le journal.
        .filter(|t| t.len() == 32 && t.bytes().all(|b| b.is_ascii_hexdigit()));
    state.note_unmatched(&u, trace);
}

/// Réponse servie : celle préparée pour l'enregistrement (headers, body encodé, partagés), plus
/// le header de correspondance propre à la requête.
fn mock_response(state: &AppState, set: &MockSet, r: &MockResponse) -> Response<ProxyBody> {
    let prepared = set.prepared(r);
    // Trailers enregistrés (gRPC) : rejoués après le body, sans quoi le client échoue.
    let body = match &prepared.trailers {
        Some(trailers) => boxed(http_body_util::StreamBody::new(futures_util::stream::iter(
            [
                Ok::<_, std::convert::Infallible>(hyper::body::Frame::data(prepared.body.clone())),
                Ok(hyper::body::Frame::trailers(trailers.clone())),
            ],
        ))),
        None => full(prepared.body.clone()),
    };
    let mut response = Response::new(body);
    *response.status_mut() = prepared.status;
    *response.headers_mut() = prepared.headers.clone();
    if state.config.mock.debug_header
        && let Ok(v) = HeaderValue::from_str(&format!("{};score={:.2}", r.kind.as_str(), r.score))
    {
        response.headers_mut().insert("x-mimic-match", v);
    }
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
        count_request(&state, Direction::Outbound, mode, label, "CONNECT", 200);
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
        mock_metric(&state, label, "unmatched");
        count_request(&state, Direction::Outbound, mode, label, "CONNECT", 403);
        return text(
            StatusCode::FORBIDDEN,
            "mimic: tunnel refusé en mode mock (TLS non intercepté)",
        );
    }
    count_request(&state, Direction::Outbound, mode, label, "CONNECT", 200);
    tokio::spawn(async move {
        let upgraded = match hyper::upgrade::on(req).await {
            Ok(u) => u,
            Err(e) => {
                tracing::debug!(error = %e, "upgrade CONNECT échoué");
                return;
            }
        };
        let timeout = state.config.outbound.connect_timeout;
        match crate::connect::tcp(authority.as_str(), timeout).await {
            Ok(mut upstream) => {
                let mut client = hyper_util::rt::TokioIo::new(upgraded);
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            }
            Err(e) => {
                if let Some(omitted) = state.upstream_log.allow(Instant::now()) {
                    tracing::warn!(%authority, error = %e, omitted, "tunnel vers l'amont impossible");
                }
                let kind = if e.kind() == std::io::ErrorKind::TimedOut {
                    "connect_timeout"
                } else {
                    "connect"
                };
                state
                    .metrics
                    .count_upstream_error(Direction::Outbound, label, kind);
            }
        }
    });
    Response::new(boxed(Empty::<Bytes>::new()))
}

/// Attente maximale, en sortie et en mock, qu'une connexion muette se mette à parler : rien
/// n'est ouvert vers la dépendance pendant ce temps (O3). Au-delà, elle est traitée comme un
/// protocole inconnu : refusée en `mock` (sauf `mock.tcp: relay`), relayée en `mock-partial`.
pub const MOCK_SILENCE_WAIT: Duration = Duration::from_secs(30);

/// Issue d'une connexion restée muette au-delà du délai de détection (spec §3.1).
pub enum Silent {
    /// L'application a parlé la première : sa connexion, le protocole de ses premiers octets,
    /// et la connexion déjà ouverte vers la destination quand elle peut servir à un tunnel.
    Client(
        tokio::net::TcpStream,
        crate::net::Protocol,
        Option<tokio::net::TcpStream>,
    ),
    /// Déjà traitée ici : tunnel vers un serveur qui a parlé le premier, refus en mock, ou
    /// abandon.
    Done,
}

/// Tranche une connexion muette au premier octet. Un pool de connexions ouvre souvent une
/// connexion avant d'en avoir l'usage : si l'application finit par y parler HTTP, elle doit
/// être traitée en HTTP, et non relayée à l'aveugle. Un serveur qui parle en premier (MySQL,
/// SMTP) attend, lui, qu'on le joigne : on ouvre donc la connexion vers la destination et le
/// premier qui parle décide.
pub async fn resolve_silent(
    state: Arc<AppState>,
    stream: tokio::net::TcpStream,
    original_dst: SocketAddr,
    inbound: bool,
) -> Silent {
    resolve_silent_within(state, stream, original_dst, inbound, MOCK_SILENCE_WAIT).await
}

/// `resolve_silent`, avec une attente de l'application en mock réglable (tests).
pub async fn resolve_silent_within(
    state: Arc<AppState>,
    stream: tokio::net::TcpStream,
    original_dst: SocketAddr,
    inbound: bool,
    mock_wait: Duration,
) -> Silent {
    let mode = state.mode();
    if !inbound && matches!(mode, Mode::Mock | Mode::MockPartial) {
        // En mock, joindre la dépendance pour savoir si elle parle la première la
        // contacterait : on attend l'application, sans rien ouvrir.
        return match tokio::time::timeout(mock_wait, stream.readable()).await {
            Ok(Ok(())) => {
                let protocol = crate::net::sniff(&stream).await;
                Silent::Client(stream, protocol, None)
            }
            Ok(Err(_)) => Silent::Done,
            // Toujours muette : protocole inconnu, refusé en mock selon `mock.tcp` (§3.4).
            Err(_) if refuses_tcp(&state) => {
                refuse_tunnel(
                    &state,
                    original_dst,
                    crate::net::Protocol::Other,
                    "connexion sortante muette refusée en mode mock (mock.tcp: refuse)",
                );
                Silent::Done
            }
            Err(_) => Silent::Client(stream, crate::net::Protocol::Other, None),
        };
    }
    let target = if inbound {
        state.inbound_targets.resolve(original_dst).await
    } else {
        original_dst
    };
    let timeout = state.config.outbound.connect_timeout;
    let upstream = match crate::connect::tcp(target, timeout).await {
        Ok(u) => u,
        Err(e) => {
            tunnel_failed(&state, target, &e, inbound, crate::net::Protocol::Other);
            return Silent::Done;
        }
    };
    let _ = upstream.set_nodelay(true);
    let client_first = tokio::select! {
        r = stream.readable() => {
            if r.is_err() {
                return Silent::Done;
            }
            true
        }
        _ = upstream.readable() => false,
    };
    if !client_first {
        // Le serveur parle le premier : tunnel opaque, avec la connexion déjà ouverte.
        let protocol = crate::net::Protocol::Other;
        tunnel(
            state,
            stream,
            original_dst,
            protocol,
            inbound,
            Some(upstream),
        )
        .await;
        return Silent::Done;
    }
    match crate::net::sniff(&stream).await {
        // La connexion ouverte vers la destination n'a pas servi : elle est refermée.
        crate::net::Protocol::Http => Silent::Client(stream, crate::net::Protocol::Http, None),
        p => Silent::Client(stream, p, Some(upstream)),
    }
}

/// Relais TCP opaque d'une connexion redirigée non HTTP (spec §3.1). `upstream` est la
/// connexion déjà ouverte vers la destination, s'il y en a une.
pub async fn tunnel(
    state: Arc<AppState>,
    stream: tokio::net::TcpStream,
    original_dst: SocketAddr,
    protocol: crate::net::Protocol,
    inbound: bool,
    upstream: Option<tokio::net::TcpStream>,
) {
    // En mock, ni un appel chiffré non intercepté, ni un protocole que le mock ne sait pas
    // rejouer ne doivent atteindre une dépendance (O3, spec §3.4).
    if !inbound && state.mode() == Mode::Mock {
        match protocol {
            crate::net::Protocol::Tls => {
                refuse_tunnel(
                    &state,
                    original_dst,
                    protocol,
                    "connexion TLS sortante refusée en mode mock (hôte hors de tls.intercept)",
                );
                return;
            }
            crate::net::Protocol::Other if refuses_tcp(&state) => {
                refuse_tunnel(
                    &state,
                    original_dst,
                    protocol,
                    "connexion TCP sortante refusée en mode mock (mock.tcp: refuse)",
                );
                return;
            }
            _ => {}
        }
    }
    let upstream = match upstream {
        Some(u) => u,
        None => {
            let target = if inbound {
                state.inbound_targets.resolve(original_dst).await
            } else {
                original_dst
            };
            match crate::connect::tcp(target, state.config.outbound.connect_timeout).await {
                Ok(u) => {
                    let _ = u.set_nodelay(true);
                    u
                }
                Err(e) => {
                    tunnel_failed(&state, target, &e, inbound, protocol);
                    return;
                }
            }
        }
    };
    count_tunnel(&state, inbound, protocol, "relayed");
    let mut stream = stream;
    let mut upstream = upstream;
    let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
}

/// Vrai si le TCP sortant non HTTP est refusé : `mock` avec `mock.tcp: refuse` (spec §3.4).
/// `mock-partial` relaie ce qu'il ne sait pas servir.
fn refuses_tcp(state: &AppState) -> bool {
    state.mode() == Mode::Mock && state.config.mock.tcp == MockTcp::Refuse
}

/// Connexion sortante refermée en mock sans rien ouvrir vers sa destination : comptée, et
/// journalisée au plus une fois par seconde avec la destination d'origine (spec §3.1).
fn refuse_tunnel(
    state: &AppState,
    original_dst: SocketAddr,
    protocol: crate::net::Protocol,
    message: &'static str,
) {
    count_tunnel(state, false, protocol, "refused");
    if let Some(omitted) = state.tunnel_refused_log.allow(Instant::now()) {
        tracing::warn!(
            destination = %original_dst,
            kind = protocol.as_str(),
            omitted,
            "{message}"
        );
    }
}

fn count_tunnel(
    state: &AppState,
    inbound: bool,
    protocol: crate::net::Protocol,
    result: &'static str,
) {
    state
        .metrics
        .passthrough
        .get_or_create(&crate::metrics::PassthroughLabels {
            direction: if inbound { "inbound" } else { "outbound" },
            kind: protocol.as_str(),
            result,
        })
        .inc();
}

fn tunnel_failed(
    state: &AppState,
    target: SocketAddr,
    e: &std::io::Error,
    inbound: bool,
    protocol: crate::net::Protocol,
) {
    if let Some(omitted) = state.upstream_log.allow(Instant::now()) {
        tracing::warn!(%target, error = %e, omitted, "tunnel impossible");
    }
    count_tunnel(state, inbound, protocol, "error");
}

fn count_request(
    state: &AppState,
    direction: Direction,
    mode: Mode,
    authority: &'static str,
    method: &str,
    status: u16,
) {
    state
        .metrics
        .count_request(direction, mode, authority, method, status);
}

fn observe_duration(
    state: &AppState,
    direction: Direction,
    mode: Mode,
    authority: &'static str,
    d: Duration,
) {
    state
        .metrics
        .observe_duration(direction, mode, authority, d);
}

fn mock_metric(state: &AppState, authority: &'static str, kind: &'static str) {
    state.metrics.count_mock(authority, kind);
}

fn dropped(state: &AppState, reason: &'static str) {
    state.metrics.count_dropped(reason);
}

pub fn dir_str(d: Direction) -> &'static str {
    crate::metrics::direction_label(d)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Retire les headers propres à la connexion, et ceux que `Connection` nomme (RFC 9110
/// §7.6.1). Pour une poignée de main WebSocket, `Upgrade` et `Connection: upgrade` restent.
fn strip_hop_by_hop(headers: &mut HeaderMap, websocket: bool) {
    let named: Vec<String> = connection_tokens(headers).collect();
    let upgrade = headers.get(http::header::UPGRADE).cloned();
    for name in HOP_BY_HOP {
        headers.remove(*name);
    }
    for name in &named {
        headers.remove(name.as_str());
    }
    if websocket && let Some(upgrade) = upgrade {
        headers.insert(http::header::UPGRADE, upgrade);
        headers.insert(
            http::header::CONNECTION,
            HeaderValue::from_static("upgrade"),
        );
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
    fn hop_by_hop_includes_headers_named_by_connection() {
        let mut h = HeaderMap::new();
        h.insert("connection", HeaderValue::from_static("Upgrade, X-Hop"));
        h.insert("upgrade", HeaderValue::from_static("websocket"));
        h.insert("x-hop", HeaderValue::from_static("1"));
        h.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        h.insert("x-end", HeaderValue::from_static("2"));
        let mut plain = h.clone();
        strip_hop_by_hop(&mut plain, false);
        let names: Vec<_> = plain.keys().map(|k| k.as_str()).collect();
        assert_eq!(names, vec!["x-end"]);
        strip_hop_by_hop(&mut h, true);
        assert_eq!(h["upgrade"], "websocket");
        assert_eq!(h["connection"], "upgrade");
        assert!(!h.contains_key("x-hop") && !h.contains_key("keep-alive"));
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
