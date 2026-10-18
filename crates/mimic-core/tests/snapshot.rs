//! Critères SNP-01, SNP-02, SNP-06 et MCK-20 de la spec (§5.6) : construction des snapshots
//! de mock et de leur forme de mock.

mod common;

use common::{ExchangeExt, exchange};
use mimic_core::mock::{MockConfig, MockDecision, MockEngine, MockRequest, Strategy};
use mimic_core::model::{Body, Direction, Exchange, Header};
use mimic_core::snapshot::{MockSnapshot, Snapshot, SnapshotOptions, build};
use proptest::prelude::*;
use serde_json::json;

fn out(path: &str, at: u64, id: &str) -> Exchange {
    let mut e = exchange("GET", path).at(at, id);
    e.direction = Direction::Outbound;
    e.authority = "api.saas.io:443".into();
    e.resp_json(json!({ "id": id }))
}

#[test]
fn snp_01_same_exchanges_same_snapshot_whatever_the_order() {
    let a = vec![
        out("/a", 1, "x-1"),
        out("/b", 2, "y-1"),
        out("/a", 3, "x-2"),
    ];
    let mut b = a.clone();
    b.reverse();
    b.push(a[0].clone());
    let opts = SnapshotOptions::default();
    let (sa, sb) = (build(a, &opts), build(b, &opts));
    assert_eq!(sa.id, sb.id);
    assert_eq!(sa.exchanges, sb.exchanges);
    assert_eq!(sa.exchanges.len(), 3);
    assert_eq!(sa.id.len(), 16);
}

#[test]
fn snapshot_keeps_only_finished_outbound_exchanges() {
    let mut inbound = out("/in", 1, "i");
    inbound.direction = Direction::Inbound;
    let mut unfinished = out("/u", 2, "u");
    unfinished.status = 0;
    let s = build(
        vec![inbound, unfinished, out("/ok", 3, "o")],
        &SnapshotOptions::default(),
    );
    let ids: Vec<_> = s.exchanges.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["o"]);
}

#[test]
fn snp_02_keeps_the_most_recent_per_key() {
    let recs: Vec<_> = (0..10).map(|i| out("/a", i, &format!("{i:02}"))).collect();
    let s = build(recs, &SnapshotOptions { max_per_key: 3 });
    let ids: Vec<_> = s.exchanges.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["07", "08", "09"]);
}

#[test]
fn snp_06_replayable_examples_survive_more_recent_bodyless_ones() {
    // Comme un sidecar qui a gardé le body des 2 premiers appels seulement.
    let recs: Vec<_> = (0..10)
        .map(|i| {
            let mut e = out("/a", i, &format!("{i:02}"));
            if i >= 2 {
                e.resp_body = Body::Skipped { size: 9 };
            }
            e
        })
        .collect();
    let s = build(recs, &SnapshotOptions { max_per_key: 3 });
    let ids: Vec<_> = s.exchanges.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["00", "01", "07", "08", "09"]);

    let engine = MockEngine::new(s.exchanges, MockConfig::default());
    let req = MockRequest {
        authority: "api.saas.io:443".into(),
        method: "GET".into(),
        path: "/a".into(),
        ..MockRequest::default()
    };
    match engine.respond(&req) {
        MockDecision::Matched(r) => assert_eq!(r.exchange_id, "01"),
        MockDecision::Unmatched => panic!("l'endpoint doit rester servi"),
    }
}

#[test]
fn different_content_gives_different_id() {
    let opts = SnapshotOptions::default();
    let a = build(vec![out("/a", 1, "1")], &opts);
    let b = build(vec![out("/a", 1, "1").status(500)], &opts);
    assert_ne!(a.id, b.id);
}

#[test]
fn jsonl_roundtrip_checks_the_id() {
    let s = build(
        vec![out("/a", 1, "1"), out("/b", 2, "2")],
        &SnapshotOptions::default(),
    );
    let bytes = s.to_jsonl();
    assert_eq!(Snapshot::from_jsonl(&bytes, &s.id).unwrap(), s);
    let mut tampered = bytes.clone();
    let pos = tampered.iter().position(|b| *b == b'/').unwrap();
    tampered[pos + 1] = b'z';
    assert!(Snapshot::from_jsonl(&tampered, &s.id).is_err());
}

proptest! {
    #[test]
    fn snp_01_order_and_duplicates_do_not_matter(
        items in proptest::collection::vec((0u8..4, 0u64..20, 0u8..30), 0..40),
        seed in any::<u64>(),
    ) {
        let recs: Vec<Exchange> = items
            .iter()
            .map(|(p, at, id)| out(&format!("/p{p}"), *at, &format!("{id}-{p}-{at}")))
            .collect();
        let mut shuffled = recs.clone();
        // Permutation déterministe dérivée de la graine, avec doublons.
        let n = shuffled.len().max(1);
        shuffled.rotate_left((seed as usize) % n);
        if let Some(first) = recs.first() {
            shuffled.push(first.clone());
        }
        let opts = SnapshotOptions { max_per_key: 5 };
        // Le contrôleur construit sur des références (§5.5.2) : même snapshot.
        prop_assert_eq!(build(&shuffled, &opts), build(recs.clone(), &opts));
        prop_assert_eq!(build(recs, &opts), build(shuffled, &opts));
    }
}

/// Requête qui rejoue `e` : même authority, méthode, chemin, query, headers et body.
fn request_of(e: &Exchange) -> MockRequest {
    MockRequest {
        authority: e.authority.clone(),
        method: e.method.clone(),
        path: e.path.clone(),
        query: e.query.clone(),
        headers: e.req_headers.clone(),
        body: e.req_body.bytes().unwrap_or_default().to_vec(),
    }
}

/// Requêtes jamais enregistrées telles quelles : même forme qu'un enregistrement, ou aucune.
fn probes() -> Vec<MockRequest> {
    let mut out = vec![];
    for authority in ["api.saas.io:443", "stock.internal:8080"] {
        for method in ["GET", "post"] {
            for path in ["/users/97", "/users/2/cart", "/c/ch_zz", "/a", "/nope"] {
                for page in [None, Some("1")] {
                    out.push(MockRequest {
                        authority: authority.into(),
                        method: method.into(),
                        path: path.into(),
                        query: page
                            .map(|p| ("page".into(), p.into()))
                            .into_iter()
                            .collect(),
                        headers: vec![Header::new("accept", "a/1")],
                        body: br#"{"q":1}"#.to_vec(),
                    });
                }
            }
        }
    }
    out
}

/// Échange sortant varié : chemins de même forme (`/users/{n}`), de forte cardinalité
/// (`/c/ch_{n}`), deux authorities, codes `1xx` et d'erreur, bodies rejouables ou non.
fn varied(item: &(u8, u8, u8, u8, u16, u64), n: usize) -> Exchange {
    let (path, status, body, query, latency, at) = *item;
    let path = match path % 5 {
        0 => format!("/users/{}", path % 4),
        1 => format!("/c/ch_{n}"),
        2 => "/a".to_string(),
        3 => "/A".to_string(),
        _ => format!("/users/{}/cart", n % 3),
    };
    let mut e = out(&path, at, &format!("pod{}-{n:04}", n % 2));
    if n.is_multiple_of(3) {
        e.authority = "stock.internal:8080".into();
    }
    if n % 4 == 1 {
        e.method = "POST".into();
        e = e.req_json(json!({ "q": query % 2 }));
    }
    e.status = [101, 200, 200, 404, 500][status as usize % 5];
    e.resp_body = match body % 4 {
        0 => Body::captured(format!("{{\"v\":{}}}", body % 3)),
        1 => Body::Empty,
        2 => Body::Skipped { size: 12 },
        _ => Body::Truncated {
            size: 2_000_000,
            prefix: b"{\"big\":".to_vec(),
        },
    };
    if query % 3 != 0 {
        e.query = vec![("page".into(), (query % 3).to_string())];
    }
    e.req_headers
        .push(Header::new("accept", ["a/1", "a/2"][n % 2]));
    e.total_us = latency as u64;
    e
}

#[test]
fn mck_20_mock_form_keeps_replayable_records_and_latencies_only() {
    let mut recs = vec![];
    // Endpoint de forte cardinalité : 2 000 chemins distincts, deux seulement avec body.
    for i in 0..2_000u64 {
        let mut e = out(&format!("/v1/charges/ch_{i}"), i, &format!("a-{i:05}"));
        if i >= 2 {
            e.resp_body = Body::Skipped { size: 400 };
        }
        e.total_us = 1_000 + i;
        recs.push(e);
    }
    let mut ws = out("/ws", 5_000, "a-ws");
    ws.status = 101;
    ws.resp_body = Body::Empty;
    recs.push(ws);
    let full = build(recs, &SnapshotOptions::default());
    assert_eq!(full.exchanges.len(), 2_001);

    let light = MockSnapshot::from_snapshot(&full);
    assert_eq!(light.id, full.id);
    assert_eq!(light.count, 2_001);
    let ids: Vec<_> = light.records.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["a-00000", "a-00001"]);
    assert_eq!(
        light.records[0], full.exchanges[0],
        "échange rejouable tel quel"
    );
    // Une opération par forme de chemin, désignée par son premier échange, rangée sous son
    // authority et sa méthode ; le `101` n'en est pas une (le moteur l'ignore).
    assert_eq!(light.operations.len(), 1);
    let ops = &light.operations[0];
    assert_eq!(
        (ops.authority.as_str(), ops.method.as_str()),
        ("api.saas.io:443", "GET")
    );
    assert_eq!(
        ops.paths.len(),
        2_000,
        "chemins non reconnus comme identifiants"
    );
    let mut latencies: Vec<u64> = ops.paths.iter().flat_map(|p| p.1.iter().copied()).collect();
    latencies.sort_unstable();
    assert_eq!(
        latencies,
        (0..2_000).map(|i| 1_000 + i).collect::<Vec<u64>>()
    );

    let bytes = light.to_json();
    assert!(
        bytes.len() * 10 < full.to_jsonl().len(),
        "{} octets contre {}",
        bytes.len(),
        full.to_jsonl().len()
    );
    assert_eq!(MockSnapshot::from_json(&bytes, &full.id).unwrap(), light);
    // Un autre identifiant, ou un contenu altéré, est refusé.
    assert!(MockSnapshot::from_json(&bytes, "0000000000000000").is_err());
    assert!(MockSnapshot::from_json(&bytes[..bytes.len() - 2], &full.id).is_err());
}

#[test]
fn mck_20_paths_of_the_same_shape_share_one_operation() {
    let recs: Vec<_> = (0..100u64)
        .map(|i| {
            let mut e = out(&format!("/users/{i}"), i, &format!("u-{i:03}"));
            if i >= 20 {
                e.resp_body = Body::Skipped { size: 40 };
            }
            e.total_us = 10 * i;
            e
        })
        .collect();
    let full = build(recs, &SnapshotOptions::default());
    let light = MockSnapshot::from_snapshot(&full);
    assert_eq!(light.records.len(), 20);
    assert_eq!(light.operations.len(), 1);
    let ops = &light.operations[0];
    assert_eq!(ops.paths.len(), 1);
    let op = &ops.paths[0];
    assert_eq!(op.0, "/users/0", "désignée par son premier échange");
    assert_eq!(op.1, (0..100).map(|i| 10 * i).collect::<Vec<u64>>());
}

proptest! {
    #[test]
    fn mck_20_engine_on_the_mock_form_is_the_engine_on_the_snapshot(
        items in proptest::collection::vec(
            (0u8..10, 0u8..5, 0u8..4, 0u8..6, 0u16..5_000, 0u64..40), 0..60),
        max_per_key in 1usize..4,
        seed in any::<u64>(),
        rotate in any::<usize>(),
    ) {
        let recs: Vec<Exchange> = items.iter().enumerate().map(|(n, it)| varied(it, n)).collect();
        let opts = SnapshotOptions { max_per_key };
        let full = build(&recs, &opts);
        let light = MockSnapshot::from_snapshot(&full);
        let bytes = light.to_json();
        let relit = MockSnapshot::from_json(&bytes, &full.id).unwrap();
        prop_assert_eq!(&relit, &light);
        prop_assert!(relit.records.iter().all(|e| e.resp_body.is_replayable() && e.status >= 200));

        // Mêmes octets quel que soit l'ordre d'arrivée des échanges.
        let mut shuffled = recs.clone();
        shuffled.rotate_left(rotate % recs.len().max(1));
        prop_assert_eq!(MockSnapshot::from_snapshot(&build(&shuffled, &opts)).to_json(), bytes);

        let mut requests: Vec<MockRequest> = recs.iter().map(request_of).collect();
        requests.extend(probes());
        for strategy in [
            Strategy::Best,
            Strategy::RoundRobin,
            Strategy::Sequence,
            Strategy::WeightedStatus,
        ] {
            let config = MockConfig {
                strategy,
                seed,
                match_headers: vec!["accept".into()],
                ..MockConfig::default()
            };
            let a = MockEngine::new(full.exchanges.iter(), config.clone());
            let b = MockEngine::from_snapshot(&relit, config);
            prop_assert_eq!(a.len(), b.len());
            prop_assert_eq!(a.replayable(), b.replayable());
            prop_assert_eq!(b.replayable(), relit.records.len());
            prop_assert_eq!(a.authorities(), b.authorities());
            // Deux passes : les curseurs de `round-robin` et `sequence` avancent pareil.
            for req in requests.iter().chain(requests.iter()) {
                prop_assert_eq!(a.candidates(req), b.candidates(req));
                prop_assert_eq!(a.respond(req), b.respond(req), "{:?} {:?}", strategy, req);
            }
        }
    }
}
