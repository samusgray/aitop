use std::{
    path::{Path, PathBuf},
    process::Command,
};

use crate::model::{DirtyFile, GitStatus};

pub fn project_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

pub fn status_for_cwd(cwd: &Path) -> Option<GitStatus> {
    let root = git_output(cwd, &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(root.trim());
    let branch = git_output(cwd, &["branch", "--show-current"])
        .map(|branch| branch.trim().to_string())
        .filter(|branch| !branch.is_empty());
    let status = git_output(cwd, &["status", "--porcelain=v1"]).unwrap_or_default();
    let dirty_files = status
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| DirtyFile {
            code: line.get(0..2).unwrap_or("").trim().to_string(),
            path: line.get(3..).unwrap_or("").to_string(),
        })
        .collect();

    Some(GitStatus {
        root,
        branch,
        dirty_files,
    })
}

fn git_output(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}
