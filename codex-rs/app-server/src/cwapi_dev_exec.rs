use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::Path;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExecFailure {
    RuntimeMissing,
    StartFailed,
    TimedOut,
    OutputTooLarge,
    Failed,
}

pub(super) async fn run_bounded(
    program: &str,
    argv: &[OsString],
    cwd: &Path,
    timeout_limit: Duration,
    output_limit: usize,
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

    let status = match timeout(timeout_limit, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(ExecFailure::Failed);
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(ExecFailure::TimedOut);
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
}
