//! Haute disponibilité (spec §5.5.4) : deux réplicas, une seule active, élue par un bail.
//!
//! Les décisions sont celles de `mimic-core::lease` (fonctions pures) ; ce module les applique
//! au rythme de `retry_period`, sur un bail Kubernetes (`crate::kube::KubeLease`) ou en mémoire
//! (tests, hors cluster). La passive n'a aucun état et ne touche pas au support. À la prise du
//! bail, la nouvelle active recharge l'état persisté, sert les sidecars, puis pose sa marque,
//! que le Service sélectionne. À la perte du bail, elle cesse aussitôt de servir et d'écrire.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures_util::future::BoxFuture;
use mimic_core::lease::{
    Decision, Lease, Record, Timings, decide, observe, release, still_leading,
};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::state::Ctl;

/// Label de la réplique active, sélectionné par le Service.
pub const LEADER_LABEL: (&str, &str) = ("mimic.platform.io/leader", "true");
/// Coût de suppression de la réplique active : une réduction retire la passive d'abord.
pub const DELETION_COST: (&str, &str) = ("controller.kubernetes.io/pod-deletion-cost", "100");
/// Attente de la liste initiale des `MimicWorkload` à la prise de relais.
const INITIAL_LIST: Duration = Duration::from_secs(30);

/// Bail et marque de la réplique active.
pub trait Coordination: Send + Sync {
    /// Bail courant, absent s'il n'a jamais été créé.
    fn get(&self) -> BoxFuture<'_, Result<Option<Lease>, String>>;
    /// Écrit le bail si sa version est encore `expected` (`None` : création) ; échoue sinon.
    fn write(&self, record: Record, expected: Option<String>) -> BoxFuture<'_, Result<(), String>>;
    /// Pose (`active`) la marque sur le pod `me`, après l'avoir retirée des autres, ou la retire.
    fn announce(&self, me: &str, active: bool) -> BoxFuture<'_, Result<(), String>>;
}

/// Bail et marques en mémoire, partagés par des répliques d'un même processus.
#[derive(Default)]
pub struct MemoryLease {
    lease: Mutex<Option<(Record, u64)>>,
    marked: Mutex<BTreeSet<String>>,
}

impl MemoryLease {
    pub fn holder(&self) -> Option<String> {
        self.lease
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(|(r, _)| r.holder.clone())
    }

    /// Pods qui portent la marque.
    pub fn marked(&self) -> BTreeSet<String> {
        self.marked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Coordination for MemoryLease {
    fn get(&self) -> BoxFuture<'_, Result<Option<Lease>, String>> {
        let lease = self
            .lease
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|(record, v)| Lease {
                record: record.clone(),
                version: v.to_string(),
            });
        Box::pin(async move { Ok(lease) })
    }

    fn write(&self, record: Record, expected: Option<String>) -> BoxFuture<'_, Result<(), String>> {
        let result = {
            let mut lease = self.lease.lock().unwrap_or_else(PoisonError::into_inner);
            let current = lease.as_ref().map(|(_, v)| v.to_string());
            if current == expected {
                let version = lease.as_ref().map_or(1, |(_, v)| v + 1);
                *lease = Some((record, version));
                Ok(())
            } else {
                Err(format!(
                    "conflit : version {current:?}, attendue {expected:?}"
                ))
            }
        };
        Box::pin(async move { result })
    }

    fn announce(&self, me: &str, active: bool) -> BoxFuture<'_, Result<(), String>> {
        {
            let mut marked = self.marked.lock().unwrap_or_else(PoisonError::into_inner);
            if active {
                marked.clear();
                marked.insert(me.to_string());
            } else {
                marked.remove(me);
            }
        }
        Box::pin(async { Ok(()) })
    }
}

/// Rôle de la réplique.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Passive : attend le bail.
    Passive,
    /// Bail pris, état persisté en cours de rechargement.
    Taking,
    /// Active : sert les sidecars, marque posée.
    Active,
    /// Bail perdu : ne sert plus ; le processus doit s'arrêter.
    Lost,
}

/// Une passive (ou une active qui vient de perdre le bail) ne répond qu'à `/healthz` et
/// `/metrics` ; le reste reçoit `503`, sans toucher au support.
pub async fn passive_guard(State(ctl): State<Arc<Ctl>>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if ctl.is_active() || path == "/healthz" || path == "/metrics" {
        return next.run(req).await;
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, "2")],
        "réplique passive du contrôleur : la réplique active sert les sidecars\n",
    )
        .into_response()
}

/// Élection d'une réplique.
pub struct Election {
    pub coord: Arc<dyn Coordination>,
    pub me: String,
    pub timings: Timings,
    role: watch::Sender<Role>,
    /// Tâches de l'active : prise de relais, maintenance, surveillance, statut.
    tasks: Mutex<Vec<JoinHandle<()>>>,
    campaign: Mutex<Option<JoinHandle<()>>>,
}

impl Election {
    pub fn new(coord: Arc<dyn Coordination>, me: String, timings: Timings) -> Arc<Self> {
        Arc::new(Self {
            coord,
            me,
            timings,
            role: watch::Sender::new(Role::Passive),
            tasks: Mutex::new(vec![]),
            campaign: Mutex::new(None),
        })
    }

    pub fn role(&self) -> Role {
        *self.role.borrow()
    }

    /// Attend que la réplique soit dans ce rôle.
    pub async fn wait(&self, role: Role) {
        let mut rx = self.role.subscribe();
        let _ = rx.wait_for(|r| *r == role).await;
    }

    /// Lance la campagne : attente du bail, prise de relais, renouvellement.
    pub fn spawn(self: &Arc<Self>, ctl: Arc<Ctl>, client: Option<kube::Client>) {
        let task = tokio::spawn(self.clone().campaign(ctl, client));
        *self.campaign.lock().unwrap_or_else(PoisonError::into_inner) = Some(task);
    }

    fn retry(&self) -> Duration {
        Duration::from_millis(self.timings.retry_period_ms)
    }

    fn push(&self, task: JoinHandle<()>) {
        self.tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(task);
    }

    /// Arrête les tâches de l'active, y compris celles qu'une prise de relais lançait.
    async fn stop_tasks(&self) {
        loop {
            let batch: Vec<_> = self
                .tasks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .drain(..)
                .collect();
            if batch.is_empty() {
                return;
            }
            for t in &batch {
                t.abort();
            }
            for t in batch {
                let _ = t.await;
            }
        }
    }

    /// Arrêt brutal (tests) : plus rien ne tourne, rien n'est écrit ni rendu.
    pub fn kill(&self) {
        if let Some(c) = self
            .campaign
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            c.abort();
        }
        for t in self
            .tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
        {
            t.abort();
        }
    }

    /// Arrêt propre : marque retirée, service arrêté, exemples écrits, bail rendu.
    pub async fn shutdown(&self, ctl: &Ctl) {
        let campaign = self
            .campaign
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(c) = campaign {
            c.abort();
            let _ = c.await;
        }
        let role = self.role();
        let holding = matches!(role, Role::Taking | Role::Active);
        ctl.set_active(false);
        if role == Role::Active
            && let Err(e) = self.coord.announce(&self.me, false).await
        {
            tracing::warn!(error = %e, "marque de la réplique active non retirée");
        }
        self.stop_tasks().await;
        if !holding {
            return;
        }
        ctl.flush_all().await;
        let released = match self.coord.get().await {
            Ok(Some(lease)) => match release(&self.me, &lease) {
                Some((record, version)) => self.coord.write(record, Some(version)).await,
                None => Ok(()),
            },
            Ok(None) => Ok(()),
            Err(e) => Err(e),
        };
        match released {
            Ok(()) => tracing::info!(identity = %self.me, "bail rendu"),
            Err(e) => tracing::warn!(error = %e, "bail non rendu : il expirera"),
        }
        self.role.send_replace(Role::Passive);
    }

    async fn campaign(self: Arc<Self>, ctl: Arc<Ctl>, client: Option<kube::Client>) {
        let clock = tokio::time::Instant::now();
        let now = move || clock.elapsed().as_millis() as u64;
        let t = self.timings.clone();
        let mut observed = None;
        let mut holder: Option<Option<String>> = None;
        // Passive : relecture du bail jusqu'à le prendre.
        let mut last_ok = loop {
            let start = now();
            match self.coord.get().await {
                Ok(lease) => {
                    observed = observe(observed.take(), lease.as_ref(), now());
                    match decide(&self.me, lease.as_ref(), observed.as_ref(), &t, now()) {
                        Decision::Write {
                            record, expected, ..
                        } => match self.coord.write(record, expected).await {
                            // Une réponse arrivée après renew_deadline ne vaut rien.
                            Ok(()) if still_leading(start, now(), &t) => break start,
                            Ok(()) => {}
                            Err(e) => tracing::debug!(error = %e, "prise du bail"),
                        },
                        Decision::Wait { holder: h } => {
                            if holder.as_ref() != Some(&h) {
                                tracing::info!(holder = ?h, "réplique passive");
                                holder = Some(h);
                            }
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "lecture du bail"),
            }
            tokio::time::sleep(self.retry()).await;
        };
        tracing::info!(identity = %self.me, "bail pris : prise de relais");
        self.role.send_replace(Role::Taking);
        self.push(tokio::spawn(self.clone().take_over(ctl.clone(), client)));

        // Active : renouvellement, abandon à renew_deadline.
        loop {
            let deadline = clock + Duration::from_millis(last_ok + t.renew_deadline_ms);
            let renew = async {
                tokio::time::sleep(self.retry()).await;
                let start = now();
                let lease = self.coord.get().await?;
                match decide(&self.me, lease.as_ref(), None, &t, now()) {
                    Decision::Write {
                        record,
                        expected,
                        acquire: false,
                    } => {
                        self.coord.write(record, expected).await?;
                        Ok::<_, String>(Some(start))
                    }
                    _ => Ok(None),
                }
            };
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    tracing::error!(identity = %self.me, "bail non renouvelé à temps : la réplique cesse de servir");
                    break;
                }
                r = renew => match r {
                    Ok(Some(start)) => last_ok = start,
                    Ok(None) => {
                        tracing::error!(identity = %self.me, "bail tenu par une autre réplique : la réplique cesse de servir");
                        break;
                    }
                    Err(e) => tracing::warn!(error = %e, "renouvellement du bail"),
                },
            }
        }
        // Perte : plus rien ne sert ni n'écrit, exemples compris.
        ctl.set_active(false);
        self.stop_tasks().await;
        self.role.send_replace(Role::Lost);
    }

    /// Recharge l'état persisté, lance les tâches de l'active, sert, puis pose la marque.
    async fn take_over(self: Arc<Self>, ctl: Arc<Ctl>, client: Option<kube::Client>) {
        let started = std::time::Instant::now();
        if let Some(c) = &client {
            // Garde-fou production avant de servir un sidecar (spec §5.5.1).
            while let Err(e) = crate::kube::load_namespaces(&ctl, c).await {
                tracing::warn!(error = %e, "prise de relais");
                tokio::time::sleep(self.retry()).await;
            }
        }
        ctl.warm_up().await;
        self.push(tokio::spawn(crate::maintenance(ctl.clone())));
        if let Some(c) = client {
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.push(tokio::spawn(crate::kube::watch_namespaces(
                ctl.clone(),
                c.clone(),
            )));
            self.push(tokio::spawn(crate::kube::watch(
                ctl.clone(),
                c.clone(),
                Some(tx),
            )));
            self.push(tokio::spawn(crate::kube::publish_status(ctl.clone(), c)));
            if tokio::time::timeout(INITIAL_LIST, rx).await.is_err() {
                tracing::warn!("liste initiale des MimicWorkload non reçue : l'état persisté sert");
            }
        }
        ctl.set_active(true);
        while let Err(e) = self.coord.announce(&self.me, true).await {
            tracing::warn!(error = %e, "marque de la réplique active");
            tokio::time::sleep(self.retry()).await;
        }
        ctl.metrics
            .takeover_seconds
            .set(started.elapsed().as_secs_f64());
        tracing::info!(identity = %self.me, seconds = started.elapsed().as_secs_f64(), "réplique active");
        self.role.send_replace(Role::Active);
    }
}
