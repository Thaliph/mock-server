//! Barrière de démarrage d'un sidecar piloté par le contrôleur (spec §5.5.5).
//!
//! Fonction pure : le sidecar lui signale les états reçus, son alignement et l'échéance de
//! son délai ; elle décide si l'application peut démarrer et s'il faut appliquer le repli.
//! L'horloge et le réseau restent dans le sidecar.

use serde::{Deserialize, Serialize};

/// Repli quand aucun état souhaité n'est connu à l'échéance du délai (`share.startup.fallback`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Fallback {
    /// Mode `record` : l'application démarre et joint ses dépendances.
    #[default]
    Record,
    /// Mode `passthrough`.
    Passthrough,
    /// Mode `mock` sur les données locales : rien ne sort.
    MockLocal,
    /// Barrière fermée tant que le contrôleur n'a pas répondu ; attente en mock.
    Fail,
}

impl Fallback {
    pub fn as_str(self) -> &'static str {
        match self {
            Fallback::Record => "record",
            Fallback::Passthrough => "passthrough",
            Fallback::MockLocal => "mock-local",
            Fallback::Fail => "fail",
        }
    }

    /// Vrai si ce repli ne laisse rien sortir sans état connu : le sidecar attend en mock.
    pub fn mocks(self) -> bool {
        matches!(self, Fallback::MockLocal | Fallback::Fail)
    }
}

/// État de la barrière, exposé par `/readyz` et `mimic_startup{state}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Fermée : ni alignement, ni échéance (ou `fail` sans état connu).
    Waiting,
    /// État souhaité reçu et appliqué, snapshot chargé s'il en désigne un.
    Aligned,
    /// Délai écoulé : l'état connu (reçu sans son snapshot, ou rechargé) reste appliqué.
    LastKnown,
    /// Délai écoulé sans état connu : le repli est appliqué.
    Fallback,
}

impl Phase {
    pub const ALL: [Phase; 4] = [
        Phase::Waiting,
        Phase::Aligned,
        Phase::LastKnown,
        Phase::Fallback,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Waiting => "waiting",
            Phase::Aligned => "aligned",
            Phase::LastKnown => "last-known",
            Phase::Fallback => "fallback",
        }
    }

    pub fn to_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(v: u8) -> Phase {
        Phase::ALL[v as usize % Phase::ALL.len()]
    }

    /// L'application peut démarrer.
    pub fn is_open(self) -> bool {
        self != Phase::Waiting
    }
}

/// Barrière de démarrage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    phase: Phase,
    /// Un état souhaité est connu : reçu pendant ce démarrage, ou rechargé du disque.
    known: bool,
    /// Le délai est écoulé.
    expired: bool,
    fallback: Fallback,
}

impl Gate {
    pub fn new(fallback: Fallback) -> Self {
        Self {
            phase: Phase::Waiting,
            known: false,
            expired: false,
            fallback,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn is_open(&self) -> bool {
        self.phase.is_open()
    }

    pub fn known(&self) -> bool {
        self.known
    }

    pub fn fallback(&self) -> Fallback {
        self.fallback
    }

    /// Sans état connu, le sidecar attend en mock avec un repli qui ne laisse rien sortir.
    pub fn waits_in_mock(&self) -> bool {
        !self.known && self.fallback.mocks()
    }

    /// Un état souhaité est appliqué sans être complet (snapshot pas encore chargé), ou
    /// rechargé du disque. Après l'échéance, il ouvre la barrière, ou remplace le repli.
    pub fn received(&mut self) {
        self.known = true;
        if self.expired && matches!(self.phase, Phase::Waiting | Phase::Fallback) {
            self.phase = Phase::LastKnown;
        }
    }

    /// L'état souhaité est appliqué en entier : la barrière s'ouvre.
    pub fn aligned(&mut self) {
        self.known = true;
        self.phase = Phase::Aligned;
    }

    /// Échéance du délai. `Some(repli)` : le sidecar doit appliquer le mode de ce repli.
    pub fn timeout(&mut self) -> Option<Fallback> {
        if self.expired || self.phase != Phase::Waiting {
            self.expired = true;
            return None;
        }
        self.expired = true;
        if self.known {
            self.phase = Phase::LastKnown;
            return None;
        }
        match self.fallback {
            Fallback::Fail => None,
            f => {
                self.phase = Phase::Fallback;
                Some(f)
            }
        }
    }
}
