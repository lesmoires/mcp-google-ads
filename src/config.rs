use std::path::PathBuf;

use crate::error::Result;

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_path_or(key: &str, default: &str) -> PathBuf {
    expand_tilde(&env_or(key, default))
}

fn env_f64_or(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u32_or(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_bool_or(key: &str, default: bool) -> bool {
    std::env::var(key)
        .ok()
        .map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(default)
}

fn env_list(key: &str) -> Vec<String> {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .map(|v| v.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default()
}

/// Normalise a Google Ads customer ID to its bare digits.
///
/// The API accepts `123-456-7890` in config/env and `1234567890` from callers
/// (and vice versa). Comparing the raw strings would let a caller slip past an
/// allow-list simply by changing the separator, so every comparison — and the
/// allow-list itself — is done on digits only.
pub fn normalize_customer_id(cid: &str) -> String {
    cid.chars().filter(|c| c.is_ascii_digit()).collect()
}

#[derive(Debug, Clone)]
pub struct GoogleConfig {
    pub credentials_path: PathBuf,
    pub token_path: PathBuf,
}

impl Default for GoogleConfig {
    fn default() -> Self {
        Self {
            credentials_path: expand_tilde("~/.mcp-google-ads/credentials.json"),
            token_path: expand_tilde("~/.mcp-google-ads/token.json"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AdsConfig {
    pub developer_token: String,
    pub customer_id: String,
    pub login_customer_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SafetyConfig {
    pub max_daily_budget: f64,
    pub max_bid_increase_pct: u32,
    pub require_dry_run: bool,
    pub log_file: PathBuf,
    pub blocked_operations: Vec<String>,
    /// Directory backing the pending/applied plan store. Point this at a
    /// shared mount so drafted plans survive a container restart, not just
    /// the per-call process spawn.
    pub plan_store_dir: PathBuf,
    /// Customer IDs this server is permitted to read or write.
    ///
    /// Empty = unrestricted (the historical behaviour). This is deliberately
    /// opt-in: an operator must enumerate the accounts in scope rather than
    /// inherit "every account the token can reach" by default. Values are
    /// stored normalised to bare digits — see [`normalize_customer_id`].
    pub allowed_customer_ids: Vec<String>,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            max_daily_budget: 50.0,
            max_bid_increase_pct: 100,
            require_dry_run: true,
            log_file: expand_tilde("~/.mcp-google-ads/audit.log"),
            blocked_operations: Vec::new(),
            plan_store_dir: expand_tilde("~/.mcp-google-ads/plans"),
            allowed_customer_ids: Vec::new(),
        }
    }
}

impl SafetyConfig {
    /// Is `customer_id` within the configured allow-list?
    ///
    /// An empty list means unrestricted — the guard is inert until an operator
    /// opts in. Returns `(allowed, reason)` so callers can surface *why* a call
    /// was refused instead of a bare boolean.
    pub fn customer_allowed(&self, customer_id: &str) -> (bool, String) {
        if self.allowed_customer_ids.is_empty() {
            return (true, "GOOGLE_ADS_ALLOWED_CIDS not set (unrestricted)".to_string());
        }

        let requested = normalize_customer_id(customer_id);
        if requested.is_empty() {
            return (
                false,
                "customer_id is empty or contains no digits; refusing".to_string(),
            );
        }

        if self.allowed_customer_ids.iter().any(|a| *a == requested) {
            return (true, format!("customer_id {requested} is allow-listed"));
        }

        (
            false,
            format!(
                "customer_id {requested} is not in GOOGLE_ADS_ALLOWED_CIDS \
                 (allowed: {})",
                self.allowed_customer_ids.join(", ")
            ),
        )
    }
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub google: GoogleConfig,
    pub ads: AdsConfig,
    pub safety: SafetyConfig,
    /// When true, only READ tools are exposed. All write/mutate tools are hidden.
    pub read_only: bool,
}

impl Config {
    /// Load configuration entirely from environment variables.
    ///
    /// | Env var | Default |
    /// |---|---|
    /// | `GOOGLE_ADS_CREDENTIALS_PATH` | `~/.mcp-google-ads/credentials.json` |
    /// | `GOOGLE_ADS_TOKEN_PATH` | `~/.mcp-google-ads/token.json` |
    /// | `GOOGLE_ADS_DEVELOPER_TOKEN` | (empty) |
    /// | `GOOGLE_ADS_CUSTOMER_ID` | (empty) |
    /// | `GOOGLE_ADS_LOGIN_CUSTOMER_ID` | (none) |
    /// | `GOOGLE_ADS_MAX_DAILY_BUDGET` | `50.0` |
    /// | `GOOGLE_ADS_MAX_BID_INCREASE_PCT` | `100` |
    /// | `GOOGLE_ADS_REQUIRE_DRY_RUN` | `true` |
    /// | `GOOGLE_ADS_AUDIT_LOG` | `~/.mcp-google-ads/audit.log` |
    /// | `GOOGLE_ADS_BLOCKED_OPS` | (empty, comma-separated) |
    /// | `GOOGLE_ADS_PLAN_STORE_DIR` | `~/.mcp-google-ads/plans` |
    /// | `GOOGLE_ADS_ALLOWED_CIDS` | (empty = unrestricted, comma-separated) |
    /// | `GOOGLE_ADS_READ_ONLY` | `false` |
    pub fn load() -> Result<Self> {
        let login_customer_id = std::env::var("GOOGLE_ADS_LOGIN_CUSTOMER_ID")
            .ok()
            .filter(|v| !v.is_empty());

        // Normalise at load time so every later comparison is a plain
        // string equality on digits — no separator-dependent mismatches.
        let allowed_customer_ids: Vec<String> = env_list("GOOGLE_ADS_ALLOWED_CIDS")
            .iter()
            .map(|c| normalize_customer_id(c))
            .filter(|c| !c.is_empty())
            .collect();

        Ok(Config {
            google: GoogleConfig {
                credentials_path: env_path_or(
                    "GOOGLE_ADS_CREDENTIALS_PATH",
                    "~/.mcp-google-ads/credentials.json",
                ),
                token_path: env_path_or("GOOGLE_ADS_TOKEN_PATH", "~/.mcp-google-ads/token.json"),
            },
            ads: AdsConfig {
                developer_token: env_or("GOOGLE_ADS_DEVELOPER_TOKEN", ""),
                customer_id: env_or("GOOGLE_ADS_CUSTOMER_ID", ""),
                login_customer_id,
            },
            safety: SafetyConfig {
                max_daily_budget: env_f64_or("GOOGLE_ADS_MAX_DAILY_BUDGET", 50.0),
                max_bid_increase_pct: env_u32_or("GOOGLE_ADS_MAX_BID_INCREASE_PCT", 100),
                require_dry_run: env_bool_or("GOOGLE_ADS_REQUIRE_DRY_RUN", true),
                log_file: env_path_or("GOOGLE_ADS_AUDIT_LOG", "~/.mcp-google-ads/audit.log"),
                blocked_operations: env_list("GOOGLE_ADS_BLOCKED_OPS"),
                plan_store_dir: env_path_or(
                    "GOOGLE_ADS_PLAN_STORE_DIR",
                    "~/.mcp-google-ads/plans",
                ),
                allowed_customer_ids,
            },
            read_only: env_bool_or("GOOGLE_ADS_READ_ONLY", false),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.safety.max_daily_budget, 50.0);
        assert_eq!(config.safety.max_bid_increase_pct, 100);
        assert!(config.safety.require_dry_run);
        assert!(config.safety.blocked_operations.is_empty());
        assert!(config.safety.allowed_customer_ids.is_empty());
    }

    #[test]
    fn test_expand_tilde() {
        let path = expand_tilde("~/.mcp-google-ads/config.yaml");
        assert!(!path.to_str().unwrap_or_default().starts_with('~'));
    }

    #[test]
    fn test_expand_tilde_no_tilde() {
        let path = expand_tilde("/absolute/path");
        assert_eq!(path, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn test_env_or_default() {
        assert_eq!(
            env_or("__MCP_TEST_NONEXISTENT_VAR__", "fallback"),
            "fallback"
        );
    }

    #[test]
    fn test_env_f64_or_default() {
        assert_eq!(env_f64_or("__MCP_TEST_NONEXISTENT_VAR__", 42.5), 42.5);
    }

    #[test]
    fn test_env_u32_or_default() {
        assert_eq!(env_u32_or("__MCP_TEST_NONEXISTENT_VAR__", 200), 200);
    }

    #[test]
    fn test_env_bool_or_default() {
        assert!(env_bool_or("__MCP_TEST_NONEXISTENT_VAR__", true));
        assert!(!env_bool_or("__MCP_TEST_NONEXISTENT_VAR__", false));
    }

    #[test]
    fn test_env_list_empty() {
        let list = env_list("__MCP_TEST_NONEXISTENT_VAR__");
        assert!(list.is_empty());
    }

    // ── customer_id normalisation ──────────────────────────────────────

    #[test]
    fn test_normalize_strips_dashes() {
        assert_eq!(normalize_customer_id("123-456-7890"), "1234567890");
        assert_eq!(normalize_customer_id("1234567890"), "1234567890");
    }

    #[test]
    fn test_normalize_strips_surrounding_whitespace() {
        assert_eq!(normalize_customer_id("  849-615-2956 "), "8496152956");
    }

    #[test]
    fn test_normalize_keeps_leading_zeros() {
        // MCC-style ids can start with 0 — dropping them would widen the match.
        assert_eq!(normalize_customer_id("0238-706-4043"), "02387064043");
    }

    #[test]
    fn test_normalize_non_numeric_yields_empty() {
        assert_eq!(normalize_customer_id("not-a-cid"), "");
        assert_eq!(normalize_customer_id(""), "");
    }

    // ── allow-list behaviour ───────────────────────────────────────────

    #[test]
    fn test_empty_allow_list_is_unrestricted() {
        let config = SafetyConfig::default();
        let (allowed, _) = config.customer_allowed("999-999-9999");
        assert!(allowed, "an unset allow-list must not block anything");
    }

    #[test]
    fn test_allow_list_permits_member_regardless_of_dashes() {
        let config = SafetyConfig {
            allowed_customer_ids: vec!["8496152956".to_string()],
            ..SafetyConfig::default()
        };
        assert!(config.customer_allowed("849-615-2956").0);
        assert!(config.customer_allowed("8496152956").0);
    }

    #[test]
    fn test_allow_list_rejects_non_member() {
        let config = SafetyConfig {
            allowed_customer_ids: vec!["8496152956".to_string()],
            ..SafetyConfig::default()
        };
        let (allowed, reason) = config.customer_allowed("214-712-1846");
        assert!(!allowed, "a CID outside the list must be refused");
        assert!(
            reason.contains("2147121846"),
            "reason should name the offending CID: {reason}"
        );
    }

    #[test]
    fn test_allow_list_rejects_empty_cid_when_restricted() {
        // An empty CID resolves to config.ads.customer_id in practice, but if
        // that is also unset we must refuse rather than fall through to the
        // API with an empty target.
        let config = SafetyConfig {
            allowed_customer_ids: vec!["8496152956".to_string()],
            ..SafetyConfig::default()
        };
        let (allowed, _) = config.customer_allowed("");
        assert!(!allowed);
    }

    #[test]
    fn test_allow_list_loads_and_normalises_from_env() {
        std::env::set_var("GOOGLE_ADS_ALLOWED_CIDS", "214-712-1846, 8496152956");
        let config = Config::load().unwrap();
        assert_eq!(
            config.safety.allowed_customer_ids,
            vec!["2147121846".to_string(), "8496152956".to_string()]
        );
        std::env::remove_var("GOOGLE_ADS_ALLOWED_CIDS");
    }

    #[test]
    fn test_allow_list_empty_env_stays_unrestricted() {
        std::env::remove_var("GOOGLE_ADS_ALLOWED_CIDS");
        let config = Config::load().unwrap();
        assert!(config.safety.allowed_customer_ids.is_empty());
        assert!(config.safety.customer_allowed("123-456-7890").0);
    }

    #[test]
    fn test_allow_list_skips_non_numeric_entries() {
        std::env::set_var("GOOGLE_ADS_ALLOWED_CIDS", "849-615-2956, garbage");
        let config = Config::load().unwrap();
        assert_eq!(config.safety.allowed_customer_ids, vec!["8496152956".to_string()]);
        std::env::remove_var("GOOGLE_ADS_ALLOWED_CIDS");
    }

    #[test]
    fn test_load_defaults() {
        // Clear any env vars that could interfere
        std::env::remove_var("GOOGLE_ADS_DEVELOPER_TOKEN");
        std::env::remove_var("GOOGLE_ADS_CUSTOMER_ID");
        std::env::remove_var("GOOGLE_ADS_LOGIN_CUSTOMER_ID");
        std::env::remove_var("GOOGLE_ADS_MAX_DAILY_BUDGET");
        std::env::remove_var("GOOGLE_ADS_MAX_BID_INCREASE_PCT");
        std::env::remove_var("GOOGLE_ADS_REQUIRE_DRY_RUN");
        std::env::remove_var("GOOGLE_ADS_BLOCKED_OPS");

        let config = Config::load().unwrap();
        assert_eq!(config.ads.developer_token, "");
        assert_eq!(config.ads.customer_id, "");
        assert!(config.ads.login_customer_id.is_none());
        assert_eq!(config.safety.max_daily_budget, 50.0);
        assert_eq!(config.safety.max_bid_increase_pct, 100);
        assert!(config.safety.require_dry_run);
        assert!(config.safety.blocked_operations.is_empty());
    }

    #[test]
    fn test_load_from_env() {
        std::env::set_var("GOOGLE_ADS_DEVELOPER_TOKEN", "test-dev-token");
        std::env::set_var("GOOGLE_ADS_CUSTOMER_ID", "123-456-7890");
        std::env::set_var("GOOGLE_ADS_LOGIN_CUSTOMER_ID", "999-888-7777");
        std::env::set_var("GOOGLE_ADS_MAX_DAILY_BUDGET", "200.0");
        std::env::set_var("GOOGLE_ADS_MAX_BID_INCREASE_PCT", "50");
        std::env::set_var("GOOGLE_ADS_REQUIRE_DRY_RUN", "false");
        std::env::set_var("GOOGLE_ADS_BLOCKED_OPS", "delete_campaign,remove_entity");
        std::env::set_var("GOOGLE_ADS_CREDENTIALS_PATH", "/tmp/creds.json");
        std::env::set_var("GOOGLE_ADS_TOKEN_PATH", "/tmp/token.json");
        std::env::set_var("GOOGLE_ADS_AUDIT_LOG", "/tmp/audit.log");

        let config = Config::load().unwrap();
        assert_eq!(config.ads.developer_token, "test-dev-token");
        assert_eq!(config.ads.customer_id, "123-456-7890");
        assert_eq!(
            config.ads.login_customer_id,
            Some("999-888-7777".to_string())
        );
        assert_eq!(config.safety.max_daily_budget, 200.0);
        assert_eq!(config.safety.max_bid_increase_pct, 50);
        assert!(!config.safety.require_dry_run);
        assert_eq!(
            config.safety.blocked_operations,
            vec!["delete_campaign", "remove_entity"]
        );
        assert_eq!(
            config.google.credentials_path,
            PathBuf::from("/tmp/creds.json")
        );
        assert_eq!(config.google.token_path, PathBuf::from("/tmp/token.json"));
        assert_eq!(config.safety.log_file, PathBuf::from("/tmp/audit.log"));

        // Clean up
        std::env::remove_var("GOOGLE_ADS_DEVELOPER_TOKEN");
        std::env::remove_var("GOOGLE_ADS_CUSTOMER_ID");
        std::env::remove_var("GOOGLE_ADS_LOGIN_CUSTOMER_ID");
        std::env::remove_var("GOOGLE_ADS_MAX_DAILY_BUDGET");
        std::env::remove_var("GOOGLE_ADS_MAX_BID_INCREASE_PCT");
        std::env::remove_var("GOOGLE_ADS_REQUIRE_DRY_RUN");
        std::env::remove_var("GOOGLE_ADS_BLOCKED_OPS");
        std::env::remove_var("GOOGLE_ADS_CREDENTIALS_PATH");
        std::env::remove_var("GOOGLE_ADS_TOKEN_PATH");
        std::env::remove_var("GOOGLE_ADS_AUDIT_LOG");
    }

    #[test]
    fn test_plan_store_dir_defaults_and_env_override() {
        std::env::remove_var("GOOGLE_ADS_PLAN_STORE_DIR");
        let config = Config::load().unwrap();
        assert!(
            config.safety.plan_store_dir.to_string_lossy().contains(".mcp-google-ads"),
            "default plan store dir should live under ~/.mcp-google-ads"
        );

        std::env::set_var("GOOGLE_ADS_PLAN_STORE_DIR", "/tmp/gads-plans-test");
        let config = Config::load().unwrap();
        assert_eq!(
            config.safety.plan_store_dir,
            PathBuf::from("/tmp/gads-plans-test")
        );
        std::env::remove_var("GOOGLE_ADS_PLAN_STORE_DIR");
    }

    #[test]
    fn test_bool_parsing_variants() {
        std::env::set_var("__MCP_TEST_BOOL__", "true");
        assert!(env_bool_or("__MCP_TEST_BOOL__", false));
        std::env::set_var("__MCP_TEST_BOOL__", "1");
        assert!(env_bool_or("__MCP_TEST_BOOL__", false));
        std::env::set_var("__MCP_TEST_BOOL__", "yes");
        assert!(env_bool_or("__MCP_TEST_BOOL__", false));
        std::env::set_var("__MCP_TEST_BOOL__", "false");
        assert!(!env_bool_or("__MCP_TEST_BOOL__", true));
        std::env::set_var("__MCP_TEST_BOOL__", "0");
        assert!(!env_bool_or("__MCP_TEST_BOOL__", true));
        std::env::set_var("__MCP_TEST_BOOL__", "no");
        assert!(!env_bool_or("__MCP_TEST_BOOL__", true));
        std::env::remove_var("__MCP_TEST_BOOL__");
    }

    #[test]
    fn test_env_list_parsing() {
        std::env::set_var("__MCP_TEST_LIST__", "a, b , c");
        let list = env_list("__MCP_TEST_LIST__");
        assert_eq!(list, vec!["a", "b", "c"]);
        std::env::remove_var("__MCP_TEST_LIST__");
    }

    #[test]
    fn test_login_customer_id_empty_string() {
        std::env::set_var("GOOGLE_ADS_LOGIN_CUSTOMER_ID", "");
        let config = Config::load().unwrap();
        assert!(config.ads.login_customer_id.is_none());
        std::env::remove_var("GOOGLE_ADS_LOGIN_CUSTOMER_ID");
    }
}