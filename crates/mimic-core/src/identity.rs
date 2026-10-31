//! Workload identity of a pod (spec §7.2): which pod names a deployment can generate.

/// Characters of a `pod-template-hash` and of the random suffix of a generated name.
const ALPHABET: &str = "bcdfghjklmnpqrstvwxz2456789";
/// Longest generated-name prefix: 63 minus the 5 random characters.
const PREFIX_MAX: usize = 58;
const SUFFIX_LEN: usize = 5;
/// Longest `pod-template-hash` (a 32-bit hash in the alphabet).
const HASH_MAX: usize = 10;

fn in_alphabet(s: &str) -> bool {
    s.chars().all(|c| ALPHABET.contains(c))
}

/// Whether `pod` is a name the ReplicaSet of `deployment` can generate:
/// `<deployment>-<pod-template-hash>-<5 chars>`, the part before the 5 chars truncated to 58.
pub fn pod_of_deployment(pod: &str, deployment: &str) -> bool {
    if deployment.is_empty() || !pod.is_ascii() || !deployment.is_ascii() || pod.len() <= SUFFIX_LEN
    {
        return false;
    }
    let (base, suffix) = pod.split_at(pod.len() - SUFFIX_LEN);
    if !in_alphabet(suffix) || base.len() > PREFIX_MAX {
        return false;
    }
    complete(base, deployment) || (base.len() == PREFIX_MAX && truncated(base, deployment))
}

/// `base` is `<deployment>-<hash>-`.
fn complete(base: &str, deployment: &str) -> bool {
    base.strip_suffix('-')
        .and_then(|b| b.rsplit_once('-'))
        .is_some_and(|(name, hash)| {
            name == deployment && (1..=HASH_MAX).contains(&hash.len()) && in_alphabet(hash)
        })
}

/// `base` is the first 58 characters of a longer `<deployment>-<hash>-`.
fn truncated(base: &str, deployment: &str) -> bool {
    if deployment.len() >= PREFIX_MAX {
        return base == &deployment[..PREFIX_MAX];
    }
    base.strip_prefix(deployment)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|hash| hash.len() <= HASH_MAX && in_alphabet(hash))
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
        assert!(pod_of_deployment(&format!("{}x2k9p", &long[..58]), &long));
        assert!(!pod_of_deployment(&format!("{}x2k9p", &long[..57]), &long));
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
