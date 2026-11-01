#[cfg(test)]
mod tests {
    use std::time::Duration;

    use mimic_runtime::config::Env;

    use super::*;

    fn env(extra: &[(&str, &str)]) -> Env {
        Env::from_pairs(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())))
    }

    #[test]
    fn ctl_14_defaults() {
        let config = PushConfig::from_env(&env(&[])).unwrap();
        assert_eq!(config.rate, 10);
        assert_eq!(config.max_report, 32 << 20);
        assert_eq!(config.body_budget, 64 << 20);
        assert_eq!(config.max_push_endpoints, 256);
        assert_eq!(config.store.ttl, Duration::from_secs(30 * 86_400));
        assert_eq!(config.store.limits.max_endpoints, 512);
        assert_eq!(config.store.limits.max_literals, 32);
        assert_eq!(config.store.limits.sample_budget, 4 << 20);
        assert_eq!(config.store.max_deployments, 50);
        assert_eq!(config.status_interval, Duration::from_secs(30));
        assert_eq!(config.pod_window, Duration::from_secs(300));
        assert_eq!(config.ui_addr, "127.0.0.1:8080".parse().unwrap());
    }

    #[test]
    fn ctl_14_valid_values_are_taken() {
        let config = PushConfig::from_env(&env(&[
            ("MIMIC_SIDECAR_API_RATE", "10000"),
            ("MIMIC_SIDECAR_API_MAX_REPORT", "64Ki"),
            ("MIMIC_SIDECAR_API_BODY_BUDGET", "4Gi"),
            ("MIMIC_STORE_TTL", "2h"),
            ("MIMIC_STORE_MAX_ENDPOINTS", "10"),
            ("MIMIC_STORE_MAX_PUSH_ENDPOINTS", "10"),
            ("MIMIC_STORE_MAX_LITERALS", "1000"),
            ("MIMIC_STORE_SAMPLE_BUDGET", "1Gi"),
            ("MIMIC_STORE_MAX_DEPLOYMENTS", "100000"),
            ("MIMIC_STATUS_INTERVAL", "1m"),
            ("MIMIC_STATUS_POD_WINDOW", "24h"),
            ("MIMIC_UI_ADDR", "127.0.0.1:9090"),
        ]))
        .unwrap();
        assert_eq!(config.rate, 10_000);
        assert_eq!(config.max_report, 64 << 10);
        assert_eq!(config.body_budget, 4 << 30);
        assert_eq!(config.store.ttl, Duration::from_secs(7200));
        assert_eq!(config.status_interval, Duration::from_secs(60));
        assert_eq!(config.pod_window, Duration::from_secs(86_400));
        assert_eq!(config.ui_addr.port(), 9090);
        let thirty_days = env(&[("MIMIC_STORE_TTL", "30d")]);
        let config = PushConfig::from_env(&thirty_days).unwrap();
        assert_eq!(config.store.ttl, Duration::from_secs(30 * 86_400));
    }

    #[test]
    fn ctl_14_invalid_values_are_refused_and_named() {
        let invalid: &[(&str, &str)] = &[
            ("MIMIC_SIDECAR_API_RATE", "0"),
            ("MIMIC_SIDECAR_API_RATE", "10001"),
            ("MIMIC_SIDECAR_API_RATE", "fast"),
            ("MIMIC_SIDECAR_API_MAX_REPORT", "63Ki"),
            ("MIMIC_SIDECAR_API_MAX_REPORT", "2Gi"),
            ("MIMIC_SIDECAR_API_MAX_REPORT", "32Mb"),
            ("MIMIC_SIDECAR_API_MAX_REPORT", "Mi"),
            ("MIMIC_SIDECAR_API_BODY_BUDGET", "5Gi"),
            ("MIMIC_SIDECAR_API_BODY_BUDGET", "31Mi"),
            ("MIMIC_STORE_TTL", "9s"),
            ("MIMIC_STORE_TTL", "366d"),
            ("MIMIC_STORE_TTL", "30"),
            ("MIMIC_STORE_TTL", "30w"),
            ("MIMIC_STORE_MAX_ENDPOINTS", "0"),
            ("MIMIC_STORE_MAX_ENDPOINTS", "10001"),
            ("MIMIC_STORE_MAX_PUSH_ENDPOINTS", "0"),
            ("MIMIC_STORE_MAX_PUSH_ENDPOINTS", "513"),
            ("MIMIC_STORE_MAX_LITERALS", "1"),
            ("MIMIC_STORE_MAX_LITERALS", "1001"),
            ("MIMIC_STORE_SAMPLE_BUDGET", "512Ki"),
            ("MIMIC_STORE_SAMPLE_BUDGET", "2Gi"),
            ("MIMIC_STORE_MAX_DEPLOYMENTS", "0"),
            ("MIMIC_STORE_MAX_DEPLOYMENTS", "100001"),
            ("MIMIC_STATUS_INTERVAL", "0s"),
            ("MIMIC_STATUS_INTERVAL", "2h"),
            ("MIMIC_STATUS_POD_WINDOW", "0s"),
            ("MIMIC_STATUS_POD_WINDOW", "25h"),
            ("MIMIC_UI_ADDR", "localhost"),
            ("MIMIC_UI_ADDR", ":8080"),
        ];
        for (var, value) in invalid {
            let error = PushConfig::from_env(&env(&[(var, value)]))
                .err()
                .unwrap_or_else(|| panic!("{var}={value} was accepted"));
            assert!(error.to_string().contains(var), "{var}={value}: {error}");
        }
    }

    #[test]
    fn ctl_14_the_budget_covers_a_report_and_a_push_fits_the_store() {
        let error = PushConfig::from_env(&env(&[
            ("MIMIC_SIDECAR_API_MAX_REPORT", "2Mi"),
            ("MIMIC_SIDECAR_API_BODY_BUDGET", "1Mi"),
        ]))
        .err()
        .expect("budget below the largest push");
        assert!(error.to_string().contains("MIMIC_SIDECAR_API_BODY_BUDGET"));
        // The default push limit (256) is above this store limit.
        let error = PushConfig::from_env(&env(&[("MIMIC_STORE_MAX_ENDPOINTS", "100")]))
            .err()
            .expect("push limit above the store limit");
        assert!(error.to_string().contains("MIMIC_STORE_MAX_PUSH_ENDPOINTS"));
        let ok = env(&[
            ("MIMIC_STORE_MAX_ENDPOINTS", "100"),
            ("MIMIC_STORE_MAX_PUSH_ENDPOINTS", "100"),
        ]);
        assert!(PushConfig::from_env(&ok).is_ok());
    }
}
