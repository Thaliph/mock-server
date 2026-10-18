//! Relais WebSocket de bout en bout : PRX-12 à PRX-14 et PRX-16 (spec §3.4, §5.3 et §11.2).

mod common;

use std::time::Duration;

use common::ws::*;
use common::*;
use mimic_core::model::{Body, Exchange};
use mimic_proxy::config::{Mode, MockWebsocket, Unmatched};
use serde_json::Value;

const SECRET: &str = "s3cr3t-ws-token";

fn ws_config(server: &EchoServer, mode: Mode) -> mimic_proxy::config::Config {
    config(Some(server.addr), mode)
}

fn decode(v: &Value) -> Exchange {
    serde_json::from_value(v.clone()).unwrap()
}

#[tokio::test]
async fn prx_12_websocket_is_relayed_in_every_mode() {
    for mode in [
        Mode::Passthrough,
        Mode::Record,
        Mode::Mock,
        Mode::MockPartial,
    ] {
        let server = echo_server().await;
        let p = proxy(ws_config(&server, mode)).await;

        let ws = inbound_ws(&p, "/ws", &[]).await;
        echo_and_close(ws, Duration::ZERO).await;
        let (ws, _) = outbound_ws(&p, &server, "/ws", &[]).await;
        echo_and_close(ws, Duration::ZERO).await;

        assert_eq!(server.connections(), 2, "{mode:?}");
        let m = metrics(&p).await;
        assert_eq!(ws_metric(&m, "inbound", "relayed"), 1.0, "{mode:?}\n{m}");
        assert_eq!(ws_metric(&m, "outbound", "relayed"), 1.0, "{mode:?}\n{m}");
        assert_eq!(ws_metric(&m, "outbound", "refused"), 0.0, "{mode:?}\n{m}");
        assert!(
            m.contains(r#"method="GET",status="101"}"#),
            "{mode:?} : 101 compté dans mimic_requests_total\n{m}"
        );
    }
}

#[tokio::test]
async fn prx_13_record_keeps_the_handshake_not_the_frames() {
    let server = echo_server().await;
    let p = proxy(ws_config(&server, Mode::Record)).await;
    let auth = format!("Bearer {SECRET}");
    let extra = [("Authorization", auth.as_str())];
    let linger = Duration::from_millis(50);

    let ws = inbound_ws(&p, "/ws?room=7", &extra).await;
    echo_and_close(ws, linger).await;
    let (ws, _) = outbound_ws(&p, &server, "/ws?room=7", &extra).await;
    echo_and_close(ws, linger).await;

    for direction in ["inbound", "outbound"] {
        let list = wait_exchanges(&p, &format!("direction={direction}"), 1).await;
        assert_eq!(list.len(), 1, "{direction} : un échange par connexion");
        let e = decode(&list[0]);
        assert_eq!(e.method, "GET");
        assert_eq!(e.path, "/ws");
        assert_eq!(e.query, vec![("room".to_string(), "7".to_string())]);
        assert_eq!(e.status, 101);
        assert_eq!(e.req_body, Body::Empty, "{direction} : aucune trame");
        assert_eq!(e.resp_body, Body::Empty, "{direction} : aucune trame");
        let authz = e
            .req_header("authorization")
            .expect("header gardé, redacté");
        assert!(authz.starts_with("<redacted:"), "{authz}");
        assert!(
            e.req_header("upgrade")
                .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
        );
        assert!(e.resp_header("sec-websocket-accept").is_some());
        assert!(e.total_us >= linger.as_micros() as u64, "{}", e.total_us);
        assert!(e.ttfb_us <= e.total_us);
    }
    let raw = client()
        .get(admin(&p, "/exchanges?limit=100"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!raw.contains(SECRET), "secret dans le store");
    assert!(!raw.contains(FRAME), "trame dans le store");

    // En passthrough, rien n'est enregistré.
    let server = echo_server().await;
    let p = proxy(ws_config(&server, Mode::Passthrough)).await;
    echo_and_close(inbound_ws(&p, "/ws", &[]).await, Duration::ZERO).await;
    echo_and_close(outbound_ws(&p, &server, "/ws", &[]).await.0, Duration::ZERO).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(exchanges(&p, "limit=100").await.is_empty());
}

#[tokio::test]
async fn prx_14_mock_relays_websocket_without_replaying_the_handshake() {
    for mode in [Mode::Mock, Mode::MockPartial] {
        let server = echo_server().await;
        let mut c = ws_config(&server, mode);
        c.mock.unmatched = Unmatched::Error;
        let p = proxy(c).await;

        let (ws, head) = outbound_ws(&p, &server, "/ws", &[]).await;
        assert!(
            !head.to_ascii_lowercase().contains("x-mimic-unmatched"),
            "{head}"
        );
        echo_and_close(ws, Duration::ZERO).await;
        assert_eq!(server.connections(), 1, "{mode:?}");
        let m = metrics(&p).await;
        assert!(
            !m.contains("mimic_mock_responses_total{"),
            "{mode:?} : la poignée de main ne passe pas par le moteur\n{m}"
        );

        // La poignée de main relayée est enregistrée, mais jamais rejouée.
        let list = wait_exchanges(&p, "direction=outbound", 1).await;
        assert_eq!(decode(&list[0]).status, 101);
        if mode == Mode::Mock {
            let r = app_client(&p)
                .get(format!("http://{}/ws", server.addr))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status().as_u16(), 599);
            assert_eq!(r.headers()["x-mimic-unmatched"], "true");
        }
    }
}

/// `mock.websocket: refuse` : en mock, la poignée de main sortante reçoit `503` sans que la
/// dépendance soit jointe ; l'entrant et les autres modes relaient toujours (spec §3.4).
#[tokio::test]
async fn prx_16_refuse_answers_503_without_reaching_the_dependency() {
    for (mode, unmatched) in [
        (Mode::Mock, Unmatched::Error),
        (Mode::Mock, Unmatched::Passthrough),
        (Mode::MockPartial, Unmatched::Error),
    ] {
        let case = format!("{mode:?}, unmatched {unmatched:?}");
        let server = echo_server().await;
        let mut c = ws_config(&server, mode);
        c.mock.unmatched = unmatched;
        c.mock.websocket = MockWebsocket::Refuse;
        let p = proxy(c).await;

        let (head, body) = outbound_refused(&p, &server, "/ws").await;
        let lower = head.to_ascii_lowercase();
        assert!(head.starts_with("HTTP/1.1 503"), "{case} : {head}");
        assert!(lower.contains("x-mimic-websocket: refused"), "{case} : {head}");
        assert!(!lower.contains("x-mimic-unmatched"), "{case} : {head}");
        assert!(!lower.contains("sec-websocket-accept"), "{case} : {head}");
        assert!(body.contains("mock.websocket"), "{case} : {body}");
        assert_eq!(server.accepted(), 0, "{case} : dépendance jointe");

        // L'application reçoit toujours ses WebSockets.
        echo_and_close(inbound_ws(&p, "/ws", &[]).await, Duration::ZERO).await;
        assert_eq!(server.accepted(), 1, "{case} : seule la connexion entrante");
        assert_eq!(server.connections(), 1, "{case}");

        let m = metrics(&p).await;
        assert_eq!(ws_metric(&m, "outbound", "refused"), 1.0, "{case}\n{m}");
        assert_eq!(ws_metric(&m, "outbound", "relayed"), 0.0, "{case}\n{m}");
        assert_eq!(ws_metric(&m, "inbound", "relayed"), 1.0, "{case}\n{m}");
        assert!(
            m.lines().any(|l| l.starts_with("mimic_requests_total{")
                && l.contains(r#"direction="outbound""#)
                && l.contains(r#"status="503""#)),
            "{case} : refus compté dans mimic_requests_total\n{m}"
        );
        assert!(
            !m.contains("mimic_mock_responses_total{"),
            "{case} : le refus ne passe pas par le moteur\n{m}"
        );
        // Rien n'a été échangé avec la dépendance : rien n'est enregistré.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            exchanges(&p, "direction=outbound").await.is_empty(),
            "{case}"
        );
    }

    // Hors mock, le réglage est sans effet : la poignée de main est relayée.
    for mode in [Mode::Record, Mode::Passthrough] {
        let server = echo_server().await;
        let mut c = ws_config(&server, mode);
        c.mock.websocket = MockWebsocket::Refuse;
        let p = proxy(c).await;
        let (ws, _) = outbound_ws(&p, &server, "/ws", &[]).await;
        echo_and_close(ws, Duration::ZERO).await;
        assert_eq!(server.connections(), 1, "{mode:?}");
        let m = metrics(&p).await;
        assert_eq!(ws_metric(&m, "outbound", "relayed"), 1.0, "{mode:?}\n{m}");
        assert_eq!(ws_metric(&m, "outbound", "refused"), 0.0, "{mode:?}\n{m}");
    }
}
