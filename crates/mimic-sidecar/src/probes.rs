//! stub
use std::sync::Arc;

use axum::Router;
use mimic_runtime::config::{ConfigError, Env};

pub struct Probes;

impl Probes {
    pub fn from_env(_env: &Env) -> Result<Probes, ConfigError> {
        Ok(Probes)
    }
}

pub fn router(_probes: Arc<Probes>) -> Router {
    Router::new()
}
