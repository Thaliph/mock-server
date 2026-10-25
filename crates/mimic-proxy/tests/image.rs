//! Critère IMG-01 (spec §9.4b) : deux images sans shell superflu, bases épinglées, publication
//! signée.

const DOCKERFILE: &str = include_str!("../../../Dockerfile");
const RELEASE: &str = include_str!("../../../.github/workflows/release.yml");

/// Valeur d'un `ARG nom=valeur` du Dockerfile.
fn arg(name: &str) -> String {
    let prefix = format!("ARG {name}=");
    DOCKERFILE
        .lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("ARG {name} absent"))
        .trim()
        .to_string()
}

/// Lignes de la cible `name`, du `FROM … AS name` au `FROM` suivant.
fn stage(name: &str) -> Vec<&'static str> {
    let mut lines = DOCKERFILE.lines().skip_while(|l| {
        !(l.starts_with("FROM ") && l.trim_end().ends_with(&format!(" AS {name}")))
    });
    let first = lines
        .next()
        .unwrap_or_else(|| panic!("cible {name} absente"));
    std::iter::once(first)
        .chain(lines.take_while(|l| !l.starts_with("FROM ")))
        .collect()
}

#[test]
fn img_01_two_pinned_non_root_images() {
    for name in ["RUST_IMAGE", "RUNTIME_IMAGE", "INIT_IMAGE"] {
        let image = arg(name);
        assert!(image.contains("@sha256:"), "{name} non épinglée : {image}");
    }
    assert!(arg("RUNTIME_IMAGE").starts_with("gcr.io/distroless/cc-debian13:nonroot@"));
    assert!(arg("INIT_IMAGE").starts_with("debian:trixie-slim@"));
    // Chaque FROM passe par un ARG épinglé ou une cible de ce Dockerfile.
    for l in DOCKERFILE.lines().filter(|l| l.starts_with("FROM ")) {
        assert!(l.starts_with("FROM ${"), "FROM non épinglé : {l}");
    }

    // La chaîne Rust est celle de l'image, jamais téléchargée pendant la construction.
    let rust = arg("RUST_IMAGE");
    let version = rust
        .strip_prefix("rust:")
        .and_then(|r| r.split('-').next())
        .unwrap();
    assert!(
        DOCKERFILE.contains(&format!("RUSTUP_TOOLCHAIN={version}")),
        "RUSTUP_TOOLCHAIN ≠ {version}"
    );
    // Plugin CNI statique : il tourne avec la libc du nœud.
    assert!(DOCKERFILE.contains("-unknown-linux-musl"), "cible musl");

    let runtime = stage("runtime").join("\n");
    let init = stage("init").join("\n");
    for (name, text) in [("runtime", &runtime), ("init", &init)] {
        assert!(text.contains("USER 1338:1338"), "{name} : USER 1338:1338");
    }
    for bin in [
        "mimic-proxy",
        "mimic-controller",
        "mimic-injector",
        "mimicctl",
        "mimic-cni",
    ] {
        assert!(runtime.contains(bin), "runtime sans {bin}");
    }
    assert!(
        !runtime.contains("apt-get"),
        "runtime sans gestionnaire de paquets"
    );
    assert!(init.contains("iptables") && init.contains("mimic-init"));
    assert!(
        DOCKERFILE
            .trim_end()
            .ends_with("ENTRYPOINT [\"/usr/local/bin/mimic-proxy\"]"),
        "runtime en dernier : cible par défaut"
    );

    // Publication : deux images, SBOM, provenance, signature.
    for needle in [
        "mimic-init",
        "target: init",
        "target: runtime",
        "sbom:",
        "provenance:",
        "cosign sign",
    ] {
        assert!(RELEASE.contains(needle), "release.yml sans {needle}");
    }
}
