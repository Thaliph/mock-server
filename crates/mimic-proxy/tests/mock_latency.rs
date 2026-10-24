//! Critère MCK-15 (spec §5.3 et §11.2) : latence rejouée fidèle, comptée depuis l'arrivée de la
//! requête. Seul test de ce binaire : aucune autre charge ne fausse les durées mesurées.

mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use common::*;
use mimic_core::model::{Direction, Exchange};
use mimic_proxy::Running;
use mimic_proxy::config::Mode;
use mimic_proxy::store::Origin;
use tokio::net::TcpListener;

/// Dépendance réelle : `/fast` répond aussitôt, `/slow` après environ 3,5 ms.
async fn dependency() -> SocketAddr {
    let app = Router::new()
        .route(
            "/fast",
            get(|| async { axum::Json(serde_json::json!({"ok": true})) }),
        )
        .route(
            "/slow",
            get(|| async {
                // Attente précise (le temporisateur de tokio arrondit à la milliseconde) :
                // la dépendance garde la même distribution d'un bout à l'autre du test.
                let wait = || std::thread::sleep(Duration::from_micros(3_300));
                tokio::task::spawn_blocking(wait).await.unwrap();
                axum::Json(serde_json::json!({"ok": true}))
            }),
        );
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

/// Durée d'un appel vu par l'application, en microsecondes.
async fn call(app: &reqwest::Client, url: &str) -> u64 {
    let t = Instant::now();
    let r = app.get(url).send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    t.elapsed().as_micros() as u64
}

fn p50(mut v: Vec<u64>) -> u64 {
    v.sort_unstable();
    v[v.len() / 2]
}

/// Sidecar en mock sur les enregistrements de `from`.
async fn replaying(from: &Running, latency_factor: f64) -> Running {
    let mut c = config(None, Mode::Record);
    c.mock.latency_factor = latency_factor;
    let p = proxy(c).await;
    for e in from.state().store.shared_direction(Direction::Outbound) {
        p.state().store.add(Exchange::clone(&e), Origin::Peer);
    }
    set_mode(&p, "mock").await;
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mck_15_replayed_latency_matches_the_dependency() {
    let dep = dependency().await;
    let paths = [format!("http://{dep}/slow"), format!("http://{dep}/fast")];
    let record = proxy(config(None, Mode::Record)).await;
    let via_record = app_client(&record);
    for url in &paths {
        for _ in 0..100 {
            call(&via_record, url).await;
        }
    }
    wait_exchanges(&record, "direction=outbound&limit=1000", 200).await;
    let mock = replaying(&record, 1.0).await;
    let none = replaying(&record, 0.0).await;
    let (via_mock, via_none) = (app_client(&mock), app_client(&none));

    // Appels entrelacés : la dépendance réelle (via le sidecar en record) et ses rejeux
    // subissent les mêmes aléas de la machine.
    let mut seen: [[Vec<u64>; 2]; 2] = Default::default();
    let mut without = vec![];
    for _ in 0..300 {
        for (i, url) in paths.iter().enumerate() {
            seen[i][0].push(call(&via_record, url).await);
            seen[i][1].push(call(&via_mock, url).await);
        }
        without.push(call(&via_none, &paths[0]).await);
    }
    let [[rec_slow, mock_slow], [rec_fast, mock_fast]] = seen.map(|s| s.map(p50));
    let without = p50(without);
    let stored = |path: &str| {
        p50(record
            .state()
            .store
            .shared_direction(Direction::Outbound)
            .iter()
            .filter(|e| e.path == path)
            .map(|e| e.total_us)
            .collect())
    };
    let stored_slow = stored("/slow");
    eprintln!("enregistré : {stored_slow} / {} µs", stored("/fast"));
    eprintln!(
        "p50 vu par l'application : dépendance {rec_slow} / {rec_fast} µs, rejouée {mock_slow} / {mock_fast} µs, sans latence {without} µs sur /slow"
    );

    let ratio = mock_slow as f64 / rec_slow as f64;
    assert!(
        (0.9..=1.1).contains(&ratio),
        "dépendance de {rec_slow} µs rejouée en {mock_slow} µs"
    );
    // Sans latence, seul reste le coût du sidecar, celui du rejeu avec latence : un seuil
    // absolu dépendrait de la machine et du profil de compilation.
    assert!(
        without + stored_slow / 2 <= mock_slow,
        "sans latence : {without} µs, rejeu avec latence de {stored_slow} µs : {mock_slow} µs"
    );
    assert!(
        mock_fast <= rec_fast + rec_fast / 2,
        "dépendance de {rec_fast} µs rejouée en {mock_fast} µs"
    );
}
