//! Retained worker worktrees and explicit, conservative application to the parent.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    path::{Component, Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const LIMIT: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Base {
    root: PathBuf,
    relative: PathBuf,
    commit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum State {
    Preparing,
    Running,
    Ready,
    Applying,
    Applied,
    Discarded,
    Failed,
}

/// A durable record. Worktrees are retained until explicit discard, never on Drop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    version: u32,
    pub id: String,
    parent: String,
    base: Base,
    pub state: State,
    pub tree: Option<String>,
    pub check: Option<String>,
    pub check_passed: Option<bool>,
    #[serde(default)]
    pub validation: Option<crate::validation::CheckReport>,
    pub note: String,
}

fn storage() -> Result<PathBuf> {
    let path = crate::config::global_dir()
        .context("no Worksmith home")?
        .join("worktrees");
    Ok(path.canonicalize().unwrap_or(path))
}

fn uuid(value: &str) -> Result<()> {
    ensure!(
        uuid::Uuid::parse_str(value)?.to_string() == value,
        "invalid workspace identity"
    );
    Ok(())
}
async fn git(
    root: &Path,
    args: &[&str],
    index: Option<&Path>,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .arg("--no-pager")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.filemode=true",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_LITERAL_PATHSPECS", "1");
    // Ambient Git overrides must not redirect operations to another repository/index.
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(name);
    }
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let output = crate::process::run(command, Duration::from_secs(60), cancel, LIMIT).await?;
    ensure!(
        output.status.success(),
        "git {}: {}",
        args.first().unwrap_or(&""),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}
fn string(bytes: Vec<u8>) -> Result<String> {
    Ok(String::from_utf8(bytes)?.trim().to_owned())
}
fn path_text(path: &Path) -> Result<&str> {
    path.to_str().context("workspace paths must be UTF-8")
}

impl Base {
    /// Capture one clean commit for an entire request, including queued workers.
    pub async fn capture(cwd: &Path) -> Result<Self> {
        let cancel = CancellationToken::new();
        let cwd = cwd.canonicalize()?;
        let root = PathBuf::from(string(
            git(&cwd, &["rev-parse", "--show-toplevel"], None, &cancel).await?,
        )?)
        .canonicalize()?;
        let relative = cwd.strip_prefix(&root)?.to_owned();
        let commit = string(
            git(
                &root,
                &["rev-parse", "--verify", "HEAD^{commit}"],
                None,
                &cancel,
            )
            .await?,
        )?;
        ensure!(
            git(
                &root,
                &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
                None,
                &cancel
            )
            .await?
            .is_empty(),
            "isolated workers require a clean Git checkout; commit pending work or explicitly use --shared"
        );
        let flags = git(&root, &["ls-files", "-v", "-z"], None, &cancel).await?;
        ensure!(
            !flags.split(|b| *b == 0).any(|entry| entry
                .first()
                .is_some_and(|flag| flag.is_ascii_lowercase() || *flag == b'S')),
            "isolated workers do not support assume-unchanged or skip-worktree index entries"
        );
        let files = git(&root, &["ls-files", "--stage", "-z"], None, &cancel).await?;
        ensure!(
            !files.split(|b| *b == 0).any(|v| v.starts_with(b"160000 ")),
            "submodules are not supported by isolated workers"
        );
        if let Ok(value) = git(
            &root,
            &["config", "--bool", "core.sparseCheckout"],
            None,
            &cancel,
        )
        .await
        {
            ensure!(
                string(value)? != "true",
                "sparse checkouts are not supported by isolated workers"
            );
        }
        let home = storage()?;
        tokio::fs::create_dir_all(&home).await?;
        ensure!(
            !home.canonicalize()?.starts_with(&root),
            "worktree storage must be outside the source checkout"
        );
        Ok(Self {
            root,
            relative,
            commit,
        })
    }
}

impl Workspace {
    pub fn new(base: Base, parent: &str, id: &str, check: Option<String>) -> Result<Self> {
        uuid(parent)?;
        uuid(id)?;
        Ok(Self {
            version: 1,
            id: id.into(),
            parent: parent.into(),
            base,
            state: State::Preparing,
            tree: None,
            check,
            check_passed: None,
            validation: None,
            note: String::new(),
        })
    }
    fn directory(&self) -> Result<PathBuf> {
        uuid(&self.parent)?;
        uuid(&self.id)?;
        Ok(storage()?.join(&self.parent).join(&self.id))
    }
    pub fn root(&self) -> Result<PathBuf> {
        Ok(self.directory()?.join("tree"))
    }
    pub fn cwd(&self) -> Result<PathBuf> {
        Ok(self.root()?.join(&self.base.relative))
    }
    async fn save(&self) -> Result<()> {
        let dir = self.directory()?;
        tokio::fs::create_dir_all(&dir).await?;
        let temporary = dir.join("record.tmp");
        tokio::fs::write(&temporary, serde_json::to_vec_pretty(self)?).await?;
        tokio::fs::rename(temporary, dir.join("record.json")).await?;
        Ok(())
    }
    /// Held by a running worker or review operation; OS releases it after a crash.
    pub fn lock(&self) -> Result<File> {
        let dir = self.directory()?;
        std::fs::create_dir_all(&dir)?;
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("lock"))?;
        file.try_lock()
            .context("workspace is busy; stop the worker before review/apply/discard")?;
        Ok(file)
    }
    pub async fn create(&mut self, cancel: &CancellationToken) -> Result<()> {
        self.save().await?;
        let root = self.root()?;
        let result = git(
            &self.base.root,
            &[
                "worktree",
                "add",
                "--detach",
                path_text(&root)?,
                &self.base.commit,
            ],
            None,
            cancel,
        )
        .await;
        match result {
            Ok(_) => {
                self.state = State::Running;
                self.save().await
            }
            Err(error) => {
                self.state = State::Failed;
                self.note = error.to_string();
                self.save().await?;
                Err(error)
            }
        }
    }
    async fn registered(&self) -> Result<()> {
        let root = self.root()?;
        ensure!(
            root.canonicalize()? == root,
            "workspace path contains a symlink or changed storage location"
        );
        ensure!(
            root.symlink_metadata()?.file_type().is_dir(),
            "workspace root is not a directory"
        );
        let listing = git(
            &self.base.root,
            &["worktree", "list", "--porcelain", "-z"],
            None,
            &CancellationToken::new(),
        )
        .await?;
        let expected = format!("worktree {}", root.display());
        ensure!(
            listing.split(|b| *b == 0).any(|p| p == expected.as_bytes()),
            "workspace is not registered in its owning repository"
        );
        Ok(())
    }
    async fn snapshot(&self) -> Result<String> {
        self.registered().await?;
        let root = self.root()?;
        let index = self
            .directory()?
            .join(format!("index-{}", uuid::Uuid::new_v4()));
        let cancel = CancellationToken::new();
        let result = async {
            git(
                &root,
                &["read-tree", &self.base.commit],
                Some(&index),
                &cancel,
            )
            .await?;
            git(&root, &["add", "-A", "--", "."], Some(&index), &cancel).await?;
            string(git(&root, &["write-tree"], Some(&index), &cancel).await?)
        }
        .await;
        let _ = tokio::fs::remove_file(&index).await;
        result
    }
    pub async fn finish(&mut self, passed: Option<bool>, note: String) -> Result<Vec<String>> {
        self.check_passed = passed;
        if passed.is_none() {
            self.validation = None;
        }
        self.note = note;
        match self.snapshot().await {
            Ok(tree) => {
                self.tree = Some(tree);
                self.state = State::Ready;
            }
            Err(e) => {
                self.state = State::Failed;
                self.note.push_str(&format!("; result capture failed: {e}"));
            }
        }
        self.save().await?;
        ensure!(
            self.state == State::Ready,
            "{}; retained {}",
            self.note,
            self.root()?.display()
        );
        self.paths().await
    }
    async fn patch(&self) -> Result<Vec<u8>> {
        let tree = self
            .tree
            .as_deref()
            .context("no captured result; inspect the retained workspace")?;
        git(
            &self.base.root,
            &[
                "diff",
                "--binary",
                "--full-index",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                &self.base.commit,
                tree,
                "--",
            ],
            None,
            &CancellationToken::new(),
        )
        .await
    }
    async fn paths(&self) -> Result<Vec<String>> {
        let Some(tree) = &self.tree else {
            return Ok(Vec::new());
        };
        let bytes = git(
            &self.base.root,
            &[
                "diff",
                "--name-only",
                "-z",
                "--no-renames",
                &self.base.commit,
                tree,
                "--",
            ],
            None,
            &CancellationToken::new(),
        )
        .await?;
        bytes
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| Ok(String::from_utf8(p.to_vec())?))
            .collect()
    }
    async fn unchanged(&self) -> Result<()> {
        ensure!(
            self.tree.is_some(),
            "no captured result: {}; inspect {}",
            self.note,
            self.root()?.display()
        );
        ensure!(
            self.tree.as_deref() == Some(&self.snapshot().await?),
            "workspace changed since result capture; prior validation no longer applies; inspect manually"
        );
        Ok(())
    }
    async fn files_match(&self, root: &Path, tree: &str, paths: &[String]) -> Result<bool> {
        let index = self
            .directory()?
            .join(format!("verify-{}", uuid::Uuid::new_v4()));
        let cancel = CancellationToken::new();
        let result = async {
            git(root, &["read-tree", tree], Some(&index), &cancel).await?;
            let mut args = vec!["add", "-A", "--"];
            for path in paths {
                if root.join(path).try_exists()?
                    || !git(root, &["ls-files", "--", path], Some(&index), &cancel)
                        .await?
                        .is_empty()
                {
                    args.push(path);
                }
            }
            if args.len() > 3 {
                git(root, &args, Some(&index), &cancel).await?;
            }
            let expected = string(
                git(
                    root,
                    &["rev-parse", &format!("{tree}^{{tree}}")],
                    None,
                    &cancel,
                )
                .await?,
            )?;
            Ok(string(git(root, &["write-tree"], Some(&index), &cancel).await?)? == expected)
        }
        .await;
        let _ = tokio::fs::remove_file(index).await;
        result
    }

    async fn preconditions(&self, paths: &[String]) -> Result<()> {
        let cancel = CancellationToken::new();
        ensure!(
            string(git(&self.base.root, &["rev-parse", "HEAD"], None, &cancel).await?)?
                == self.base.commit,
            "parent HEAD changed; result not applied"
        );
        for path in paths {
            safe_path(&self.base.root, path)?;
            safe_path(&self.root()?, path)?;
            ensure!(
                git(
                    &self.base.root,
                    &[
                        "diff",
                        "--cached",
                        "--no-ext-diff",
                        "--no-textconv",
                        &self.base.commit,
                        "--",
                        path
                    ],
                    None,
                    &cancel
                )
                .await?
                .is_empty(),
                "parent changed {path}; result not applied"
            );
            let entry = git(
                &self.base.root,
                &["ls-tree", &self.base.commit, "--", path],
                None,
                &cancel,
            )
            .await?;
            if entry.is_empty() {
                ensure!(
                    !self.base.root.join(path).try_exists()?,
                    "new-file collision at {path}"
                );
            }
            for tree in [&self.base.commit, self.tree.as_ref().context("no result")?] {
                let entry = git(
                    &self.base.root,
                    &["ls-tree", tree, "--", path],
                    None,
                    &cancel,
                )
                .await?;
                ensure!(
                    entry.is_empty()
                        || entry.starts_with(b"100644 ")
                        || entry.starts_with(b"100755 "),
                    "unsupported file type at {path}; inspect manually"
                );
            }
        }
        // A fresh index sees actual files even if the parent uses assume-unchanged.
        ensure!(
            self.files_match(&self.base.root, &self.base.commit, paths)
                .await?,
            "parent changed affected files; result not applied"
        );
        Ok(())
    }
    async fn apply(&mut self) -> Result<String> {
        ensure!(
            self.state == State::Ready,
            "result is {:?}; cannot apply (interrupted applies require manual inspection)",
            self.state
        );
        self.unchanged().await?;
        let paths = self.paths().await?;
        ensure!(!paths.is_empty(), "no changes to apply");
        // Serialize parent application across sessions/processes, not just this manager.
        use sha2::{Digest, Sha256};
        let key = Sha256::digest(self.base.root.as_os_str().as_encoded_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(storage()?.join(format!("apply-{key}.lock")))?;
        lock.try_lock()
            .context("another result is being applied to this repository")?;
        self.preconditions(&paths).await?;
        let patch = self.directory()?.join("result.patch");
        tokio::fs::write(&patch, self.patch().await?).await?;
        let cancel = CancellationToken::new();
        git(
            &self.base.root,
            &["apply", "--check", path_text(&patch)?],
            None,
            &cancel,
        )
        .await?;
        self.preconditions(&paths).await?;
        self.state = State::Applying;
        self.save().await?;
        git(
            &self.base.root,
            &["apply", path_text(&patch)?],
            None,
            &cancel,
        )
        .await?;
        let tree = self.tree.as_ref().context("no result")?;
        ensure!(
            self.files_match(&self.base.root, tree, &paths).await?,
            "apply verification failed; inspect retained patch"
        );
        self.state = State::Applied;
        self.save().await?;
        Ok(format!(
            "applied {} file(s), unstaged; run parent validation separately. Retained {}",
            paths.len(),
            self.id
        ))
    }
}

fn safe_path(root: &Path, relative: &str) -> Result<()> {
    let path = Path::new(relative);
    ensure!(
        !path.as_os_str().is_empty()
            && path.components().all(|c| matches!(c, Component::Normal(_))),
        "unsafe result path"
    );
    let mut full = root.to_owned();
    for component in path.components() {
        full.push(component);
        if let Ok(metadata) = full.symlink_metadata() {
            ensure!(
                !metadata.file_type().is_symlink(),
                "symlink at {}; inspect manually",
                full.display()
            );
            ensure!(
                metadata.is_dir() || metadata.is_file(),
                "unsupported file at {}",
                full.display()
            );
        }
    }
    Ok(())
}

/// Load retained records for the current project without starting workers.
pub async fn list(cwd: &Path) -> Result<Vec<Workspace>> {
    let root = PathBuf::from(string(
        git(
            cwd,
            &["rev-parse", "--show-toplevel"],
            None,
            &CancellationToken::new(),
        )
        .await?,
    )?)
    .canonicalize()?;
    let storage = storage()?;
    if !storage.exists() {
        return Ok(Vec::new());
    }
    let mut parents = tokio::fs::read_dir(storage).await?;
    let mut records = Vec::new();
    while let Some(parent) = parents.next_entry().await? {
        if !parent.file_type().await?.is_dir()
            || uuid(&parent.file_name().to_string_lossy()).is_err()
        {
            continue;
        }
        let mut children = tokio::fs::read_dir(parent.path()).await?;
        while let Some(child) = children.next_entry().await? {
            if !child.file_type().await?.is_dir()
                || uuid(&child.file_name().to_string_lossy()).is_err()
            {
                continue;
            }
            let path = child.path().join("record.json");
            if !path.exists() {
                continue;
            }
            let record: Workspace = serde_json::from_slice(&tokio::fs::read(&path).await?)?;
            ensure!(
                record.version == 1 && record.directory()? == child.path(),
                "invalid workspace record at {}",
                path.display()
            );
            if record.base.root == root && record.state != State::Discarded {
                records.push(record);
            }
        }
    }
    records.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(records)
}

/// Shared by the CLI and both interactive frontends. Only explicit user commands call this.
pub async fn review(cwd: &Path, action: &str, id: Option<&str>, confirmed: bool) -> Result<String> {
    let mut records = list(cwd).await?;
    if action == "retained" || action == "list" {
        return Ok(records
            .iter()
            .map(|r| {
                format!(
                    "{} {} {} — {}",
                    r.id,
                    if matches!(r.state, State::Running | State::Preparing) && r.lock().is_ok() {
                        "Interrupted".into()
                    } else {
                        format!("{:?}", r.state)
                    },
                    r.root()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    r.note
                )
            })
            .collect::<Vec<_>>()
            .join("\n"));
    }
    let id = id.context("a worker session UUID is required")?;
    let record = records
        .iter_mut()
        .find(|r| r.id == id)
        .context("no retained workspace for this project and worker")?;
    let _lock = record.lock()?;
    if action == "diff" {
        if record.state == State::Preparing || record.state == State::Running {
            record
                .finish(None, "interrupted worker; validation unknown".into())
                .await?;
        }
        record.unchanged().await?;
        return Ok(format!(
            "{} {:?}\nworkspace: {}\n{}\n{}\n{}",
            record.id,
            record.state,
            record.root()?.display(),
            crate::report::validation_detail(
                record.validation.as_ref(),
                record.check.as_deref(),
                record.check_passed
            ),
            record.note,
            String::from_utf8_lossy(&record.patch().await?)
        ));
    }
    if action == "apply" {
        return record.apply().await;
    }
    if action == "discard" {
        ensure!(
            confirmed
                || record.tree.is_some()
                    && record.unchanged().await.is_ok()
                    && (record.state == State::Applied || record.paths().await?.is_empty()),
            "unapplied/interrupted work may be lost; inspect it, then repeat discard with --yes to confirm"
        );
        let root = record.root()?;
        if !root.exists() && matches!(record.state, State::Preparing | State::Failed) {
            let listing = git(
                &record.base.root,
                &["worktree", "list", "--porcelain", "-z"],
                None,
                &CancellationToken::new(),
            )
            .await?;
            let expected = format!("worktree {}", root.display());
            if !listing.split(|b| *b == 0).any(|p| p == expected.as_bytes()) {
                record.state = State::Discarded;
                record.save().await?;
                return Ok(format!("discarded failed setup {}", record.id));
            }
        } else {
            record.registered().await?;
        }
        git(
            &record.base.root,
            &["worktree", "remove", "--force", path_text(&root)?],
            None,
            &CancellationToken::new(),
        )
        .await?;
        record.state = State::Discarded;
        record.save().await?;
        return Ok(format!("discarded {}", record.id));
    }
    bail!("usage: agents retained | diff/apply/discard <worker-session> [--yes]")
}
