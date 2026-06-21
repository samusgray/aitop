use std::{
    fs,
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

pub fn repo_root_for_path(path: &Path) -> Option<PathBuf> {
    let mut current = if path.is_file() {
        path.parent()?.to_path_buf()
    } else {
        path.to_path_buf()
    };

    loop {
        let dot_git = current.join(".git");
        if dot_git.is_dir() {
            return Some(current);
        }
        if dot_git.is_file() {
            return root_from_git_file(&dot_git).or(Some(current));
        }
        if !current.pop() {
            return None;
        }
    }
}

fn root_from_git_file(dot_git: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(dot_git).ok()?;
    let gitdir = text.trim().strip_prefix("gitdir:")?.trim();
    let gitdir = PathBuf::from(gitdir);
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        dot_git.parent()?.join(gitdir)
    };
    let gitdir = gitdir.canonicalize().unwrap_or(gitdir);
    let mut parts = gitdir.components().peekable();
    let mut root = PathBuf::new();
    while let Some(part) = parts.next() {
        if part.as_os_str() == ".git"
            && matches!(
                parts.peek(),
                Some(next) if next.as_os_str() == "worktrees"
            )
        {
            return Some(root);
        }
        root.push(part.as_os_str());
    }
    dot_git.parent().map(Path::to_path_buf)
}

pub fn status_for_cwd(cwd: &Path) -> Option<GitStatus> {
    let root = git_output(cwd, &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(root.trim());
    let root = repo_root_for_path(&root).unwrap_or(root);
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
