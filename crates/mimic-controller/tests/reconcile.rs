//! Critères CRD-* de la spec (§5.5.1) : réconciliation de `MimicWorkload`, sans cluster.

use std::collections::BTreeMap;

use mimic_controller::crd::{CaptureSpec, LabelSelector, LabelSelectorRequirement};
use mimic_controller::reconcile::{
    Action, Desired, MimicWorkloadSpec, PRODUCTION_REASON, PodReport, PodState, desired_for_pod,
    guard_pod, is_production, pod_state, reconcile, reconcile_guarded, selects, status_of,
    summarize,
};

fn spec(mode: &str, snapshot: Option<&str>) -> MimicWorkloadSpec {
    MimicWorkloadSpec {
        mode: mode.into(),
        snapshot: snapshot.map(Into::into),
        ..MimicWorkloadSpec::default()
    }
}

fn desired(mode: &str, snapshot: Option<&str>) -> Desired {
    Desired {
        mode: Some(mode.into()),
        snapshot: snapshot.map(Into::into),
        ..Desired::default()
    }
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

const ID: &str = "3f9a0c1e5b7d2a46";

#[test]
fn crd_01_mock_without_snapshot_asks_for_one() {
    let o = reconcile(Some(&spec("mock", None)), &Desired::default(), &|_| false);
    assert_eq!(o.action, Action::BuildSnapshot);
    assert_eq!(
        o.desired,
        Desired::default(),
        "rien ne change avant le snapshot"
    );
    // Une fois le snapshot construit, il devient l'état souhaité.
    let o = reconcile(Some(&spec("mock", None)), &desired("record", None), &|_| {
        false
    });
    assert_eq!(o.action, Action::BuildSnapshot);
}

#[test]
fn crd_02_current_snapshot_is_kept_while_in_mock() {
    let current = desired("mock", Some(ID));
    let o = reconcile(Some(&spec("mock", None)), &current, &|id| id == ID);
    assert_eq!(o.action, Action::None);
    assert_eq!(o.desired, current);
    assert_eq!(o.error, None);
    // mock-partial garde aussi le snapshot courant.
    let o = reconcile(Some(&spec("mock-partial", None)), &current, &|id| id == ID);
    assert_eq!(o.desired, desired("mock-partial", Some(ID)));
}

#[test]
fn crd_03_unknown_pinned_snapshot_keeps_previous_state() {
    let previous = desired("record", None);
    let o = reconcile(
        Some(&spec("mock", Some("0000000000000000"))),
        &previous,
        &|_| false,
    );
    assert_eq!(o.desired, previous);
    assert_eq!(o.error.as_deref(), Some("SnapshotNotFound"));
    let o = reconcile(Some(&spec("mock", Some(ID))), &previous, &|id| id == ID);
    assert_eq!(o.desired, desired("mock", Some(ID)));
    assert_eq!(o.error, None);
}

#[test]
fn crd_04_record_drops_the_snapshot() {
    let o = reconcile(
        Some(&spec("record", None)),
        &desired("mock", Some(ID)),
        &|_| true,
    );
    assert_eq!(o.desired, desired("record", None));
    assert_eq!(o.action, Action::None);
}

#[test]
fn crd_05_deleted_resource_empties_the_desired_state() {
    let o = reconcile(None, &desired("mock", Some(ID)), &|_| true);
    assert_eq!(o.desired, Desired::default());
}

#[test]
fn invalid_mode_is_reported_without_change() {
    let previous = desired("record", None);
    let o = reconcile(Some(&spec("turbo", None)), &previous, &|_| true);
    assert_eq!(o.desired, previous);
    assert_eq!(o.error.as_deref(), Some("InvalidMode"));
}

#[test]
fn crd_06_status_counts_aligned_pods() {
    let want = desired("mock", Some(ID));
    let aligned = PodReport {
        mode: "mock".into(),
        snapshot: Some(ID.into()),
        ..PodReport::default()
    };
    let late = PodReport {
        mode: "record".into(),
        snapshot: None,
        ..PodReport::default()
    };
    let now = 100_000;
    assert_eq!(
        pod_state(&aligned, now - 1_000, now, 30_000, &want),
        PodState::Aligned
    );
    assert_eq!(
        pod_state(&late, now - 1_000, now, 30_000, &want),
        PodState::Diverging
    );
    assert_eq!(
        pod_state(&aligned, now - 60_000, now, 30_000, &want),
        PodState::Absent
    );
    // Sans état souhaité, un pod présent est aligné quel que soit son mode.
    assert_eq!(
        pod_state(&late, now, now, 30_000, &Desired::default()),
        PodState::Aligned
    );
    let s = summarize(
        &[PodState::Aligned, PodState::Diverging, PodState::Absent],
        None,
    );
    assert_eq!((s.pods, s.aligned, s.ready), (2, 1, false));
    let s = summarize(&[PodState::Aligned], None);
    assert!(s.ready);
    let s = summarize(&[PodState::Aligned], Some("SnapshotNotFound"));
    assert!(!s.ready);
    assert_eq!(s.reason.as_deref(), Some("SnapshotNotFound"));
}

#[test]
fn crd_07_production_namespace_refuses_mock() {
    assert!(is_production(&labels(&[("env", "production")])));
    assert!(!is_production(&labels(&[("env", "staging")])));
    assert!(!is_production(&labels(&[("environment", "production")])));
    assert!(!is_production(&BTreeMap::new()));

    for mode in ["mock", "mock-partial"] {
        // Depuis record : rien ne change, aucun snapshot n'est construit.
        let previous = desired("record", None);
        let o = reconcile_guarded(Some(&spec(mode, None)), &previous, &|_| true, true);
        assert_eq!(o.action, Action::None, "{mode}");
        assert_eq!(o.desired, previous);
        assert_eq!(o.error.as_deref(), Some(PRODUCTION_REASON));
        // Snapshot épinglé : refusé aussi.
        let o = reconcile_guarded(Some(&spec(mode, Some(ID))), &previous, &|_| true, true);
        assert_eq!(o.desired, previous);
        assert_eq!(o.error.as_deref(), Some(PRODUCTION_REASON));
        // Depuis le mock (namespace labellisé après coup) : retour en record.
        let o = reconcile_guarded(
            Some(&spec(mode, None)),
            &desired("mock", Some(ID)),
            &|_| true,
            true,
        );
        assert_eq!(o.desired, desired("record", None));
        assert_eq!(o.error.as_deref(), Some(PRODUCTION_REASON));
        // Ressource toute neuve : l'état souhaité reste vide.
        let o = reconcile_guarded(
            Some(&spec(mode, None)),
            &Desired::default(),
            &|_| true,
            true,
        );
        assert_eq!(o.desired, Desired::default());
    }
    // record et passthrough restent permis.
    let o = reconcile_guarded(
        Some(&spec("passthrough", None)),
        &desired("mock", Some(ID)),
        &|_| true,
        true,
    );
    assert_eq!(o.desired, desired("passthrough", None));
    assert_eq!(o.error, None);
    // Label retiré : le mock est de nouveau accepté.
    let o = reconcile_guarded(
        Some(&spec("mock", None)),
        &desired("record", None),
        &|_| false,
        false,
    );
    assert_eq!(o.action, Action::BuildSnapshot);
    assert_eq!(o.error, None);

    // Battement : jamais de mock vers un pod de production, même annoncé par le pod.
    assert_eq!(
        guard_pod(Desired::default(), true, "mock"),
        desired("record", None)
    );
    assert_eq!(
        guard_pod(Desired::default(), true, "mock-partial"),
        desired("record", None)
    );
    assert_eq!(
        guard_pod(Desired::default(), true, "record"),
        Desired::default()
    );
    assert_eq!(
        guard_pod(desired("mock", Some(ID)), true, "record"),
        desired("record", None)
    );
    assert_eq!(
        guard_pod(desired("mock", Some(ID)), false, "record"),
        desired("mock", Some(ID))
    );

    // Statut : conditions Aligned et Allowed explicites.
    let s = summarize(&[PodState::Aligned], Some(PRODUCTION_REASON));
    let st = status_of(&desired("record", None), &s, Some(3), None);
    assert_eq!(st.observed_generation, Some(3));
    let cond = |t: &str| st.conditions.iter().find(|c| c.type_ == t).unwrap().clone();
    assert_eq!(cond("Aligned").status, "False");
    assert_eq!(cond("Aligned").reason.as_deref(), Some(PRODUCTION_REASON));
    let allowed = cond("Allowed");
    assert_eq!(allowed.status, "False");
    assert_eq!(allowed.reason.as_deref(), Some(PRODUCTION_REASON));
    assert!(
        allowed
            .message
            .as_deref()
            .unwrap()
            .contains("env=production"),
        "{allowed:?}"
    );
    let ok = status_of(
        &desired("mock", Some(ID)),
        &summarize(&[PodState::Aligned], None),
        Some(4),
        Some(3),
    );
    assert_eq!(ok.snapshot.as_deref(), Some(ID));
    assert_eq!(ok.snapshot_exchanges, Some(3));
    let allowed = ok.conditions.iter().find(|c| c.type_ == "Allowed").unwrap();
    assert_eq!(allowed.status, "True");
}

/// `SnapshotEmpty` (CTL-45) : vraie seulement en mock avec un snapshot sans aucun échange ;
/// fausse hors mock, sans snapshot, ou si le compte est inconnu (`None`, jamais pris pour 0).
#[test]
fn ctl_45_snapshot_empty_condition() {
    let empty = |mode: &str, snapshot: Option<&str>, exchanges: Option<u64>| {
        status_of(
            &desired(mode, snapshot),
            &summarize(&[PodState::Aligned], None),
            None,
            exchanges,
        )
        .conditions
        .into_iter()
        .find(|c| c.type_ == "SnapshotEmpty")
        .unwrap()
        .status
    };
    assert_eq!(empty("mock", Some(ID), Some(0)), "True");
    assert_eq!(empty("mock-partial", Some(ID), Some(0)), "True");
    assert_eq!(empty("mock", Some(ID), Some(1)), "False");
    assert_eq!(empty("mock", Some(ID), None), "False");
    assert_eq!(empty("record", Some(ID), Some(0)), "False");
    assert_eq!(empty("mock", None, Some(0)), "False");
}

#[test]
fn crd_09_capture_inbound_is_part_of_the_desired_state() {
    let mut s = spec("mock", Some(ID));
    s.capture = Some(CaptureSpec {
        inbound: Some("metadata".into()),
    });
    let o = reconcile(Some(&s), &Desired::default(), &|_| true);
    assert_eq!(o.desired.capture_inbound.as_deref(), Some("metadata"));
    assert_eq!(o.desired.mode.as_deref(), Some("mock"));
    // En record aussi, la capture demandée est transmise.
    let mut r = spec("record", None);
    r.capture = s.capture.clone();
    let o = reconcile(Some(&r), &Desired::default(), &|_| true);
    assert_eq!(o.desired.capture_inbound.as_deref(), Some("metadata"));
    // Une capture inconnue est refusée sans changer l'état souhaité.
    let mut bad = spec("mock", Some(ID));
    bad.capture = Some(CaptureSpec {
        inbound: Some("bodies".into()),
    });
    let previous = desired("record", None);
    let o = reconcile(Some(&bad), &previous, &|_| true);
    assert_eq!(o.desired, previous);
    assert_eq!(o.error.as_deref(), Some("InvalidCapture"));
    // Sans spec.capture, aucune capture imposée.
    let o = reconcile(Some(&spec("mock", Some(ID))), &Desired::default(), &|_| {
        true
    });
    assert_eq!(o.desired.capture_inbound, None);

    // Alignement : la capture demandée doit être appliquée.
    let want = Desired {
        capture_inbound: Some("metadata".into()),
        ..desired("mock", Some(ID))
    };
    let report = |capture: Option<&str>| PodReport {
        mode: "mock".into(),
        snapshot: Some(ID.into()),
        capture_inbound: capture.map(Into::into),
        ..PodReport::default()
    };
    let now = 100_000;
    assert_eq!(
        pod_state(&report(Some("full")), now, now, 30_000, &want),
        PodState::Diverging
    );
    assert_eq!(
        pod_state(&report(None), now, now, 30_000, &want),
        PodState::Diverging,
        "ancien sidecar qui n'annonce pas sa capture"
    );
    assert_eq!(
        pod_state(&report(Some("metadata")), now, now, 30_000, &want),
        PodState::Aligned
    );
    // Sans capture imposée, celle du pod est libre.
    assert_eq!(
        pod_state(
            &report(Some("full")),
            now,
            now,
            30_000,
            &desired("mock", Some(ID))
        ),
        PodState::Aligned
    );
}

fn requirement(key: &str, operator: &str, values: &[&str]) -> LabelSelectorRequirement {
    LabelSelectorRequirement {
        key: key.into(),
        operator: operator.into(),
        values: values.iter().map(|v| v.to_string()).collect(),
    }
}

#[test]
fn crd_08_selector_limits_the_desired_state_to_selected_pods() {
    let v1 = labels(&[("app", "checkout"), ("version", "v1")]);
    let v2 = labels(&[("app", "checkout"), ("version", "v2")]);
    let none = BTreeMap::new();

    // Sémantique Kubernetes.
    let by_labels = LabelSelector {
        match_labels: labels(&[("version", "v2")]),
        ..LabelSelector::default()
    };
    assert!(by_labels.matches(&v2));
    assert!(!by_labels.matches(&v1));
    assert!(!by_labels.matches(&none));
    assert!(
        LabelSelector::default().matches(&v1),
        "sélecteur vide : tout"
    );
    let expr = |r: LabelSelectorRequirement| LabelSelector {
        match_expressions: vec![r],
        ..LabelSelector::default()
    };
    assert!(expr(requirement("version", "In", &["v2", "v3"])).matches(&v2));
    assert!(!expr(requirement("version", "In", &["v2", "v3"])).matches(&v1));
    assert!(!expr(requirement("version", "In", &["v2"])).matches(&none));
    assert!(expr(requirement("version", "NotIn", &["v1"])).matches(&v2));
    assert!(!expr(requirement("version", "NotIn", &["v1"])).matches(&v1));
    assert!(expr(requirement("version", "NotIn", &["v1"])).matches(&none));
    assert!(expr(requirement("version", "Exists", &[])).matches(&v1));
    assert!(!expr(requirement("version", "Exists", &[])).matches(&none));
    assert!(expr(requirement("version", "DoesNotExist", &[])).matches(&none));
    assert!(!expr(requirement("version", "DoesNotExist", &[])).matches(&v1));
    assert!(!expr(requirement("version", "Matches", &["v2"])).matches(&v2));
    assert!(!expr(requirement("version", "In", &[])).matches(&v2));
    assert!(!expr(requirement("version", "Exists", &["v2"])).matches(&v2));
    // matchLabels et matchExpressions se combinent par un ET.
    let both = LabelSelector {
        match_labels: labels(&[("app", "checkout")]),
        match_expressions: vec![requirement("version", "NotIn", &["v1"])],
    };
    assert!(both.matches(&v2));
    assert!(!both.matches(&v1));

    // L'état souhaité porte le sélecteur ; chaque pod reçoit le sien.
    let mut s = spec("mock", Some(ID));
    s.selector = Some(by_labels.clone());
    let o = reconcile(Some(&s), &Desired::default(), &|_| true);
    assert_eq!(o.desired.selector.as_ref(), Some(&by_labels));
    assert!(selects(&o.desired, &v2));
    assert!(!selects(&o.desired, &v1));
    assert_eq!(desired_for_pod(&o.desired, &v2), desired("mock", Some(ID)));
    assert_eq!(
        desired_for_pod(&o.desired, &v1),
        Desired::default(),
        "repli : mode de la configuration"
    );
    // Sans sélecteur, tous les pods.
    let all = desired("mock", Some(ID));
    assert!(selects(&all, &v1));
    assert_eq!(desired_for_pod(&all, &v1), all);

    // Alignement : un pod non sélectionné en record n'est pas compté ; c'est au résumé
    // de ne compter que les pods sélectionnés.
    let s = summarize(&[PodState::Aligned], None);
    assert_eq!((s.pods, s.aligned, s.ready), (1, 1, true));
}

#[test]
fn crd_10_mock_websocket_is_part_of_the_desired_state() {
    use mimic_controller::crd::MockSpec;
    let with = |mode: &str, websocket: &str| MimicWorkloadSpec {
        mock: Some(MockSpec {
            websocket: Some(websocket.into()),
        }),
        ..spec(mode, Some(ID))
    };
    let o = reconcile(Some(&with("mock", "refuse")), &Desired::default(), &|_| {
        true
    });
    assert_eq!(o.desired.mock_websocket.as_deref(), Some("refuse"));
    assert_eq!(o.desired.mode.as_deref(), Some("mock"));
    assert_eq!(o.error, None);
    // En record aussi le réglage est transmis : il ne prend effet qu'en mock.
    let mut r = with("record", "refuse");
    r.snapshot = None;
    let o = reconcile(Some(&r), &Desired::default(), &|_| true);
    assert_eq!(o.desired.mock_websocket.as_deref(), Some("refuse"));
    // Valeur inconnue : état souhaité inchangé, raison InvalidMock.
    let previous = desired("record", None);
    let o = reconcile(Some(&with("mock", "drop")), &previous, &|_| true);
    assert_eq!(o.desired, previous);
    assert_eq!(o.error.as_deref(), Some("InvalidMock"));
    // Sans spec.mock, aucun réglage imposé.
    let o = reconcile(Some(&spec("mock", Some(ID))), &Desired::default(), &|_| {
        true
    });
    assert_eq!(o.desired.mock_websocket, None);

    // Passer de relay à refuse sans quitter le mock garde le snapshot courant.
    let current = Desired {
        mock_websocket: Some("relay".into()),
        ..desired("mock", Some(ID))
    };
    let mut unpinned = with("mock", "refuse");
    unpinned.snapshot = None;
    let o = reconcile(Some(&unpinned), &current, &|id| id == ID);
    assert_eq!(o.action, Action::None);
    assert_eq!(o.desired.snapshot.as_deref(), Some(ID));
    assert_eq!(o.desired.mock_websocket.as_deref(), Some("refuse"));

    // Alignement : le réglage imposé doit être appliqué.
    let want = Desired {
        mock_websocket: Some("refuse".into()),
        ..desired("mock", Some(ID))
    };
    let report = |websocket: Option<&str>| PodReport {
        mode: "mock".into(),
        snapshot: Some(ID.into()),
        mock_websocket: websocket.map(Into::into),
        ..PodReport::default()
    };
    let now = 100_000;
    assert_eq!(
        pod_state(&report(Some("relay")), now, now, 30_000, &want),
        PodState::Diverging
    );
    assert_eq!(
        pod_state(&report(None), now, now, 30_000, &want),
        PodState::Diverging,
        "sidecar d'une version antérieure, qui n'annonce pas le réglage"
    );
    assert_eq!(
        pod_state(&report(Some("refuse")), now, now, 30_000, &want),
        PodState::Aligned
    );
    // Sans réglage imposé, celui du pod est libre.
    assert_eq!(
        pod_state(
            &report(Some("relay")),
            now,
            now,
            30_000,
            &desired("mock", Some(ID))
        ),
        PodState::Aligned
    );
    // Le pod sélectionné reçoit le réglage, qui voyage dans la réponse au battement.
    let pod = desired_for_pod(&want, &labels(&[]));
    assert_eq!(pod.mock_websocket.as_deref(), Some("refuse"));
    let json = serde_json::to_value(&pod).unwrap();
    assert_eq!(json["mock_websocket"], "refuse");
}
