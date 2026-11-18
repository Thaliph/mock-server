//! RTE-46: the cost of `Table::select` over 100 rules (`cargo bench -p mimic-core --bench
//! mesh_route`); the budget is 10 µs for a request matching the last rule in precedence and for
//! one matching none (design §14). Building the table is measured, with no budget.
//! FLT-10 (group `mesh_fault`): the fault decision over 16 faults of 16 header items each, none
//! holding, at most 2 µs, and over no fault at most 50 ns. "None holding" is the worst case: 15
//! items of each fault hold and the last fails, so all 256 items are evaluated, on a request of
//! 20 lines whose names are in upper case. Measured with no budget: the cheap case, where the
//! request carries none of the headers and the first item of each fault fails, and the case
//! where no two faults share an item, so the request carries 244 lines.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use mimic_core::mesh::fault::Faults;
use mimic_core::mesh::policy::{Fault, FaultAbort};
use mimic_core::mesh::route::{
    MatchView, NameValue, PathView, RequestParts, RouteView, RuleView, Table,
};

fn nv(name: &str, value: &str) -> NameValue {
    NameValue {
        name: name.into(),
        value: value.into(),
    }
}

/// 10 routes of 10 rules; each match has a path (all of one length, so the last rule of the last
/// route is last in precedence), a method, 2 header items and 1 query item.
fn routes() -> Vec<RouteView> {
    (0..10)
        .map(|r| RouteView {
            name: format!("shop/r{r}"),
            ports: None,
            rules: (0..10)
                .map(|k| RuleView {
                    backends: vec![],
                    filters: None,
                    matches: vec![MatchView {
                        headers: vec![nv("x-canary", "1"), nv("x-tenant", "acme")],
                        method: Some("GET".into()),
                        path: Some(PathView {
                            kind: "PathPrefix".into(),
                            value: format!("/p{r}{k}"),
                        }),
                        query_params: vec![nv("v", "2")],
                    }],
                    name: None,
                    timeouts: None,
                })
                .collect(),
        })
        .collect()
}

fn select(c: &mut Criterion) {
    let routes = routes();
    let table = Table::new(&routes, 80);
    let headers: [(&str, &[u8]); 4] = [
        ("host", b"cart.shop"),
        ("x-canary", b"1"),
        ("x-tenant", b"acme"),
        ("accept", b"*/*"),
    ];
    let request = |path| RequestParts {
        method: "GET",
        path,
        query: Some("a=1&v=2&z=%20"),
        headers: &headers,
    };
    let last = request("/p99/items/1");
    assert_eq!(table.select(&last), Some((9, 9)));
    let none = request("/other");
    assert_eq!(table.select(&none), None);

    let mut group = c.benchmark_group("mesh_route");
    group.bench_function("select_last_100_rules", |b| {
        b.iter(|| black_box(table.select(black_box(&last))));
    });
    group.bench_function("select_none_100_rules", |b| {
        b.iter(|| black_box(table.select(black_box(&none))));
    });
    group.bench_function("build_100_rules", |b| {
        b.iter(|| black_box(Table::new(black_box(&routes), 80)));
    });
    group.finish();
}

/// An abort whose match asks for a method, a path prefix and 16 headers: `<prefix>0` to
/// `<prefix>14` equal `1`, and `<prefix>15` equal `last`.
fn fault(prefix: &str, last: &str) -> Fault {
    Fault {
        abort: Some(FaultAbort {
            percent: 10,
            status: 503,
        }),
        delay: None,
        r#match: Some(MatchView {
            headers: (0..16)
                .map(|i| nv(&format!("{prefix}{i}"), if i < 15 { "1" } else { last }))
                .collect(),
            method: Some("GET".into()),
            path: Some(PathView {
                kind: "PathPrefix".into(),
                value: "/shop".into(),
            }),
            query_params: vec![],
        }),
    }
}

fn faults(c: &mut Criterion) {
    let typical: [(&str, &[u8]); 4] = [
        ("host", b"cart.shop"),
        ("accept", b"*/*"),
        ("user-agent", b"bench"),
        ("content-type", b"application/json"),
    ];
    // The request holds the first 15 items of every fault and fails the last one.
    let shared = Faults::new(
        &(0..16)
            .map(|f| fault("x-h", &format!("f{f}")))
            .collect::<Vec<_>>(),
    );
    let names: Vec<String> = (0..16).map(|i| format!("X-H{i}")).collect();
    let mut worst = typical.to_vec();
    worst.extend(names.iter().map(|n| (n.as_str(), &b"1"[..])));
    // No item shared: fault `f` asks for its own names, and the request carries the 240 lines
    // of the items that hold.
    let distinct = Faults::new(
        &(0..16)
            .map(|f| fault(&format!("x-f{f}-h"), "-"))
            .collect::<Vec<_>>(),
    );
    let lines: Vec<String> = (0..16)
        .flat_map(|f| (0..15).map(move |i| format!("X-F{f}-H{i}")))
        .collect();
    let mut every = typical.to_vec();
    every.extend(lines.iter().map(|n| (n.as_str(), &b"1"[..])));
    let none = Faults::new(&[]);
    let request = |headers| RequestParts {
        method: "GET",
        path: "/shop/items",
        query: None,
        headers,
    };
    assert_eq!(shared.decide(&request(&worst), 0, 0), None);
    assert_eq!(shared.decide(&request(&typical), 0, 0), None);
    assert_eq!(distinct.decide(&request(&every), 0, 0), None);
    // With `x-h15: f0` in place of `X-H15: 1`, the first fault holds: the items are evaluated.
    let mut all = worst.clone();
    all[19] = ("x-h15", b"f0");
    assert_eq!(
        shared.decide(&request(&all), 0, 0).map(|d| d.index),
        Some(0)
    );

    let mut group = c.benchmark_group("mesh_fault");
    for (name, faults, headers) in [
        (
            "decide_16_faults_16_headers_none_holding",
            &shared,
            &worst[..],
        ),
        (
            "decide_16_faults_16_headers_first_item_fails",
            &shared,
            &typical[..],
        ),
        (
            "decide_16_faults_256_distinct_items_none_holding",
            &distinct,
            &every[..],
        ),
        ("decide_no_faults", &none, &typical[..]),
    ] {
        let r = request(headers);
        group.bench_function(name, |b| {
            b.iter(|| black_box(black_box(faults).decide(black_box(&r), 0, 0)));
        });
    }
    group.finish();
}

criterion_group!(benches, select, faults);
criterion_main!(benches);
