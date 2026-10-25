//! `mimicctl install` (spec §9.5b) : manifests du socle, embarqués dans le binaire, écrits
//! sur la sortie standard. Fonction pure : rien n'est appliqué.

/// Rendus de `kubectl kustomize deploy/controller*` (scripts/render-install.sh).
const CONTROLLER_SHARED: &str = include_str!("../../../deploy/install/controller.yaml");
const CONTROLLER_RWO: &str = include_str!("../../../deploy/install/controller-rwo.yaml");
const CONTROLLER_S3: &str = include_str!("../../../deploy/install/controller-s3.yaml");
const INJECTOR: &str = include_str!("../../../deploy/injector/injector.yaml");
const CNI: &str = include_str!("../../../deploy/cni/mimic-cni.yaml");
const PRODUCTION_GUARD: &str = include_str!("../../../deploy/controller/production-guard.yaml");

/// Images écrites dans les manifests livrés : remplacées telles quelles, sans analyser le YAML.
const SHIPPED_IMAGE: &str = "ghcr.io/thaliph/mimic:0.1.0";
const SHIPPED_INIT_IMAGE: &str = "ghcr.io/thaliph/mimic-init:0.1.0";
/// Ligne commentée de la configuration du webhook que `--cni` active.
const SHIPPED_CNI_OFF: &str = "\n    # cni: true\n";
const CNI_ON: &str = "\n    cni: true\n";

const DEFAULT_REPOSITORY: &str = "ghcr.io/thaliph/";

/// Support du store du contrôleur : choisit la variante du socle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Storage {
    /// PVC `ReadWriteMany` (`deploy/controller`).
    #[default]
    Shared,
    /// PVC `ReadWriteOnce`, répliques sur le nœud du volume (`deploy/controller-rwo`).
    Rwo,
    /// Store `s3://`, sans PVC (`deploy/controller-s3`).
    S3,
}

impl Storage {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "shared" => Ok(Self::Shared),
            "rwo" => Ok(Self::Rwo),
            "s3" => Ok(Self::S3),
            other => Err(format!(
                "stockage inconnu « {other} » : shared, rwo ou s3 attendu"
            )),
        }
    }

    fn controller(self) -> &'static str {
        match self {
            Self::Shared => CONTROLLER_SHARED,
            Self::Rwo => CONTROLLER_RWO,
            Self::S3 => CONTROLLER_S3,
        }
    }
}

/// Options de `mimicctl install`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallOptions {
    pub storage: Storage,
    /// `<registre>/mimic:<tag>` ; par défaut `ghcr.io/thaliph/mimic:<version de mimicctl>`.
    pub image: Option<String>,
    /// Ajoute le DaemonSet `mimic-cni` et `cni: true` dans la configuration du webhook.
    pub cni: bool,
    /// Ajoute la `ValidatingAdmissionPolicy` du garde-fou production.
    pub production_guard: bool,
}

/// Image et image d'init déduite (`<registre>/mimic-init:<tag>`) ; refusée si elle n'a pas
/// de tag, si son nom n'est pas `mimic` ou si elle contient un caractère hors d'une référence
/// d'image (rien ne doit pouvoir s'injecter dans le YAML).
fn images(image: &str) -> Result<(String, String), String> {
    let refuse = |why: &str| format!("image « {image} » refusée : {why} (<registre>/mimic:<tag>)");
    let (prefix, name_tag) = image.split_at(image.rfind('/').map_or(0, |i| i + 1));
    let Some((name, tag)) = name_tag.split_once(':') else {
        return Err(refuse("tag absent"));
    };
    if name != "mimic" {
        return Err(refuse("le nom de l'image doit être mimic"));
    }
    let tag_ok = !tag.is_empty()
        && tag.len() <= 128
        && !tag.starts_with(['.', '-'])
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    if !tag_ok {
        return Err(refuse("tag invalide"));
    }
    let prefix_ok = prefix.bytes().all(|b| {
        b.is_ascii_lowercase()
            || b.is_ascii_digit()
            || matches!(b, b'.' | b'-' | b'_' | b'/' | b':')
    }) && !prefix.starts_with('/')
        && !prefix.contains("//");
    if !prefix_ok {
        return Err(refuse("dépôt invalide"));
    }
    Ok((image.to_owned(), format!("{prefix}mimic-init:{tag}")))
}

/// Manifests à appliquer, dans l'ordre : le socle de la variante, le webhook, puis le plugin
/// CNI et le garde-fou production s'ils sont demandés ; documents séparés par `---`.
pub fn manifests(options: &InstallOptions) -> Result<String, String> {
    let (image, init_image) = match &options.image {
        Some(image) => images(image)?,
        None => {
            let version = env!("CARGO_PKG_VERSION");
            (
                format!("{DEFAULT_REPOSITORY}mimic:{version}"),
                format!("{DEFAULT_REPOSITORY}mimic-init:{version}"),
            )
        }
    };
    let mut injector = INJECTOR.to_owned();
    if options.cni {
        injector = injector.replacen(SHIPPED_CNI_OFF, CNI_ON, 1);
    }
    let mut parts = vec![options.storage.controller(), &injector];
    if options.cni {
        parts.push(CNI);
    }
    if options.production_guard {
        parts.push(PRODUCTION_GUARD);
    }
    let mut out = String::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push_str("---\n");
        }
        out.push_str(part);
        if !part.ends_with('\n') {
            out.push('\n');
        }
    }
    Ok(out
        .replace(SHIPPED_IMAGE, &image)
        .replace(SHIPPED_INIT_IMAGE, &init_image))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Les remplacements exacts supposent que les manifests livrés portent ces chaînes.
    #[test]
    fn shipped_manifests_carry_the_replaced_strings() {
        for m in [
            CONTROLLER_SHARED,
            CONTROLLER_RWO,
            CONTROLLER_S3,
            INJECTOR,
            CNI,
        ] {
            assert!(m.contains(SHIPPED_IMAGE));
        }
        assert!(INJECTOR.contains(SHIPPED_INIT_IMAGE));
        assert_eq!(INJECTOR.matches(SHIPPED_CNI_OFF).count(), 1);
    }
}
