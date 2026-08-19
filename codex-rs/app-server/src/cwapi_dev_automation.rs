use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use super::ToolError;
use super::bounded_stdout;
use super::cwapi_dev_exec::ExecFailure;
use super::cwapi_dev_exec::run_bounded_cancellable;
use super::exact_workspace_commit;
use super::run_git;
use super::tool_error;
use super::validate_commit;
use super::validate_existing_directory;
use super::validate_git_path;
use super::validate_workspace_member;

const AUTOMATION_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const AUTOMATION_OUTPUT_LIMIT: usize = 1024 * 1024;
const AUTOMATION_ARGUMENT_LIMIT: usize = 64;
const AUTOMATION_ARGUMENT_BYTE_MAX: usize = 4096;
const AUTOMATION_ENTRYPOINT_MAX: usize = 512;
const EXECUTION_ID_MAX: usize = 128;
const PENDING_CANCEL_LIMIT: usize = 256;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WorkspaceAutomationArgs {
    git_path: PathBuf,
    workspace_root: PathBuf,
    workspace_path: PathBuf,
    expected_commit: String,
    execution_id: String,
    entrypoint: String,
    sha256: String,
    arguments: Vec<String>,
}

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

struct ExecutionLease {
    execution_id: String,
    token: CancellationToken,
}

impl ExecutionLease {
    fn token(&self) -> CancellationToken {
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

pub(super) async fn automation_run(arguments: Option<JsonValue>) -> Result<JsonValue, ToolError> {
    let value = arguments.unwrap_or_else(|| json!({}));
    if value
        .get("cancelOnly")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false)
    {
        return request_cancel(value);
    }

    let args = decode_args(value)?;
    let lease = begin_execution(&args.execution_id)?;
    let workspace = validate_automation_workspace(&args)?;

    exact_workspace_commit(&args.git_path, &workspace, &args.expected_commit).await?;
    ensure_workspace_clean(&args.git_path, &workspace).await?;
    ensure_regular_tracked_entrypoint(&args.git_path, &workspace, &args.entrypoint).await?;
    verify_blob_sha256(
        &args.git_path,
        &workspace,
        &args.expected_commit,
        &args.entrypoint,
        &args.sha256,
    )
    .await?;

    let script = validate_execution_entrypoint(&workspace, &args.entrypoint)?;
    verify_worktree_sha256(&script, &args.sha256)?;
    run_entrypoint(
        &script,
        &workspace,
        &args.entrypoint,
        &args.arguments,
        lease.token(),
    )
    .await?;
    verify_worktree_sha256(&script, &args.sha256)?;

    let actual_commit =
        exact_workspace_commit(&args.git_path, &workspace, &args.expected_commit).await?;
    ensure_workspace_clean(&args.git_path, &workspace).await?;

    Ok(json!({
        "actualCommit": actual_commit,
        "entrypoint": args.entrypoint,
        "sha256": args.sha256,
        "exitCode": 0,
    }))
}

fn decode_args(value: JsonValue) -> Result<WorkspaceAutomationArgs, ToolError> {
    let mut value: WorkspaceAutomationArgs = serde_json::from_value(value).map_err(|_| {
        tool_error(
            "CWAPI_AUTOMATION_ARGUMENTS_INVALID",
            "invalid structured automation arguments",
        )
    })?;

    validate_execution_id(&value.execution_id)?;
    validate_entrypoint(&value.entrypoint)?;
    validate_sha256(&value.sha256)?;
    if value.arguments.len() > AUTOMATION_ARGUMENT_LIMIT {
        return Err(tool_error(
            "CWAPI_AUTOMATION_ARGUMENT_LIMIT",
            "automation argument count exceeded the structured limit",
        ));
    }
    for argument in &value.arguments {
        if argument.len() > AUTOMATION_ARGUMENT_BYTE_MAX || argument.contains('\0') {
            return Err(tool_error(
                "CWAPI_AUTOMATION_ARGUMENT_INVALID",
                "automation argument exceeded the structured limit",
            ));
        }
    }
    value.sha256.make_ascii_lowercase();
    Ok(value)
}

fn request_cancel(value: JsonValue) -> Result<JsonValue, ToolError> {
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

fn begin_execution(execution_id: &str) -> Result<ExecutionLease, ToolError> {
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
    if let Some(index) = registry.pending.iter().position(|value| value == execution_id) {
        registry.pending.remove(index);
        token.cancel();
    }
    registry.active.insert(execution_id.to_string(), token.clone());
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
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
        })
    {
        return Err(tool_error(
            "CWAPI_AUTOMATION_EXECUTION_ID_INVALID",
            "automation execution id is invalid",
        ));
    }
    Ok(())
}

fn validate_entrypoint(value: &str) -> Result<(), ToolError> {
    if value.is_empty()
        || value.len() > AUTOMATION_ENTRYPOINT_MAX
        || value.contains('\0')
        || value.contains('\\')
        || !value.starts_with("automation/")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-' | b'.'))
    {
        return Err(tool_error(
            "CWAPI_AUTOMATION_ENTRYPOINT_INVALID",
            "automation entrypoint is invalid",
        ));
    }

    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(tool_error(
            "CWAPI_AUTOMATION_ENTRYPOINT_INVALID",
            "automation entrypoint is invalid",
        ));
    }

    match path.extension().and_then(|value| value.to_str()) {
        Some("ps1") | Some("py") => Ok(()),
        _ => Err(tool_error(
            "CWAPI_AUTOMATION_ENTRYPOINT_TYPE_UNSUPPORTED",
            "automation entrypoint type is unsupported",
        )),
    }
}

fn validate_sha256(value: &str) -> Result<(), ToolError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(tool_error(
            "CWAPI_AUTOMATION_SHA256_INVALID",
            "automation SHA-256 is invalid",
        ));
    }
    Ok(())
}

fn validate_automation_workspace(args: &WorkspaceAutomationArgs) -> Result<PathBuf, ToolError> {
    validate_git_path(&args.git_path)?;
    validate_commit(&args.expected_commit)?;
    validate_workspace_member(&args.workspace_root, &args.workspace_path)?;
    validate_existing_directory(&args.workspace_root, "workspaceRoot")?;
    validate_existing_directory(&args.workspace_path, "workspacePath")?;

    let canonical_root = std::fs::canonicalize(&args.workspace_root).map_err(|_| {
        tool_error(
            "CWAPI_WORKSPACE_PATH_INVALID",
            "workspaceRoot could not be canonicalized",
        )
    })?;
    let canonical_workspace = std::fs::canonicalize(&args.workspace_path).map_err(|_| {
        tool_error(
            "CWAPI_WORKSPACE_PATH_INVALID",
            "workspacePath could not be canonicalized",
        )
    })?;
    if canonical_workspace.parent() != Some(canonical_root.as_path()) {
        return Err(tool_error(
            "CWAPI_WORKSPACE_PATH_INVALID",
            "workspacePath must remain a direct child of workspaceRoot after canonicalization",
        ));
    }
    Ok(canonical_workspace)
}

async fn ensure_workspace_clean(git_path: &Path, workspace: &Path) -> Result<(), ToolError> {
    let status = run_git(
        git_path,
        Some(workspace),
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )
    .await?;
    if !status.stdout.is_empty() {
        return Err(tool_error(
            "CWAPI_AUTOMATION_WORKTREE_DIRTY",
            "automation requires a clean managed workspace",
        ));
    }
    Ok(())
}

async fn ensure_regular_tracked_entrypoint(
    git_path: &Path,
    workspace: &Path,
    entrypoint: &str,
) -> Result<(), ToolError> {
    let output = run_git(
        git_path,
        Some(workspace),
        &["ls-files", "--stage", "--", entrypoint],
    )
    .await?;
    let line = bounded_stdout(&output.stdout, 4096)?;
    let mode = line.split_whitespace().next().unwrap_or_default();
    if mode != "100644" && mode != "100755" {
        return Err(tool_error(
            "CWAPI_AUTOMATION_ENTRYPOINT_NOT_REGULAR",
            "automation entrypoint must be a regular tracked file",
        ));
    }
    Ok(())
}

async fn verify_blob_sha256(
    git_path: &Path,
    workspace: &Path,
    expected_commit: &str,
    entrypoint: &str,
    expected_sha256: &str,
) -> Result<(), ToolError> {
    let object = format!("{expected_commit}:{entrypoint}");
    let output = run_git(git_path, Some(workspace), &["show", &object]).await?;
    let actual = format!("{:x}", Sha256::digest(&output.stdout));
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(tool_error(
            "CWAPI_AUTOMATION_HASH_MISMATCH",
            "automation entrypoint did not match the requested SHA-256",
        ));
    }
    Ok(())
}

fn validate_execution_entrypoint(workspace: &Path, entrypoint: &str) -> Result<PathBuf, ToolError> {
    let requested = workspace.join(Path::new(entrypoint));
    let metadata = std::fs::symlink_metadata(&requested).map_err(|_| {
        tool_error(
            "CWAPI_AUTOMATION_ENTRYPOINT_UNAVAILABLE",
            "automation entrypoint is unavailable in the managed workspace",
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(tool_error(
            "CWAPI_AUTOMATION_ENTRYPOINT_NOT_REGULAR",
            "automation entrypoint must remain a regular file in the managed workspace",
        ));
    }
    let canonical = std::fs::canonicalize(&requested).map_err(|_| {
        tool_error(
            "CWAPI_AUTOMATION_ENTRYPOINT_UNAVAILABLE",
            "automation entrypoint could not be canonicalized",
        )
    })?;
    if !canonical.starts_with(workspace) {
        return Err(tool_error(
            "CWAPI_AUTOMATION_ENTRYPOINT_ESCAPE",
            "automation entrypoint escaped the managed workspace",
        ));
    }
    Ok(canonical)
}

fn verify_worktree_sha256(script: &Path, expected_sha256: &str) -> Result<(), ToolError> {
    let bytes = std::fs::read(script).map_err(|_| {
        tool_error(
            "CWAPI_AUTOMATION_ENTRYPOINT_UNAVAILABLE",
            "automation entrypoint could not be read from the managed workspace",
        )
    })?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(tool_error(
            "CWAPI_AUTOMATION_WORKTREE_HASH_MISMATCH",
            "checked-out automation bytes differ from the canonical Git blob",
        ));
    }
    Ok(())
}

async fn run_entrypoint(
    script: &Path,
    workspace: &Path,
    entrypoint: &str,
    arguments: &[String],
    cancellation: CancellationToken,
) -> Result<(), ToolError> {
    let extension = Path::new(entrypoint)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let (program, mut argv) = if extension == "ps1" {
        (
            "pwsh",
            vec![
                OsString::from("-NoLogo"),
                OsString::from("-NoProfile"),
                OsString::from("-NonInteractive"),
                OsString::from("-ExecutionPolicy"),
                OsString::from("Bypass"),
                OsString::from("-File"),
                script.as_os_str().to_owned(),
            ],
        )
    } else {
        ("python", vec![script.as_os_str().to_owned()])
    };
    argv.extend(arguments.iter().map(OsString::from));

    run_bounded_cancellable(
        program,
        &argv,
        workspace,
        AUTOMATION_TIMEOUT,
        AUTOMATION_OUTPUT_LIMIT,
        cancellation,
    )
    .await
    .map(|_| ())
    .map_err(map_exec_failure)
}

fn map_exec_failure(failure: ExecFailure) -> ToolError {
    let code = match failure {
        ExecFailure::RuntimeMissing => "CWAPI_AUTOMATION_RUNTIME_MISSING",
        ExecFailure::StartFailed => "CWAPI_AUTOMATION_START_FAILED",
        ExecFailure::TimedOut => "CWAPI_AUTOMATION_TIMED_OUT",
        ExecFailure::Cancelled => "CWAPI_AUTOMATION_CANCELLED",
        ExecFailure::OutputTooLarge => "CWAPI_AUTOMATION_OUTPUT_TOO_LARGE",
        ExecFailure::Failed => "CWAPI_AUTOMATION_FAILED",
    };
    tool_error(code, "hash-bound automation execution failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entrypoint_is_repo_relative_and_bounded() {
        assert!(validate_entrypoint("automation/check.ps1").is_ok());
        assert!(validate_entrypoint("automation/sub/check.py").is_ok());
        for value in [
            "../automation/check.ps1",
            "automation/../check.ps1",
            "automation\\check.ps1",
            "scripts/check.ps1",
            "automation/check.exe",
            "automation/check file.ps1",
            "automation/check.PS1",
        ] {
            assert!(validate_entrypoint(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn arguments_reject_free_command_field() {
        let value = json!({
            "gitPath": "/git",
            "workspaceRoot": "/worktrees",
            "workspacePath": "/worktrees/ws-1",
            "expectedCommit": "0123456789abcdef0123456789abcdef01234567",
            "executionId": "req-free-command",
            "entrypoint": "automation/check.py",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "arguments": [],
            "command": "python evil.py"
        });
        assert!(serde_json::from_value::<WorkspaceAutomationArgs>(value).is_err());
    }

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

    #[test]
    fn worktree_hash_must_match_requested_blob_hash() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("check.py");
        std::fs::write(&script, b"print('ok')\n").unwrap();
        let hash = format!("{:x}", Sha256::digest(b"print('ok')\n"));
        assert!(verify_worktree_sha256(&script, &hash).is_ok());
        assert!(verify_worktree_sha256(&script, &"0".repeat(64)).is_err());
    }

    #[test]
    fn error_mapping_is_fixed() {
        assert_eq!(
            map_exec_failure(ExecFailure::RuntimeMissing).code,
            "CWAPI_AUTOMATION_RUNTIME_MISSING"
        );
        assert_eq!(
            map_exec_failure(ExecFailure::Cancelled).code,
            "CWAPI_AUTOMATION_CANCELLED"
        );
        assert_eq!(
            map_exec_failure(ExecFailure::OutputTooLarge).code,
            "CWAPI_AUTOMATION_OUTPUT_TOO_LARGE"
        );
    }
}
