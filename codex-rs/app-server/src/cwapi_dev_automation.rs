use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::fs::File;
use std::io::ErrorKind;
use std::io::Read;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

use super::ToolError;
use super::exact_workspace_commit;
use super::tool_error;
use super::validate_commit;
use super::validate_existing_directory;
use super::validate_git_path;
use super::validate_workspace_member;

const AUTOMATION_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const AUTOMATION_OUTPUT_LIMIT: usize = 1024 * 1024;
const AUTOMATION_SCRIPT_MAX_BYTES: u64 = 4 * 1024 * 1024;
const AUTOMATION_ENTRYPOINT_MAX_BYTES: usize = 240;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct AutomationRunArgs {
    git_path: PathBuf,
    workspace_root: PathBuf,
    workspace_path: PathBuf,
    expected_commit: String,
    entrypoint: String,
    sha256: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AutomationResult {
    actual_commit: String,
    entrypoint: String,
    sha256: String,
    exit_code: i32,
}

pub(super) async fn automation_run(
    arguments: Option<JsonValue>,
) -> Result<AutomationResult, ToolError> {
    let args = decode_args(arguments)?;
    let workspace = validate_workspace(&args)?;
    let expected_sha256 = validate_sha256(&args.sha256)?;
    let script = reviewed_script_path(&workspace, &args.entrypoint)?;

    exact_workspace_commit(&args.git_path, &workspace, &args.expected_commit).await?;
    verify_script_hash(&script, &expected_sha256)?;
    run_reviewed_script(&script, &workspace).await?;
    let actual_commit =
        exact_workspace_commit(&args.git_path, &workspace, &args.expected_commit).await?;
    verify_script_hash(&script, &expected_sha256)?;

    Ok(AutomationResult {
        actual_commit,
        entrypoint: args.entrypoint,
        sha256: expected_sha256,
        exit_code: 0,
    })
}

fn decode_args(arguments: Option<JsonValue>) -> Result<AutomationRunArgs, ToolError> {
    serde_json::from_value(arguments.unwrap_or_else(|| json!({}))).map_err(|_| {
        tool_error(
            "CWAPI_AUTOMATION_ARGUMENTS_INVALID",
            "invalid structured automation arguments",
        )
    })
}

fn validate_workspace(args: &AutomationRunArgs) -> Result<PathBuf, ToolError> {
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

fn validate_sha256(value: &str) -> Result<String, ToolError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(tool_error(
            "CWAPI_AUTOMATION_SHA256_INVALID",
            "sha256 must be a full 64-character hexadecimal digest",
        ));
    }
    Ok(value.to_ascii_lowercase())
}

fn reviewed_script_path(workspace: &Path, entrypoint: &str) -> Result<PathBuf, ToolError> {
    if entrypoint.len() > AUTOMATION_ENTRYPOINT_MAX_BYTES || entrypoint.contains('\\') {
        return Err(tool_error(
            "CWAPI_AUTOMATION_ENTRYPOINT_INVALID",
            "automation entrypoint is invalid",
        ));
    }
    let relative = Path::new(entrypoint);
    if relative.is_absolute() {
        return Err(tool_error(
            "CWAPI_AUTOMATION_ENTRYPOINT_INVALID",
            "automation entrypoint must be repository relative",
        ));
    }
    let mut components = relative.components();
    match components.next() {
        Some(Component::Normal(value)) if value == "automation" => {}
        _ => {
            return Err(tool_error(
                "CWAPI_AUTOMATION_ENTRYPOINT_INVALID",
                "automation entrypoint must be under automation",
            ));
        }
    }
    if components.any(|component| !matches!(component, Component::Normal(_))) {
        return Err(tool_error(
            "CWAPI_AUTOMATION_ENTRYPOINT_INVALID",
            "automation entrypoint contains invalid path components",
        ));
    }
    let file_name = relative.file_name().and_then(|value| value.to_str()).ok_or_else(|| {
        tool_error(
            "CWAPI_AUTOMATION_ENTRYPOINT_INVALID",
            "automation entrypoint filename is invalid",
        )
    })?;
    if !file_name.ends_with("_reviewed.ps1") {
        return Err(tool_error(
            "CWAPI_AUTOMATION_ENTRYPOINT_NOT_REVIEWED",
            "automation entrypoint must be a reviewed PowerShell wrapper",
        ));
    }

    let automation_root = std::fs::canonicalize(workspace.join("automation")).map_err(|_| {
        tool_error(
            "CWAPI_AUTOMATION_ROOT_MISSING",
            "workspace automation directory is unavailable",
        )
    })?;
    let script = std::fs::canonicalize(workspace.join(relative)).map_err(|_| {
        tool_error(
            "CWAPI_AUTOMATION_ENTRYPOINT_MISSING",
            "reviewed automation entrypoint is unavailable",
        )
    })?;
    if !script.is_file() || !script.starts_with(&automation_root) {
        return Err(tool_error(
            "CWAPI_AUTOMATION_ENTRYPOINT_INVALID",
            "reviewed automation entrypoint escaped the automation root",
        ));
    }
    Ok(script)
}

fn verify_script_hash(script: &Path, expected_sha256: &str) -> Result<(), ToolError> {
    let metadata = script.metadata().map_err(|_| {
        tool_error(
            "CWAPI_AUTOMATION_ENTRYPOINT_MISSING",
            "reviewed automation entrypoint metadata is unavailable",
        )
    })?;
    if metadata.len() > AUTOMATION_SCRIPT_MAX_BYTES {
        return Err(tool_error(
            "CWAPI_AUTOMATION_ENTRYPOINT_TOO_LARGE",
            "reviewed automation entrypoint exceeds the size limit",
        ));
    }
    let mut file = File::open(script).map_err(|_| {
        tool_error(
            "CWAPI_AUTOMATION_ENTRYPOINT_MISSING",
            "reviewed automation entrypoint could not be opened",
        )
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = file.read(&mut buffer).map_err(|_| {
            tool_error(
                "CWAPI_AUTOMATION_HASH_FAILED",
                "reviewed automation entrypoint could not be hashed",
            )
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected_sha256 {
        return Err(tool_error(
            "CWAPI_AUTOMATION_SHA256_MISMATCH",
            "reviewed automation entrypoint hash did not match",
        ));
    }
    Ok(())
}

async fn run_reviewed_script(script: &Path, workspace: &Path) -> Result<(), ToolError> {
    let mut command = Command::new("pwsh");
    command
        .arg("-NoLogo")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-File")
        .arg(script)
        .current_dir(workspace)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            tool_error(
                "CWAPI_AUTOMATION_RUNTIME_MISSING",
                "PowerShell runtime is unavailable",
            )
        } else {
            tool_error(
                "CWAPI_AUTOMATION_START_FAILED",
                "reviewed automation could not start",
            )
        }
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        tool_error(
            "CWAPI_AUTOMATION_START_FAILED",
            "reviewed automation stdout pipe was unavailable",
        )
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        tool_error(
            "CWAPI_AUTOMATION_START_FAILED",
            "reviewed automation stderr pipe was unavailable",
        )
    })?;
    let stdout_task = tokio::spawn(drain_bounded(stdout, AUTOMATION_OUTPUT_LIMIT));
    let stderr_task = tokio::spawn(drain_bounded(stderr, AUTOMATION_OUTPUT_LIMIT));

    let status = match timeout(AUTOMATION_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(tool_error(
                "CWAPI_AUTOMATION_FAILED",
                "reviewed automation wait failed",
            ));
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(tool_error(
                "CWAPI_AUTOMATION_TIMED_OUT",
                "reviewed automation timed out",
            ));
        }
    };

    let stdout_overflow = reader_overflow(stdout_task).await?;
    let stderr_overflow = reader_overflow(stderr_task).await?;
    if stdout_overflow || stderr_overflow {
        return Err(tool_error(
            "CWAPI_AUTOMATION_OUTPUT_TOO_LARGE",
            "reviewed automation exceeded bounded output",
        ));
    }
    if !status.success() {
        return Err(tool_error(
            "CWAPI_AUTOMATION_FAILED",
            "reviewed automation failed",
        ));
    }
    Ok(())
}

async fn drain_bounded<R>(mut reader: R, limit: usize) -> std::io::Result<bool>
where
    R: AsyncRead + Unpin,
{
    let mut total = 0usize;
    let mut overflow = false;
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(overflow);
        }
        total = total.saturating_add(read);
        if total > limit {
            overflow = true;
        }
    }
}

async fn reader_overflow(
    task: tokio::task::JoinHandle<std::io::Result<bool>>,
) -> Result<bool, ToolError> {
    match task.await {
        Ok(Ok(overflow)) => Ok(overflow),
        _ => Err(tool_error(
            "CWAPI_AUTOMATION_FAILED",
            "reviewed automation output drain failed",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn automation_arguments_reject_free_command_fields() {
        let value = json!({
            "gitPath": "/git",
            "workspaceRoot": "/worktrees",
            "workspacePath": "/worktrees/ws-1",
            "expectedCommit": "0123456789abcdef0123456789abcdef01234567",
            "entrypoint": "automation/x_reviewed.ps1",
            "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "arguments": ["-Command", "whoami"]
        });
        assert!(serde_json::from_value::<AutomationRunArgs>(value).is_err());
    }

    #[test]
    fn automation_sha256_requires_full_digest() {
        assert!(validate_sha256("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").is_ok());
        assert!(validate_sha256("abc").is_err());
        assert!(validate_sha256("z123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").is_err());
    }

    #[test]
    fn script_hash_is_exact_bytes() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"abc\r\ndef\n").unwrap();
        let expected = format!("{:x}", Sha256::digest(b"abc\r\ndef\n"));
        verify_script_hash(file.path(), &expected).unwrap();
        assert!(verify_script_hash(file.path(), &"0".repeat(64)).is_err());
    }

    #[tokio::test]
    async fn automation_output_drain_reports_overflow() {
        let (mut writer, reader) = tokio::io::duplex(32);
        let write = tokio::spawn(async move {
            writer.write_all(&vec![b'x'; 128]).await.unwrap();
        });
        let overflow = drain_bounded(reader, 64).await.unwrap();
        write.await.unwrap();
        assert!(overflow);
    }
}
