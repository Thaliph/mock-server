//! Webhook d'admission mutant qui injecte `mimic-init` et le sidecar `mimic` dans les
//! pods (spec §9.4). La mutation est une fonction pure (`inject`) ; le serveur n'est
//! qu'un adaptateur HTTPS autour d'elle.

pub mod config;
pub mod inject;
pub mod server;
