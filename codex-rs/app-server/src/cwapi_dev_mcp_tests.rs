use super::*;

#[test]
fn commit_requires_full_sha() {
    assert!(validate_commit("0123456789abcdef0123456789abcdef01234567").is_ok());
    assert!(validate_commit("HEAD").is_err());
    assert!(validate_commit("0123456789abcdef0123456789abcdef0123456z").is_err());
}

#[test]
fn workspace_must_be_direct_child_of_root() {
    let root = if cfg!(windows) {
        PathBuf::from(r"C:\cwapi\worktrees")
    } else {
        PathBuf::from("/tmp/cwapi/worktrees")
    };
    assert!(validate_workspace_member(&root, &root.join("ws-1")).is_ok());
    assert!(validate_workspace_member(&root, &root.join("nested").join("ws-1")).is_err());
}

#[test]
fn git_args_reject_backend_field_injection() {
    let value = json!({
        "gitPath": "/git",
        "workspaceRoot": "/worktrees",
        "workspacePath": "/worktrees/ws-1",
        "expectedCommit": "0123456789abcdef0123456789abcdef01234567",
        "command": "status"
    });
    assert!(serde_json::from_value::<WorkspaceGitArgs>(value).is_err());
}

#[test]
fn status_output_is_bounded() {
    let exact = vec![b'x'; STATUS_PORCELAIN_LIMIT];
    assert!(bounded_stdout(&exact, STATUS_PORCELAIN_LIMIT).is_ok());
    let oversized = vec![b'x'; STATUS_PORCELAIN_LIMIT + 1];
    let error = bounded_stdout(&oversized, STATUS_PORCELAIN_LIMIT).unwrap_err();
    assert_eq!(error.code, "CWAPI_GIT_OUTPUT_TOO_LARGE");
}
