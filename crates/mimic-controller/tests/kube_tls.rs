//! Le client Kubernetes du contrôleur se construit sans panique : deux fournisseurs
//! cryptographiques rustls (ring, et aws-lc-rs tiré par object_store) sont compilés,
//! rustls ne peut donc pas en choisir un seul (échec vu sur kind, E2E-05).

#[tokio::test]
async fn kube_client_builds_with_both_crypto_providers_compiled() {
    mimic_controller::install_crypto_provider();
    let config = kube::Config::new("https://127.0.0.1:6443".parse().unwrap());
    kube::Client::try_from(config).unwrap();
}
