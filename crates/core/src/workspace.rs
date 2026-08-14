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

/// Validate an agent id used as a path segment for a workspace root.
///
/// Rejects empty ids and any id containing characters that could be used for
/// path traversal (path separators, `..`, NUL, etc.). Agent ids are UUIDs in
/// practice, so we only allow `[A-Za-z0-9_-]`.
pub fn validate_agent_id(agent_id: &str) -> Result<()> {
    if agent_id.is_empty() {
        bail!("invalid agent_id: must not be empty");
    }
    if !agent_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("invalid agent_id: contains disallowed characters");
    }
    Ok(())
}

/// ワークスペースのベーステンプレート（例: `data/agents/{agent_id}/workspace`）を
/// agent_id で展開する唯一の入口（#48）。
///
/// agent_id は必ず `validate_agent_id` を通す。naive な `.replace("{agent_id}", ..)`
/// を呼び出し元に散在させると、検証を忘れた経路がパストラバーサル（`../` 入りの
/// agent_id）を招き、置換を忘れた経路がリテラル `{agent_id}` ディレクトリを生む。
/// テンプレート展開はこの関数に一本化すること。
pub fn resolve_agent_workspace(base_template: &str, agent_id: &str) -> Result<PathBuf> {
    validate_agent_id(agent_id)?;
    Ok(PathBuf::from(base_template.replace("{agent_id}", agent_id)))
}

impl Workspace {
    /// Create a new Workspace rooted at the given directory.
    ///
    /// The directory will be created if it does not exist.
    pub fn new(agent_id: &str, base_path: &str) -> Result<Self> {
        validate_agent_id(agent_id)?;
        let root: PathBuf = Path::new(base_path).join("workspaces").join(agent_id);
        std::fs::create_dir_all(&root)
            .with_context(|| format!("Failed to create workspace directory: {}", root.display()))?;

        // Canonicalize to resolve any symlinks in the root itself.
        let root = root.canonicalize().with_context(|| {
            format!("Failed to canonicalize workspace root: {}", root.display())
        })?;

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
        let root = root.canonicalize().with_context(|| {
            format!("Failed to canonicalize workspace root: {}", root.display())
        })?;

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
                            bail!("Path traversal detected: '{}' escapes workspace", relative);
                        }
                        // Re-check that we haven't escaped.
                        if !normalized.starts_with(&self.root) {
                            bail!("Path traversal detected: '{}' escapes workspace", relative);
                        }
                    }
                    std::path::Component::CurDir => {} // ignore "."
                    std::path::Component::RootDir => {
                        bail!(
                            "Absolute paths are not allowed in workspace: '{}'",
                            relative
                        );
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

    /// Open `relative_path` **once** and return a [`LineReader`] positioned at `start_line`
    /// (1-based), together with the file size in bytes (`metadata().len()`, O(1)).
    ///
    /// The reader wraps a [`std::io::BufReader`] with a [`RANGE_SCAN_BYTE_CAP`] window and never
    /// materializes more than that window plus one line's first `max_line_chars` chars in memory —
    /// so a single gigantic line (base64 / minified JSON, #616) is read safely. Reaching
    /// `start_line` and finding line ends is done by counting `\n` with `memchr` only; nothing is
    /// tokenized here. This replaces the old per-call `open`+`seek` window read: paging a huge
    /// offloaded file (#564: measured 509MB) no longer re-opens the file ~2,800 times to walk one
    /// page (#567) — the whole page is served from this single open.
    pub fn line_reader(
        &self,
        relative_path: &str,
        start_line: usize,
        max_line_chars: usize,
    ) -> Result<(LineReader, u64)> {
        use std::io::BufRead;
        let path = self.resolve_path(relative_path)?;
        let file = std::fs::File::open(&path)
            .with_context(|| format!("Failed to open file: {}", path.display()))?;
        let total = file
            .metadata()
            .with_context(|| format!("Failed to stat file: {}", path.display()))?
            .len();
        let mut reader = std::io::BufReader::with_capacity(RANGE_SCAN_BYTE_CAP, file);
        // Skip to `start_line` (1-based) by consuming `start_line - 1` newlines. Bounded memory:
        // the skipped bytes are never copied, only the buffer is advanced. If EOF is hit first the
        // reader is left at EOF and the first `next_line` returns `None` (empty page).
        let mut to_skip = start_line.saturating_sub(1);
        while to_skip > 0 {
            let consumed = {
                let buf = reader
                    .fill_buf()
                    .with_context(|| format!("Failed to read file: {}", path.display()))?;
                if buf.is_empty() {
                    break;
                }
                match memchr::memchr(b'\n', buf) {
                    Some(pos) => {
                        to_skip -= 1;
                        pos + 1
                    }
                    None => buf.len(),
                }
            };
            reader.consume(consumed);
        }
        tracing::debug!(path = %path.display(), start_line, "Opened workspace line reader");
        Ok((
            LineReader {
                reader,
                next_no: start_line,
                max_chars: max_line_chars,
            },
            total,
        ))
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

/// 逐次行読みで 1 度に埋める窓（＝`BufReader` の容量, 硬い上限）。行の走査・スキップは常に
/// この窓ぶんずつ前進するので、1 回の IO・メモリは全ファイル長にも単一行の長さにも依らず
/// この窓で頭打ちになる（#564 実測 509MB・単一行でも固まらない / #567）。値は 32 KiB。
pub const RANGE_SCAN_BYTE_CAP: usize = 32_768;

/// [`LineReader::next_line`] が返す 1 行。`text` は先頭 `max_chars` 文字までに切り詰めてあり、
/// `overflow_chars` は切り捨てた文字数（切っていなければ 0）。単位は**文字数**（`char`）。
#[derive(Debug, Clone)]
pub struct TruncatedLine {
    /// 1 始まりの行番号。
    pub number: usize,
    /// 行頭から高々 `max_chars` 文字（超過ぶんは含まない）。
    pub text: String,
    /// 切り捨てた文字数（`元の文字数 − max_chars`, 切っていなければ 0）。
    pub overflow_chars: usize,
}

/// [`Workspace::line_reader`] が返す、単一 open の逐次行読み。`next_line` を呼ぶたびに次の 1 行を
/// **行頭から `max_chars` 文字まで**読み、残りは `\n` まで読み飛ばして超過文字数だけ数える。
/// トークン計算は一切しない（呼び出し側が組み上げたページにだけ掛ける / #617）。
pub struct LineReader {
    reader: std::io::BufReader<std::fs::File>,
    /// 次に返す行の 1 始まり行番号。
    next_no: usize,
    /// 1 行あたりに読む最大文字数。
    max_chars: usize,
}

impl LineReader {
    /// 次の 1 行を返す。EOF なら `None`。行頭 `max_chars` 文字だけを `text` に積み、残りは
    /// `\n`（または EOF）まで読み飛ばして `overflow_chars` に数える。窓（[`RANGE_SCAN_BYTE_CAP`]）
    /// を跨いでマルチバイト文字が割れても、割れたバイトだけを次窓へ繰り越す（`carry`）ので、
    /// バイト境界補正はこの 1 手だけで済む。
    pub fn next_line(&mut self) -> Result<Option<TruncatedLine>> {
        use std::io::BufRead;
        let mut text = String::new();
        let mut total_chars = 0usize;
        // 窓末尾で割れたマルチバイト文字の未完バイト（< 4 バイト）を次窓へ繰り越す。
        let mut carry: Vec<u8> = Vec::new();
        let mut saw_any = false;
        loop {
            let mut bytes = std::mem::take(&mut carry);
            let (consumed, nl_found);
            {
                let buf = self
                    .reader
                    .fill_buf()
                    .context("Failed to read workspace file line")?;
                if buf.is_empty() {
                    break; // EOF
                }
                saw_any = true;
                match memchr::memchr(b'\n', buf) {
                    Some(pos) => {
                        bytes.extend_from_slice(&buf[..pos]);
                        consumed = pos + 1;
                        nl_found = true;
                    }
                    None => {
                        bytes.extend_from_slice(buf);
                        consumed = buf.len();
                        nl_found = false;
                    }
                }
            }
            self.reader.consume(consumed);
            // `bytes` の妥当な UTF-8 プレフィックスだけを文字に割る。末尾に未完バイトが残れば
            // 次窓の先頭バイトと繋がるので carry へ（`\n` は継続バイトになり得ないので、改行を
            // 見つけた窓では未完バイトは残らない）。
            let valid_len = match std::str::from_utf8(&bytes) {
                Ok(_) => bytes.len(),
                Err(e) => e.valid_up_to(),
            };
            let valid =
                std::str::from_utf8(&bytes[..valid_len]).expect("valid_up_to is a char boundary");
            for ch in valid.chars() {
                if total_chars < self.max_chars {
                    text.push(ch);
                }
                total_chars += 1;
            }
            if valid_len < bytes.len() {
                carry.extend_from_slice(&bytes[valid_len..]);
            }
            if nl_found {
                return Ok(Some(self.take_line(text, total_chars)));
            }
        }
        // EOF。何も読めていなければ行は無い。末尾に改行の無い最終行は 1 行として返す。
        if !saw_any {
            return Ok(None);
        }
        Ok(Some(self.take_line(text, total_chars)))
    }

    fn take_line(&mut self, text: String, total_chars: usize) -> TruncatedLine {
        let number = self.next_no;
        self.next_no += 1;
        TruncatedLine {
            number,
            text,
            overflow_chars: total_chars.saturating_sub(self.max_chars),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn resolve_agent_workspace_expands_and_validates() {
        let p =
            super::resolve_agent_workspace("data/agents/{agent_id}/workspace", "crab-1").unwrap();
        assert_eq!(p, std::path::PathBuf::from("data/agents/crab-1/workspace"));

        // 検証を必ず通す: トラバーサル/空は拒否
        assert!(super::resolve_agent_workspace("data/{agent_id}", "../evil").is_err());
        assert!(super::resolve_agent_workspace("data/{agent_id}", "a/b").is_err());
        assert!(super::resolve_agent_workspace("data/{agent_id}", "").is_err());

        // テンプレートに {agent_id} が無い場合は素通し（共有ベース運用）
        let p = super::resolve_agent_workspace("/tmp", "crab-1").unwrap();
        assert_eq!(p, std::path::PathBuf::from("/tmp"));
    }

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

    /// #617: `line_reader` は **1 回の open** で `next_line` を繰り返して全行を順に返す（毎行
    /// open+seek し直す O(n²) ではない）。単一 open であることは、1 つの reader インスタンスから
    /// 連続した行番号・本文が順に出てくることで観測できる。
    #[test]
    fn line_reader_reads_sequentially_from_single_open() {
        let (_dir, ws) = temp_workspace();
        ws.write_file("multi.txt", "alpha\nbravo\ncharlie\ndelta")
            .unwrap();

        // start_line=2 から。open は 1 回だけ、以降は同じ reader を前進させる。
        let (mut reader, total) = ws.line_reader("multi.txt", 2, 512).unwrap();
        assert_eq!(total, "alpha\nbravo\ncharlie\ndelta".len() as u64);

        let l2 = reader.next_line().unwrap().unwrap();
        assert_eq!(
            (l2.number, l2.text.as_str(), l2.overflow_chars),
            (2, "bravo", 0)
        );
        let l3 = reader.next_line().unwrap().unwrap();
        assert_eq!(l3.number, 3);
        assert_eq!(l3.text, "charlie");
        let l4 = reader.next_line().unwrap().unwrap();
        assert_eq!((l4.number, l4.text.as_str()), (4, "delta")); // 末尾に改行なし
        assert!(reader.next_line().unwrap().is_none(), "EOF");
    }

    /// 1 行あたりの文字数上限が効き、超過ぶんは `overflow_chars` に出る（バイトではなく文字数）。
    /// マルチバイト（3 バイト/字）でも切りは文字境界で、割れた文字は出さない。
    #[test]
    fn line_reader_truncates_by_chars_not_bytes() {
        let (_dir, ws) = temp_workspace();
        // "あ"×10（30 バイト）を 1 行。max_chars=4 で 4 文字だけ、6 文字あふれる。
        ws.write_file("jp.txt", &"あ".repeat(10)).unwrap();
        let (mut reader, _total) = ws.line_reader("jp.txt", 1, 4).unwrap();
        let l = reader.next_line().unwrap().unwrap();
        assert_eq!(l.text, "ああああ", "文字境界で 4 文字だけ返す");
        assert_eq!(l.text.chars().count(), 4);
        assert_eq!(l.overflow_chars, 6, "切り捨ては文字数で数える");
    }

    /// start_line がファイル末尾を越えたら行は無い（空ページ）。
    #[test]
    fn line_reader_start_past_end_is_empty() {
        let (_dir, ws) = temp_workspace();
        ws.write_file("f.txt", "a\nb\nc").unwrap();
        let (mut reader, _total) = ws.line_reader("f.txt", 100, 512).unwrap();
        assert!(reader.next_line().unwrap().is_none());
    }
}
