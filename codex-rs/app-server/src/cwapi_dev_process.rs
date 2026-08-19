use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use serde_json::json;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use super::ToolError;
use super::cwapi_dev_exec::ExecFailure;
use super::cwapi_dev_exec::run_bounded;
use super::exact_workspace_commit;
use super::tool_error;
use super::validate_commit;
use super::validate_existing_directory;
use super::validate_git_path;
use super::validate_workspace_member;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const PROCESS_OUTPUT_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WorkspaceProcessArgs {
    git_path: PathBuf,
    workspace_root: PathBuf,
    workspace_path: PathBuf,
    expected_commit: String,
    profile: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProcessResult {
    actual_commit: String,
    profile: String,
    exit_code: i32,
}

pub(super) async fn test_run(arguments: Option<JsonValue>) -> Result<ProcessResult, ToolError> {
    let args = decode_args(arguments)?;
    let workspace = validate_process_workspace(&args)?;
    exact_workspace_commit(&args.git_path, &workspace, &args.expected_commit).await?;
    let (program, argv): (&str, &[&str]) = match args.profile.as_str() {
        "go.all" => ("go", &["test", "./..."]),
        "cargo.workspace" => ("cargo", &["test", "--workspace", "--no-fail-fast"]),
        "pytest.tests" => ("python", &["-m", "pytest", "tests"]),
        _ => {
            return Err(tool_error(
                "CWAPI_TEST_PROFILE_UNSUPPORTED",
                "unsupported structured test profile",
            ));
        }
    };
    run_profile(program, argv, &workspace, "CWAPI_TEST").await?;
    let actual_commit =
        exact_workspace_commit(&args.git_path, &workspace, &args.expected_commit).await?;
    Ok(ProcessResult {
        actual_commit,
        profile: args.profile,
        exit_code: 0,
    })
}

pub(super) async fn build_run(arguments: Option<JsonValue>) -> Result<ProcessResult, ToolError> {
    let args = decode_args(arguments)?;
    let workspace = validate_process_workspace(&args)?;
    exact_workspace_commit(&args.git_path, &workspace, &args.expected_commit).await?;
    let (program, argv): (&str, &[&str]) = match args.profile.as_str() {
        "go.all" => ("go", &["build", "./..."]),
        "cargo.workspace" => ("cargo", &["build", "--workspace"]),
        _ => {
            return Err(tool_error(
                "CWAPI_BUILD_PROFILE_UNSUPPORTED",
                "unsupported structured build profile",
            ));
        }
    };
    run_profile(program, argv, &workspace, "CWAPI_BUILD").await?;
    let actual_commit =
        exact_workspace_commit(&args.git_path, &workspace, &args.expected_commit).await?;
    Ok(ProcessResult {
        actual_commit,
        profile: args.profile,
        exit_code: 0,
    })
}

fn decode_args(arguments: Option<JsonValue>) -> Result<WorkspaceProcessArgs, ToolError> {
    serde_json::from_value(arguments.unwrap_or_else(|| json!({}))).map_err(|_| {
        tool_error(
            "CWAPI_PROCESS_ARGUMENTS_INVALID",
            "invalid structured process tool arguments",
        )
    })
}

fn validate_process_workspace(args: &WorkspaceProcessArgs) -> Result<PathBuf, ToolError> {
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

async fn run_profile(
    program: &str,
    argv: &[&str],
    workspace: &Path,
    error_prefix: &str,
) -> Result<(), ToolError> {
    let argv = argv.iter().map(OsString::from).collect::<Vec<_>>();
    run_bounded(
        program,
        &argv,
        workspace,
        PROCESS_TIMEOUT,
        PROCESS_OUTPUT_LIMIT,
    )
    .await
    .map(|_| ())
    .map_err(|failure| map_exec_failure(error_prefix, failure))
}

fn map_exec_failure(error_prefix: &str, failure: ExecFailure) -> ToolError {
    let suffix = match failure {
        ExecFailure::RuntimeMissing => "_RUNTIME_MISSING",
        ExecFailure::StartFailed => "_START_FAILED",
        ExecFailure::TimedOut => "_TIMED_OUT",
        ExecFailure::Cancelled => "_CANCELLED",
        ExecFailure::OutputTooLarge => "_OUTPUT_TOO_LARGE",
        ExecFailure::Failed => "_FAILED",
    };
    tool_error(
        leak_static_code(error_prefix, suffix),
        "structured process profile failed",
    )
}

fn leak_static_code(prefix: &str, suffix: &str) -> &'static str {
    match (prefix, suffix) {
        ("CWAPI_TEST", "_TIMED_OUT") => "CWAPI_TEST_TIMED_OUT",
        ("CWAPI_TEST", "_CANCELLED") => "CWAPI_TEST_CANCELLED",
        ("CWAPI_TEST", "_RUNTIME_MISSING") => "CWAPI_TEST_RUNTIME_MISSING",
        ("CWAPI_TEST", "_START_FAILED") => "CWAPI_TEST_START_FAILED",
        ("CWAPI_TEST", "_OUTPUT_TOO_LARGE") => "CWAPI_TEST_OUTPUT_TOO_LARGE",
        ("CWAPI_TEST", "_FAILED") => "CWAPI_TEST_FAILED",
        ("CWAPI_BUILD", "_TIMED_OUT") => "CWAPI_BUILD_TIMED_OUT",
        ("CWAPI_BUILD", "_CANCELLED") => "CWAPI_BUILD_CANCELLED",
        ("CWAPI_BUILD", "_RUNTIME_MISSING") => "CWAPI_BUILD_RUNTIME_MISSING",
        ("CWAPI_BUILD", "_START_FAILED") => "CWAPI_BUILD_START_FAILED",
        ("CWAPI_BUILD", "_OUTPUT_TOO_LARGE") => "CWAPI_BUILD_OUTPUT_TOO_LARGE",
        ("CWAPI_BUILD", "_FAILED") => "CWAPI_BUILD_FAILED",
        _ => "CWAPI_PROCESS_FAILED",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_arguments_reject_free_command_fields() {
        let value = json!({
            "gitPath": "/git",
            "workspaceRoot": "/worktrees",
            "workspacePath": "/worktrees/ws-1",
            "expectedCommit": "0123456789abcdef0123456789abcdef01234567",
            "profile": "go.all",
            "command": "go test ./..."
        });
        assert!(serde_json::from_value::<WorkspaceProcessArgs>(value).is_err());
    }

    #[test]
    fn process_error_codes_are_fixed() {
        assert_eq!(
            leak_static_code("CWAPI_TEST", "_FAILED"),
            "CWAPI_TEST_FAILED"
        );
        assert_eq!(
            leak_static_code("CWAPI_TEST", "_CANCELLED"),
            "CWAPI_TEST_CANCELLED"
        );
        assert_eq!(
            leak_static_code("CWAPI_BUILD", "_RUNTIME_MISSING"),
            "CWAPI_BUILD_RUNTIME_MISSING"
        );
        assert_eq!(
            leak_static_code("invalid", "invalid"),
            "CWAPI_PROCESS_FAILED"
        );
    }
}
