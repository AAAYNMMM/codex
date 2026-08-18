use codex_app_server_protocol::McpServerToolCallResponse;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use serde_json::json;
use std::path::Path;
use std::path::PathBuf;
use std::process::Output;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

#[path = "cwapi_dev_automation.rs"]
mod cwapi_dev_automation;
#[path = "cwapi_dev_exec.rs"]
mod cwapi_dev_exec;
#[path = "cwapi_dev_process.rs"]
mod cwapi_dev_process;

pub(crate) const SERVER_NAME: &str = "cwapi-dev";
const TOOL_TIMEOUT: Duration = Duration::from_secs(120);
const OUTPUT_LIMIT: usize = 1024 * 1024;
const STATUS_PORCELAIN_LIMIT: usize = 8 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WorkspaceOpenArgs {
    git_path: PathBuf,
    repository_path: PathBuf,
    workspace_root: PathBuf,
    workspace_path: PathBuf,
    expected_commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WorkspaceStatusArgs {
    git_path: PathBuf,
    workspace_root: PathBuf,
    workspace_path: PathBuf,
    expected_commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WorkspaceCloseArgs {
    git_path: PathBuf,
    repository_path: PathBuf,
    workspace_root: PathBuf,
    workspace_path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WorkspaceGitArgs {
    git_path: PathBuf,
    workspace_root: PathBuf,
    workspace_path: PathBuf,
    expected_commit: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceResult {
    actual_commit: String,
    clean: bool,
    workspace_path: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GitRevParseResult {
    actual_commit: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GitStatusResult {
    actual_commit: String,
    clean: bool,
    porcelain: String,
}

pub(crate) async fn call(tool: &str, arguments: Option<JsonValue>) -> McpServerToolCallResponse {
    match tool {
        "workspace.open" => match decode_args::<WorkspaceOpenArgs>(arguments) {
            Ok(args) => match workspace_open(args).await {
                Ok(value) => success_response(value),
                Err(error) => error_response(error),
            },
            Err(error) => error_response(error),
        },
        "workspace.status" => match decode_args::<WorkspaceStatusArgs>(arguments) {
            Ok(args) => match workspace_status(args).await {
                Ok(value) => success_response(value),
                Err(error) => error_response(error),
            },
            Err(error) => error_response(error),
        },
        "workspace.close" => match decode_args::<WorkspaceCloseArgs>(arguments) {
            Ok(args) => match workspace_close(args).await {
                Ok(value) => success_response(value),
                Err(error) => error_response(error),
            },
            Err(error) => error_response(error),
        },
        "git.rev_parse" => match decode_args::<WorkspaceGitArgs>(arguments) {
            Ok(args) => match git_rev_parse(args).await {
                Ok(value) => success_response(value),
                Err(error) => error_response(error),
            },
            Err(error) => error_response(error),
        },
        "git.status" => match decode_args::<WorkspaceGitArgs>(arguments) {
            Ok(args) => match git_status(args).await {
                Ok(value) => success_response(value),
                Err(error) => error_response(error),
            },
            Err(error) => error_response(error),
        },
        "test.run" => match cwapi_dev_process::test_run(arguments).await {
            Ok(value) => success_response(value),
            Err(error) => error_response(error),
        },
        "build.run" => match cwapi_dev_process::build_run(arguments).await {
            Ok(value) => success_response(value),
            Err(error) => error_response(error),
        },
        "automation.run" => match cwapi_dev_automation::automation_run(arguments).await {
            Ok(value) => success_response(value),
            Err(error) => error_response(error),
        },
        _ => error_response(tool_error(
            "CWAPI_TOOL_UNKNOWN",
            format!("unknown cwapi-dev tool: {tool}"),
        )),
    }
}

fn decode_args<T: for<'de> Deserialize<'de>>(arguments: Option<JsonValue>) -> Result<T, ToolError> {
    serde_json::from_value(arguments.unwrap_or_else(|| json!({}))).map_err(|error| {
        tool_error(
            "CWAPI_TOOL_ARGUMENTS_INVALID",
            format!("invalid structured tool arguments: {error}"),
        )
    })
}

async fn workspace_open(args: WorkspaceOpenArgs) -> Result<WorkspaceResult, ToolError> {
    validate_git_path(&args.git_path)?;
    validate_existing_directory(&args.repository_path, "repositoryPath")?;
    validate_workspace_target(&args.workspace_root, &args.workspace_path)?;
    validate_commit(&args.expected_commit)?;

    tokio::fs::create_dir_all(&args.workspace_root)
        .await
        .map_err(|error| tool_error("CWAPI_WORKSPACE_ROOT_CREATE_FAILED", error.to_string()))?;

    let commit_spec = format!("{}^{{commit}}", args.expected_commit);
    let verified = run_git(
        &args.git_path,
        Some(&args.repository_path),
        &["rev-parse", "--verify", &commit_spec],
    )
    .await?;
    let verified_commit = stdout_line(&verified)?;
    if !verified_commit.eq_ignore_ascii_case(&args.expected_commit) {
        return Err(tool_error(
            "CWAPI_WORKSPACE_COMMIT_NOT_FOUND",
            "repository did not resolve the requested exact commit",
        ));
    }

    let workspace_text = path_text(&args.workspace_path)?;
    run_git(
        &args.git_path,
        Some(&args.repository_path),
        &[
            "worktree",
            "add",
            "--detach",
            &workspace_text,
            &args.expected_commit,
        ],
    )
    .await?;

    match workspace_result(&args.git_path, &args.workspace_path, &args.expected_commit).await {
        Ok(result) => Ok(result),
        Err(error) => {
            let _ =
                remove_worktree(&args.git_path, &args.repository_path, &args.workspace_path).await;
            Err(error)
        }
    }
}

async fn workspace_status(args: WorkspaceStatusArgs) -> Result<WorkspaceResult, ToolError> {
    validate_git_path(&args.git_path)?;
    validate_workspace_member(&args.workspace_root, &args.workspace_path)?;
    validate_existing_directory(&args.workspace_path, "workspacePath")?;
    validate_commit(&args.expected_commit)?;
    workspace_result(&args.git_path, &args.workspace_path, &args.expected_commit).await
}

async fn workspace_close(args: WorkspaceCloseArgs) -> Result<JsonValue, ToolError> {
    validate_git_path(&args.git_path)?;
    validate_existing_directory(&args.repository_path, "repositoryPath")?;
    validate_workspace_member(&args.workspace_root, &args.workspace_path)?;

    if args.workspace_path.exists() {
        remove_worktree(&args.git_path, &args.repository_path, &args.workspace_path).await?;
    }
    Ok(json!({
        "closed": true,
        "workspacePath": path_text(&args.workspace_path)?,
    }))
}

async fn git_rev_parse(args: WorkspaceGitArgs) -> Result<GitRevParseResult, ToolError> {
    let workspace_path = validate_git_workspace(&args)?;
    let actual_commit =
        exact_workspace_commit(&args.git_path, &workspace_path, &args.expected_commit).await?;
    Ok(GitRevParseResult { actual_commit })
}

async fn git_status(args: WorkspaceGitArgs) -> Result<GitStatusResult, ToolError> {
    let workspace_path = validate_git_workspace(&args)?;
    let actual_commit =
        exact_workspace_commit(&args.git_path, &workspace_path, &args.expected_commit).await?;
    let status = run_git(
        &args.git_path,
        Some(&workspace_path),
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )
    .await?;
    let porcelain = bounded_stdout(&status.stdout, STATUS_PORCELAIN_LIMIT)?;
    Ok(GitStatusResult {
        actual_commit,
        clean: porcelain.is_empty(),
        porcelain,
    })
}

fn validate_git_workspace(args: &WorkspaceGitArgs) -> Result<PathBuf, ToolError> {
    validate_git_path(&args.git_path)?;
    validate_commit(&args.expected_commit)?;
    validate_workspace_member(&args.workspace_root, &args.workspace_path)?;
    validate_existing_directory(&args.workspace_root, "workspaceRoot")?;
    validate_existing_directory(&args.workspace_path, "workspacePath")?;

    let canonical_root = std::fs::canonicalize(&args.workspace_root).map_err(|error| {
        tool_error(
            "CWAPI_WORKSPACE_PATH_INVALID",
            format!("workspaceRoot could not be canonicalized: {error}"),
        )
    })?;
    let canonical_workspace = std::fs::canonicalize(&args.workspace_path).map_err(|error| {
        tool_error(
            "CWAPI_WORKSPACE_PATH_INVALID",
            format!("workspacePath could not be canonicalized: {error}"),
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

async fn exact_workspace_commit(
    git_path: &Path,
    workspace_path: &Path,
    expected_commit: &str,
) -> Result<String, ToolError> {
    let head = run_git(git_path, Some(workspace_path), &["rev-parse", "HEAD"]).await?;
    let actual_commit = stdout_line(&head)?;
    if !actual_commit.eq_ignore_ascii_case(expected_commit) {
        return Err(tool_error(
            "CWAPI_WORKSPACE_COMMIT_MISMATCH",
            format!("expected {expected_commit}, actual {actual_commit}"),
        ));
    }
    Ok(actual_commit)
}

async fn workspace_result(
    git_path: &Path,
    workspace_path: &Path,
    expected_commit: &str,
) -> Result<WorkspaceResult, ToolError> {
    let actual_commit = exact_workspace_commit(git_path, workspace_path, expected_commit).await?;
    let status = run_git(git_path, Some(workspace_path), &["status", "--porcelain"]).await?;
    let clean = status.stdout.is_empty();
    if !clean {
        return Err(tool_error(
            "CWAPI_WORKSPACE_NOT_CLEAN",
            "managed workspace is not clean",
        ));
    }
    Ok(WorkspaceResult {
        actual_commit,
        clean,
        workspace_path: path_text(workspace_path)?,
    })
}

async fn remove_worktree(
    git_path: &Path,
    repository_path: &Path,
    workspace_path: &Path,
) -> Result<(), ToolError> {
    let workspace_text = path_text(workspace_path)?;
    run_git(
        git_path,
        Some(repository_path),
        &["worktree", "remove", "--force", &workspace_text],
    )
    .await?;
    let _ = run_git(git_path, Some(repository_path), &["worktree", "prune"]).await;
    Ok(())
}

fn validate_git_path(path: &Path) -> Result<(), ToolError> {
    if !path.is_absolute() || !path.is_file() {
        return Err(tool_error(
            "CWAPI_GIT_PATH_INVALID",
            "gitPath must be an existing absolute file",
        ));
    }
    Ok(())
}

fn validate_existing_directory(path: &Path, field: &str) -> Result<(), ToolError> {
    if !path.is_absolute() || !path.is_dir() {
        return Err(tool_error(
            "CWAPI_PATH_INVALID",
            format!("{field} must be an existing absolute directory"),
        ));
    }
    Ok(())
}

fn validate_workspace_target(root: &Path, target: &Path) -> Result<(), ToolError> {
    validate_workspace_member(root, target)?;
    if target.exists() {
        return Err(tool_error(
            "CWAPI_WORKSPACE_ALREADY_EXISTS",
            "workspacePath already exists",
        ));
    }
    Ok(())
}

fn validate_workspace_member(root: &Path, target: &Path) -> Result<(), ToolError> {
    if !root.is_absolute() || !target.is_absolute() || target.parent() != Some(root) {
        return Err(tool_error(
            "CWAPI_WORKSPACE_PATH_INVALID",
            "workspacePath must be a direct child of workspaceRoot",
        ));
    }
    Ok(())
}

fn validate_commit(value: &str) -> Result<(), ToolError> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(tool_error(
            "CWAPI_COMMIT_INVALID",
            "expectedCommit must be a full 40-character hexadecimal SHA",
        ));
    }
    Ok(())
}

async fn run_git(git_path: &Path, cwd: Option<&Path>, args: &[&str]) -> Result<Output, ToolError> {
    let mut command = Command::new(git_path);
    command.args(args).kill_on_drop(true);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = timeout(TOOL_TIMEOUT, command.output())
        .await
        .map_err(|_| tool_error("CWAPI_GIT_TIMEOUT", "structured Git operation timed out"))?
        .map_err(|error| tool_error("CWAPI_GIT_START_FAILED", error.to_string()))?;
    if output.stdout.len() > OUTPUT_LIMIT || output.stderr.len() > OUTPUT_LIMIT {
        return Err(tool_error(
            "CWAPI_GIT_OUTPUT_TOO_LARGE",
            "structured Git operation exceeded output limit",
        ));
    }
    if !output.status.success() {
        return Err(tool_error(
            "CWAPI_GIT_FAILED",
            bounded_stderr(&output.stderr),
        ));
    }
    Ok(output)
}

fn stdout_line(output: &Output) -> Result<String, ToolError> {
    bounded_stdout(&output.stdout, OUTPUT_LIMIT).map(|value| value.trim().to_string())
}

fn bounded_stdout(value: &[u8], limit: usize) -> Result<String, ToolError> {
    if value.len() > limit {
        return Err(tool_error(
            "CWAPI_GIT_OUTPUT_TOO_LARGE",
            "structured Git output exceeded tool result limit",
        ));
    }
    String::from_utf8(value.to_vec())
        .map(|value| {
            value
                .trim_end_matches(|c| c == '\r' || c == '\n')
                .to_string()
        })
        .map_err(|_| tool_error("CWAPI_GIT_OUTPUT_INVALID", "Git stdout was not UTF-8"))
}

fn bounded_stderr(value: &[u8]) -> String {
    let limit = value.len().min(8192);
    String::from_utf8_lossy(&value[..limit]).trim().to_string()
}

fn path_text(path: &Path) -> Result<String, ToolError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| tool_error("CWAPI_PATH_ENCODING_INVALID", "path is not valid UTF-8"))
}

#[derive(Debug)]
struct ToolError {
    code: &'static str,
    message: String,
}

fn tool_error(code: &'static str, message: impl Into<String>) -> ToolError {
    ToolError {
        code,
        message: message.into(),
    }
}

fn success_response<T: Serialize>(value: T) -> McpServerToolCallResponse {
    let structured = serde_json::to_value(value).unwrap_or_else(|_| json!({"ok": true}));
    McpServerToolCallResponse {
        content: vec![json!({"type": "text", "text": structured.to_string()})],
        structured_content: Some(structured),
        is_error: Some(false),
        meta: Some(json!({"server": SERVER_NAME, "modelTurnStarted": false})),
    }
}

fn error_response(error: ToolError) -> McpServerToolCallResponse {
    let structured = json!({
        "ok": false,
        "error": {
            "code": error.code,
            "message": error.message,
        }
    });
    McpServerToolCallResponse {
        content: vec![json!({"type": "text", "text": structured.to_string()})],
        structured_content: Some(structured),
        is_error: Some(true),
        meta: Some(json!({"server": SERVER_NAME, "modelTurnStarted": false})),
    }
}

#[cfg(test)]
#[path = "cwapi_dev_mcp_tests.rs"]
mod tests;
