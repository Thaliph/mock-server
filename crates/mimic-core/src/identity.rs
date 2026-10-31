//! Workload identity of a pod (spec §7.2): which pod names a deployment can generate.

/// Whether `pod` is a name the ReplicaSet of `deployment` can generate:
/// `<deployment>-<pod-template-hash>-<5 chars>`, the part before the 5 chars truncated to 58.
pub fn pod_of_deployment(pod: &str, deployment: &str) -> bool {
    let _ = (pod, deployment);
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_06_generated_names_are_accepted_for_their_deployment_only() {
        assert!(pod_of_deployment("pay-7d9f8b6c4d-x2k9p", "pay"));
        assert!(!pod_of_deployment("pay-7d9f8b6c4d", "pay"));
        assert!(!pod_of_deployment("pay-7d9f8b6c4d-x2k9p", "pa"));
        assert!(!pod_of_deployment("pay-7d9f8b6c4d-x2k9p", "pay-7d9f8b6c4d"));
        assert!(!pod_of_deployment("pay-7d9f8b6c4d-x2k9p", ""));
        assert!(!pod_of_deployment("", "pay"));
    }

    #[test]
    fn tls_06_nested_deployment_names_are_told_apart() {
        assert!(pod_of_deployment("a-b-6d4c8f9b7-xxxxx", "a-b"));
        assert!(!pod_of_deployment("a-b-6d4c8f9b7-xxxxx", "a"));
        assert!(pod_of_deployment("a-6d4c8f9b7-xxxxx", "a"));
        assert!(!pod_of_deployment("a-6d4c8f9b7-xxxxx", "a-b"));
    }

    #[test]
    fn tls_06_truncated_generated_prefixes_are_accepted() {
        // 63 characters: only the 58-character prefix is left of the deployment name.
        let long = "d".repeat(63);
        assert!(pod_of_deployment(
            &format!("{}x2k9p", &long[..58]),
            &long
        ));
        assert!(!pod_of_deployment(
            &format!("{}x2k9p", &long[..57]),
            &long
        ));
        assert!(!pod_of_deployment(
            &format!("{}x2k9p", &long[..58]),
            &"e".repeat(63)
        ));
        // 50 characters: "<name>-<hash>-" is cut at 58, in the middle of the hash.
        let mid = "m".repeat(50);
        assert!(pod_of_deployment(&format!("{mid}-7d9f8b6x2k9p"), &mid));
        assert!(!pod_of_deployment(&format!("{mid}-7d9f8bax2k9p"), &mid));
        assert!(!pod_of_deployment(&format!("{mid}x-7d9f8b6x2k9p"), &mid));
    }

    #[test]
    fn tls_06_characters_outside_the_alphabet_are_rejected() {
        assert!(!pod_of_deployment("pay-7d9f8b6c4a-x2k9p", "pay"));
        assert!(!pod_of_deployment("pay-7d9f8b6c4d-x2k9a", "pay"));
        assert!(!pod_of_deployment("pay-7d9f8b6c4d-x2k9P", "pay"));
        assert!(!pod_of_deployment("pay--x2k9p", "pay"));
        assert!(!pod_of_deployment("pay-7d9f8b6c4d5-x2k9p", "pay"));
    }
}
