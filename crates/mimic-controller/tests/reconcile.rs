//! Critères CRD-* de la spec (§5.5.1) : réconciliation de `MimicWorkload`, sans cluster.

use mimic_controller::reconcile::{
    Action, Desired, MimicWorkloadSpec, PodReport, PodState, pod_state, reconcile, summarize,
};

fn spec(mode: &str, snapshot: Option<&str>) -> MimicWorkloadSpec {
    MimicWorkloadSpec {
        mode: mode.into(),
        snapshot: snapshot.map(Into::into),
    }
}

fn desired(mode: &str, snapshot: Option<&str>) -> Desired {
    Desired {
        mode: Some(mode.into()),
        snapshot: snapshot.map(Into::into),
    }
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
    };
    let late = PodReport {
        mode: "record".into(),
        snapshot: None,
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
