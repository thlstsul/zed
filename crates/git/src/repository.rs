use crate::status::FileStatus;
use crate::SHORT_SHA_LENGTH;
use crate::{blame::Blame, status::GitStatus};
use anyhow::{anyhow, Context, Result};
use askpass::{AskPassResult, AskPassSession};
use collections::{HashMap, HashSet};
use futures::{select_biased, FutureExt as _};
use git2::BranchType;
use gpui::SharedString;
use parking_lot::Mutex;
use rope::Rope;
use schemars::JsonSchema;
use serde::Deserialize;
use std::borrow::Borrow;
use std::io::Write as _;
use std::process::Stdio;
use std::sync::LazyLock;
use std::{
    cmp::Ordering,
    path::{Component, Path, PathBuf},
    sync::Arc,
};
use sum_tree::MapSeekTarget;
use util::command::{new_smol_command, new_std_command};
use util::ResultExt;

pub const REMOTE_CANCELLED_BY_USER: &str = "Operation cancelled by user";

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Branch {
    pub is_head: bool,
    pub name: SharedString,
    pub upstream: Option<Upstream>,
    pub most_recent_commit: Option<CommitSummary>,
}

impl Branch {
    pub fn tracking_status(&self) -> Option<UpstreamTrackingStatus> {
        self.upstream
            .as_ref()
            .and_then(|upstream| upstream.tracking.status())
    }

    pub fn priority_key(&self) -> (bool, Option<i64>) {
        (
            self.is_head,
            self.most_recent_commit
                .as_ref()
                .map(|commit| commit.commit_timestamp),
        )
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Upstream {
    pub ref_name: SharedString,
    pub tracking: UpstreamTracking,
}

impl Upstream {
    pub fn remote_name(&self) -> Option<&str> {
        self.ref_name
            .strip_prefix("refs/remotes/")
            .and_then(|stripped| stripped.split("/").next())
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum UpstreamTracking {
    /// Remote ref not present in local repository.
    Gone,
    /// Remote ref present in local repository (fetched from remote).
    Tracked(UpstreamTrackingStatus),
}

impl From<UpstreamTrackingStatus> for UpstreamTracking {
    fn from(status: UpstreamTrackingStatus) -> Self {
        UpstreamTracking::Tracked(status)
    }
}

impl UpstreamTracking {
    pub fn is_gone(&self) -> bool {
        matches!(self, UpstreamTracking::Gone)
    }

    pub fn status(&self) -> Option<UpstreamTrackingStatus> {
        match self {
            UpstreamTracking::Gone => None,
            UpstreamTracking::Tracked(status) => Some(*status),
        }
    }
}

#[derive(Debug)]
pub struct RemoteCommandOutput {
    pub stdout: String,
    pub stderr: String,
}

impl RemoteCommandOutput {
    pub fn is_empty(&self) -> bool {
        self.stdout.is_empty() && self.stderr.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct UpstreamTrackingStatus {
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct CommitSummary {
    pub sha: SharedString,
    pub subject: SharedString,
    /// This is a unix timestamp
    pub commit_timestamp: i64,
    pub has_parent: bool,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct CommitDetails {
    pub sha: SharedString,
    pub message: SharedString,
    pub commit_timestamp: i64,
    pub committer_email: SharedString,
    pub committer_name: SharedString,
}

impl CommitDetails {
    pub fn short_sha(&self) -> SharedString {
        self.sha[..SHORT_SHA_LENGTH].to_string().into()
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct Remote {
    pub name: SharedString,
}

pub enum ResetMode {
    // reset the branch pointer, leave index and worktree unchanged
    // (this will make it look like things that were committed are now
    // staged)
    Soft,
    // reset the branch pointer and index, leave worktree unchanged
    // (this makes it look as though things that were committed are now
    // unstaged)
    Mixed,
}

pub trait GitRepository: Send + Sync {
    fn reload_index(&self);

    /// Returns the contents of an entry in the repository's index, or None if there is no entry for the given path.
    ///
    /// Also returns `None` for symlinks.
    fn load_index_text(&self, path: &RepoPath) -> Option<String>;

    /// Returns the contents of an entry in the repository's HEAD, or None if HEAD does not exist or has no entry for the given path.
    ///
    /// Also returns `None` for symlinks.
    fn load_committed_text(&self, path: &RepoPath) -> Option<String>;

    fn set_index_text(
        &self,
        path: &RepoPath,
        content: Option<String>,
        env: &HashMap<String, String>,
    ) -> anyhow::Result<()>;

    /// Returns the URL of the remote with the given name.
    fn remote_url(&self, name: &str) -> Option<String>;

    /// Returns the SHA of the current HEAD.
    fn head_sha(&self) -> Option<String>;

    fn merge_head_shas(&self) -> Vec<String>;

    /// Returns the list of git statuses, sorted by path
    fn status(&self, path_prefixes: &[RepoPath]) -> Result<GitStatus>;

    fn branches(&self) -> Result<Vec<Branch>>;
    fn change_branch(&self, _: &str) -> Result<()>;
    fn create_branch(&self, _: &str) -> Result<()>;
    fn branch_exits(&self, _: &str) -> Result<bool>;

    fn reset(&self, commit: &str, mode: ResetMode, env: &HashMap<String, String>) -> Result<()>;
    fn checkout_files(
        &self,
        commit: &str,
        paths: &[RepoPath],
        env: &HashMap<String, String>,
    ) -> Result<()>;

    fn show(&self, commit: &str) -> Result<CommitDetails>;

    fn blame(&self, path: &Path, content: Rope) -> Result<crate::blame::Blame>;

    /// Returns the absolute path to the repository. For worktrees, this will be the path to the
    /// worktree's gitdir within the main repository (typically `.git/worktrees/<name>`).
    fn path(&self) -> PathBuf;

    /// Returns the absolute path to the ".git" dir for the main repository, typically a `.git`
    /// folder. For worktrees, this will be the path to the repository the worktree was created
    /// from. Otherwise, this is the same value as `path()`.
    ///
    /// Git documentation calls this the "commondir", and for git CLI is overridden by
    /// `GIT_COMMON_DIR`.
    fn main_repository_path(&self) -> PathBuf;

    /// Updates the index to match the worktree at the given paths.
    ///
    /// If any of the paths have been deleted from the worktree, they will be removed from the index if found there.
    fn stage_paths(&self, paths: &[RepoPath], env: &HashMap<String, String>) -> Result<()>;
    /// Updates the index to match HEAD at the given paths.
    ///
    /// If any of the paths were previously staged but do not exist in HEAD, they will be removed from the index.
    fn unstage_paths(&self, paths: &[RepoPath], env: &HashMap<String, String>) -> Result<()>;

    fn commit(
        &self,
        message: &str,
        name_and_email: Option<(&str, &str)>,
        env: &HashMap<String, String>,
    ) -> Result<()>;

    fn push(
        &self,
        branch_name: &str,
        upstream_name: &str,
        options: Option<PushOptions>,
        askpass: AskPassSession,
        env: &HashMap<String, String>,
    ) -> Result<RemoteCommandOutput>;

    fn pull(
        &self,
        branch_name: &str,
        upstream_name: &str,
        askpass: AskPassSession,
        env: &HashMap<String, String>,
    ) -> Result<RemoteCommandOutput>;
    fn fetch(
        &self,
        askpass: AskPassSession,
        env: &HashMap<String, String>,
    ) -> Result<RemoteCommandOutput>;

    fn get_remotes(&self, branch_name: Option<&str>) -> Result<Vec<Remote>>;

    /// returns a list of remote branches that contain HEAD
    fn check_for_pushed_commit(&self) -> Result<Vec<SharedString>>;

    /// Run git diff
    fn diff(&self, diff: DiffType) -> Result<String>;
}

pub enum DiffType {
    HeadToIndex,
    HeadToWorktree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
pub enum PushOptions {
    SetUpstream,
    Force,
}

impl std::fmt::Debug for dyn GitRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("dyn GitRepository<...>").finish()
    }
}

pub struct RealGitRepository {
    pub repository: Mutex<git2::Repository>,
    pub git_binary_path: PathBuf,
}

impl RealGitRepository {
    pub fn new(repository: git2::Repository, git_binary_path: Option<PathBuf>) -> Self {
        Self {
            repository: Mutex::new(repository),
            git_binary_path: git_binary_path.unwrap_or_else(|| PathBuf::from("git")),
        }
    }

    fn working_directory(&self) -> Result<PathBuf> {
        self.repository
            .lock()
            .workdir()
            .context("failed to read git work directory")
            .map(Path::to_path_buf)
    }
}

// https://git-scm.com/book/en/v2/Git-Internals-Git-Objects
const GIT_MODE_SYMLINK: u32 = 0o120000;

impl GitRepository for RealGitRepository {
    fn reload_index(&self) {
        if let Ok(mut index) = self.repository.lock().index() {
            _ = index.read(false);
        }
    }

    fn path(&self) -> PathBuf {
        let repo = self.repository.lock();
        repo.path().into()
    }

    fn main_repository_path(&self) -> PathBuf {
        let repo = self.repository.lock();
        repo.commondir().into()
    }

    fn show(&self, commit: &str) -> Result<CommitDetails> {
        let repo = self.repository.lock();
        let Ok(commit) = repo.revparse_single(commit)?.into_commit() else {
            anyhow::bail!("{} is not a commit", commit);
        };
        let details = CommitDetails {
            sha: commit.id().to_string().into(),
            message: String::from_utf8_lossy(commit.message_raw_bytes())
                .to_string()
                .into(),
            commit_timestamp: commit.time().seconds(),
            committer_email: String::from_utf8_lossy(commit.committer().email_bytes())
                .to_string()
                .into(),
            committer_name: String::from_utf8_lossy(commit.committer().name_bytes())
                .to_string()
                .into(),
        };
        Ok(details)
    }

    fn reset(&self, commit: &str, mode: ResetMode, env: &HashMap<String, String>) -> Result<()> {
        let working_directory = self.working_directory()?;

        let mode_flag = match mode {
            ResetMode::Mixed => "--mixed",
            ResetMode::Soft => "--soft",
        };

        let output = new_std_command(&self.git_binary_path)
            .envs(env)
            .current_dir(&working_directory)
            .args(["reset", mode_flag, commit])
            .output()?;
        if !output.status.success() {
            return Err(anyhow!(
                "Failed to reset:\n{}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(())
    }

    fn checkout_files(
        &self,
        commit: &str,
        paths: &[RepoPath],
        env: &HashMap<String, String>,
    ) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let working_directory = self.working_directory()?;

        let output = new_std_command(&self.git_binary_path)
            .current_dir(&working_directory)
            .envs(env)
            .args(["checkout", commit, "--"])
            .args(paths.iter().map(|path| path.as_ref()))
            .output()?;
        if !output.status.success() {
            return Err(anyhow!(
                "Failed to checkout files:\n{}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(())
    }

    fn load_index_text(&self, path: &RepoPath) -> Option<String> {
        fn logic(repo: &git2::Repository, path: &RepoPath) -> Result<Option<String>> {
            const STAGE_NORMAL: i32 = 0;
            let index = repo.index()?;

            // This check is required because index.get_path() unwraps internally :(
            check_path_to_repo_path_errors(path)?;

            let oid = match index.get_path(path, STAGE_NORMAL) {
                Some(entry) if entry.mode != GIT_MODE_SYMLINK => entry.id,
                _ => return Ok(None),
            };

            let content = repo.find_blob(oid)?.content().to_owned();
            Ok(Some(String::from_utf8(content)?))
        }

        match logic(&self.repository.lock(), path) {
            Ok(value) => return value,
            Err(err) => log::error!("Error loading index text: {:?}", err),
        }
        None
    }

    fn load_committed_text(&self, path: &RepoPath) -> Option<String> {
        let repo = self.repository.lock();
        let head = repo.head().ok()?.peel_to_tree().log_err()?;
        let entry = head.get_path(path).ok()?;
        if entry.filemode() == i32::from(git2::FileMode::Link) {
            return None;
        }
        let content = repo.find_blob(entry.id()).log_err()?.content().to_owned();
        let content = String::from_utf8(content).log_err()?;
        Some(content)
    }

    fn set_index_text(
        &self,
        path: &RepoPath,
        content: Option<String>,
        env: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        let working_directory = self.working_directory()?;
        if let Some(content) = content {
            let mut child = new_std_command(&self.git_binary_path)
                .current_dir(&working_directory)
                .envs(env)
                .args(["hash-object", "-w", "--stdin"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()?;
            child.stdin.take().unwrap().write_all(content.as_bytes())?;
            let output = child.wait_with_output()?.stdout;
            let sha = String::from_utf8(output)?;

            log::debug!("indexing SHA: {sha}, path {path:?}");

            let output = new_std_command(&self.git_binary_path)
                .current_dir(&working_directory)
                .envs(env)
                .args(["update-index", "--add", "--cacheinfo", "100644", &sha])
                .arg(path.as_ref())
                .output()?;

            if !output.status.success() {
                return Err(anyhow!(
                    "Failed to stage:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        } else {
            let output = new_std_command(&self.git_binary_path)
                .current_dir(&working_directory)
                .envs(env)
                .args(["update-index", "--force-remove"])
                .arg(path.as_ref())
                .output()?;

            if !output.status.success() {
                return Err(anyhow!(
                    "Failed to unstage:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }

        Ok(())
    }

    fn remote_url(&self, name: &str) -> Option<String> {
        let repo = self.repository.lock();
        let remote = repo.find_remote(name).ok()?;
        remote.url().map(|url| url.to_string())
    }

    fn head_sha(&self) -> Option<String> {
        Some(self.repository.lock().head().ok()?.target()?.to_string())
    }

    fn merge_head_shas(&self) -> Vec<String> {
        let mut shas = Vec::default();
        self.repository
            .lock()
            .mergehead_foreach(|oid| {
                shas.push(oid.to_string());
                true
            })
            .ok();
        if let Some(oid) = self
            .repository
            .lock()
            .find_reference("CHERRY_PICK_HEAD")
            .ok()
            .and_then(|reference| reference.target())
        {
            shas.push(oid.to_string())
        }
        shas
    }

    fn status(&self, path_prefixes: &[RepoPath]) -> Result<GitStatus> {
        let working_directory = self
            .repository
            .lock()
            .workdir()
            .context("failed to read git work directory")?
            .to_path_buf();
        GitStatus::new(&self.git_binary_path, &working_directory, path_prefixes)
    }

    fn branch_exits(&self, name: &str) -> Result<bool> {
        let repo = self.repository.lock();
        let branch = repo.find_branch(name, BranchType::Local);
        match branch {
            Ok(_) => Ok(true),
            Err(e) => match e.code() {
                git2::ErrorCode::NotFound => Ok(false),
                _ => Err(anyhow!(e)),
            },
        }
    }

    fn branches(&self) -> Result<Vec<Branch>> {
        let working_directory = self
            .repository
            .lock()
            .workdir()
            .context("failed to read git work directory")?
            .to_path_buf();
        let fields = [
            "%(HEAD)",
            "%(objectname)",
            "%(parent)",
            "%(refname)",
            "%(upstream)",
            "%(upstream:track)",
            "%(committerdate:unix)",
            "%(contents:subject)",
        ]
        .join("%00");
        let args = vec!["for-each-ref", "refs/heads/**/*", "--format", &fields];

        let output = new_std_command(&self.git_binary_path)
            .current_dir(&working_directory)
            .args(args)
            .output()?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to git git branches:\n{}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let input = String::from_utf8_lossy(&output.stdout);

        let mut branches = parse_branch_input(&input)?;
        if branches.is_empty() {
            let args = vec!["symbolic-ref", "--quiet", "--short", "HEAD"];

            let output = new_std_command(&self.git_binary_path)
                .current_dir(&working_directory)
                .args(args)
                .output()?;

            // git symbolic-ref returns a non-0 exit code if HEAD points
            // to something other than a branch
            if output.status.success() {
                let name = String::from_utf8_lossy(&output.stdout).trim().to_string();

                branches.push(Branch {
                    name: name.into(),
                    is_head: true,
                    upstream: None,
                    most_recent_commit: None,
                });
            }
        }

        Ok(branches)
    }

    fn change_branch(&self, name: &str) -> Result<()> {
        let repo = self.repository.lock();
        let revision = repo.find_branch(name, BranchType::Local)?;
        let revision = revision.get();
        let as_tree = revision.peel_to_tree()?;
        repo.checkout_tree(as_tree.as_object(), None)?;
        repo.set_head(
            revision
                .name()
                .ok_or_else(|| anyhow!("Branch name could not be retrieved"))?,
        )?;
        Ok(())
    }

    fn create_branch(&self, name: &str) -> Result<()> {
        let repo = self.repository.lock();
        let current_commit = repo.head()?.peel_to_commit()?;
        repo.branch(name, &current_commit, false)?;
        Ok(())
    }

    fn blame(&self, path: &Path, content: Rope) -> Result<crate::blame::Blame> {
        let working_directory = self
            .repository
            .lock()
            .workdir()
            .with_context(|| format!("failed to get git working directory for file {:?}", path))?
            .to_path_buf();

        const REMOTE_NAME: &str = "origin";
        let remote_url = self.remote_url(REMOTE_NAME);

        crate::blame::Blame::for_path(
            &self.git_binary_path,
            &working_directory,
            path,
            &content,
            remote_url,
        )
    }

    fn diff(&self, diff: DiffType) -> Result<String> {
        let working_directory = self.working_directory()?;
        let args = match diff {
            DiffType::HeadToIndex => Some("--staged"),
            DiffType::HeadToWorktree => None,
        };

        let output = new_std_command(&self.git_binary_path)
            .current_dir(&working_directory)
            .args(["diff"])
            .args(args)
            .output()?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to run git diff:\n{}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn stage_paths(&self, paths: &[RepoPath], env: &HashMap<String, String>) -> Result<()> {
        let working_directory = self.working_directory()?;

        if !paths.is_empty() {
            let output = new_std_command(&self.git_binary_path)
                .current_dir(&working_directory)
                .envs(env)
                .args(["update-index", "--add", "--remove", "--"])
                .args(paths.iter().map(|p| p.as_ref()))
                .output()?;

            if !output.status.success() {
                return Err(anyhow!(
                    "Failed to stage paths:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }
        Ok(())
    }

    fn unstage_paths(&self, paths: &[RepoPath], env: &HashMap<String, String>) -> Result<()> {
        let working_directory = self.working_directory()?;

        if !paths.is_empty() {
            let output = new_std_command(&self.git_binary_path)
                .current_dir(&working_directory)
                .envs(env)
                .args(["reset", "--quiet", "--"])
                .args(paths.iter().map(|p| p.as_ref()))
                .output()?;

            if !output.status.success() {
                return Err(anyhow!(
                    "Failed to unstage:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }
        Ok(())
    }

    fn commit(
        &self,
        message: &str,
        name_and_email: Option<(&str, &str)>,
        env: &HashMap<String, String>,
    ) -> Result<()> {
        let working_directory = self.working_directory()?;

        let mut cmd = new_std_command(&self.git_binary_path);
        cmd.current_dir(&working_directory)
            .envs(env)
            .args(["commit", "--quiet", "-m"])
            .arg(message)
            .arg("--cleanup=strip");

        if let Some((name, email)) = name_and_email {
            cmd.arg("--author").arg(&format!("{name} <{email}>"));
        }

        let output = cmd.output()?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to commit:\n{}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(())
    }

    fn push(
        &self,
        branch_name: &str,
        remote_name: &str,
        options: Option<PushOptions>,
        ask_pass: AskPassSession,
        env: &HashMap<String, String>,
    ) -> Result<RemoteCommandOutput> {
        let working_directory = self.working_directory()?;

        let mut command = new_smol_command("git");
        command
            .envs(env)
            .env("GIT_ASKPASS", ask_pass.script_path())
            .env("SSH_ASKPASS", ask_pass.script_path())
            .env("SSH_ASKPASS_REQUIRE", "force")
            .current_dir(&working_directory)
            .args(["push"])
            .args(options.map(|option| match option {
                PushOptions::SetUpstream => "--set-upstream",
                PushOptions::Force => "--force-with-lease",
            }))
            .arg(remote_name)
            .arg(format!("{}:{}", branch_name, branch_name))
            .stdout(smol::process::Stdio::piped())
            .stderr(smol::process::Stdio::piped());
        let git_process = command.spawn()?;

        run_remote_command(ask_pass, git_process)
    }

    fn pull(
        &self,
        branch_name: &str,
        remote_name: &str,
        ask_pass: AskPassSession,
        env: &HashMap<String, String>,
    ) -> Result<RemoteCommandOutput> {
        let working_directory = self.working_directory()?;

        let mut command = new_smol_command("git");
        command
            .envs(env)
            .env("GIT_ASKPASS", ask_pass.script_path())
            .env("SSH_ASKPASS", ask_pass.script_path())
            .env("SSH_ASKPASS_REQUIRE", "force")
            .current_dir(&working_directory)
            .args(["pull"])
            .arg(remote_name)
            .arg(branch_name)
            .stdout(smol::process::Stdio::piped())
            .stderr(smol::process::Stdio::piped());
        let git_process = command.spawn()?;

        run_remote_command(ask_pass, git_process)
    }

    fn fetch(
        &self,
        ask_pass: AskPassSession,
        env: &HashMap<String, String>,
    ) -> Result<RemoteCommandOutput> {
        let working_directory = self.working_directory()?;

        let mut command = new_smol_command("git");
        command
            .envs(env)
            .env("GIT_ASKPASS", ask_pass.script_path())
            .env("SSH_ASKPASS", ask_pass.script_path())
            .env("SSH_ASKPASS_REQUIRE", "force")
            .current_dir(&working_directory)
            .args(["fetch", "--all"])
            .stdout(smol::process::Stdio::piped())
            .stderr(smol::process::Stdio::piped());
        let git_process = command.spawn()?;

        run_remote_command(ask_pass, git_process)
    }

    fn get_remotes(&self, branch_name: Option<&str>) -> Result<Vec<Remote>> {
        let working_directory = self.working_directory()?;

        if let Some(branch_name) = branch_name {
            let output = new_std_command(&self.git_binary_path)
                .current_dir(&working_directory)
                .args(["config", "--get"])
                .arg(format!("branch.{}.remote", branch_name))
                .output()?;

            if output.status.success() {
                let remote_name = String::from_utf8_lossy(&output.stdout);

                return Ok(vec![Remote {
                    name: remote_name.trim().to_string().into(),
                }]);
            }
        }

        let output = new_std_command(&self.git_binary_path)
            .current_dir(&working_directory)
            .args(["remote"])
            .output()?;

        if output.status.success() {
            let remote_names = String::from_utf8_lossy(&output.stdout)
                .split('\n')
                .filter(|name| !name.is_empty())
                .map(|name| Remote {
                    name: name.trim().to_string().into(),
                })
                .collect();

            return Ok(remote_names);
        } else {
            return Err(anyhow!(
                "Failed to get remotes:\n{}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }

    fn check_for_pushed_commit(&self) -> Result<Vec<SharedString>> {
        let working_directory = self.working_directory()?;
        let git_cmd = |args: &[&str]| -> Result<String> {
            let output = new_std_command(&self.git_binary_path)
                .current_dir(&working_directory)
                .args(args)
                .output()?;
            if output.status.success() {
                Ok(String::from_utf8(output.stdout)?)
            } else {
                Err(anyhow!(String::from_utf8_lossy(&output.stderr).to_string()))
            }
        };

        let head = git_cmd(&["rev-parse", "HEAD"])
            .context("Failed to get HEAD")?
            .trim()
            .to_owned();

        let mut remote_branches = vec![];
        let mut add_if_matching = |remote_head: &str| {
            if let Ok(merge_base) = git_cmd(&["merge-base", &head, remote_head]) {
                if merge_base.trim() == head {
                    if let Some(s) = remote_head.strip_prefix("refs/remotes/") {
                        remote_branches.push(s.to_owned().into());
                    }
                }
            }
        };

        // check the main branch of each remote
        let remotes = git_cmd(&["remote"]).context("Failed to get remotes")?;
        for remote in remotes.lines() {
            if let Ok(remote_head) =
                git_cmd(&["symbolic-ref", &format!("refs/remotes/{remote}/HEAD")])
            {
                add_if_matching(remote_head.trim());
            }
        }

        // ... and the remote branch that the checked-out one is tracking
        if let Ok(remote_head) = git_cmd(&["rev-parse", "--symbolic-full-name", "@{u}"]) {
            add_if_matching(remote_head.trim());
        }

        Ok(remote_branches)
    }
}

fn run_remote_command(
    mut ask_pass: AskPassSession,
    git_process: smol::process::Child,
) -> std::result::Result<RemoteCommandOutput, anyhow::Error> {
    smol::block_on(async {
        select_biased! {
            result = ask_pass.run().fuse() => {
                match result {
                    AskPassResult::CancelledByUser => {
                        Err(anyhow!(REMOTE_CANCELLED_BY_USER))?
                    }
                    AskPassResult::Timedout => {
                        Err(anyhow!("Connecting to host timed out"))?
                    }
                }
            }
            output = git_process.output().fuse() => {
                let output = output?;
                if !output.status.success() {
                    Err(anyhow!(
                        "Operation failed:\n{}",
                        String::from_utf8_lossy(&output.stderr)
                    ))
                } else {
                    Ok(RemoteCommandOutput {
                        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                    })
                }
            }
        }
    })
}

#[derive(Debug, Clone)]
pub struct FakeGitRepository {
    state: Arc<Mutex<FakeGitRepositoryState>>,
}

#[derive(Debug, Clone)]
pub struct FakeGitRepositoryState {
    pub path: PathBuf,
    pub event_emitter: smol::channel::Sender<PathBuf>,
    pub head_contents: HashMap<RepoPath, String>,
    pub index_contents: HashMap<RepoPath, String>,
    pub blames: HashMap<RepoPath, Blame>,
    pub statuses: HashMap<RepoPath, FileStatus>,
    pub current_branch_name: Option<String>,
    pub branches: HashSet<String>,
    pub simulated_index_write_error_message: Option<String>,
}

impl FakeGitRepository {
    pub fn open(state: Arc<Mutex<FakeGitRepositoryState>>) -> Arc<dyn GitRepository> {
        Arc::new(FakeGitRepository { state })
    }
}

impl FakeGitRepositoryState {
    pub fn new(path: PathBuf, event_emitter: smol::channel::Sender<PathBuf>) -> Self {
        FakeGitRepositoryState {
            path,
            event_emitter,
            head_contents: Default::default(),
            index_contents: Default::default(),
            blames: Default::default(),
            statuses: Default::default(),
            current_branch_name: Default::default(),
            branches: Default::default(),
            simulated_index_write_error_message: None,
        }
    }
}

impl GitRepository for FakeGitRepository {
    fn reload_index(&self) {}

    fn load_index_text(&self, path: &RepoPath) -> Option<String> {
        let state = self.state.lock();
        state.index_contents.get(path.as_ref()).cloned()
    }

    fn load_committed_text(&self, path: &RepoPath) -> Option<String> {
        let state = self.state.lock();
        state.head_contents.get(path.as_ref()).cloned()
    }

    fn set_index_text(
        &self,
        path: &RepoPath,
        content: Option<String>,
        _env: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        if let Some(message) = state.simulated_index_write_error_message.clone() {
            return Err(anyhow::anyhow!(message));
        }
        if let Some(content) = content {
            state.index_contents.insert(path.clone(), content);
        } else {
            state.index_contents.remove(path);
        }
        state
            .event_emitter
            .try_send(state.path.clone())
            .expect("Dropped repo change event");
        Ok(())
    }

    fn remote_url(&self, _name: &str) -> Option<String> {
        None
    }

    fn head_sha(&self) -> Option<String> {
        None
    }

    fn merge_head_shas(&self) -> Vec<String> {
        vec![]
    }

    fn show(&self, _: &str) -> Result<CommitDetails> {
        unimplemented!()
    }

    fn reset(&self, _: &str, _: ResetMode, _: &HashMap<String, String>) -> Result<()> {
        unimplemented!()
    }

    fn checkout_files(&self, _: &str, _: &[RepoPath], _: &HashMap<String, String>) -> Result<()> {
        unimplemented!()
    }

    fn path(&self) -> PathBuf {
        let state = self.state.lock();
        state.path.clone()
    }

    fn main_repository_path(&self) -> PathBuf {
        self.path()
    }

    fn status(&self, path_prefixes: &[RepoPath]) -> Result<GitStatus> {
        let state = self.state.lock();

        let mut entries = state
            .statuses
            .iter()
            .filter_map(|(repo_path, status)| {
                if path_prefixes
                    .iter()
                    .any(|path_prefix| repo_path.0.starts_with(path_prefix))
                {
                    Some((repo_path.to_owned(), *status))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        entries.sort_unstable_by(|(a, _), (b, _)| a.cmp(&b));

        Ok(GitStatus {
            entries: entries.into(),
        })
    }

    fn branches(&self) -> Result<Vec<Branch>> {
        let state = self.state.lock();
        let current_branch = &state.current_branch_name;
        Ok(state
            .branches
            .iter()
            .map(|branch_name| Branch {
                is_head: Some(branch_name) == current_branch.as_ref(),
                name: branch_name.into(),
                most_recent_commit: None,
                upstream: None,
            })
            .collect())
    }

    fn branch_exits(&self, name: &str) -> Result<bool> {
        let state = self.state.lock();
        Ok(state.branches.contains(name))
    }

    fn change_branch(&self, name: &str) -> Result<()> {
        let mut state = self.state.lock();
        state.current_branch_name = Some(name.to_owned());
        state
            .event_emitter
            .try_send(state.path.clone())
            .expect("Dropped repo change event");
        Ok(())
    }

    fn create_branch(&self, name: &str) -> Result<()> {
        let mut state = self.state.lock();
        state.branches.insert(name.to_owned());
        state
            .event_emitter
            .try_send(state.path.clone())
            .expect("Dropped repo change event");
        Ok(())
    }

    fn blame(&self, path: &Path, _content: Rope) -> Result<crate::blame::Blame> {
        let state = self.state.lock();
        state
            .blames
            .get(path)
            .with_context(|| format!("failed to get blame for {:?}", path))
            .cloned()
    }

    fn stage_paths(&self, _paths: &[RepoPath], _env: &HashMap<String, String>) -> Result<()> {
        unimplemented!()
    }

    fn unstage_paths(&self, _paths: &[RepoPath], _env: &HashMap<String, String>) -> Result<()> {
        unimplemented!()
    }

    fn commit(
        &self,
        _message: &str,
        _name_and_email: Option<(&str, &str)>,
        _env: &HashMap<String, String>,
    ) -> Result<()> {
        unimplemented!()
    }

    fn push(
        &self,
        _branch: &str,
        _remote: &str,
        _options: Option<PushOptions>,
        _ask_pass: AskPassSession,
        _env: &HashMap<String, String>,
    ) -> Result<RemoteCommandOutput> {
        unimplemented!()
    }

    fn pull(
        &self,
        _branch: &str,
        _remote: &str,
        _ask_pass: AskPassSession,
        _env: &HashMap<String, String>,
    ) -> Result<RemoteCommandOutput> {
        unimplemented!()
    }

    fn fetch(
        &self,
        _ask_pass: AskPassSession,
        _env: &HashMap<String, String>,
    ) -> Result<RemoteCommandOutput> {
        unimplemented!()
    }

    fn get_remotes(&self, _branch: Option<&str>) -> Result<Vec<Remote>> {
        unimplemented!()
    }

    fn check_for_pushed_commit(&self) -> Result<Vec<SharedString>> {
        unimplemented!()
    }

    fn diff(&self, _diff: DiffType) -> Result<String> {
        unimplemented!()
    }
}

fn check_path_to_repo_path_errors(relative_file_path: &Path) -> Result<()> {
    match relative_file_path.components().next() {
        None => anyhow::bail!("repo path should not be empty"),
        Some(Component::Prefix(_)) => anyhow::bail!(
            "repo path `{}` should be relative, not a windows prefix",
            relative_file_path.to_string_lossy()
        ),
        Some(Component::RootDir) => {
            anyhow::bail!(
                "repo path `{}` should be relative",
                relative_file_path.to_string_lossy()
            )
        }
        Some(Component::CurDir) => {
            anyhow::bail!(
                "repo path `{}` should not start with `.`",
                relative_file_path.to_string_lossy()
            )
        }
        Some(Component::ParentDir) => {
            anyhow::bail!(
                "repo path `{}` should not start with `..`",
                relative_file_path.to_string_lossy()
            )
        }
        _ => Ok(()),
    }
}

pub static WORK_DIRECTORY_REPO_PATH: LazyLock<RepoPath> =
    LazyLock::new(|| RepoPath(Path::new("").into()));

#[derive(Clone, Debug, Ord, Hash, PartialOrd, Eq, PartialEq)]
pub struct RepoPath(pub Arc<Path>);

impl RepoPath {
    pub fn new(path: PathBuf) -> Self {
        debug_assert!(path.is_relative(), "Repo paths must be relative");

        RepoPath(path.into())
    }

    pub fn from_str(path: &str) -> Self {
        let path = Path::new(path);
        debug_assert!(path.is_relative(), "Repo paths must be relative");

        RepoPath(path.into())
    }
}

impl std::fmt::Display for RepoPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.to_string_lossy().fmt(f)
    }
}

impl From<&Path> for RepoPath {
    fn from(value: &Path) -> Self {
        RepoPath::new(value.into())
    }
}

impl From<Arc<Path>> for RepoPath {
    fn from(value: Arc<Path>) -> Self {
        RepoPath(value)
    }
}

impl From<PathBuf> for RepoPath {
    fn from(value: PathBuf) -> Self {
        RepoPath::new(value)
    }
}

impl From<&str> for RepoPath {
    fn from(value: &str) -> Self {
        Self::from_str(value)
    }
}

impl Default for RepoPath {
    fn default() -> Self {
        RepoPath(Path::new("").into())
    }
}

impl AsRef<Path> for RepoPath {
    fn as_ref(&self) -> &Path {
        self.0.as_ref()
    }
}

impl std::ops::Deref for RepoPath {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Borrow<Path> for RepoPath {
    fn borrow(&self) -> &Path {
        self.0.as_ref()
    }
}

#[derive(Debug)]
pub struct RepoPathDescendants<'a>(pub &'a Path);

impl MapSeekTarget<RepoPath> for RepoPathDescendants<'_> {
    fn cmp_cursor(&self, key: &RepoPath) -> Ordering {
        if key.starts_with(self.0) {
            Ordering::Greater
        } else {
            self.0.cmp(key)
        }
    }
}

fn parse_branch_input(input: &str) -> Result<Vec<Branch>> {
    let mut branches = Vec::new();
    for line in input.split('\n') {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\x00');
        let is_current_branch = fields.next().context("no HEAD")? == "*";
        let head_sha: SharedString = fields.next().context("no objectname")?.to_string().into();
        let parent_sha: SharedString = fields.next().context("no parent")?.to_string().into();
        let ref_name: SharedString = fields
            .next()
            .context("no refname")?
            .strip_prefix("refs/heads/")
            .context("unexpected format for refname")?
            .to_string()
            .into();
        let upstream_name = fields.next().context("no upstream")?.to_string();
        let upstream_tracking = parse_upstream_track(fields.next().context("no upstream:track")?)?;
        let commiterdate = fields.next().context("no committerdate")?.parse::<i64>()?;
        let subject: SharedString = fields
            .next()
            .context("no contents:subject")?
            .to_string()
            .into();

        branches.push(Branch {
            is_head: is_current_branch,
            name: ref_name,
            most_recent_commit: Some(CommitSummary {
                sha: head_sha,
                subject,
                commit_timestamp: commiterdate,
                has_parent: !parent_sha.is_empty(),
            }),
            upstream: if upstream_name.is_empty() {
                None
            } else {
                Some(Upstream {
                    ref_name: upstream_name.into(),
                    tracking: upstream_tracking,
                })
            },
        })
    }

    Ok(branches)
}

fn parse_upstream_track(upstream_track: &str) -> Result<UpstreamTracking> {
    if upstream_track == "" {
        return Ok(UpstreamTracking::Tracked(UpstreamTrackingStatus {
            ahead: 0,
            behind: 0,
        }));
    }

    let upstream_track = upstream_track
        .strip_prefix("[")
        .ok_or_else(|| anyhow!("missing ["))?;
    let upstream_track = upstream_track
        .strip_suffix("]")
        .ok_or_else(|| anyhow!("missing ["))?;
    let mut ahead: u32 = 0;
    let mut behind: u32 = 0;
    for component in upstream_track.split(", ") {
        if component == "gone" {
            return Ok(UpstreamTracking::Gone);
        }
        if let Some(ahead_num) = component.strip_prefix("ahead ") {
            ahead = ahead_num.parse::<u32>()?;
        }
        if let Some(behind_num) = component.strip_prefix("behind ") {
            behind = behind_num.parse::<u32>()?;
        }
    }
    Ok(UpstreamTracking::Tracked(UpstreamTrackingStatus {
        ahead,
        behind,
    }))
}

#[test]
fn test_branches_parsing() {
    // suppress "help: octal escapes are not supported, `\0` is always null"
    #[allow(clippy::octal_escapes)]
    let input = "*\0060964da10574cd9bf06463a53bf6e0769c5c45e\0\0refs/heads/zed-patches\0refs/remotes/origin/zed-patches\0\01733187470\0generated protobuf\n";
    assert_eq!(
        parse_branch_input(&input).unwrap(),
        vec![Branch {
            is_head: true,
            name: "zed-patches".into(),
            upstream: Some(Upstream {
                ref_name: "refs/remotes/origin/zed-patches".into(),
                tracking: UpstreamTracking::Tracked(UpstreamTrackingStatus {
                    ahead: 0,
                    behind: 0
                })
            }),
            most_recent_commit: Some(CommitSummary {
                sha: "060964da10574cd9bf06463a53bf6e0769c5c45e".into(),
                subject: "generated protobuf".into(),
                commit_timestamp: 1733187470,
                has_parent: false,
            })
        }]
    )
}
