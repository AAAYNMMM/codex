use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_json::json;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::OnceLock;
use tokio_util::sync::CancellationToken;

use super::ToolError;
use super::tool_error;

const EXECUTION_ID_MAX: usize = 128;
const PENDING_CANCEL_LIMIT: usize = 256;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct AutomationCancelArgs {
    execution_id: String,
    cancel_only: bool,
}

#[derive(Default)]
struct ExecutionRegistry {
    active: HashMap<String, CancellationToken>,
    pending: VecDeque<String>,
}

pub(super) struct ExecutionLease {
    execution_id: String,
    token: CancellationToken,
}

impl ExecutionLease {
    pub(super) fn token(&self) -> CancellationToken {
        self.token.clone()
    }
}

impl Drop for ExecutionLease {
    fn drop(&mut self) {
        registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .active
            .remove(&self.execution_id);
    }
}

pub(super) fn request_cancel(value: JsonValue) -> Result<JsonValue, ToolError> {
    let args: AutomationCancelArgs = serde_json::from_value(value).map_err(|_| {
        tool_error(
            "CWAPI_AUTOMATION_CANCEL_ARGUMENTS_INVALID",
            "invalid structured automation cancellation arguments",
        )
    })?;
    if !args.cancel_only {
        return Err(tool_error(
            "CWAPI_AUTOMATION_CANCEL_ARGUMENTS_INVALID",
            "cancelOnly must be true for automation cancellation",
        ));
    }
    validate_execution_id(&args.execution_id)?;

    let active = {
        let mut registry = registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(token) = registry.active.get(&args.execution_id).cloned() {
            drop(registry);
            token.cancel();
            true
        } else {
            if !registry.pending.iter().any(|value| value == &args.execution_id) {
                if registry.pending.len() >= PENDING_CANCEL_LIMIT {
                    registry.pending.pop_front();
                }
                registry.pending.push_back(args.execution_id.clone());
            }
            false
        }
    };

    Ok(json!({
        "executionId": args.execution_id,
        "accepted": true,
        "active": active,
    }))
}

pub(super) fn begin_execution(execution_id: &str) -> Result<ExecutionLease, ToolError> {
    validate_execution_id(execution_id)?;
    let mut registry = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if registry.active.contains_key(execution_id) {
        return Err(tool_error(
            "CWAPI_AUTOMATION_EXECUTION_DUPLICATE",
            "automation execution id is already active",
        ));
    }

    let token = CancellationToken::new();
    if let Some(index) = registry
        .pending
        .iter()
        .position(|value| value == execution_id)
    {
        registry.pending.remove(index);
        token.cancel();
    }
    registry
        .active
        .insert(execution_id.to_string(), token.clone());
    Ok(ExecutionLease {
        execution_id: execution_id.to_string(),
        token,
    })
}

fn registry() -> &'static Mutex<ExecutionRegistry> {
    static REGISTRY: OnceLock<Mutex<ExecutionRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(ExecutionRegistry::default()))
}

fn validate_execution_id(value: &str) -> Result<(), ToolError> {
    if value.is_empty()
        || value.len() > EXECUTION_ID_MAX
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(tool_error(
            "CWAPI_AUTOMATION_EXECUTION_ID_INVALID",
            "automation execution id is invalid",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_before_registration_is_observed() {
        let execution_id = "req-cancel-before-registration";
        request_cancel(json!({"executionId": execution_id, "cancelOnly": true})).unwrap();
        let lease = begin_execution(execution_id).unwrap();
        assert!(lease.token().is_cancelled());
    }

    #[test]
    fn active_cancellation_signals_execution_token() {
        let execution_id = "req-active-cancellation";
        let lease = begin_execution(execution_id).unwrap();
        let token = lease.token();
        request_cancel(json!({"executionId": execution_id, "cancelOnly": true})).unwrap();
        assert!(token.is_cancelled());
    }
}
