use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio::process::Command;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExecFailure {
    RuntimeMissing,
    StartFailed,
    TimedOut,
    Cancelled,
    OutputTooLarge,
    Failed,
}

enum WaitOutcome {
    Exited(std::io::Result<ExitStatus>),
    TimedOut,
    Cancelled,
}

pub(super) async fn run_bounded(
    program: &str,
    argv: &[OsString],
    cwd: &Path,
    timeout_limit: Duration,
    output_limit: usize,
) -> Result<ExitStatus, ExecFailure> {
    run_bounded_inner(program, argv, cwd, timeout_limit, output_limit, None).await
}

pub(super) async fn run_bounded_cancellable(
    program: &str,
    argv: &[OsString],
    cwd: &Path,
    timeout_limit: Duration,
    output_limit: usize,
    cancellation: CancellationToken,
) -> Result<ExitStatus, ExecFailure> {
    run_bounded_inner(
        program,
        argv,
        cwd,
        timeout_limit,
        output_limit,
        Some(cancellation),
    )
    .await
}

async fn run_bounded_inner(
    program: &str,
    argv: &[OsString],
    cwd: &Path,
    timeout_limit: Duration,
    output_limit: usize,
    cancellation: Option<CancellationToken>,
) -> Result<ExitStatus, ExecFailure> {
    let mut command = Command::new(program);
    command
        .args(argv)
        .current_dir(cwd)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            ExecFailure::RuntimeMissing
        } else {
            ExecFailure::StartFailed
        }
    })?;
    let stdout = child.stdout.take().ok_or(ExecFailure::StartFailed)?;
    let stderr = child.stderr.take().ok_or(ExecFailure::StartFailed)?;
    let stdout_task = tokio::spawn(drain_bounded(stdout, output_limit));
    let stderr_task = tokio::spawn(drain_bounded(stderr, output_limit));

    let status = match wait_for_child(&mut child, timeout_limit, cancellation).await {
        WaitOutcome::Exited(Ok(status)) => status,
        WaitOutcome::Exited(Err(_)) => {
            let _ = terminate_owned_tree(&mut child).await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(ExecFailure::Failed);
        }
        WaitOutcome::TimedOut => {
            if terminate_owned_tree(&mut child).await.is_err() {
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return Err(ExecFailure::Failed);
            }
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(ExecFailure::TimedOut);
        }
        WaitOutcome::Cancelled => {
            if terminate_owned_tree(&mut child).await.is_err() {
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return Err(ExecFailure::Failed);
            }
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(ExecFailure::Cancelled);
        }
    };

    let stdout_overflow = reader_overflow(stdout_task).await?;
    let stderr_overflow = reader_overflow(stderr_task).await?;
    if stdout_overflow || stderr_overflow {
        return Err(ExecFailure::OutputTooLarge);
    }
    if !status.success() {
        return Err(ExecFailure::Failed);
    }
    Ok(status)
}

async fn wait_for_child(
    child: &mut Child,
    timeout_limit: Duration,
    cancellation: Option<CancellationToken>,
) -> WaitOutcome {
    let wait = child.wait();
    tokio::pin!(wait);
    let timer = sleep(timeout_limit);
    tokio::pin!(timer);
    if let Some(token) = cancellation {
        tokio::select! {
            result = &mut wait => WaitOutcome::Exited(result),
            _ = &mut timer => WaitOutcome::TimedOut,
            _ = token.cancelled() => WaitOutcome::Cancelled,
        }
    } else {
        tokio::select! {
            result = &mut wait => WaitOutcome::Exited(result),
            _ = &mut timer => WaitOutcome::TimedOut,
        }
    }
}

#[cfg(windows)]
async fn terminate_owned_tree(child: &mut Child) -> Result<(), ()> {
    let pid = child.id().ok_or(())?;
    let taskkill = windows_taskkill_path().ok_or(())?;
    let status = Command::new(taskkill)
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|_| ())?;
    if !status.success() {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return Ok(());
        }
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(());
    }
    child.wait().await.map(|_| ()).map_err(|_| ())
}

#[cfg(windows)]
fn windows_taskkill_path() -> Option<PathBuf> {
    let system_root = std::env::var_os("SystemRoot")?;
    let path = PathBuf::from(system_root)
        .join("System32")
        .join("taskkill.exe");
    path.is_file().then_some(path)
}

#[cfg(not(windows))]
async fn terminate_owned_tree(child: &mut Child) -> Result<(), ()> {
    child.kill().await.map_err(|_| ())?;
    child.wait().await.map(|_| ()).map_err(|_| ())
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
) -> Result<bool, ExecFailure> {
    match task.await {
        Ok(Ok(overflow)) => Ok(overflow),
        _ => Err(ExecFailure::Failed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn bounded_drain_reports_overflow_without_buffering_stream() {
        let (mut writer, reader) = tokio::io::duplex(32);
        let write = tokio::spawn(async move {
            writer.write_all(&vec![b'x'; 128]).await.unwrap();
        });
        let overflow = drain_bounded(reader, 64).await.unwrap();
        write.await.unwrap();
        assert!(overflow);
    }

    #[test]
    fn cancelled_is_distinct_from_timeout() {
        assert_ne!(ExecFailure::Cancelled, ExecFailure::TimedOut);
    }

    #[cfg(windows)]
    #[test]
    fn owned_tree_termination_uses_system_taskkill() {
        let path = windows_taskkill_path().expect("SystemRoot taskkill.exe must exist");
        assert!(path.is_absolute());
        assert_eq!(
            path.file_name().and_then(|value| value.to_str()),
            Some("taskkill.exe")
        );
    }
}
