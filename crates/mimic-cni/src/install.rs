//! Installation du plugin sur un nœud (spec §3.1b) : kubeconfig, jeton et CA dans
//! `mimic-cni.d/`, et entrée `mimic-cni` en fin de chaîne du premier fichier de
//! configuration CNI.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// Sous-répertoire de la configuration CNI réservé au plugin.
pub const OWN_DIR: &str = "mimic-cni.d";
const TYPE: &str = "mimic-cni";

/// Répertoires vus depuis le conteneur d'installation, et depuis l'hôte.
#[derive(Debug, Clone)]
pub struct Dirs {
    /// Configuration CNI de l'hôte, montée dans le conteneur (`/host/etc/cni/net.d`).
    pub conf_dir: PathBuf,
    /// La même, vue par l'hôte : chemins écrits dans la configuration (`/etc/cni/net.d`).
    pub host_conf_dir: String,
    /// Jeton et CA du ServiceAccount du DaemonSet.
    pub service_account_dir: PathBuf,
    /// Serveur de l'API, joignable depuis le nœud.
    pub server: String,
}

/// Premier fichier de configuration (ordre lexical), comme le runtime le choisit.
fn primary(conf_dir: &Path) -> Result<Option<PathBuf>, String> {
    let mut names: Vec<PathBuf> = std::fs::read_dir(conf_dir)
        .map_err(|e| format!("{}: {e}", conf_dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("conflist" | "conf" | "json")
            )
        })
        .collect();
    names.sort();
    Ok(names.into_iter().next())
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Écrit d'abord dans un fichier voisin, puis le renomme : le runtime ne lit jamais un
/// fichier à moitié écrit.
fn write_atomic(path: &Path, content: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("mimic-tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Configuration avec `mimic-cni` en fin de chaîne ; `None` s'il y est déjà. Un fichier
/// d'un seul plugin devient une liste.
pub fn chained(conf: &Value, kubeconfig: &str) -> Option<Value> {
    let ours = json!({"type": TYPE, "kubeconfig": kubeconfig});
    if let Some(plugins) = conf["plugins"].as_array() {
        if plugins.iter().any(|p| p["type"] == TYPE) {
            return None;
        }
        let mut out = conf.clone();
        out["plugins"].as_array_mut()?.push(ours);
        return Some(out);
    }
    let mut single = conf.clone();
    let object = single.as_object_mut()?;
    let version = object.remove("cniVersion").unwrap_or(json!("0.4.0"));
    let name = object.remove("name").unwrap_or(json!("k8s-pod-network"));
    Some(json!({"cniVersion": version, "name": name, "plugins": [single, ours]}))
}

/// Configuration sans `mimic-cni` ; `None` s'il n'y était pas.
pub fn unchained(conf: &Value) -> Option<Value> {
    let plugins = conf["plugins"].as_array()?;
    if !plugins.iter().any(|p| p["type"] == TYPE) {
        return None;
    }
    let mut out = conf.clone();
    out["plugins"] = Value::Array(
        plugins
            .iter()
            .filter(|p| p["type"] != TYPE)
            .cloned()
            .collect(),
    );
    Some(out)
}

/// Kubeconfig du plugin : chemins de l'hôte.
pub fn kubeconfig(server: &str, ca: &str, token: &str) -> String {
    format!(
        "apiVersion: v1\nkind: Config\nclusters:\n  - name: local\n    cluster:\n      server: {server}\n      certificate-authority: {ca}\nusers:\n  - name: mimic-cni\n    user:\n      tokenFile: {token}\ncontexts:\n  - name: mimic-cni\n    context: {{cluster: local, user: mimic-cni}}\ncurrent-context: mimic-cni\n"
    )
}

/// Installe ou rafraîchit : jeton, CA et kubeconfig recopiés, plugin chaîné s'il manque.
pub fn install(d: &Dirs) -> Result<(), String> {
    let own = d.conf_dir.join(OWN_DIR);
    std::fs::create_dir_all(&own).map_err(|e| format!("{}: {e}", own.display()))?;
    for file in ["token", "ca.crt"] {
        let content = std::fs::read(d.service_account_dir.join(file))
            .map_err(|e| format!("{file} du ServiceAccount: {e}"))?;
        write_atomic(&own.join(file), &content)?;
    }
    let host_own = format!("{}/{OWN_DIR}", d.host_conf_dir.trim_end_matches('/'));
    let kubeconfig_path = format!("{host_own}/kubeconfig");
    write_atomic(
        &own.join("kubeconfig"),
        kubeconfig(
            &d.server,
            &format!("{host_own}/ca.crt"),
            &format!("{host_own}/token"),
        )
        .as_bytes(),
    )?;
    let Some(path) = primary(&d.conf_dir)? else {
        return Err(format!(
            "aucune configuration CNI dans {} : le fournisseur réseau n'est pas encore installé",
            d.conf_dir.display()
        ));
    };
    let conf = read_json(&path)?;
    let Some(new) = chained(&conf, &kubeconfig_path) else {
        return Ok(());
    };
    let target = if path.extension().and_then(|e| e.to_str()) == Some("conflist") {
        path.clone()
    } else {
        path.with_extension("conflist")
    };
    let text = serde_json::to_vec_pretty(&new).map_err(|e| e.to_string())?;
    write_atomic(&target, &text)?;
    if target != path {
        std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

/// Retire le plugin de la chaîne (arrêt du DaemonSet).
pub fn uninstall(d: &Dirs) -> Result<(), String> {
    let Some(path) = primary(&d.conf_dir)? else {
        return Ok(());
    };
    let conf = read_json(&path)?;
    if let Some(new) = unchained(&conf) {
        let text = serde_json::to_vec_pretty(&new).map_err(|e| e.to_string())?;
        write_atomic(&path, &text)?;
    }
    Ok(())
}
