//! Relevé des requêtes sans réponse du mock (spec §5.3, §9.2) : bornes, ordre, accès
//! réservé à la boucle locale et remise à zéro au tir suivant (ADM-07, MCK-27).

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use mimic_core::snapshot::{MockSnapshot, Snapshot};
use mimic_proxy::Running;
use mimic_proxy::config::Mode;
use serde_json::Value;

async fn unmatched(p: &Running, query: &str) -> Value {
    let r = client()
        .get(admin(p, &format!("/api/unmatched{query}")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

#[tokio::test]
async fn adm_07_listing_is_bounded_ordered_and_local_only() {
    let mut c = config(None, Mode::Mock);
    c.admin.listen = "0.0.0.0:0".parse().unwrap();
    let p = proxy(c).await;
    let app = app_client(&p);
    // Par lots de 10, séparés de quelques ms : les lots ont des horodatages distincts.
    for n in 0..150 {
        let r = app
            .get(format!("http://api.saas.io/p{n}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 599);
        if n % 10 == 9 {
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    }
    let port = p.admin_addr().port();
    let url = format!("http://127.0.0.1:{port}/api/unmatched");
    let report: Value = client()
        .get(&url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(report["total"], 150);
    let entries = report["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 100);
    let last: Vec<u64> = entries
        .iter()
        .map(|e| e["last_ms"].as_u64().unwrap())
        .collect();
    assert!(last.windows(2).all(|w| w[0] >= w[1]), "{last:?}");
    let mut kept: Vec<String> = entries
        .iter()
        .map(|e| e["path"].as_str().unwrap().to_string())
        .collect();
    kept.sort();
    let mut want: Vec<String> = (50..150).map(|n| format!("/p{n}")).collect();
    want.sort();
    assert_eq!(kept, want, "les 100 clés les plus récentes");

    let five: Value = client()
        .get(format!("{url}?limit=5"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(five["total"], 150);
    assert_eq!(five["entries"].as_array().unwrap()[..], entries[..5]);

    let remote = reqwest::Client::builder()
        .no_proxy()
        .local_address(std::net::IpAddr::from([127, 0, 0, 6]))
        .build()
        .unwrap();
    assert_eq!(remote.get(&url).send().await.unwrap().status(), 403);
}

#[tokio::test]
async fn mck_27_listing_survives_record_and_is_reset_by_the_next_mock() {
    let saas = fake_server().await;
    let p = proxy(config(None, Mode::Mock)).await;
    let app = app_client(&p);
    let refused = |path: &str| {
        let (app, url) = (app.clone(), saas.url(path));
        async move { app.get(url).send().await.unwrap().status().as_u16() }
    };
    assert_eq!(refused("/a/1").await, 599);
    assert_eq!(unmatched(&p, "").await["total"], 1);

    // En record, le relevé du tir reste lisible.
    p.state().set_mode(Mode::Record);
    assert_eq!(unmatched(&p, "").await["total"], 1);
    // Retour en mock : nouveau tir, relevé vide.
    p.state().set_mode(Mode::Mock);
    assert_eq!(unmatched(&p, "").await["total"], 0);
    assert_eq!(refused("/b/2").await, 599);
    // Même mode : rien n'est remis à zéro.
    p.state().set_mode(Mode::Mock);
    assert_eq!(unmatched(&p, "").await["total"], 1);

    // Un autre snapshot : relevé vide.
    let snap = |id: &str| {
        Some(Arc::new(MockSnapshot::from_snapshot(&Snapshot {
            id: id.into(),
            exchanges: vec![],
        })))
    };
    p.state().set_snapshot(snap("s1"));
    assert_eq!(unmatched(&p, "").await["total"], 0);
    assert_eq!(refused("/c/3").await, 599);
    p.state().set_snapshot(snap("s1"));
    assert_eq!(unmatched(&p, "").await["total"], 1, "même snapshot");
    p.state().set_snapshot(snap("s2"));
    assert_eq!(unmatched(&p, "").await["total"], 0);

    // mock-partial : la requête relayée a reçu la réponse de la dépendance.
    p.state().set_mode(Mode::MockPartial);
    p.state().set_snapshot(None);
    assert_eq!(refused("/users/3").await, 200);
    let report = unmatched(&p, "").await;
    assert_eq!(report["total"], 0, "{report}");
}
