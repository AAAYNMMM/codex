use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub(super) struct OutputPaths {
    pub(super) stdout: PathBuf,
    pub(super) stderr: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct OutputResource {
    pub(super) kind: &'static str,
    pub(super) sha256: String,
    pub(super) size_bytes: u64,
}

pub(super) fn prepare_output_paths(
    resource_root: &Path,
    execution_id: &str,
) -> std::io::Result<OutputPaths> {
    if !resource_root.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "resource root must be absolute",
        ));
    }
    std::fs::create_dir_all(resource_root)?;
    let request_key = format!("{:x}", Sha256::digest(execution_id.as_bytes()));
    let directory = resource_root.join(request_key);
    std::fs::create_dir_all(&directory)?;
    Ok(OutputPaths {
        stdout: directory.join("stdout"),
        stderr: directory.join("stderr"),
    })
}

pub(super) fn output_resources(paths: &OutputPaths) -> std::io::Result<Vec<OutputResource>> {
    Ok(vec![
        metadata("stdout", &paths.stdout)?,
        metadata("stderr", &paths.stderr)?,
    ])
}

fn metadata(kind: &'static str, path: &Path) -> std::io::Result<OutputResource> {
    let bytes = std::fs::read(path)?;
    Ok(OutputResource {
        kind,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        size_bytes: bytes.len() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_paths_do_not_embed_request_identity() {
        let dir = tempfile::tempdir().unwrap();
        let paths = prepare_output_paths(dir.path(), "REQ:with:colon").unwrap();
        let parent = paths.stdout.parent().unwrap();
        assert_ne!(
            parent.file_name().unwrap().to_string_lossy(),
            "REQ:with:colon"
        );
        assert_eq!(parent.file_name().unwrap().to_string_lossy().len(), 64);
    }

    #[test]
    fn output_metadata_is_hash_bound() {
        let dir = tempfile::tempdir().unwrap();
        let paths = prepare_output_paths(dir.path(), "REQ-output").unwrap();
        std::fs::write(&paths.stdout, b"hello").unwrap();
        std::fs::write(&paths.stderr, b"").unwrap();
        let resources = output_resources(&paths).unwrap();
        assert_eq!(resources[0].kind, "stdout");
        assert_eq!(resources[0].size_bytes, 5);
        assert_eq!(
            resources[0].sha256,
            format!("{:x}", Sha256::digest(b"hello"))
        );
    }
}
