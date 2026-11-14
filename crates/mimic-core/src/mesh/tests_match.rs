//! RTE-40 to RTE-42: route matching and precedence (spec §7.9 "Routing"), `Table::select`.

use super::route::{
    MatchView, NameValue, PathView, RequestParts, RouteView, RuleView, Table, split_target,
};

fn nv(items: &[(&str, &str)]) -> Vec<NameValue> {
    items
        .iter()
        .map(|(n, v)| NameValue {
            name: (*n).into(),
            value: (*v).into(),
        })
        .collect()
}

/// A match of path `(type, value)`, method, headers and query items.
fn m(
    path: Option<(&str, &str)>,
    method: Option<&str>,
    headers: &[(&str, &str)],
    query: &[(&str, &str)],
) -> MatchView {
    MatchView {
        headers: nv(headers),
        method: method.map(Into::into),
        path: path.map(|(kind, value)| PathView {
            kind: kind.into(),
            value: value.into(),
        }),
        query_params: nv(query),
    }
}

fn prefix(value: &str) -> MatchView {
    m(Some(("PathPrefix", value)), None, &[], &[])
}

fn exact(value: &str) -> MatchView {
    m(Some(("Exact", value)), None, &[], &[])
}

fn rule(matches: Vec<MatchView>) -> RuleView {
    RuleView {
        backends: vec![],
        filters: None,
        matches,
        name: None,
        timeouts: None,
    }
}

fn route(name: &str, ports: Option<Vec<u16>>, rules: Vec<RuleView>) -> RouteView {
    RouteView {
        name: name.into(),
        ports,
        rules,
    }
}

/// One route of one rule per match, in order.
fn one(rules: Vec<RuleView>) -> Vec<RouteView> {
    vec![route("shop/r", None, rules)]
}

fn select(
    routes: &[RouteView],
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
) -> Option<(usize, usize)> {
    let table = Table::new(routes, 80);
    let (path, query) = split_target(target);
    let headers: Vec<(&str, &[u8])> = headers.iter().map(|(n, v)| (*n, v.as_bytes())).collect();
    table.select(&RequestParts {
        method,
        path,
        query,
        headers: &headers,
    })
}

fn get(routes: &[RouteView], target: &str) -> Option<(usize, usize)> {
    select(routes, "GET", target, &[])
}

// ---- RTE-40: precedence

#[test]
fn rte_40_an_exact_path_beats_a_longer_prefix() {
    let routes = one(vec![prefix("/cart/"), exact("/cart")]
        .into_iter()
        .map(|x| rule(vec![x]))
        .collect());
    assert_eq!(get(&routes, "/cart"), Some((0, 1)));
}

#[test]
fn rte_40_a_longer_prefix_beats_a_shorter_one_lengths_as_written() {
    let routes = one(vec![
        rule(vec![prefix("/cart")]),
        rule(vec![prefix("/cart/items")]),
    ]);
    assert_eq!(get(&routes, "/cart/items/1"), Some((0, 1)));
    assert_eq!(get(&routes, "/cart/other"), Some((0, 0)));
    let routes = one(vec![
        rule(vec![prefix("/cart")]),
        rule(vec![prefix("/cart/")]),
    ]);
    assert_eq!(get(&routes, "/cart/1"), Some((0, 1)));
}

#[test]
fn rte_40_a_method_then_more_headers_then_more_query_items() {
    let with = |method: Option<&str>, headers: &[(&str, &str)], query: &[(&str, &str)]| {
        rule(vec![m(
            Some(("PathPrefix", "/cart")),
            method,
            headers,
            query,
        )])
    };
    // method over none, whatever the others
    let routes = one(vec![
        with(None, &[("x-a", "1"), ("x-b", "1")], &[]),
        with(Some("GET"), &[], &[]),
    ]);
    assert_eq!(
        select(&routes, "GET", "/cart", &[("x-a", "1"), ("x-b", "1")]),
        Some((0, 1))
    );
    // more headers
    let routes = one(vec![
        with(None, &[("x-a", "1")], &[("q", "1"), ("r", "1")]),
        with(None, &[("x-a", "1"), ("x-b", "1")], &[]),
    ]);
    assert_eq!(
        select(
            &routes,
            "GET",
            "/cart?q=1&r=1",
            &[("x-a", "1"), ("x-b", "1")]
        ),
        Some((0, 1))
    );
    // more query items
    let routes = one(vec![
        with(None, &[], &[("q", "1")]),
        with(None, &[], &[("q", "1"), ("r", "1")]),
    ]);
    assert_eq!(get(&routes, "/cart?q=1&r=1"), Some((0, 1)));
    // an item that fails takes the match out
    assert_eq!(get(&routes, "/cart?q=1"), Some((0, 0)));
}

#[test]
fn rte_40_path_ranks_above_method_and_headers() {
    let routes = one(vec![
        rule(vec![m(
            Some(("PathPrefix", "/cart")),
            Some("GET"),
            &[("x-a", "1")],
            &[("q", "1")],
        )]),
        rule(vec![exact("/cart")]),
    ]);
    assert_eq!(
        select(&routes, "GET", "/cart?q=1", &[("x-a", "1")]),
        Some((0, 1))
    );
}

#[test]
fn rte_40_ties_go_to_the_older_route_then_the_first_rule() {
    let routes = vec![
        route("shop/a", None, vec![rule(vec![prefix("/cart")])]),
        route("shop/b", None, vec![rule(vec![prefix("/cart")])]),
    ];
    assert_eq!(get(&routes, "/cart"), Some((0, 0)));
    let routes = one(vec![
        rule(vec![prefix("/cart")]),
        rule(vec![prefix("/cart")]),
    ]);
    assert_eq!(get(&routes, "/cart"), Some((0, 0)));
    // precedence is per match, over every rule: the second match of rule 0 wins
    let routes = one(vec![
        rule(vec![prefix("/"), exact("/cart")]),
        rule(vec![prefix("/cart")]),
    ]);
    assert_eq!(get(&routes, "/cart"), Some((0, 0)));
    // a later route with a better match beats an older route
    let routes = vec![
        route("shop/a", None, vec![rule(vec![prefix("/")])]),
        route("shop/b", None, vec![rule(vec![prefix("/cart")])]),
    ];
    assert_eq!(get(&routes, "/cart"), Some((1, 0)));
}

#[test]
fn rte_40_two_header_items_of_one_name_count_one_and_the_second_is_ignored() {
    let routes = vec![
        route(
            "shop/a",
            None,
            vec![rule(vec![m(None, None, &[("x-b", "1")], &[])])],
        ),
        route(
            "shop/b",
            None,
            vec![rule(vec![m(
                None,
                None,
                &[("x-a", "1"), ("x-a", "2")],
                &[],
            )])],
        ),
    ];
    // counted once each: a tie, the older route wins (two would make b win)
    assert_eq!(
        select(&routes, "GET", "/", &[("x-a", "1"), ("x-b", "1")]),
        Some((0, 0))
    );
    // the second item (`x-a: 2`) is ignored: a request failing it still matches
    assert_eq!(select(&routes, "GET", "/", &[("x-a", "1")]), Some((1, 0)));
    // the first one is not
    assert_eq!(select(&routes, "GET", "/", &[("x-a", "2")]), None);
}

#[test]
fn rte_40_ports_and_empty_tables() {
    let routes = vec![
        route("shop/a", Some(vec![81]), vec![rule(vec![prefix("/")])]),
        route(
            "shop/b",
            Some(vec![80, 82]),
            vec![rule(vec![prefix("/cart")])],
        ),
        route("shop/c", None, vec![rule(vec![exact("/x")])]),
    ];
    // route a takes no part on 80
    assert_eq!(get(&routes, "/cart"), Some((1, 0)));
    assert_eq!(get(&routes, "/x"), Some((2, 0)));
    assert_eq!(get(&routes, "/other"), None);
    assert!(!Table::new(&routes, 80).is_empty());
    let only81 = vec![routes[0].clone()];
    let table = Table::new(&only81, 80);
    assert!(table.is_empty(), "no route on the port is `no routes`");
    assert_eq!(table.select(&RequestParts::default()), None);
    assert!(Table::new(&[], 80).is_empty());
    // a route of the port without rules is a route: not empty, nothing matches
    let ruleless = vec![route("shop/a", None, vec![])];
    assert!(!Table::new(&ruleless, 80).is_empty());
    assert_eq!(get(&ruleless, "/"), None);
}

// ---- RTE-41: paths

#[test]
fn rte_41_prefix_matches() {
    for value in ["/cart", "/cart/"] {
        let routes = one(vec![rule(vec![prefix(value)])]);
        for path in ["/cart", "/cart/", "/cart/1"] {
            assert_eq!(get(&routes, path), Some((0, 0)), "{value} on {path}");
        }
        for path in ["/carts", "/Cart", "/car", "/"] {
            assert_eq!(get(&routes, path), None, "{value} on {path}");
        }
    }
    let routes = one(vec![rule(vec![prefix("/")])]);
    for path in ["/", "/a", "/a/b/", "//"] {
        assert_eq!(get(&routes, path), Some((0, 0)), "/ on {path}");
    }
}

#[test]
fn rte_41_exact_matches_the_path_only() {
    let routes = one(vec![rule(vec![exact("/cart")])]);
    assert_eq!(get(&routes, "/cart"), Some((0, 0)));
    assert_eq!(get(&routes, "/cart?x=1"), Some((0, 0)));
    assert_eq!(get(&routes, "/cart/"), None);
    assert_eq!(get(&routes, "/cart/1"), None);
    assert_eq!(get(&routes, "/Cart"), None);
}

#[test]
fn rte_41_targets_split_into_path_and_query() {
    assert_eq!(split_target("/cart?x=1&y"), ("/cart", Some("x=1&y")));
    assert_eq!(split_target("/cart"), ("/cart", None));
    assert_eq!(split_target("/cart?"), ("/cart", Some("")));
    assert_eq!(
        split_target("http://shop:80/cart/1?q=2"),
        ("/cart/1", Some("q=2"))
    );
    assert_eq!(split_target("http://shop"), ("", None));
    assert_eq!(split_target("http://shop?q=1"), ("", Some("q=1")));
    assert_eq!(split_target("*"), ("*", None));
    assert_eq!(split_target(""), ("", None));
}

#[test]
fn rte_41_paths_are_compared_as_received() {
    let cart = one(vec![rule(vec![prefix("/cart")])]);
    assert_eq!(get(&cart, "/cart/../admin"), Some((0, 0)));
    assert_eq!(get(&cart, "//cart"), None);
    assert_eq!(get(&cart, "/%63art"), None);
    assert_eq!(get(&cart, "/cart%2F1"), None);
    let encoded = one(vec![rule(vec![exact("/%63art")])]);
    assert_eq!(get(&encoded, "/%63art"), Some((0, 0)));
    assert_eq!(get(&encoded, "/cart"), None);
}

#[test]
fn rte_41_star_and_empty_paths_match_no_path_item() {
    let routes = one(vec![
        rule(vec![prefix("/")]),
        rule(vec![exact("/")]),
        rule(vec![exact("*")]),
    ]);
    assert_eq!(get(&routes, "*"), None);
    assert_eq!(get(&routes, ""), None);
    assert_eq!(get(&routes, "http://shop"), None);
    // a match without a path item does not look at the path
    let routes = one(vec![rule(vec![m(None, Some("GET"), &[], &[])])]);
    assert_eq!(get(&routes, "*"), Some((0, 0)));
    assert_eq!(get(&routes, "/anything"), Some((0, 0)));
}

// ---- RTE-42: headers, query, method

fn hdr(items: &[(&str, &str)], req: &[(&str, &str)]) -> Option<(usize, usize)> {
    let routes = one(vec![rule(vec![m(None, None, items, &[])])]);
    select(&routes, "GET", "/", req)
}

#[test]
fn rte_42_header_names_in_any_case_values_exactly() {
    assert_eq!(
        hdr(&[("x-canary", "1")], &[("X-Canary", "1")]),
        Some((0, 0))
    );
    assert_eq!(
        hdr(&[("x-canary", "1")], &[("x-canary", "1")]),
        Some((0, 0))
    );
    assert_eq!(hdr(&[("x-canary", "abc")], &[("x-canary", "ABC")]), None);
    assert_eq!(hdr(&[("x-canary", "1")], &[("x-canary", " 1")]), None);
    assert_eq!(hdr(&[("x-canary", "1")], &[("x-canary", "11")]), None);
    assert_eq!(hdr(&[("x-canary", "1")], &[("x-canary", "")]), None);
    assert_eq!(hdr(&[("x-canary", "")], &[("x-canary", "")]), Some((0, 0)));
    // a missing header fails the item
    assert_eq!(hdr(&[("x-canary", "1")], &[("x-other", "1")]), None);
    assert_eq!(hdr(&[("x-canary", "1")], &[]), None);
}

#[test]
fn rte_42_repeated_lines_match_as_joined_with_a_comma() {
    let two = [("x-l", "a"), ("X-L", "b")];
    assert_eq!(hdr(&[("x-l", "a,b")], &two), Some((0, 0)));
    assert_eq!(hdr(&[("x-l", "a")], &two), None);
    assert_eq!(hdr(&[("x-l", "b")], &two), None);
    assert_eq!(hdr(&[("x-l", "a, b")], &two), None);
    // other names in between do not matter, the order received does
    let mixed = [("x-l", "a"), ("x-m", "z"), ("x-l", "b")];
    assert_eq!(hdr(&[("x-l", "a,b")], &mixed), Some((0, 0)));
    assert_eq!(hdr(&[("x-l", "b,a")], &mixed), None);
    // a header value that is not UTF-8 never equals a match value
    let routes = one(vec![rule(vec![m(None, None, &[("x-l", "é")], &[])])]);
    let table = Table::new(&routes, 80);
    let headers: [(&str, &[u8]); 1] = [("x-l", &[0xff, 0xfe])];
    let parts = RequestParts {
        method: "GET",
        path: "/",
        query: None,
        headers: &headers,
    };
    assert_eq!(table.select(&parts), None);
    let headers: [(&str, &[u8]); 1] = [("x-l", "é".as_bytes())];
    let parts = RequestParts {
        headers: &headers,
        ..parts
    };
    assert_eq!(table.select(&parts), Some((0, 0)));
}

fn qry(items: &[(&str, &str)], query: &str) -> Option<(usize, usize)> {
    let routes = one(vec![rule(vec![m(None, None, &[], items)])]);
    get(&routes, &format!("/?{query}"))
}

#[test]
fn rte_42_query_first_pair_wins_and_values_are_decoded() {
    assert_eq!(qry(&[("v", "2")], "v=2&v=3"), Some((0, 0)));
    assert_eq!(qry(&[("v", "3")], "v=2&v=3"), None);
    assert_eq!(qry(&[("q", "a b")], "q=a%20b"), Some((0, 0)));
    assert_eq!(qry(&[("q", "a+b")], "q=a+b"), Some((0, 0)));
    assert_eq!(qry(&[("q", "a b")], "q=a+b"), None);
    assert_eq!(qry(&[("x", "")], "x"), Some((0, 0)));
    assert_eq!(qry(&[("x", "")], "x="), Some((0, 0)));
    assert_eq!(qry(&[("x", "1")], "x"), None);
    assert_eq!(qry(&[("x", "")], "y=1"), None);
    assert_eq!(qry(&[("x", "")], ""), None);
    // a value holding `=` is split on the first one
    assert_eq!(qry(&[("a", "b=c")], "a=b=c"), Some((0, 0)));
    // the name is decoded too, and compared with case
    assert_eq!(qry(&[("a b", "1")], "a%20b=1"), Some((0, 0)));
    assert_eq!(qry(&[("v", "1")], "V=1"), None);
    // a decoded value may be anything, including `&` and `%`
    assert_eq!(qry(&[("q", "a&b%")], "q=a%26b%25"), Some((0, 0)));
}

#[test]
fn rte_42_a_pair_with_an_invalid_escape_is_ignored() {
    assert_eq!(qry(&[("q", "1")], "q=%zz&q=1"), Some((0, 0)));
    assert_eq!(qry(&[("q", "%zz")], "q=%zz"), None);
    assert_eq!(qry(&[("q", "1")], "q=%zz"), None);
    assert_eq!(qry(&[("q", "1")], "q=1%"), None);
    assert_eq!(qry(&[("q", "1")], "q=1%4"), None);
    // an invalid value after a mismatch is still invalid, not a mismatch of the first pair
    assert_eq!(qry(&[("q", "2")], "q=1%zz&q=2"), Some((0, 0)));
    assert_eq!(qry(&[("q", "1")], "q=2%zz&q=1"), Some((0, 0)));
    // a pair of another name with a bad escape is nothing to us
    assert_eq!(qry(&[("q", "1")], "%zz=2&q=1"), Some((0, 0)));
}

#[test]
fn rte_42_method_is_exact() {
    let routes = one(vec![rule(vec![m(None, Some("GET"), &[], &[])])]);
    assert_eq!(select(&routes, "GET", "/", &[]), Some((0, 0)));
    assert_eq!(select(&routes, "get", "/", &[]), None);
    assert_eq!(select(&routes, "HEAD", "/", &[]), None);
    assert_eq!(select(&routes, "POST", "/", &[]), None);
}
