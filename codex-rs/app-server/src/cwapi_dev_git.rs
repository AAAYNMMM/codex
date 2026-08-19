use std::ffi::OsString;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::oneshot;
use tokio::time::timeout;

const PROCESS_START_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESS_STOP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(super) enum GitExecError {
    Start(String),
    Wait(String),
    TimedOut,
    Cleanup(String),
    Join(String),
}

impl GitExecError {
    pub(super) fn is_timeout(&self) -> bool {
        matches!(self, Self::TimedOut | Self::Cleanup(_))
    }
}

impl fmt::Display for GitExecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Start(message) => write!(formatter, "failed to start structured Git: {message}"),
            Self::Wait(message) => write!(formatter, "failed while waiting for structured Git: {message}"),
            Self::TimedOut => formatter.write_str("structured Git operation timed out"),
            Self::Cleanup(message) => write!(formatter, "structured Git timeout cleanup failed: {message}"),
            Self::Join(message) => write!(formatter, "structured Git worker failed: {message}"),
        }
    }
}

pub(super) async fn run(
    git_path: &Path,
    cwd: Option<&Path>,
    args: &[&str],
    timeout_limit: Duration,
) -> Result<Output, GitExecError> {
    let program = git_path.to_path_buf();
    let cwd = cwd.map(Path::to_path_buf);
    let argv = args.iter().map(OsString::from).collect::<Vec<_>>();
    let (pid_tx, pid_rx) = oneshot::channel::<u32>();
    let started = Instant::now();

    // Git for Windows can stall when launched through tokio::process from the
    // app-server process. Keep this small structured Git surface on the
    // standard-library process implementation while preserving an async API.
    let mut worker = tokio::task::spawn_blocking(move || {
        let mut command = std::process::Command::new(program);
        command.args(argv).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let child = command
            .spawn()
            .map_err(|error| GitExecError::Start(error.to_string()))?;
        let _ = pid_tx.send(child.id());
        child
            .wait_with_output()
            .map_err(|error| GitExecError::Wait(error.to_string()))
    });

    let start_budget = timeout_limit.min(PROCESS_START_TIMEOUT);
    let pid = match timeout(start_budget, pid_rx).await {
        Ok(Ok(pid)) => pid,
        Ok(Err(_)) => return join_worker(&mut worker).await,
        Err(_) => {
            worker.abort();
            return Err(GitExecError::TimedOut);
        }
    };

    let remaining = timeout_limit.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return stop_timed_out_worker(pid, &mut worker).await;
    }

    match timeout(remaining, &mut worker).await {
        Ok(joined) => flatten_join(joined),
        Err(_) => stop_timed_out_worker(pid, &mut worker).await,
    }
}

async fn join_worker(
    worker: &mut tokio::task::JoinHandle<Result<Output, GitExecError>>,
) -> Result<Output, GitExecError> {
    flatten_join(worker.await)
}

fn flatten_join(
    joined: Result<Result<Output, GitExecError>, tokio::task::JoinError>,
) -> Result<Output, GitExecError> {
    joined.map_err(|error| GitExecError::Join(error.to_string()))?
}

async fn stop_timed_out_worker(
    pid: u32,
    worker: &mut tokio::task::JoinHandle<Result<Output, GitExecError>>,
) -> Result<Output, GitExecError> {
    let cleanup = terminate_process_tree(pid).await;
    match timeout(PROCESS_STOP_TIMEOUT, worker).await {
        Ok(_) => Err(GitExecError::TimedOut),
        Err(_) => match cleanup {
            Ok(()) => Err(GitExecError::Cleanup(
                "Git worker did not exit after process-tree termination".to_string(),
            )),
            Err(message) => Err(GitExecError::Cleanup(message)),
        },
    }
}

#[cfg(windows)]
async fn terminate_process_tree(pid: u32) -> Result<(), String> {
    let system_root =
        std::env::var_os("SystemRoot").ok_or_else(|| "SystemRoot is unavailable".to_string())?;
    let taskkill = PathBuf::from(system_root)
        .join("System32")
        .join("taskkill.exe");
    if !taskkill.is_file() {
        return Err(format!(
            "taskkill.exe is unavailable at {}",
            taskkill.display()
        ));
    }
    let status = tokio::process::Command::new(taskkill)
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("taskkill exited with {status}"))
    }
}

#[cfg(not(windows))]
async fn terminate_process_tree(pid: u32) -> Result<(), String> {
    let status = tokio::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("kill exited with {status}"))
    }
}
