//! A status condition of a Kubernetes object (`metav1.Condition`), as the pure parts compute
//! it: the mesh's `MimicConfig` and `HTTPRoute` statuses (spec §7.9) and the Gateway API status
//! of the mimic Gateway (§7.10).

/// The longest condition message (the CRDs' `maxLength`).
pub const MAX_MESSAGE: usize = 1024;

/// `text` cut to [`MAX_MESSAGE`] bytes on a character boundary (status messages).
pub fn bounded(text: &str) -> String {
    let mut end = text.len().min(MAX_MESSAGE);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// One status condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub ok: bool,
    pub reason: String,
    pub message: String,
}

impl Condition {
    pub fn ok(reason: &str) -> Condition {
        Condition {
            ok: true,
            reason: reason.into(),
            message: String::new(),
        }
    }

    pub fn refused(reason: &str, message: String) -> Condition {
        Condition {
            ok: false,
            reason: reason.into(),
            message,
        }
    }

    /// The condition as a `metav1.Condition` of type `kind`: `message` always present (a required
    /// field) and cut to 1024 bytes; `previous` is the cluster's condition of the same type, whose
    /// `lastTransitionTime` stays while the `status` does not change, else it is `now` (RTE-14).
    pub fn to_json(
        &self,
        kind: &str,
        generation: i64,
        previous: Option<&serde_json::Value>,
        now: &str,
    ) -> serde_json::Value {
        let status = if self.ok { "True" } else { "False" };
        let since = previous
            .filter(|p| p["status"] == status)
            .and_then(|p| p["lastTransitionTime"].as_str())
            .unwrap_or(now);
        serde_json::json!({
            "type": kind,
            "status": status,
            "reason": self.reason,
            "message": bounded(&self.message),
            "observedGeneration": generation,
            "lastTransitionTime": since,
        })
    }
}
