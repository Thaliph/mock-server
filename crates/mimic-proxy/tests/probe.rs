//! Sonde `exec` du sidecar dans un pod du mesh (spec §3.3.4, PRX-18).

mod common;

use std::time::{Duration, Instant};

use common::*;
use mimic_proxy::config::Mode;
use mimic_proxy::probe;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prx_18_probe_queries_the_admin_api_and_fails_fast_when_it_is_gone() {
    let mut c = config(None, Mode::Record);
    let p = proxy(c.clone()).await;
    // Même configuration que le sidecar : seul le port lié au démarrage change ici.
    c.admin.listen = p.admin_addr();
    let cfg = c.clone();
    let ok = tokio::task::spawn_blocking(move || probe(&cfg, "/healthz"))
        .await
        .unwrap();
    assert_eq!(ok, Ok(()));
    let cfg = c.clone();
    let missing = tokio::task::spawn_blocking(move || probe(&cfg, "/absent"))
        .await
        .unwrap();
    assert!(missing.is_err(), "404 : {missing:?}");

    p.shutdown().await;
    let started = Instant::now();
    let gone = tokio::task::spawn_blocking(move || probe(&c, "/healthz"))
        .await
        .unwrap();
    let err = gone.expect_err("sidecar arrêté");
    assert!(!err.is_empty());
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn prx_18_unspecified_admin_address_is_probed_on_loopback() {
    let mut c = config(None, Mode::Record);
    c.admin.listen = "0.0.0.0:15100".parse().unwrap();
    assert_eq!(
        mimic_proxy::probe_addr(&c),
        "127.0.0.1:15100".parse().unwrap()
    );
    c.admin.listen = "[::]:15100".parse().unwrap();
    assert_eq!(mimic_proxy::probe_addr(&c), "[::1]:15100".parse().unwrap());
}
