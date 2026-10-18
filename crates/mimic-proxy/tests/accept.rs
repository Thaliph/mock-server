//! Critère PRX-10 de la spec (§9.3) : plus de descripteurs (EMFILE) sous un tir.
//!
//! Seul test de ce binaire, car il abaisse la limite de descripteurs de tout le processus.

mod common;

use std::io::{Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::time::Duration;

use common::*;
use mimic_proxy::config::Mode;
use nix::sys::resource::{Resource, getrlimit, setrlimit};

/// Temps CPU du processus (utime + stime), en ticks d'horloge.
fn cpu_ticks(stat: &mut std::fs::File) -> u64 {
    let mut text = String::new();
    stat.rewind().unwrap();
    stat.read_to_string(&mut text).unwrap();
    let after = text.rsplit_once(')').unwrap().1;
    let f: Vec<&str> = after.split_whitespace().collect();
    f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prx_10_accept_without_descriptors_does_not_spin() {
    let p = proxy(config(None, Mode::Record)).await;
    // Ouvert avant d'abaisser la limite : le relire ne demande plus de descripteur.
    let mut stat = std::fs::File::open("/proc/self/stat").unwrap();
    // USER_HZ, fixe à 100 pour /proc sous Linux.
    let ticks_per_sec = 100;

    // Plus petit descripteur libre : tous ceux en dessous sont pris.
    let free = std::fs::File::open("/dev/null").unwrap().as_raw_fd() as u64;
    let (soft, hard) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();
    setrlimit(Resource::RLIMIT_NOFILE, free + 1, hard).unwrap();
    // Le client prend le dernier descripteur : l'accept du sidecar échoue en EMFILE.
    let connected = std::net::TcpStream::connect(p.admin_addr());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let before = cpu_ticks(&mut stat);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let spent = cpu_ticks(&mut stat) - before;
    setrlimit(Resource::RLIMIT_NOFILE, soft, hard).unwrap();

    let mut stream = connected.unwrap();
    assert!(
        spent * 1000 / ticks_per_sec < 200,
        "{} ms de CPU en 1 s d'EMFILE : boucle d'accept à vide",
        spent * 1000 / ticks_per_sec
    );
    // L'erreur cessée, la connexion en attente est servie.
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nhost: mimic\r\nconnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
}
