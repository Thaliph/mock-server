//! Exemples par endpoint (spec §5.5.3) : ébauche, implémentée après les tests.

use std::sync::Arc;

use crate::model::Exchange;

/// Borne par défaut du nombre d'endpoints gardés avec un exemple, par workload.
pub const DEFAULT_MAX_ENDPOINTS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExemplarOffer {
    Added,
    Replaced,
    Ignored,
    Refused,
}

pub fn endpoint_key(_e: &Exchange) -> String {
    String::new()
}

pub fn is_exemplar(_e: &Exchange) -> bool {
    false
}

#[derive(Debug, Clone, Default)]
pub struct Exemplars {
    list: Vec<Arc<Exchange>>,
}

impl Exemplars {
    pub fn new(_max_endpoints: usize) -> Self {
        Self::default()
    }

    pub fn offer(&mut self, _e: &Exchange) -> ExemplarOffer {
        ExemplarOffer::Ignored
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn refused(&self) -> u64 {
        0
    }

    pub fn version(&self) -> u64 {
        0
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<Exchange>> {
        self.list.iter()
    }

    pub fn to_jsonl(&self) -> Vec<u8> {
        Vec::new()
    }

    pub fn parse(_bytes: &[u8]) -> Vec<Exchange> {
        Vec::new()
    }
}
