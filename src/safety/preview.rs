use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::models::{AdStatus, NextActionHint};

/// Discriminator that tells `confirm_and_apply` how to dispatch this plan.
///
/// - `MutateOperations` (default) -> POST `/v23/customers/{cid}/googleAds:mutate`
/// - `ApplyRecommendation` -> POST `/v23/customers/{cid}/recommendations:apply`
/// - `DismissRecommendation` -> POST `/v23/customers/{cid}/recommendations:dismiss`
///
/// The recommendation variants exist because `applyRecommendationOperation` and
/// `dismissRecommendationOperation` are NOT valid `MutateOperation` keys in
/// Google Ads v23 — they live on dedicated RPCs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanDispatch {
    #[default]
    MutateOperations,
    ApplyRecommendation {
        /// Full resource name(s): `customers/{cid}/recommendations/{rec_id}`.
        resource_names: Vec<String>,
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
                "Review the changes above. To apply, call confirm_and_apply with plan_id=\'{}\' and dry_run=false.",
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

// ---------------------------------------------------------------------------
// FILE-BACKED PLAN STORE (v0.14.0)
// ---------------------------------------------------------------------------
//
// Why this exists (EXEC-BLOCKER 2026-10-06, prod):
//   The previous implementation kept pending plans in a process-local
//   `static Mutex<HashMap<..>>`. LiteLLM's stdio MCP client spawns a FRESH
//   server process for EVERY tool call and tears it down when the call ends,
//   so the draft (`xxx_entity`) and the apply (`confirm_and_apply`) never ran
//   in the same process. Every confirm therefore failed with `PlanNotFound`,
//   deterministically — 6/6, never a cache issue.
//
// The store is now file-backed so plans survive:
//   * process exit (the actual failure mode),
//   * gunicorn worker recycling (`--max_requests_before_restart`),
//   * container restarts, when pointed at a mounted/shared directory.
//
// Layout under `plan_store_dir`:
//   pending/<plan_id>.json    a drafted, not-yet-claimed plan
//   applied/<plan_id>.json    a claimed plan + the recorded result (idempotency)

/// Default TTL for a pending plan. A plan older than this is treated as
/// expired and removed on read.
const DEFAULT_PLAN_TTL_SECS: i64 = 15 * 60;

/// Directory configured at server boot (from `Config`), or `None` for the
/// default location. Set exactly once per process; there is no reason for it to
/// change while the server runs.
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

/// Guard against a crafted `plan_id` escaping the store directory
/// (`../`, absolute paths, separators). Plan ids are generated server-side,
/// but `confirm_and_apply` accepts them from the wire.
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

/// Parse a plan, treating an unparsable file as absent (and pruning it).
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
        // Unparsable timestamp: treat as expired rather than immortal.
        return true;
    };
    // `signed_duration_since` returns a plain TimeDelta in this chrono
    // version: negative values mean the plan is timestamped in the future.
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

/// TTL override from the environment, if set. Read per call so tests can
/// exercise expiry without restarting the process.
fn ttl_secs() -> i64 {
    std::env::var("GOOGLE_ADS_PLAN_TTL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_PLAN_TTL_SECS)
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

/// Atomically claim a pending plan for apply.
///
/// `rename(pending -> applied)` is atomic on POSIX, so exactly one caller can
/// ever claim a given plan even under concurrent confirms. The plan is moved
/// BEFORE any HTTP traffic; a crashed apply therefore leaves the plan in
/// `applied/` with no result, which [`get_claimed_plan`] surfaces as
/// `AppliedRecord { result: null }` — the operator can inspect it rather than
/// the plan silently vanishing.
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
/// caller can retry it. Writes the plan back atomically, then clears the
/// applied record.
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
    // A crashed mid-apply leaves the bare ChangePlan on disk; treat that as a
    // record with no recorded result.
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
    // running it after the write would mis-parse the record we just stored and
    // delete it as "unreadable".
    prune_applied_excluding(plan_id);

    let record = AppliedRecord {
        plan,
        result: result.clone(),
    };
    if let Ok(raw) = serde_json::to_string(&record) {
        let _ = write_atomic(&path, &raw);
    }
}

/// Drop applied records older than the TTL so the store stays bounded.
/// `keep` protects a record we are about to rewrite (see `finalize_plan`).
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
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        if stem == keep {
            continue;
        }
        // An applied/ file may hold either a bare ChangePlan (crashed apply) or
        // an AppliedRecord. Only delete on confirmed expiry, never on a parse
        // failure — an unrecognised record may be the only evidence of what
        // happened, so leave it for an operator to inspect.
        let created_at = read_created_at(&path);
        if created_at.map(|c| is_expired(&c, ttl)).unwrap_or(false) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Extract `created_at` from either serialisation (bare ChangePlan or AppliedRecord).
fn read_created_at(path: &Path) -> Option<ChangePlan> {
    let raw = std::fs::read_to_string(path).ok()?;
    if let Ok(rec) = serde_json::from_str::<AppliedRecord>(&raw) {
        return Some(rec.plan);
    }
    serde_json::from_str::<ChangePlan>(&raw).ok()
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

    /// Point the test at its own temp dir so it never touches
    /// `~/.mcp-google-ads`. Scoped to this thread, so tests stay isolated even
    /// when the harness runs them in parallel.
    fn use_temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mcp-gads-plans-{}-{}", tag, uuid::Uuid::new_v4()));
        set_plan_store_override(dir.clone());
        dir
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
        let dir = use_temp_dir("store_retrieve");
        let plan = make_plan();
        let plan_id = plan.plan_id.clone();
        store_plan(plan);

        let retrieved = get_plan(&plan_id).expect("plan should be retrievable");
        assert_eq!(retrieved.operation, "test_operation");
        // Cleanup
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_remove_plan() {
        let dir = use_temp_dir("remove");
        let plan = make_plan();
        let plan_id = plan.plan_id.clone();
        store_plan(plan);

        remove_plan(&plan_id);

        assert!(get_plan(&plan_id).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_get_nonexistent_plan() {
        let _dir = use_temp_dir("nonexistent");
        assert!(get_plan("does-not-exist-xyz").is_none());
    }

    #[test]
    fn test_plan_survives_new_process_view() {
        // The regression this store exists for: state written by one process
        // must be visible to a *different* process reading the same dir.
        let dir = use_temp_dir("cross_process");
        let plan = make_plan();
        let plan_id = plan.plan_id.clone();
        store_plan(plan);
        assert!(dir.join("pending").join(format!("{}.json", plan_id)).exists());
        // Drop any cached handle and re-resolve the directory from scratch,
        // the way a freshly spawned server process would.
        set_plan_store_override(dir.clone());
        assert!(get_plan(&plan_id).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_expired_plan_is_not_returned_and_is_pruned() {
        let dir = use_temp_dir("expiry");
        let mut plan = make_plan();
        let plan_id = plan.plan_id.clone();
        // Backdate the plan well past any sane TTL.
        plan.created_at = (Utc::now() - Duration::hours(2)).to_rfc3339();
        store_plan(plan);

        assert!(get_plan(&plan_id).is_none(), "expired plan must not resolve");
        assert!(
            !dir.join("pending").join(format!("{}.json", plan_id)).exists(),
            "expired plan must be pruned from disk"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_claim_then_get_claimed_returns_recorded_result() {
        let dir = use_temp_dir("claim");
        let plan = make_plan();
        let plan_id = plan.plan_id.clone();
        store_plan(plan);

        let claimed = claim_plan(&plan_id).expect("claim should succeed");
        assert_eq!(claimed.plan_id, plan_id);
        assert!(
            get_plan(&plan_id).is_none(),
            "a claimed plan must no longer be pending"
        );

        let result = serde_json::json!({"status": "APPLIED"});
        finalize_plan(&plan_id, &result);

        let rec = get_claimed_plan(&plan_id).expect("applied record should exist");
        assert_eq!(rec.result["status"], "APPLIED");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_claim_is_single_winner() {
        let dir = use_temp_dir("single_winner");
        let plan = make_plan();
        let plan_id = plan.plan_id.clone();
        store_plan(plan);

        assert!(claim_plan(&plan_id).is_some());
        assert!(
            claim_plan(&plan_id).is_none(),
            "a second claim must fail — this is what makes double-apply impossible"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_restore_plan_makes_failed_apply_retryable() {
        // Failed applies must release the claim so the documented
        // "plan is kept so the user can retry" contract still holds.
        let dir = use_temp_dir("restore");
        let plan = make_plan();
        let plan_id = plan.plan_id.clone();
        store_plan(plan);

        let claimed = claim_plan(&plan_id).expect("claim");
        assert!(get_plan(&plan_id).is_none(), "claimed plans are not pending");

        restore_plan(&claimed);

        let recovered = get_plan(&plan_id).expect("plan must be retryable again");
        assert_eq!(recovered.plan_id, plan_id);
        assert!(get_claimed_plan(&plan_id).is_none(), "release must clear the applied record");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_crafted_plan_id_cannot_escape_store() {
        let _dir = use_temp_dir("traversal");
        for bad in ["../escape", "..%2fescape", "/abs/path", "a/b", ""] {
            assert!(!plan_id_is_safe(bad), "must reject {bad:?}");
            assert!(get_plan(bad).is_none());
            store_plan(ChangePlan {
                plan_id: bad.to_string(),
                ..make_plan()
            });
            assert!(
                !store_root().join("pending").join(format!("{}.json", bad)).exists(),
                "must not write outside the store for {bad:?}"
            );
        }
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