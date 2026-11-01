//! Read API on `MIMIC_UI_ADDR` (spec §7.5).

use std::sync::Arc;

use axum::Router;

use crate::store::Store;

pub fn router(_store: Arc<Store>) -> Router {
    Router::new()
}

#[cfg(test)]
mod tests;
