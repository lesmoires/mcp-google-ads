use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use chrono::Utc;
use serde_json::json;

use crate::error::Result;

/// Expand ~ to home directory
fn expand_path(path: &str) -> PathBuf {
    if path.starts_with("~/") || path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home.join(&path[2..]);
        }
    }
    PathBuf::from(path)
}

/// Caller attribution label for audit records (2026-10-13, D — audit caller field).
///
/// The 2026-10-12 prod-CID write incident (6 update_campaign 09:57-09:59Z on
/// 214-712-1846) ended "attribution: UNKNOWN" because this audit schema carried
/// no actor field whatsoever. The stdio MCP protocol offers no per-call caller
/// identity, so we expose an operator-set env label instead: each deployment
/// sets GOOGLE_ADS_CALLER_LABEL to a distinct value (e.g. "litellm-prod",
/// "litellm-staging"), making every audit record's origin unambiguous.
///
/// Default "unknown" — honest when unset, never fabricated.
fn caller_label() -> String {
    std::env::var("GOOGLE_ADS_CALLER_LABEL").unwrap_or_else(|_| "unknown".to_string())
}

/// Parameters for logging a mutation to the audit log.
pub struct MutationLogEntry<'a> {
    pub log_file: &'a str,
    pub operation: &'a str,
    pub customer_id: &'a str,
    pub entity_type: &'a str,
    pub entity_id: &'a str,
    pub changes: &'a serde_json::Value,
    pub dry_run: bool,
    pub result: &'a str,
    pub error: &'a str,
}

/// Append a mutation record to the audit log file
pub fn log_mutation(entry: &MutationLogEntry) -> Result<()> {
    let path = expand_path(entry.log_file);
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }

    let record = json!({
        "timestamp": Utc::now().to_rfc3339(),
        "caller": caller_label(),
        "operation": entry.operation,
        "customer_id": entry.customer_id,
        "entity_type": entry.entity_type,
        "entity_id": entry.entity_id,
        "changes": entry.changes,
        "dry_run": entry.dry_run,
        "result": entry.result,
        "error": entry.error,
    });

    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "{}", serde_json::to_string(&record)?)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_log_path() -> PathBuf {
        let dir = std::env::temp_dir().join("mcp-google-ads-test");
        fs::create_dir_all(&dir).ok();
        dir.join(format!("audit-{}.log", uuid::Uuid::new_v4()))
    }

    #[test]
    fn test_log_mutation_creates_file() {
        let path = temp_log_path();
        let path_str = path.to_string_lossy().to_string();
        let changes = serde_json::json!({"budget": 10.0});
        let entry = MutationLogEntry {
            log_file: &path_str,
            operation: "test_op",
            customer_id: "1234567890",
            entity_type: "campaign",
            entity_id: "555",
            changes: &changes,
            dry_run: false,
            result: "SUCCESS",
            error: "",
        };

        let result = log_mutation(&entry);
        assert!(result.is_ok());
        assert!(path.exists());

        fs::remove_file(&path).ok();
    }

    #[test]
    fn test_log_mutation_appends() {
        let path = temp_log_path();
        let path_str = path.to_string_lossy().to_string();
        let changes = serde_json::json!({"budget": 10.0});

        let entry1 = MutationLogEntry {
            log_file: &path_str,
            operation: "test_op",
            customer_id: "1234567890",
            entity_type: "campaign",
            entity_id: "555",
            changes: &changes,
            dry_run: false,
            result: "SUCCESS",
            error: "",
        };
        log_mutation(&entry1).ok();

        let entry2 = MutationLogEntry {
            log_file: &path_str,
            operation: "test_op_2",
            customer_id: "1234567890",
            entity_type: "campaign",
            entity_id: "666",
            changes: &changes,
            dry_run: false,
            result: "SUCCESS",
            error: "",
        };
        log_mutation(&entry2).ok();

        let contents = fs::read_to_string(&path).unwrap_or_default();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);

        fs::remove_file(&path).ok();
    }

    #[test]
    fn test_log_mutation_json_format() {
        let path = temp_log_path();
        let path_str = path.to_string_lossy().to_string();
        let changes = serde_json::json!({"budget": 10.0});

        let entry = MutationLogEntry {
            log_file: &path_str,
            operation: "test_op",
            customer_id: "1234567890",
            entity_type: "campaign",
            entity_id: "555",
            changes: &changes,
            dry_run: false,
            result: "SUCCESS",
            error: "",
        };
        log_mutation(&entry).ok();

        let contents = fs::read_to_string(&path).unwrap_or_default();
        let parsed: serde_json::Value =
            serde_json::from_str(contents.lines().next().unwrap_or_default()).unwrap_or_default();

        assert!(parsed.get("timestamp").is_some());
        assert_eq!(parsed["operation"], "test_op");
        assert_eq!(parsed["customer_id"], "1234567890");
        assert_eq!(parsed["entity_type"], "campaign");
        assert_eq!(parsed["entity_id"], "555");
        assert_eq!(parsed["dry_run"], false);
        assert_eq!(parsed["result"], "SUCCESS");

        fs::remove_file(&path).ok();
    }

    // ── 2026-10-13, D: caller/attribution field ─────────────────────────────

    #[test]
    fn test_caller_label_defaults_to_unknown() {
        std::env::remove_var("GOOGLE_ADS_CALLER_LABEL");
        assert_eq!(caller_label(), "unknown");
    }

    #[test]
    fn test_caller_label_from_env() {
        std::env::set_var("GOOGLE_ADS_CALLER_LABEL", "litellm-prod");
        assert_eq!(caller_label(), "litellm-prod");
        std::env::remove_var("GOOGLE_ADS_CALLER_LABEL");
    }

    #[test]
    fn test_log_mutation_records_caller_from_env() {
        std::env::set_var("GOOGLE_ADS_CALLER_LABEL", "litellm-staging");
        let path = temp_log_path();
        let path_str = path.to_string_lossy().to_string();
        let changes = serde_json::json!({"budget": 10.0});
        let entry = MutationLogEntry {
            log_file: &path_str,
            operation: "update_campaign",
            customer_id: "2147121846",
            entity_type: "campaign",
            entity_id: "1",
            changes: &changes,
            dry_run: false,
            result: "SUCCESS",
            error: "",
        };
        log_mutation(&entry).expect("log_mutation must succeed");

        let contents = fs::read_to_string(&path).unwrap_or_default();
        let parsed: serde_json::Value =
            serde_json::from_str(contents.lines().next().unwrap_or_default()).unwrap_or_default();
        assert_eq!(parsed["caller"], "litellm-staging");

        std::env::remove_var("GOOGLE_ADS_CALLER_LABEL");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn test_log_mutation_caller_unknown_when_unset() {
        std::env::remove_var("GOOGLE_ADS_CALLER_LABEL");
        let path = temp_log_path();
        let path_str = path.to_string_lossy().to_string();
        let changes = serde_json::json!({"budget": 10.0});
        let entry = MutationLogEntry {
            log_file: &path_str,
            operation: "update_campaign",
            customer_id: "2147121846",
            entity_type: "campaign",
            entity_id: "1",
            changes: &changes,
            dry_run: false,
            result: "SUCCESS",
            error: "",
        };
        log_mutation(&entry).expect("log_mutation must succeed");

        let contents = fs::read_to_string(&path).unwrap_or_default();
        let parsed: serde_json::Value =
            serde_json::from_str(contents.lines().next().unwrap_or_default()).unwrap_or_default();
        assert_eq!(parsed["caller"], "unknown");

        fs::remove_file(&path).ok();
    }
}
