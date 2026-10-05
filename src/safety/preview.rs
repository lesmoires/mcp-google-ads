use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::models::{AdStatus, NextActionHint};

/// Discriminator that tells `confirm_and_apply` how to dispatch this plan.
///
/// - `MutateOperations` (default) -> POST `/v25/customers/{cid}/googleAds:mutate`
/// - `ApplyRecommendation` -> POST `/v25/customers/{cid}/recommendations:apply`
/// - `DismissRecommendation` -> POST `/v25/customers/{cid}/recommendations:dismiss`
///
/// The recommendation variants exist because `applyRecommendationOperation` and
/// `dismissRecommendationOperation` are NOT valid `MutateOperation` keys in
/// Google Ads v25 — they live on dedicated RPCs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanDispatch {
    #[default]
    MutateOperations,
    ApplyRecommendation {
        /// Full resource name(s): `customers/{cid}/recommendations/{rec_id}`.
        resource_names: Vec<String>,
        /// The `apply_parameters` oneof, for the recommendation types that
        /// cannot be applied from a bare resource name. Merged onto every
        /// operation in the batch at dispatch time.
        #[serde(default)]
        apply_parameters: Option<serde_json::Value>,
    },
    DismissRecommendation {
        /// Full resource name(s): `customers/{cid}/recommendations/{rec_id}`.
        resource_names: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangePlan {
    pub plan_id: String,
    pub operation: String,
    pub entity_type: String,
    pub entity_id: String,
    pub customer_id: String,
    pub changes: serde_json::Value,
    pub created_at: String,
    pub requires_double_confirm: bool,
    /// The actual mutate operations to execute (serialized for the API).
    ///
    /// Empty for `dispatch != MutateOperations` (recommendation plans route
    /// through dedicated RPCs and do not use this field).
    pub mutate_operations: Vec<serde_json::Value>,
    /// Dispatch route for [`crate::tools::confirm::confirm_and_apply`].
    #[serde(default)]
    pub dispatch: PlanDispatch,
    /// The lifecycle status the entity will hold immediately after a
    /// successful apply. Surfaced in the response so agents know whether
    /// they still need to call `enable_entity`.
    #[serde(default)]
    pub status_after_apply: Option<AdStatus>,
    /// Optional MCP next-step hint propagated into the apply response.
    /// Tells the agent how to continue the workflow with zero UI action.
    #[serde(default)]
    pub next_action_hint: Option<NextActionHint>,
}

impl ChangePlan {
    pub fn new(
        operation: String,
        entity_type: String,
        entity_id: String,
        customer_id: String,
        changes: serde_json::Value,
        requires_double_confirm: bool,
        mutate_operations: Vec<serde_json::Value>,
    ) -> Self {
        Self {
            plan_id: Uuid::new_v4().to_string()[..8].to_string(),
            operation,
            entity_type,
            entity_id,
            customer_id,
            changes,
            created_at: Utc::now().to_rfc3339(),
            requires_double_confirm,
            mutate_operations,
            dispatch: PlanDispatch::MutateOperations,
            status_after_apply: None,
            next_action_hint: None,
        }
    }

    /// Attach a `status_after_apply` value to the plan (builder-style).
    pub fn with_status_after_apply(mut self, status: AdStatus) -> Self {
        self.status_after_apply = Some(status);
        self
    }

    /// Attach a [`NextActionHint`] to the plan (builder-style).
    pub fn with_next_action_hint(mut self, hint: NextActionHint) -> Self {
        self.next_action_hint = Some(hint);
        self
    }

    /// Override the default `MutateOperations` dispatch with a dedicated
    /// recommendation RPC route.
    pub fn with_dispatch(mut self, dispatch: PlanDispatch) -> Self {
        self.dispatch = dispatch;
        self
    }

    pub fn to_preview(&self) -> serde_json::Value {
        let mut preview = serde_json::json!({
            "plan_id": self.plan_id,
            "operation": self.operation,
            "entity_type": self.entity_type,
            "entity_id": self.entity_id,
            "customer_id": self.customer_id,
            "changes": self.changes,
            "requires_double_confirm": self.requires_double_confirm,
            "status": "PENDING_CONFIRMATION",
            "instructions": format!(
                "Review the changes above. To apply, call confirm_and_apply with plan_id='{}' and dry_run=false.",
                self.plan_id
            ),
        });

        if let Some(obj) = preview.as_object_mut() {
            if let Some(status) = self.status_after_apply {
                obj.insert(
                    "status_after_apply".to_string(),
                    serde_json::Value::String(status.as_api_str().to_string()),
                );
            }
            if let Some(ref hint) = self.next_action_hint {
                obj.insert(
                    "next_action_hint".to_string(),
                    serde_json::to_value(hint).unwrap_or(serde_json::Value::Null),
                );
            }
        }

        preview
    }
}

/// Thread-safe store for pending plans
/// Directory configured at server boot (from `Config`), or `None` for the
/// default location. Set exactly once per process.
static PLAN_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Per-thread override used by tests so each test gets an isolated directory
/// without disturbing other tests running concurrently in the same process.
/// Production code never sets this, so runtime behaviour is unaffected.
thread_local! {
    static PLAN_DIR_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn default_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".mcp-google-ads").join("plans");
    }
    PathBuf::from(".mcp-google-ads").join("plans")
}

fn store_root() -> PathBuf {
    if let Some(dir) = PLAN_DIR_OVERRIDE.with(|o| o.borrow().clone()) {
        return dir;
    }
    let guard = PLAN_DIR.lock().expect("plan dir lock poisoned");
    match guard.as_ref() {
        Some(p) => p.clone(),
        None => default_dir(),
    }
}

/// Point the plan store at an explicit directory. Called once when the server
/// boots with a loaded `Config` so the operator-configured path wins.
pub fn init_plan_store(dir: PathBuf) {
    let mut guard = PLAN_DIR.lock().expect("plan dir lock poisoned");
    *guard = Some(dir);
}

#[cfg(test)]
fn set_plan_store_override(dir: PathBuf) {
    PLAN_DIR_OVERRIDE.with(|o| *o.borrow_mut() = Some(dir));
}

/// Test-only entry point so sibling modules' `#[cfg(test)]` code can point the
/// plan store at an isolated directory without touching the process-global
/// boot configuration.
#[cfg(test)]
pub fn init_plan_store_for_test(dir: PathBuf) {
    set_plan_store_override(dir);
}

fn pending_dir() -> PathBuf {
    store_root().join("pending")
}

fn applied_dir() -> PathBuf {
    store_root().join("applied")
}

/// Guard against a crafted `plan_id` escaping the store directory. Plan ids are
/// generated server-side, but `confirm_and_apply` accepts them from the wire.
fn plan_id_is_safe(plan_id: &str) -> bool {
    !plan_id.is_empty()
        && plan_id.len() <= 64
        && plan_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

fn read_plan_file(path: &Path) -> Option<ChangePlan> {
    let raw = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<ChangePlan>(&raw) {
        Ok(plan) => Some(plan),
        Err(_) => {
            let _ = std::fs::remove_file(path);
            None
        }
    }
}

fn is_expired(plan: &ChangePlan, ttl_secs: i64) -> bool {
    let Ok(created) = chrono::DateTime::parse_from_rfc3339(&plan.created_at) else {
        return true;
    };
    Utc::now().signed_duration_since(created).num_seconds() > ttl_secs
}

/// Persist a freshly drafted plan.
pub fn store_plan(plan: ChangePlan) {
    if !plan_id_is_safe(&plan.plan_id) {
        return;
    }
    if let Ok(raw) = serde_json::to_string(&plan) {
        let _ = write_atomic(&pending_dir().join(format!("{}.json", plan.plan_id)), &raw);
    }
}

fn ttl_secs() -> i64 {
    std::env::var("GOOGLE_ADS_PLAN_TTL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15 * 60)
}

/// Look up a pending plan. Returns `None` when unknown OR expired; an expired
/// plan is pruned so the store does not grow without bound.
pub fn get_plan(plan_id: &str) -> Option<ChangePlan> {
    if !plan_id_is_safe(plan_id) {
        return None;
    }
    let path = pending_dir().join(format!("{}.json", plan_id));
    let plan = read_plan_file(&path)?;
    if is_expired(&plan, ttl_secs()) {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(plan)
}

/// Remove a pending plan without recording a result (used on failure paths).
pub fn remove_plan(plan_id: &str) {
    if !plan_id_is_safe(plan_id) {
        return;
    }
    let _ = std::fs::remove_file(pending_dir().join(format!("{}.json", plan_id)));
}

/// An already-applied plan: the recorded outcome of a previous confirm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedRecord {
    pub plan: ChangePlan,
    /// The response the original successful apply returned, replayed verbatim
    /// on any retry so `confirm_and_apply` becomes idempotent.
    pub result: serde_json::Value,
}

/// Atomically claim a pending plan for apply. `rename(pending -> applied)` is
/// atomic on POSIX, so exactly one caller can ever claim a given plan even
/// under concurrent confirms.
pub fn claim_plan(plan_id: &str) -> Option<ChangePlan> {
    if !plan_id_is_safe(plan_id) {
        return None;
    }
    let _ = std::fs::create_dir_all(applied_dir());
    std::fs::rename(
        pending_dir().join(format!("{}.json", plan_id)),
        applied_dir().join(format!("{}.json", plan_id)),
    )
    .ok()?;
    read_plan_file(&applied_dir().join(format!("{}.json", plan_id)))
}

/// Release a claimed plan back to `pending/` after a failed apply, so the
/// caller can retry it.
pub fn restore_plan(plan: &ChangePlan) {
    if !plan_id_is_safe(&plan.plan_id) {
        return;
    }
    let Ok(raw) = serde_json::to_string(plan) else {
        return;
    };
    let _ = write_atomic(&pending_dir().join(format!("{}.json", plan.plan_id)), &raw);
    let _ = std::fs::remove_file(applied_dir().join(format!("{}.json", plan.plan_id)));
}

/// Retrieve an already-claimed plan record, if any.
pub fn get_claimed_plan(plan_id: &str) -> Option<AppliedRecord> {
    if !plan_id_is_safe(plan_id) {
        return None;
    }
    let raw = std::fs::read_to_string(applied_dir().join(format!("{}.json", plan_id))).ok()?;
    match serde_json::from_str::<AppliedRecord>(&raw) {
        Ok(rec) => Some(rec),
        Err(_) => match serde_json::from_str::<ChangePlan>(&raw) {
            Ok(plan) => Some(AppliedRecord {
                plan,
                result: serde_json::Value::Null,
            }),
            Err(_) => None,
        },
    }
}

/// Record the successful result for a claimed plan, and prune applied records
/// older than the TTL so the store stays bounded.
pub fn finalize_plan(plan_id: &str, result: &serde_json::Value) {
    if !plan_id_is_safe(plan_id) {
        return;
    }
    let path = applied_dir().join(format!("{}.json", plan_id));
    let Some(plan) = read_plan_file(&path) else {
        return;
    };
    // Prune FIRST: pruning reads applied/ files back as bare ChangePlans, so
    // running it after the write would mis-parse and delete the new record.
    prune_applied_excluding(plan_id);
    let record = AppliedRecord {
        plan,
        result: result.clone(),
    };
    if let Ok(raw) = serde_json::to_string(&record) {
        let _ = write_atomic(&path, &raw);
    }
}

/// Drop applied records older than the TTL. `keep` protects the record we are
/// about to rewrite (see `finalize_plan`). Never deletes on a parse failure: an
/// unrecognised record may be the only evidence of what happened.
fn prune_applied_excluding(keep: &str) {
    let Ok(entries) = std::fs::read_dir(applied_dir()) else {
        return;
    };
    let ttl = ttl_secs();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if path.file_stem().and_then(|s| s.to_str()).unwrap_or_default() == keep {
            continue;
        }
        let created_at = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| {
                serde_json::from_str::<AppliedRecord>(&raw)
                    .map(|r| r.plan)
                    .ok()
                    .or_else(|| serde_json::from_str::<ChangePlan>(&raw).ok())
            });
        if created_at.map(|c| is_expired(&c, ttl)).unwrap_or(false) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_plan() -> ChangePlan {
        ChangePlan::new(
            "test_operation".to_string(),
            "campaign".to_string(),
            "entity-123".to_string(),
            "1234567890".to_string(),
            serde_json::json!({"key": "value"}),
            false,
            vec![serde_json::json!({"campaignOperation": {"create": {}}})],
        )
    }

    #[test]
    fn test_change_plan_creation() {
        let plan = make_plan();
        assert!(!plan.plan_id.is_empty());
        assert!(!plan.created_at.is_empty());
        assert_eq!(plan.operation, "test_operation");
        assert_eq!(plan.entity_type, "campaign");
        assert_eq!(plan.entity_id, "entity-123");
        assert_eq!(plan.customer_id, "1234567890");
        assert!(!plan.requires_double_confirm);
        assert_eq!(plan.mutate_operations.len(), 1);
        assert_eq!(plan.dispatch, PlanDispatch::MutateOperations);
        assert!(plan.status_after_apply.is_none());
        assert!(plan.next_action_hint.is_none());
    }

    #[test]
    fn test_store_and_retrieve_plan() {
        let plan = make_plan();
        let plan_id = plan.plan_id.clone();
        store_plan(plan);

        let retrieved = get_plan(&plan_id);
        assert!(retrieved.is_some());
        let retrieved = retrieved.map(|p| p.operation).unwrap_or_default();
        assert_eq!(retrieved, "test_operation");

        // Cleanup
        remove_plan(&plan_id);
    }

    #[test]
    fn test_remove_plan() {
        let plan = make_plan();
        let plan_id = plan.plan_id.clone();
        store_plan(plan);

        remove_plan(&plan_id);
        assert!(get_plan(&plan_id).is_none());
    }

    #[test]
    fn test_get_nonexistent_plan() {
        assert!(get_plan("does-not-exist-xyz").is_none());
    }

    #[test]
    fn test_to_preview_format() {
        let plan = make_plan();
        let preview = plan.to_preview();

        assert!(preview.get("plan_id").is_some());
        assert_eq!(preview["status"], "PENDING_CONFIRMATION");
        let instructions = preview["instructions"].as_str().unwrap_or_default();
        assert!(instructions.contains(&plan.plan_id));
        assert!(instructions.contains("confirm_and_apply"));
    }

    #[test]
    fn test_to_preview_with_status_and_hint() {
        let plan = make_plan()
            .with_status_after_apply(AdStatus::Paused)
            .with_next_action_hint(NextActionHint::enable_ad("AG1", "AD1"));
        let preview = plan.to_preview();
        assert_eq!(preview["status_after_apply"], "PAUSED");
        assert_eq!(preview["next_action_hint"]["tool"], "enable_entity");
        assert_eq!(
            preview["next_action_hint"]["params"]["entity_id"],
            "AG1~AD1"
        );
    }

    #[test]
    fn test_dispatch_default_and_override() {
        let plan = make_plan();
        assert_eq!(plan.dispatch, PlanDispatch::MutateOperations);

        let plan = make_plan().with_dispatch(PlanDispatch::DismissRecommendation {
            resource_names: vec!["customers/123/recommendations/rec-1".to_string()],
        });
        match plan.dispatch {
            PlanDispatch::DismissRecommendation { resource_names } => {
                assert_eq!(resource_names.len(), 1);
            }
            _ => panic!("expected DismissRecommendation"),
        }
    }
}
