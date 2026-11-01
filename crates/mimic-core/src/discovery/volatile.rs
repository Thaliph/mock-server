//! Volatile field detection (spec §6). Stub for the red commit.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sketch {}

impl Sketch {
    pub fn insert(&mut self, _value: &str) {
        todo!()
    }

    pub fn merge(self, _other: Self) -> Self {
        todo!()
    }

    pub fn len(&self) -> usize {
        todo!()
    }

    pub fn distinct(&self) -> u64 {
        todo!()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volatility {}

impl Volatility {
    pub fn observe(&mut self, _field: &str, _value: &str, _redacted: bool) {
        todo!()
    }

    pub fn merge(self, _other: Self) -> Self {
        todo!()
    }

    pub fn len(&self) -> usize {
        todo!()
    }

    pub fn fields(&self) -> Vec<&str> {
        todo!()
    }

    pub fn is_volatile(&self, _field: &str) -> bool {
        todo!()
    }

    pub fn is_redacted(&self, _field: &str) -> bool {
        todo!()
    }
}
