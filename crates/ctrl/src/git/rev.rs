//! Pin deploys to commits (#448): which commit a source builds, and a fresh
//! checkout of a recorded one.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command;

use super::client::{GitClient, checkout_root, reserve_checkout_dir};
use super::lease::CheckoutLease;

/// The commit a source builds, and whether the deployed tree differs from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRev {
    /// Full commit id of HEAD.
    pub rev: String,
    /// Files under the deployed dir differ from `rev`, including untracked
    /// files, so `rev` alone does not reproduce the deploy.
    pub dirty: bool,
}

/// A full, lowercase, 40-hex commit id (the only form ctrl pins to).
pub fn is_commit_id(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Read-only git in `dir`. Local deploy paths may belong to another user, and
/// `status` must not run a repo-configured fsmonitor command.
fn git_in(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "safe.directory=*", "-c", "core.fsmonitor=false", "-C"])
        .arg(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

async fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let out = git_in(dir).args(args).output().await.ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Commit and dirtiness of the tree at `dir` (a repo or a subdir of one).
/// `None` when `dir` is not in a git work tree with a commit.
pub async fn source_rev(dir: &Path, config_path: &str) -> Option<SourceRev> {
    let rev = git_stdout(dir, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]).await?;
    let status = git_stdout(
        dir,
        &["status", "--porcelain", "--untracked-files=all", "--", "."],
    )
    .await?;
    let tracked = git_stdout(dir, &["ls-files", "--error-unmatch", "--", config_path])
        .await
        .is_some();
    Some(SourceRev {
        rev: rev.trim().to_string(),
        dirty: !status.trim().is_empty() || !tracked,
    })
}

impl GitClient {
    /// A fresh checkout of `repo` at commit `rev`, to rebuild a recorded
    /// generation. A local path is cloned, never checked out in place, so the
    /// operator's working tree is untouched. Returns the dir matching the
    /// deployed path: a subdir of the clone when the local path was a subdir
    /// of its repo.
    pub async fn checkout_rev(&self, repo: &str, rev: &str) -> Result<(PathBuf, CheckoutLease)> {
        if !is_commit_id(rev) {
            anyhow::bail!("rev must be a full 40-character lowercase commit id (got {rev:?})");
        }
        // Same validation as an unpinned deploy: allowlist, local-path gate.
        let (source, lease) = self.clone_or_use_local(repo).await?;
        let (checkout, lease, subdir) = if Path::new(repo).is_absolute() {
            drop(lease);
            let toplevel = git_stdout(&source, &["rev-parse", "--show-toplevel"])
                .await
                .ok_or_else(|| anyhow::anyhow!("{repo} is not in a git repository"))?;
            let prefix = git_stdout(&source, &["rev-parse", "--show-prefix"])
                .await
                .unwrap_or_default();
            let root = checkout_root();
            std::fs::create_dir_all(&root).context("failed to create checkout directory")?;
            let checkout = reserve_checkout_dir(&root, repo)?;
            let lease = CheckoutLease::active(checkout.clone());
            let out = Command::new("git")
                .args([
                    "-c",
                    "safe.directory=*",
                    "clone",
                    "--quiet",
                    "--no-checkout",
                    "--",
                ])
                .arg(toplevel.trim())
                .arg(&checkout)
                .output()
                .await?;
            if !out.status.success() {
                anyhow::bail!(
                    "clone {} for rev {rev}: {}",
                    toplevel.trim(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            (checkout, lease, PathBuf::from(prefix.trim()))
        } else {
            (source, lease, PathBuf::new())
        };

        let out = Command::new("git")
            .arg("-C")
            .arg(&checkout)
            .args(["checkout", "--quiet", "--detach", rev])
            .output()
            .await
            .context("git checkout")?;
        if !out.status.success() {
            anyhow::bail!(
                "commit {rev} is not in {}; was its history rewritten? ({})",
                super::redact_repo_url(repo),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok((checkout.join(subdir), lease))
    }
}
