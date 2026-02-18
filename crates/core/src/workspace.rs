use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing;

/// An entry in a workspace directory listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// File or directory name.
    pub name: String,
    /// Whether this entry is a directory.
    pub is_dir: bool,
    /// Size in bytes (0 for directories).
    pub size: u64,
}

/// Manages file operations within a sandboxed workspace directory.
///
/// All paths are resolved relative to the workspace root, and path
/// traversal attacks (e.g., `../`) are blocked.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// The root directory of this workspace.
    root: PathBuf,
}

impl Workspace {
    /// Create a new Workspace rooted at the given directory.
    ///
    /// The directory will be created if it does not exist.
    pub fn new(agent_id: &str, base_path: &str) -> Result<Self> {
        let root: PathBuf = Path::new(base_path).join("workspaces").join(agent_id);
        std::fs::create_dir_all(&root)
            .with_context(|| format!("Failed to create workspace directory: {}", root.display()))?;

        // Canonicalize to resolve any symlinks in the root itself.
        let root = root
            .canonicalize()
            .with_context(|| format!("Failed to canonicalize workspace root: {}", root.display()))?;

        Ok(Self { root })
    }

    /// Create a new Workspace from an explicit root path.
    ///
    /// The directory will be created if it does not exist.
    pub fn from_root(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)
            .with_context(|| format!("Failed to create workspace directory: {}", root.display()))?;

        // Canonicalize to resolve any symlinks in the root itself.
        let root = root
            .canonicalize()
            .with_context(|| format!("Failed to canonicalize workspace root: {}", root.display()))?;

        Ok(Self { root })
    }

    /// Get the workspace root path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a relative path within the workspace, ensuring it does not escape the root.
    ///
    /// Returns the absolute, canonicalized path if valid, or an error if
    /// the path would escape the workspace.
    pub fn resolve_path(&self, relative: &str) -> Result<PathBuf> {
        let relative = relative.trim();
        if relative.is_empty() {
            return Ok(self.root.clone());
        }

        // Join with the root.
        let joined = self.root.join(relative);

        // For paths that don't exist yet, we check the parent.
        if joined.exists() {
            let canonical = joined.canonicalize()?;
            if !canonical.starts_with(&self.root) {
                bail!(
                    "Path traversal detected: '{}' resolves outside workspace",
                    relative
                );
            }
            Ok(canonical)
        } else {
            // For non-existent paths, normalize manually and check components.
            let mut normalized = self.root.clone();
            for component in Path::new(relative).components() {
                match component {
                    std::path::Component::Normal(c) => normalized.push(c),
                    std::path::Component::ParentDir => {
                        if !normalized.pop() || !normalized.starts_with(&self.root) {
                            bail!(
                                "Path traversal detected: '{}' escapes workspace",
                                relative
                            );
                        }
                        // Re-check that we haven't escaped.
                        if !normalized.starts_with(&self.root) {
                            bail!(
                                "Path traversal detected: '{}' escapes workspace",
                                relative
                            );
                        }
                    }
                    std::path::Component::CurDir => {} // ignore "."
                    std::path::Component::RootDir => {
                        bail!("Absolute paths are not allowed in workspace: '{}'", relative);
                    }
                    std::path::Component::Prefix(_) => {
                        bail!("Path prefixes are not allowed in workspace: '{}'", relative);
                    }
                }
            }
            Ok(normalized)
        }
    }

    /// Read a file from the workspace.
    pub fn read_file(&self, relative_path: &str) -> Result<String> {
        let path = self.resolve_path(relative_path)?;
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read file: {}", path.display()))?;
        tracing::debug!(path = %path.display(), "Read workspace file");
        Ok(content)
    }

    /// Write content to a file in the workspace.
    ///
    /// Parent directories will be created automatically.
    pub fn write_file(&self, relative_path: &str, content: &str) -> Result<()> {
        let path = self.resolve_path(relative_path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)
            .with_context(|| format!("Failed to write file: {}", path.display()))?;
        tracing::debug!(path = %path.display(), "Wrote workspace file");
        Ok(())
    }

    /// Edit a file by replacing all occurrences of `old` with `new`.
    pub fn edit_file(&self, relative_path: &str, old: &str, new: &str) -> Result<usize> {
        let path = self.resolve_path(relative_path)?;
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read file for editing: {}", path.display()))?;

        let count = content.matches(old).count();
        if count == 0 {
            bail!("String to replace not found in file: {}", path.display());
        }

        let updated = content.replace(old, new);
        std::fs::write(&path, updated)
            .with_context(|| format!("Failed to write edited file: {}", path.display()))?;

        tracing::debug!(
            path = %path.display(),
            replacements = count,
            "Edited workspace file"
        );
        Ok(count)
    }

    /// List the contents of a directory in the workspace.
    pub fn list_dir(&self, relative_path: &str) -> Result<Vec<FileEntry>> {
        let path = self.resolve_path(relative_path)?;
        if !path.is_dir() {
            bail!("Not a directory: {}", path.display());
        }

        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            entries.push(FileEntry {
                name: entry.file_name().to_string_lossy().to_string(),
                is_dir: metadata.is_dir(),
                size: if metadata.is_file() {
                    metadata.len()
                } else {
                    0
                },
            });
        }

        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// Delete a file from the workspace.
    pub fn delete_file(&self, relative_path: &str) -> Result<()> {
        let path = self.resolve_path(relative_path)?;
        if path.is_dir() {
            bail!(
                "Cannot delete directory with delete_file, use a different method: {}",
                path.display()
            );
        }
        std::fs::remove_file(&path)
            .with_context(|| format!("Failed to delete file: {}", path.display()))?;
        tracing::debug!(path = %path.display(), "Deleted workspace file");
        Ok(())
    }

    /// Create a directory (and all parents) in the workspace.
    pub fn mkdir_sync(&self, relative_path: &str) -> Result<()> {
        let path = self.resolve_path(relative_path)?;
        std::fs::create_dir_all(&path)
            .with_context(|| format!("Failed to create directory: {}", path.display()))?;
        tracing::debug!(path = %path.display(), "Created workspace directory");
        Ok(())
    }

    // -----------------------------------------------------------------
    // Async wrapper methods
    //
    // These delegate to the synchronous implementations via
    // `tokio::task::spawn_blocking`, providing an async API that the
    // actions and server crates expect.
    // -----------------------------------------------------------------

    /// Async: read a file from the workspace.
    pub async fn read(&self, relative_path: &str) -> Result<String> {
        let ws = self.clone();
        let path = relative_path.to_string();
        tokio::task::spawn_blocking(move || ws.read_file(&path))
            .await
            .map_err(|e| anyhow::anyhow!("Blocking task failed: {e}"))?
    }

    /// Async: write content to a file in the workspace.
    pub async fn write(&self, relative_path: &str, content: &str) -> Result<()> {
        let ws = self.clone();
        let path = relative_path.to_string();
        let content = content.to_string();
        tokio::task::spawn_blocking(move || ws.write_file(&path, &content))
            .await
            .map_err(|e| anyhow::anyhow!("Blocking task failed: {e}"))?
    }

    /// Async: edit a file by replacing occurrences of `old` with `new`.
    pub async fn edit(&self, relative_path: &str, old: &str, new: &str) -> Result<usize> {
        let ws = self.clone();
        let path = relative_path.to_string();
        let old = old.to_string();
        let new = new.to_string();
        tokio::task::spawn_blocking(move || ws.edit_file(&path, &old, &new))
            .await
            .map_err(|e| anyhow::anyhow!("Blocking task failed: {e}"))?
    }

    /// Async: list the contents of a directory in the workspace.
    pub async fn list(&self, relative_path: &str) -> Result<Vec<FileEntry>> {
        let ws = self.clone();
        let path = relative_path.to_string();
        tokio::task::spawn_blocking(move || ws.list_dir(&path))
            .await
            .map_err(|e| anyhow::anyhow!("Blocking task failed: {e}"))?
    }

    /// Async: delete a file from the workspace.
    pub async fn delete(&self, relative_path: &str) -> Result<()> {
        let ws = self.clone();
        let path = relative_path.to_string();
        tokio::task::spawn_blocking(move || ws.delete_file(&path))
            .await
            .map_err(|e| anyhow::anyhow!("Blocking task failed: {e}"))?
    }

    /// Async: create a directory in the workspace.
    pub async fn mkdir(&self, relative_path: &str) -> Result<()> {
        let ws = self.clone();
        let path = relative_path.to_string();
        tokio::task::spawn_blocking(move || ws.mkdir_sync(&path))
            .await
            .map_err(|e| anyhow::anyhow!("Blocking task failed: {e}"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = Workspace::from_root(dir.path()).unwrap();
        (dir, ws)
    }

    #[test]
    fn test_new() {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = Workspace::new("agent-1", dir.path().to_str().unwrap()).unwrap();
        let expected = dir.path().join("workspaces").join("agent-1");
        assert!(expected.exists());
        assert!(ws.root().ends_with("workspaces/agent-1"));
    }

    #[test]
    fn test_from_root() {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = Workspace::from_root(dir.path()).unwrap();
        assert!(ws.root().exists());
        assert_eq!(ws.root(), dir.path().canonicalize().unwrap());
    }

    #[test]
    fn test_write_and_read() {
        let (_dir, ws) = temp_workspace();
        ws.write_file("test.txt", "hello").unwrap();
        let content = ws.read_file("test.txt").unwrap();
        assert_eq!(content, "hello");
    }

    #[test]
    fn test_parent_auto_create() {
        let (_dir, ws) = temp_workspace();
        ws.write_file("a/b/c.txt", "x").unwrap();
        let content = ws.read_file("a/b/c.txt").unwrap();
        assert_eq!(content, "x");
    }

    #[test]
    fn test_edit() {
        let (_dir, ws) = temp_workspace();
        ws.write_file("f.txt", "hello old world").unwrap();
        let count = ws.edit_file("f.txt", "old", "new").unwrap();
        assert_eq!(count, 1);
        let content = ws.read_file("f.txt").unwrap();
        assert_eq!(content, "hello new world");
    }

    #[test]
    fn test_list() {
        let (_dir, ws) = temp_workspace();
        ws.write_file("aaa.txt", "a").unwrap();
        ws.write_file("bbb.txt", "b").unwrap();
        let entries = ws.list_dir("").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "aaa.txt");
        assert_eq!(entries[1].name, "bbb.txt");
    }

    #[test]
    fn test_delete() {
        let (_dir, ws) = temp_workspace();
        ws.write_file("del.txt", "bye").unwrap();
        ws.delete_file("del.txt").unwrap();
        assert!(ws.read_file("del.txt").is_err());
    }

    #[test]
    fn test_mkdir() {
        let (_dir, ws) = temp_workspace();
        ws.mkdir_sync("newdir").unwrap();
        let entries = ws.list_dir("").unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_dir);
        assert_eq!(entries[0].name, "newdir");
    }

    #[test]
    fn test_traversal_dotdot() {
        let (_dir, ws) = temp_workspace();
        assert!(ws.resolve_path("../escape").is_err());
    }

    #[test]
    fn test_absolute_path() {
        let (_dir, ws) = temp_workspace();
        assert!(ws.resolve_path("/etc/passwd").is_err());
    }

    #[test]
    fn test_complex_traversal() {
        let (_dir, ws) = temp_workspace();
        assert!(ws.resolve_path("a/../../escape").is_err());
    }

    #[test]
    fn test_safe_dot_path() {
        let (_dir, ws) = temp_workspace();
        let result = ws.resolve_path("./valid.txt");
        assert!(result.is_ok());
    }

    #[test]
    fn test_empty_path() {
        let (_dir, ws) = temp_workspace();
        let result = ws.resolve_path("").unwrap();
        assert_eq!(result, ws.root());
    }

    #[test]
    fn test_delete_dir_fails() {
        let (_dir, ws) = temp_workspace();
        ws.mkdir_sync("dir").unwrap();
        assert!(ws.delete_file("dir").is_err());
    }
}
