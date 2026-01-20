//! Git integration for filtering files based on changes since a reference.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, RwLock},
};

use anyhow::{bail, Context};

/// A filter that tracks which files have been changed since a Git reference.
///
/// When active, only files that have been modified, added, or deleted according
/// to Git will be "acknowledged" and synced to Studio. This allows users to
/// work with large projects where they only want to sync their local changes.
///
/// Once a file is acknowledged (either initially or during the session), it
/// stays acknowledged for the entire session. This prevents files from being
/// deleted in Studio if their content is reverted to match the git reference.
#[derive(Debug)]
pub struct GitFilter {
    /// The Git repository root directory.
    repo_root: PathBuf,

    /// The Git reference to compare against (e.g., "HEAD", "main", a commit hash).
    base_ref: String,

    /// Cache of paths that are currently different from the base ref according to git.
    /// This is refreshed on every VFS event.
    git_changed_paths: RwLock<HashSet<PathBuf>>,

    /// Paths that have been acknowledged at any point during this session.
    /// Once a path is added here, it stays acknowledged forever (for this session).
    /// This prevents files from being deleted if their content is reverted.
    session_acknowledged_paths: RwLock<HashSet<PathBuf>>,
}

impl GitFilter {
    /// Creates a new GitFilter for the given repository root and base reference.
    ///
    /// The `repo_root` should be the root of the Git repository (where .git is located).
    /// The `base_ref` is the Git reference to compare against (e.g., "HEAD", "main").
    /// The `project_path` is the path to the project being served - it will always be
    /// acknowledged regardless of git status to ensure the project structure exists.
    pub fn new(repo_root: PathBuf, base_ref: String, project_path: &Path) -> anyhow::Result<Self> {
        let filter = Self {
            repo_root,
            base_ref,
            git_changed_paths: RwLock::new(HashSet::new()),
            session_acknowledged_paths: RwLock::new(HashSet::new()),
        };

        // Always acknowledge the project path and its directory so the project
        // structure exists even when there are no git changes
        filter.acknowledge_project_path(project_path);

        // Initial refresh to populate the cache with git changes
        filter.refresh()?;

        Ok(filter)
    }

    /// Acknowledges the project path and its containing directory.
    /// This ensures the project structure always exists regardless of git status.
    fn acknowledge_project_path(&self, project_path: &Path) {
        let mut session = self.session_acknowledged_paths.write().unwrap();

        // Acknowledge the project path itself (might be a directory or .project.json file)
        let canonical = project_path.canonicalize().unwrap_or_else(|_| project_path.to_path_buf());
        session.insert(canonical.clone());

        // Acknowledge all ancestor directories
        let mut current = canonical.parent();
        while let Some(parent) = current {
            session.insert(parent.to_path_buf());
            current = parent.parent();
        }

        // If it's a directory, also acknowledge default.project.json inside it
        if project_path.is_dir() {
            for name in &["default.project.json", "default.project.jsonc"] {
                let project_file = project_path.join(name);
                if let Ok(canonical_file) = project_file.canonicalize() {
                    session.insert(canonical_file);
                } else {
                    session.insert(project_file);
                }
            }
        }

        // If it's a .project.json file, also acknowledge its parent directory
        if let Some(parent) = project_path.parent() {
            let parent_canonical = parent.canonicalize().unwrap_or_else(|_| parent.to_path_buf());
            session.insert(parent_canonical);
        }

        log::debug!(
            "GitFilter: acknowledged project path {} ({} paths total)",
            project_path.display(),
            session.len()
        );
    }

    /// Finds the Git repository root for the given path.
    pub fn find_repo_root(path: &Path) -> anyhow::Result<PathBuf> {
        let output = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(path)
            .output()
            .context("Failed to execute git rev-parse")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to find Git repository root: {}", stderr.trim());
        }

        let root = String::from_utf8_lossy(&output.stdout)
            .trim()
            .to_string();

        Ok(PathBuf::from(root))
    }

    /// Refreshes the cache of acknowledged paths by querying Git.
    ///
    /// This should be called when files change to ensure newly modified files
    /// are properly acknowledged. Once a path is acknowledged, it stays
    /// acknowledged for the entire session (even if the file is reverted).
    pub fn refresh(&self) -> anyhow::Result<()> {
        let mut git_changed = HashSet::new();

        // Get files changed since the base ref (modified, added, deleted)
        let diff_output = Command::new("git")
            .args(["diff", "--name-only", &self.base_ref])
            .current_dir(&self.repo_root)
            .output()
            .context("Failed to execute git diff")?;

        if !diff_output.status.success() {
            let stderr = String::from_utf8_lossy(&diff_output.stderr);
            bail!("git diff failed: {}", stderr.trim());
        }

        let diff_files = String::from_utf8_lossy(&diff_output.stdout);
        let diff_count = diff_files.lines().filter(|l| !l.is_empty()).count();
        if diff_count > 0 {
            log::debug!("git diff found {} changed files", diff_count);
        }
        for line in diff_files.lines() {
            if !line.is_empty() {
                let path = self.repo_root.join(line);
                log::trace!("git diff: acknowledging {}", path.display());
                self.acknowledge_path(&path, &mut git_changed);
            }
        }

        // Get untracked files (new files not yet committed)
        let untracked_output = Command::new("git")
            .args(["ls-files", "--others", "--exclude-standard"])
            .current_dir(&self.repo_root)
            .output()
            .context("Failed to execute git ls-files")?;

        if !untracked_output.status.success() {
            let stderr = String::from_utf8_lossy(&untracked_output.stderr);
            bail!("git ls-files failed: {}", stderr.trim());
        }

        let untracked_files = String::from_utf8_lossy(&untracked_output.stdout);
        for line in untracked_files.lines() {
            if !line.is_empty() {
                let path = self.repo_root.join(line);
                self.acknowledge_path(&path, &mut git_changed);
            }
        }

        // Get staged files (files added to index but not yet committed)
        let staged_output = Command::new("git")
            .args(["diff", "--name-only", "--cached", &self.base_ref])
            .current_dir(&self.repo_root)
            .output()
            .context("Failed to execute git diff --cached")?;

        if staged_output.status.success() {
            let staged_files = String::from_utf8_lossy(&staged_output.stdout);
            for line in staged_files.lines() {
                if !line.is_empty() {
                    let path = self.repo_root.join(line);
                    self.acknowledge_path(&path, &mut git_changed);
                }
            }
        }

        // Update the git changed paths cache
        {
            let mut cache = self.git_changed_paths.write().unwrap();
            *cache = git_changed.clone();
        }

        // Merge newly changed paths into session acknowledged paths
        // Once acknowledged, a path stays acknowledged for the entire session
        {
            let mut session = self.session_acknowledged_paths.write().unwrap();
            for path in git_changed {
                session.insert(path);
            }
            log::debug!(
                "GitFilter refreshed: {} paths acknowledged in session",
                session.len()
            );
        }

        Ok(())
    }

    /// Acknowledges a path and all its ancestors, plus associated meta files.
    fn acknowledge_path(&self, path: &Path, acknowledged: &mut HashSet<PathBuf>) {
        // Canonicalize the path if possible, otherwise use as-is
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        // Add the path itself
        acknowledged.insert(path.clone());

        // Add all ancestor directories
        let mut current = path.parent();
        while let Some(parent) = current {
            acknowledged.insert(parent.to_path_buf());
            current = parent.parent();
        }

        // Add associated meta files
        self.acknowledge_meta_files(&path, acknowledged);
    }

    /// Acknowledges associated meta files for a given path.
    fn acknowledge_meta_files(&self, path: &Path, acknowledged: &mut HashSet<PathBuf>) {
        if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
            if let Some(parent) = path.parent() {
                // For a file like "foo.lua", also acknowledge "foo.meta.json"
                // Strip known extensions to get the base name
                let base_name = strip_lua_extension(file_name);

                let meta_path = parent.join(format!("{}.meta.json", base_name));
                if let Ok(canonical) = meta_path.canonicalize() {
                    acknowledged.insert(canonical);
                } else {
                    acknowledged.insert(meta_path);
                }

                // For init files, also acknowledge "init.meta.json" in the same directory
                if file_name.starts_with("init.") {
                    let init_meta = parent.join("init.meta.json");
                    if let Ok(canonical) = init_meta.canonicalize() {
                        acknowledged.insert(canonical);
                    } else {
                        acknowledged.insert(init_meta);
                    }
                }
            }
        }
    }

    /// Checks if a path is acknowledged (should be synced).
    ///
    /// Returns `true` if the path or any of its descendants have been changed
    /// at any point during this session. Once a file is acknowledged, it stays
    /// acknowledged even if its content is reverted to match the git reference.
    pub fn is_acknowledged(&self, path: &Path) -> bool {
        let session = self.session_acknowledged_paths.read().unwrap();

        // Try to canonicalize the path
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        // Check if this exact path is acknowledged
        if session.contains(&canonical) {
            log::trace!("Path {} is directly acknowledged", path.display());
            return true;
        }

        // Also check without canonicalization in case of path differences
        if session.contains(path) {
            log::trace!("Path {} is acknowledged (non-canonical)", path.display());
            return true;
        }

        // For directories, check if any descendant is acknowledged
        // This is done by checking if any acknowledged path starts with this path
        for acknowledged in session.iter() {
            if acknowledged.starts_with(&canonical) {
                log::trace!(
                    "Path {} has acknowledged descendant {}",
                    path.display(),
                    acknowledged.display()
                );
                return true;
            }
            // Also check non-canonical
            if acknowledged.starts_with(path) {
                log::trace!(
                    "Path {} has acknowledged descendant {} (non-canonical)",
                    path.display(),
                    acknowledged.display()
                );
                return true;
            }
        }

        log::trace!(
            "Path {} is NOT acknowledged (canonical: {})",
            path.display(),
            canonical.display()
        );
        false
    }

    /// Returns the base reference being compared against.
    pub fn base_ref(&self) -> &str {
        &self.base_ref
    }

    /// Returns the repository root path.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Explicitly acknowledges a path and all its ancestors.
    /// This is useful for ensuring certain paths are always synced regardless of git status.
    pub fn force_acknowledge(&self, path: &Path) {
        let mut acknowledged = HashSet::new();
        self.acknowledge_path(path, &mut acknowledged);

        let mut session = self.session_acknowledged_paths.write().unwrap();
        for p in acknowledged {
            session.insert(p);
        }
    }
}

/// Strips Lua-related extensions from a file name to get the base name.
fn strip_lua_extension(file_name: &str) -> &str {
    const EXTENSIONS: &[&str] = &[
        ".server.luau",
        ".server.lua",
        ".client.luau",
        ".client.lua",
        ".luau",
        ".lua",
    ];

    for ext in EXTENSIONS {
        if let Some(base) = file_name.strip_suffix(ext) {
            return base;
        }
    }

    // If no Lua extension, try to strip the regular extension
    file_name
        .rsplit_once('.')
        .map(|(base, _)| base)
        .unwrap_or(file_name)
}

/// A wrapper around GitFilter that can be shared across threads.
pub type SharedGitFilter = Arc<GitFilter>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_lua_extension() {
        assert_eq!(strip_lua_extension("foo.server.lua"), "foo");
        assert_eq!(strip_lua_extension("foo.client.luau"), "foo");
        assert_eq!(strip_lua_extension("foo.lua"), "foo");
        assert_eq!(strip_lua_extension("init.server.lua"), "init");
        assert_eq!(strip_lua_extension("bar.txt"), "bar");
        assert_eq!(strip_lua_extension("noextension"), "noextension");
    }
}
