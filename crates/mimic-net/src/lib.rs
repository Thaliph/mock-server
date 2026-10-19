//! Serveurs HTTPS du plan de contrôle de mimic, communs au webhook d'injection et au
//! contrôleur (spec §9.2b) : certificat relu à chaud quand cert-manager le renouvelle,
//! poignée de main et en-têtes bornés dans le temps, pause après une erreur d'`accept`,
//! arrêt propre.

pub mod serve;
pub mod tls;
