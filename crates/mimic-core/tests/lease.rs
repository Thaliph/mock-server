//! Critère CTL-24 de la spec (§5.5.4) : élection de la réplique active du contrôleur par un
//! bail, au niveau de `mimic-core` (fonctions pures).

use std::collections::BTreeMap;

use mimic_core::lease::{
    Decision, Lease, Observed, Record, Timings, decide, observe, release, still_leading,
};
use proptest::prelude::*;

fn t() -> Timings {
    Timings::default()
}

fn lease(holder: Option<&str>, version: &str, transitions: u32) -> Lease {
    Lease {
        record: Record {
            holder: holder.map(String::from),
            duration_ms: 15_000,
            transitions,
        },
        version: version.into(),
    }
}

fn seen(version: &str, since_ms: u64) -> Observed {
    Observed {
        version: version.into(),
        since_ms,
    }
}

#[test]
fn ctl_24_absent_lease_is_created() {
    assert_eq!(
        decide("a", None, None, &t(), 0),
        Decision::Write {
            record: Record {
                holder: Some("a".into()),
                duration_ms: 15_000,
                transitions: 0,
            },
            expected: None,
            acquire: true,
        }
    );
}

#[test]
fn ctl_24_released_lease_is_taken_at_once() {
    let l = lease(None, "7", 3);
    let o = observe(None, Some(&l), 1_000);
    assert_eq!(
        decide("b", Some(&l), o.as_ref(), &t(), 1_000),
        Decision::Write {
            record: Record {
                holder: Some("b".into()),
                duration_ms: 15_000,
                transitions: 4,
            },
            expected: Some("7".into()),
            acquire: true,
        }
    );
}

#[test]
fn ctl_24_lease_expires_on_the_local_clock_only() {
    let l = lease(Some("a"), "7", 2);
    // Vue pour la première fois à 5 000 : expirée à 20 000, pas avant.
    let o = observe(None, Some(&l), 5_000);
    assert_eq!(o, Some(seen("7", 5_000)));
    let o = observe(o, Some(&l), 19_999);
    assert_eq!(o, Some(seen("7", 5_000)), "même version : même date");
    assert_eq!(
        decide("b", Some(&l), o.as_ref(), &t(), 19_999),
        Decision::Wait {
            holder: Some("a".into())
        }
    );
    match decide("b", Some(&l), o.as_ref(), &t(), 20_000) {
        Decision::Write {
            record,
            expected,
            acquire,
        } => {
            assert_eq!(record.holder.as_deref(), Some("b"));
            assert_eq!(record.transitions, 3);
            assert_eq!(expected.as_deref(), Some("7"));
            assert!(acquire);
        }
        d => panic!("{d:?}"),
    }
    // Renouvelée entre-temps (nouvelle version) : l'attente repart.
    let renewed = lease(Some("a"), "8", 2);
    let o = observe(o, Some(&renewed), 20_000);
    assert_eq!(o, Some(seen("8", 20_000)));
    assert!(matches!(
        decide("b", Some(&renewed), o.as_ref(), &t(), 34_999),
        Decision::Wait { .. }
    ));
    // Jamais observée : pas d'expiration, quelle que soit l'heure.
    assert!(matches!(
        decide("b", Some(&renewed), None, &t(), u64::MAX / 2),
        Decision::Wait { .. }
    ));
    // Observation d'une autre version que celle relue : pas d'expiration non plus.
    assert!(matches!(
        decide("b", Some(&renewed), Some(&seen("7", 0)), &t(), 1_000_000),
        Decision::Wait { .. }
    ));
}

#[test]
fn ctl_24_lease_duration_comes_from_the_lease() {
    let mut l = lease(Some("a"), "1", 0);
    l.record.duration_ms = 1_000;
    let o = observe(None, Some(&l), 0);
    assert!(matches!(
        decide("b", Some(&l), o.as_ref(), &t(), 999),
        Decision::Wait { .. }
    ));
    assert!(matches!(
        decide("b", Some(&l), o.as_ref(), &t(), 1_000),
        Decision::Write { acquire: true, .. }
    ));
}

#[test]
fn ctl_24_holder_renews() {
    let l = lease(Some("a"), "9", 5);
    assert_eq!(
        decide("a", Some(&l), None, &t(), 0),
        Decision::Write {
            record: Record {
                holder: Some("a".into()),
                duration_ms: 15_000,
                transitions: 5,
            },
            expected: Some("9".into()),
            acquire: false,
        }
    );
}

#[test]
fn ctl_24_leader_gives_up_at_the_renew_deadline() {
    assert!(still_leading(1_000, 1_000, &t()));
    assert!(still_leading(1_000, 10_999, &t()));
    assert!(!still_leading(1_000, 11_000, &t()));
    assert!(!still_leading(1_000, 50_000, &t()));
}

#[test]
fn ctl_24_release_by_the_holder_only() {
    let l = lease(Some("a"), "4", 2);
    assert_eq!(
        release("a", &l),
        Some((
            Record {
                holder: None,
                duration_ms: 1_000,
                transitions: 2,
            },
            "4".to_string()
        ))
    );
    assert_eq!(release("b", &l), None);
    assert_eq!(release("a", &lease(None, "4", 2)), None);
}

#[test]
fn ctl_24_timings_must_be_ordered() {
    assert!(t().validate().is_ok());
    let bad = [
        (15_000, 15_000, 2_000),
        (15_000, 10_000, 10_000),
        (15_000, 10_000, 0),
        (10_000, 15_000, 2_000),
    ];
    for (lease_duration_ms, renew_deadline_ms, retry_period_ms) in bad {
        let t = Timings {
            lease_duration_ms,
            renew_deadline_ms,
            retry_period_ms,
        };
        assert!(t.validate().is_err(), "{t:?}");
    }
}

// Simulation de deux répliques et du serveur du bail : chaque requête est appliquée après
// un délai, sa réponse arrive après un autre, ou se perd. Une réplique peut aussi tomber,
// puis repartir en passive. Propriété : jamais deux répliques ne se croient actives au
// même instant.

#[derive(Debug, Clone)]
struct Net {
    apply_ms: u64,
    reply_ms: u64,
    lost: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Req {
    Get,
    Write,
}

#[derive(Debug)]
enum Ev {
    Tick(usize),
    Apply {
        r: usize,
        epoch: u64,
        req: Req,
        write: Option<(Record, Option<String>)>,
        started: u64,
        reply_ms: u64,
        lost: bool,
    },
    Reply {
        r: usize,
        epoch: u64,
        req: Req,
        result: Option<Option<Lease>>,
        started: u64,
    },
    Crash(usize, u64),
    Restart(usize),
}

#[derive(Debug, Default)]
struct Replica {
    /// Incrémentée à chaque changement de rôle : les réponses d'avant sont ignorées.
    epoch: u64,
    down: bool,
    busy: bool,
    leader: bool,
    last_ok: u64,
    observed: Option<Observed>,
    /// Début du renouvellement en cours.
    renewing: u64,
}

fn believes(r: &Replica, now: u64, t: &Timings) -> bool {
    !r.down && r.leader && still_leading(r.last_ok, now, t)
}

fn simulate(nets: Vec<Net>, crashes: Vec<(usize, u64, u64)>, t: &Timings) -> u32 {
    const END: u64 = 120_000;
    let names = ["a", "b"];
    let mut server: Option<(Record, u64)> = None;
    let mut replicas = [Replica::default(), Replica::default()];
    let mut events: BTreeMap<(u64, u64), Ev> = BTreeMap::new();
    let mut seq = 0u64;
    let mut push = |events: &mut BTreeMap<(u64, u64), Ev>, at: u64, ev: Ev| {
        seq += 1;
        events.insert((at, seq), ev);
    };
    push(&mut events, 0, Ev::Tick(0));
    push(&mut events, 7, Ev::Tick(1));
    for (r, at, down) in crashes {
        push(&mut events, at, Ev::Crash(r, down));
    }
    let mut nets = nets.into_iter().cycle();
    let mut elections = 0;
    while let Some(((now, _), ev)) = events.pop_first() {
        if now > END {
            break;
        }
        match ev {
            Ev::Tick(r) => {
                push(&mut events, now + t.retry_period_ms, Ev::Tick(r));
                let rep = &mut replicas[r];
                if rep.down || rep.busy {
                    continue;
                }
                if rep.leader && !still_leading(rep.last_ok, now, t) {
                    rep.leader = false;
                    rep.epoch += 1;
                    rep.observed = None;
                }
                rep.busy = true;
                rep.renewing = now;
                let n = nets.next().unwrap();
                push(
                    &mut events,
                    now + n.apply_ms,
                    Ev::Apply {
                        r,
                        epoch: rep.epoch,
                        req: Req::Get,
                        write: None,
                        started: now,
                        reply_ms: n.reply_ms,
                        lost: n.lost,
                    },
                );
            }
            Ev::Apply {
                r,
                epoch,
                req,
                write,
                started,
                reply_ms,
                lost,
            } => {
                let result = match (req, write) {
                    (Req::Get, _) => Some(server.as_ref().map(|(rec, v)| Lease {
                        record: rec.clone(),
                        version: v.to_string(),
                    })),
                    (Req::Write, Some((rec, expected))) => {
                        let current = server.as_ref().map(|(_, v)| v.to_string());
                        if current == expected {
                            let v = server.as_ref().map_or(1, |(_, v)| v + 1);
                            server = Some((rec, v));
                            Some(None)
                        } else {
                            None
                        }
                    }
                    (Req::Write, None) => unreachable!(),
                };
                let result = if lost { None } else { result };
                push(
                    &mut events,
                    now + reply_ms,
                    Ev::Reply {
                        r,
                        epoch,
                        req,
                        result,
                        started,
                    },
                );
            }
            Ev::Reply {
                r,
                epoch,
                req,
                result,
                started,
            } => {
                let rep = &mut replicas[r];
                if rep.down || rep.epoch != epoch {
                    continue;
                }
                match (req, result) {
                    (Req::Get, Some(lease)) => {
                        if rep.leader && !still_leading(rep.last_ok, now, t) {
                            rep.leader = false;
                            rep.epoch += 1;
                            rep.busy = false;
                            continue;
                        }
                        rep.observed = observe(rep.observed.take(), lease.as_ref(), now);
                        match decide(names[r], lease.as_ref(), rep.observed.as_ref(), t, now) {
                            Decision::Write {
                                record, expected, ..
                            } => {
                                let n = nets.next().unwrap();
                                push(
                                    &mut events,
                                    now + n.apply_ms,
                                    Ev::Apply {
                                        r,
                                        epoch,
                                        req: Req::Write,
                                        write: Some((record, expected)),
                                        started,
                                        reply_ms: n.reply_ms,
                                        lost: n.lost,
                                    },
                                );
                            }
                            Decision::Wait { .. } => {
                                if rep.leader {
                                    rep.leader = false;
                                    rep.epoch += 1;
                                }
                                rep.busy = false;
                            }
                        }
                    }
                    (Req::Write, Some(_)) => {
                        rep.busy = false;
                        if !rep.leader {
                            // Une prise dont la réponse arrive trop tard ne vaut rien.
                            if still_leading(started, now, t) {
                                rep.leader = true;
                                rep.epoch += 1;
                                rep.last_ok = started;
                                elections += 1;
                            }
                        } else {
                            rep.last_ok = started;
                        }
                    }
                    (_, None) => rep.busy = false,
                }
            }
            Ev::Crash(r, down) => {
                let rep = &mut replicas[r];
                rep.down = true;
                rep.leader = false;
                rep.busy = false;
                rep.epoch += 1;
                rep.observed = None;
                push(&mut events, now + down, Ev::Restart(r));
            }
            Ev::Restart(r) => replicas[r].down = false,
        }
        assert!(
            !(believes(&replicas[0], now, t) && believes(&replicas[1], now, t)),
            "deux actives à {now} : {replicas:?}"
        );
    }
    elections
}

fn net() -> impl Strategy<Value = Net> {
    (0u64..4_000, 0u64..4_000, prop::bool::weighted(0.1)).prop_map(|(apply_ms, reply_ms, lost)| {
        Net {
            apply_ms,
            reply_ms,
            lost,
        }
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn ctl_24_never_two_leaders(
        nets in prop::collection::vec(net(), 1..64),
        crashes in prop::collection::vec((0usize..2, 0u64..120_000, 0u64..30_000), 0..4),
    ) {
        simulate(nets, crashes, &Timings::default());
    }
}

/// Sans délai ni perte, une réplique est élue, et l'autre la remplace après sa chute.
#[test]
fn ctl_24_a_replica_is_elected_and_replaced() {
    let fast = vec![Net {
        apply_ms: 5,
        reply_ms: 5,
        lost: false,
    }];
    assert_eq!(simulate(fast.clone(), vec![], &Timings::default()), 1);
    assert!(
        simulate(
            fast,
            vec![(0, 30_000, 200_000), (1, 30_000, 200_000)],
            &Timings::default()
        ) >= 1
    );
    let elections = simulate(
        vec![Net {
            apply_ms: 5,
            reply_ms: 5,
            lost: false,
        }],
        vec![(0, 30_000, 200_000)],
        &Timings::default(),
    );
    assert_eq!(elections, 2, "a élue, puis b après la chute de a");
}
