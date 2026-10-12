use serde_json::json;

use crate::client::{GoogleAdsClient, MutateOperation};
use crate::config::Config;
use crate::error::{McpGoogleAdsError, Result};
use crate::safety::audit;
use crate::safety::preview::{
    claim_plan, finalize_plan, get_claimed_plan, get_plan, restore_plan, ChangePlan, PlanDispatch,
};

/// Parameters carried through `confirm_and_apply` callers down to the apply
/// implementation. Centralised so the hard guards (`require_dry_run`,
/// `requires_double_confirm`) can be opt-out by the caller.
#[derive(Debug, Clone, Default)]
pub struct ConfirmApplyInput {
    pub plan_id: String,
    pub dry_run: bool,
    /// Override for `config.safety.require_dry_run`. When `true`, the dry-run
    /// guard is bypassed for THIS single apply (one-shot escape hatch — does
    /// not modify config). Default: `false`.
    pub bypass_require_dry_run: bool,
    /// Acknowledgement that the caller has read and intends to apply a plan
    /// flagged `requires_double_confirm`. Without this, destructive plans
    /// return `Err(DoubleConfirmRequired)` instead of executing.
    pub confirmed_twice: bool,
}

/// Confirm and apply a previously drafted change plan.
///
/// Dispatch routing:
/// - [`PlanDispatch::MutateOperations`] -> `POST /googleAds:mutate`
/// - [`PlanDispatch::ApplyRecommendation`] -> `POST /recommendations:apply`
/// - [`PlanDispatch::DismissRecommendation`] -> `POST /recommendations:dismiss`
///
/// Hard guards (return `Err` BEFORE any HTTP traffic):
/// - `config.safety.require_dry_run && !dry_run && !bypass_require_dry_run`
///   -> [`McpGoogleAdsError::DryRunRequired`]
/// - `plan.requires_double_confirm && !confirmed_twice && !dry_run`
///   -> [`McpGoogleAdsError::DoubleConfirmRequired`]
/// - `!config.safety.customer_allowed(&plan.customer_id)`
///   -> [`McpGoogleAdsError::Safety`]
///
/// On success the applied plan and its result are recorded, and the mutation
/// is logged to the audit file. On failure the plan is returned to `pending/`
/// so the caller can retry. Successful responses NEVER include a warning — the
/// warning emitted by v0.2.x was removed (it was cosmetic and lied about safety).
///
/// Idempotency: the plan is claimed (atomically renamed out of `pending/`)
/// before any HTTP traffic. A repeated `confirm_and_apply` for an
/// already-applied plan replays the recorded result instead of mutating again,
/// so a retried confirm — or a duplicated tool call from a confused agent —
/// can never double-spend a client budget.
pub async fn confirm_and_apply(
    config: &Config,
    input: ConfirmApplyInput,
) -> Result<serde_json::Value> {
    let ConfirmApplyInput {
        plan_id,
        dry_run,
        bypass_require_dry_run,
        confirmed_twice,
    } = input;

    // Already applied? Replay the recorded result rather than failing with
    // PlanNotFound. This is what makes confirm idempotent across the
    // process-per-call stdio transport.
    //
    // This path deliberately runs BEFORE the customer_id allow-list check
    // below: replaying a recorded result performs no write, so narrowing the
    // allow-list afterwards must not break the idempotency contract.
    if let Some(record) = get_claimed_plan(&plan_id) {
        if let Some(result) = record.result.as_object() {
            let mut replay = result.clone();
            replay.insert("idempotent_replay".to_string(), json!(true));
            replay.insert("plan_id".to_string(), json!(plan_id));
            return Ok(serde_json::Value::Object(replay));
        }
        // Claimed but no result recorded: the previous apply crashed before
        // completing. Surface it explicitly instead of silently mutating again.
        return Err(McpGoogleAdsError::PlanNotFound(format!(
            "Plan '{plan_id}' was claimed by an earlier confirm that did not complete (no recorded result). It has NOT been re-applied. Inspect the plan store before retrying."
        )));
    }

    let plan = get_plan(&plan_id).ok_or_else(|| {
        McpGoogleAdsError::PlanNotFound(format!(
            "No pending plan found with ID '{plan_id}'. It may have already been applied or expired."
        ))
    })?;

    // Guard: the plan must target an account this server is allowed to write.
    //
    // Checked HERE, against `plan.customer_id`, and not only at draft time.
    // `confirm_and_apply` takes no `customer_id` argument — the target comes
    // from the persisted plan — so a guard keyed on the tool parameter would
    // never run here, which is precisely where the write happens. Plans also
    // outlive the process that drafted them, so the allow-list in force at
    // draft time may differ from the one in force now. This is the decisive
    // moment: everything above this line is read-only, everything below it
    // issues the mutate.
    let (allowed, reason) = config.safety.customer_allowed(&plan.customer_id);
    if !allowed {
        return Err(McpGoogleAdsError::Safety(format!(
            "Plan '{plan_id}' targets an out-of-scope account: {reason}"
        )));
    }

    // Dry run: return preview without executing.
    if dry_run {
        let mut preview = plan.to_preview();
        if let Some(o) = preview.as_object_mut() {
            o.insert("dry_run".to_string(), json!(true));
            o.insert(
                "message".to_string(),
                json!("Dry run — no changes applied. Call again with dry_run=false to execute."),
            );
            o.insert(
                "mutate_operations_count".to_string(),
                json!(plan.mutate_operations.len()),
            );
        }
        return Ok(preview);
    }

    // Hard guard: dry-run requirement.
    if config.safety.require_dry_run && !bypass_require_dry_run {
        return Err(McpGoogleAdsError::DryRunRequired);
    }

    // Hard guard: double-confirmation for destructive operations.
    if plan.requires_double_confirm && !confirmed_twice {
        return Err(McpGoogleAdsError::DoubleConfirmRequired);
    }

    // Claim the plan BEFORE any HTTP traffic: atomic rename out of pending/.
    // Exactly one concurrent confirm can win; the loser replays from the
    // recorded result instead of issuing a second mutate.
    let Some(plan) = claim_plan(&plan_id) else {
        // Lost the race — another confirm claimed it first.
        if let Some(record) = get_claimed_plan(&plan_id) {
            if let Some(result) = record.result.as_object() {
                let mut replay = result.clone();
                replay.insert("idempotent_replay".to_string(), json!(true));
                replay.insert("plan_id".to_string(), json!(plan_id));
                return Ok(serde_json::Value::Object(replay));
            }
        }
        return Err(McpGoogleAdsError::PlanNotFound(format!(
            "Plan '{plan_id}' could not be claimed for apply. Another confirm may be in progress."
        )));
    };

    let client = GoogleAdsClient::new(config)?;
    apply_plan(&client, config, &plan, &plan_id).await
}

/// Dispatch the plan to the correct Google Ads RPC and shape the response.
async fn apply_plan(
    client: &GoogleAdsClient,
    config: &Config,
    plan: &ChangePlan,
    plan_id: &str,
) -> Result<serde_json::Value> {
    let log_file = config.safety.log_file.to_string_lossy().to_string();

    let dispatch_result = match &plan.dispatch {
        PlanDispatch::MutateOperations => apply_mutate_operations(client, plan).await,
        PlanDispatch::ApplyRecommendation { resource_names } => {
            apply_recommendation_dispatch(client, plan, resource_names).await
        }
        PlanDispatch::DismissRecommendation { resource_names } => {
            dismiss_recommendation_dispatch(client, plan, resource_names).await
        }
    };

    match dispatch_result {
        Ok(mut result) => {
            let _ = audit::log_mutation(&audit::MutationLogEntry {
                log_file: &log_file,
                operation: &plan.operation,
                customer_id: &plan.customer_id,
                entity_type: &plan.entity_type,
                entity_id: &plan.entity_id,
                changes: &plan.changes,
                dry_run: false,
                result: "SUCCESS",
                error: "",
            });

            if let Some(obj) = result.as_object_mut() {
                obj.insert("plan_id".to_string(), json!(plan_id));
                obj.insert("operation".to_string(), json!(plan.operation));
                obj.insert("entity_type".to_string(), json!(plan.entity_type));
                obj.insert("entity_id".to_string(), json!(plan.entity_id));
                obj.insert("customer_id".to_string(), json!(plan.customer_id));
                if let Some(status) = plan.status_after_apply {
                    obj.insert("status_after_apply".to_string(), json!(status.as_api_str()));
                }
                if let Some(ref hint) = plan.next_action_hint {
                    obj.insert(
                        "next_action_hint".to_string(),
                        serde_json::to_value(hint).unwrap_or(serde_json::Value::Null),
                    );
                }
            }

            // Record the result against the claimed plan so any repeat confirm
            // replays instead of mutating again.
            finalize_plan(plan_id, &result);
            Ok(result)
        }
        Err(e) => {
            let _ = audit::log_mutation(&audit::MutationLogEntry {
                log_file: &log_file,
                operation: &plan.operation,
                customer_id: &plan.customer_id,
                entity_type: &plan.entity_type,
                entity_id: &plan.entity_id,
                changes: &plan.changes,
                dry_run: false,
                result: "FAILED",
                error: &e.to_string(),
            });
            // Release the claim so the caller can retry.
            restore_plan(plan);
            Err(e)
        }
    }
}

async fn apply_mutate_operations(
    client: &GoogleAdsClient,
    plan: &ChangePlan,
) -> Result<serde_json::Value> {
    let operations: Vec<MutateOperation> = plan
        .mutate_operations
        .iter()
        .map(|op| MutateOperation {
            operation: op.clone(),
        })
        .collect();

    let response = client.mutate(&plan.customer_id, operations).await?;

    // Mutates are sent with `partialFailure: false` (atomic — a failing
    // operation aborts the whole request as an HTTP error), so this field
    // should never be set. Kept as a safety net: if it ever appears, report
    // failure instead of "APPLIED" so the audit log can't record a false
    // SUCCESS.
    if let Some(partial_error) = response.partial_failure_error {
        return Err(McpGoogleAdsError::PartialFailure(partial_error));
    }

    Ok(json!({
        "status": "APPLIED",
        "responses": response.mutate_operation_responses,
    }))
}

async fn apply_recommendation_dispatch(
    client: &GoogleAdsClient,
    plan: &ChangePlan,
    resource_names: &[String],
) -> Result<serde_json::Value> {
    let response = client
        .apply_recommendations(&plan.customer_id, resource_names.to_vec())
        .await?;

    if let Some(partial_error) = response.partial_failure_error {
        return Err(McpGoogleAdsError::PartialFailure(partial_error));
    }

    Ok(json!({
        "status": "APPLIED",
        "results": response.results,
    }))
}

async fn dismiss_recommendation_dispatch(
    client: &GoogleAdsClient,
    plan: &ChangePlan,
    resource_names: &[String],
) -> Result<serde_json::Value> {
    let response = client
        .dismiss_recommendations(&plan.customer_id, resource_names.to_vec())
        .await?;

    Ok(json!({
        "status": "DISMISSED",
        "results": response.results,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::preview::{get_plan, store_plan, ChangePlan};
    use std::path::PathBuf;
    use uuid::Uuid;

    /// Thread-scoped store dir so these tests stay isolated from each other
    /// and from the other modules' tests running in the same process.
    fn use_temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mcp-gads-confirm-{}-{}", tag, Uuid::new_v4()));
        crate::safety::preview::init_plan_store_for_test(dir.clone());
        dir
    }

    fn plan_for(cid: &str) -> ChangePlan {
        ChangePlan::new(
            "test_op".to_string(),
            "campaign".to_string(),
            "1".to_string(),
            cid.to_string(),
            serde_json::json!({}),
            false,
            vec![serde_json::json!({"campaignOperation": {"update": {"resourceName": "x"}}})],
        )
    }

    #[test]
    fn test_plan_not_found_via_get() {
        // Attempting to get a non-existent plan returns None
        let result = get_plan("nonexistent-plan-id");
        assert!(result.is_none());
    }

    #[test]
    fn test_plan_store_and_retrieve() {
        let dir = use_temp_dir("store_retrieve");
        let plan = ChangePlan::new(
            "test_op".to_string(),
            "campaign".to_string(),
            "123".to_string(),
            "1234567890".to_string(),
            serde_json::json!({"test": true}),
            false,
            vec![serde_json::json!({"campaignOperation": {"create": {}}})],
        );

        let plan_id = plan.plan_id.clone();
        store_plan(plan);

        let retrieved = get_plan(&plan_id);
        assert!(retrieved.is_some());
        let retrieved = retrieved.map(|p| p.operation).unwrap_or_default();
        assert_eq!(retrieved, "test_op");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_require_dry_run_hard_guards_apply() {
        // Plan exists, require_dry_run=true (default), dry_run=false, no bypass.
        // Expected: Err(DryRunRequired) BEFORE any HTTP call.
        let dir = use_temp_dir("dry_run_guard");
        let plan = ChangePlan::new(
            "test_op".to_string(),
            "campaign".to_string(),
            "1".to_string(),
            "1234567890".to_string(),
            serde_json::json!({}),
            false,
            vec![serde_json::json!({"campaignOperation": {"update": {"resourceName": "x"}}})],
        );
        let plan_id = plan.plan_id.clone();
        store_plan(plan);

        let mut config = Config::default();
        config.safety.require_dry_run = true;

        let err = confirm_and_apply(
            &config,
            ConfirmApplyInput {
                plan_id: plan_id.clone(),
                dry_run: false,
                bypass_require_dry_run: false,
                confirmed_twice: false,
            },
        )
        .await
        .expect_err("expected DryRunRequired");

        assert!(matches!(err, McpGoogleAdsError::DryRunRequired));
        // Plan is preserved so the caller can retry.
        assert!(get_plan(&plan_id).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_dry_run_returns_preview_without_http() {
        let dir = use_temp_dir("dry_run_preview");
        let plan = ChangePlan::new(
            "test_op".to_string(),
            "campaign".to_string(),
            "1".to_string(),
            "1234567890".to_string(),
            serde_json::json!({}),
            false,
            vec![serde_json::json!({"campaignOperation": {"update": {"resourceName": "x"}}})],
        );
        let plan_id = plan.plan_id.clone();
        store_plan(plan);

        let config = Config::default();
        let preview = confirm_and_apply(
            &config,
            ConfirmApplyInput {
                plan_id: plan_id.clone(),
                dry_run: true,
                bypass_require_dry_run: false,
                confirmed_twice: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(preview["dry_run"], true);
        // Plan is preserved across dry runs.
        assert!(get_plan(&plan_id).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_confirm_replays_recorded_result_instead_of_mutating_twice() {
        // The idempotency contract: once a plan has been applied and recorded,
        // a repeated confirm returns the recorded result (flagged
        // idempotent_replay) and never re-enters the dispatch path.
        let dir = use_temp_dir("idempotent_replay");
        let plan = ChangePlan::new(
            "test_op".to_string(),
            "campaign".to_string(),
            "1".to_string(),
            "1234567890".to_string(),
            serde_json::json!({}),
            false,
            vec![serde_json::json!({"campaignOperation": {"update": {"resourceName": "x"}}})],
        );
        let plan_id = plan.plan_id.clone();

        // Simulate a completed apply: claim + record the result.
        store_plan(plan);
        let claimed = crate::safety::preview::claim_plan(&plan_id).expect("claim");
        assert_eq!(claimed.plan_id, plan_id);
        finalize_plan(&plan_id, &json!({"status": "APPLIED", "responses": []}));

        let config = Config::default();
        let replay = confirm_and_apply(
            &config,
            ConfirmApplyInput {
                plan_id: plan_id.clone(),
                dry_run: false,
                bypass_require_dry_run: true,
                confirmed_twice: true,
            },
        )
        .await
        .expect("replay should succeed without HTTP");

        assert_eq!(replay["status"], "APPLIED");
        assert_eq!(replay["idempotent_replay"], true);
        assert_eq!(replay["plan_id"], plan_id.as_str());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_incomplete_claim_is_reported_not_reapplied() {
        // A crash between claim and finalize must NOT silently re-apply.
        let dir = use_temp_dir("incomplete_claim");
        let plan = ChangePlan::new(
            "test_op".to_string(),
            "campaign".to_string(),
            "1".to_string(),
            "1234567890".to_string(),
            serde_json::json!({}),
            false,
            vec![serde_json::json!({"campaignOperation": {"update": {"resourceName": "x"}}})],
        );
        let plan_id = plan.plan_id.clone();
        store_plan(plan);
        crate::safety::preview::claim_plan(&plan_id).expect("claim");
        // Deliberately no finalize_plan() — simulates a crashed apply.

        let config = Config::default();
        let err = confirm_and_apply(
            &config,
            ConfirmApplyInput {
                plan_id: plan_id.clone(),
                dry_run: false,
                bypass_require_dry_run: true,
                confirmed_twice: true,
            },
        )
        .await
        .expect_err("must not re-apply an incomplete claim");

        let msg = err.to_string();
        assert!(msg.contains("did not complete"), "unexpected error: {msg}");
        let _ = std::fs::remove_dir_all(dir);
    }

    // ── customer_id allow-list (fix B3) ────────────────────────────────

    #[tokio::test]
    async fn test_apply_refuses_plan_targeting_out_of_scope_account() {
        // The regression this guard exists for: a plan drafted for an account
        // outside GOOGLE_ADS_ALLOWED_CIDS must not reach the mutate, even when
        // every other guard is satisfied (dry-run bypassed, confirmed twice).
        let dir = use_temp_dir("cid_guard_refuse");
        let plan = plan_for("214-712-1846");
        let plan_id = plan.plan_id.clone();
        store_plan(plan);

        let mut config = Config::default();
        config.safety.allowed_customer_ids = vec!["8496152956".to_string()];

        let err = confirm_and_apply(
            &config,
            ConfirmApplyInput {
                plan_id: plan_id.clone(),
                dry_run: false,
                bypass_require_dry_run: true,
                confirmed_twice: true,
            },
        )
        .await
        .expect_err("out-of-scope plan must be refused");

        let msg = err.to_string();
        assert!(msg.contains("out-of-scope"), "error should say why: {msg}");
        assert!(
            msg.contains("2147121846"),
            "error should name the offending CID: {msg}"
        );
        // The plan stays pending so it can be inspected or re-drafted once the
        // operator widens the allow-list. It must NOT have been claimed.
        assert!(
            get_plan(&plan_id).is_some(),
            "refused plan must remain pending, not claimed"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_allow_list_matches_plan_cid_regardless_of_dashes() {
        // The plan stores whatever the caller passed. Dashes must not decide
        // whether a plan is in scope.
        let dir = use_temp_dir("cid_guard_dashes");
        let mut config = Config::default();
        config.safety.allowed_customer_ids = vec!["8496152956".to_string()];

        let plan = plan_for("849-615-2956");
        let plan_id = plan.plan_id.clone();
        store_plan(plan);

        // Restricted enough that the CID guard is the only thing that could
        // reject this, and unreachable past it: the dry-run requirement fires
        // first, proving the CID guard let it through.
        let err = confirm_and_apply(
            &config,
            ConfirmApplyInput {
                plan_id: plan_id.clone(),
                dry_run: false,
                bypass_require_dry_run: false,
                confirmed_twice: false,
            },
        )
        .await
        .expect_err("dry-run guard should fire for an in-scope CID");

        assert!(
            matches!(err, McpGoogleAdsError::DryRunRequired),
            "in-scope CID must pass the allow-list and reach the dry-run guard, got: {err}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_dry_run_of_out_of_scope_plan_is_also_refused() {
        // A dry run performs no write, so one could argue it should be allowed
        // for observability. It is refused anyway: the preview echoes the plan
        // body, and letting an out-of-scope account's details through the tool
        // surface would leak exactly what the guard exists to contain.
        let dir = use_temp_dir("cid_guard_dry_run");
        let plan = plan_for("214-712-1846");
        let plan_id = plan.plan_id.clone();
        store_plan(plan);

        let mut config = Config::default();
        config.safety.allowed_customer_ids = vec!["8496152956".to_string()];

        let err = confirm_and_apply(
            &config,
            ConfirmApplyInput {
                plan_id: plan_id.clone(),
                dry_run: true,
                bypass_require_dry_run: false,
                confirmed_twice: false,
            },
        )
        .await
        .expect_err("out-of-scope plan must be refused even on dry_run");

        assert!(err.to_string().contains("out-of-scope"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_unrestricted_config_applies_normally() {
        // With no allow-list configured the guard is inert — existing
        // deployments must keep working unchanged.
        let dir = use_temp_dir("cid_guard_unset");
        let plan = plan_for("214-712-1846");
        let plan_id = plan.plan_id.clone();
        store_plan(plan);

        let config = Config::default();
        assert!(config.safety.allowed_customer_ids.is_empty());

        let result = confirm_and_apply(
            &config,
            ConfirmApplyInput {
                plan_id: plan_id.clone(),
                dry_run: true,
                bypass_require_dry_run: false,
                confirmed_twice: false,
            },
        )
        .await
        .expect("an unset allow-list must not refuse anything — the dry run should succeed");

        assert_eq!(result["dry_run"], true);
        assert_eq!(result["customer_id"], "214-712-1846");
        assert!(
            get_plan(&plan_id).is_some(),
            "the dry run must leave the plan pending"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_idempotent_replay_survives_allow_list_narrowing() {
        // Replaying a recorded result performs no write. Narrowing the
        // allow-list afterwards must not turn an already-applied plan into a
        // hard error, or a retried confirm would start failing.
        let dir = use_temp_dir("cid_guard_replay");
        let plan = plan_for("214-712-1846");
        let plan_id = plan.plan_id.clone();
        store_plan(plan);
        crate::safety::preview::claim_plan(&plan_id).expect("claim");
        finalize_plan(&plan_id, &json!({"status": "APPLIED", "responses": []}));

        let mut config = Config::default();
        config.safety.allowed_customer_ids = vec!["8496152956".to_string()];

        let replay = confirm_and_apply(
            &config,
            ConfirmApplyInput {
                plan_id: plan_id.clone(),
                dry_run: false,
                bypass_require_dry_run: true,
                confirmed_twice: true,
            },
        )
        .await
        .expect("replay must still succeed with a narrowed allow-list");

        assert_eq!(replay["idempotent_replay"], true);
        assert_eq!(replay["status"], "APPLIED");
        let _ = std::fs::remove_dir_all(dir);
    }
}
