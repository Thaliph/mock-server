//! RTE-46: the cost of `Table::select` over 100 rules (`cargo bench -p mimic-core --bench
//! mesh_route`); the budget is 10 µs for a request matching the last rule in precedence and for
//! one matching none (design §14). Building the table is measured, with no budget.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
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

criterion_group!(benches, select);
criterion_main!(benches);
