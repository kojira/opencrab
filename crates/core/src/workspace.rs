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
    /// Reading by line requires addressing content by line, and a `String`-returning read cannot
    /// do that safely for a single gigantic line (base64 / minified JSON, #616) — it would load
    /// the whole line. So this reader wraps a [`std::io::BufReader`] with a [`RANGE_SCAN_BYTE_CAP`]
    /// window and never materializes more than that window plus one line's first `max_line_chars`
    /// chars in memory. Reaching `start_line` and finding line ends is done by counting `\n` with
    /// `memchr` only; nothing is tokenized here. The whole page is served from this single open, so
    /// paging never re-opens the file per window (#567).
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
        // Skip to `start_line` (1-based) by consuming `start_line - 1` newlines. Count **all** the
        // newlines in each window at once (`memchr_iter`) so a dense window advances many lines per
        // `fill_buf` — skipping to line 1,200,000 is ~2,800 window reads, not 1,200,000. Bounded
        // memory: skipped bytes are never copied. EOF first ⇒ reader left at EOF, first `next_line`
        // returns `None` (empty page).
        let mut to_skip = start_line.saturating_sub(1);
        while to_skip > 0 {
            let consumed = {
                let buf = reader
                    .fill_buf()
                    .with_context(|| format!("Failed to read file: {}", path.display()))?;
                if buf.is_empty() {
                    break;
                }
                let mut found = 0usize;
                let mut through = 0usize; // consume through the last newline we counted
                for pos in memchr::memchr_iter(b'\n', buf) {
                    found += 1;
                    through = pos + 1;
                    if found == to_skip {
                        break;
                    }
                }
                if found == 0 {
                    buf.len() // no newline in this window; consume it all, skip none
                } else {
                    to_skip -= found;
                    through
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

/// UTF-8 の文字先頭バイト数（＝文字数）。継続バイト `0b10xxxxxx` を除いて数える。妥当な UTF-8
/// なら文字数に一致し、デコードせずに済むので、返さない超過ぶんの計数に使う（[`LineReader`] 相 B）。
///
/// `bool → usize` の総和にしてあるのは、`filter().count()` と違い LLVM が SIMD 水平加算へ
/// ベクトル化でき、90MB 級の単一行でも数十 ms 台で数え切れるため（scalar だと ~150MB/s）。
#[inline]
fn count_char_starts(bytes: &[u8]) -> usize {
    bytes.iter().map(|&b| usize::from((b & 0xC0) != 0x80)).sum()
}

impl LineReader {
    /// 次の 1 行を返す。EOF なら `None`。
    ///
    /// 2 相で走る。**相 A**（`text` が `max_chars` 未満）は窓を UTF-8 デコードして 1 文字ずつ
    /// `text` に積む。窓末尾でマルチバイト文字が割れたら割れたバイト（< 4）だけを次窓へ繰り越し
    /// （`carry`）、不正シーケンスは U+FFFD 1 文字ぶんとして飛ばすので、`carry` は常に < 4 バイトに
    /// 収まり不正 UTF-8 でも行全体を溜め込まない。**相 B**（`text` が満杯）は残りを返さない
    /// ので、もう memcpy も `from_utf8` もデコードもせず、**生バイトで文字先頭（`b & 0xC0 != 0x80`）
    /// を数えるだけ**で `\n` まで読み飛ばす。単一の巨大行（90MB base64 等）でも相 B が 1 パスで
    /// 済む。
    pub fn next_line(&mut self) -> Result<Option<TruncatedLine>> {
        use std::io::BufRead;
        let mut text = String::new();
        let mut total_chars = 0usize;
        // 窓末尾で割れたマルチバイト文字の未完バイト（< 4 バイト）を次窓へ繰り越す（相 A のみ）。
        let mut carry: Vec<u8> = Vec::new();
        let mut saw_any = false;
        loop {
            // 相 B: `text` は満杯。残りは数えて読み飛ばすだけ。
            if total_chars >= self.max_chars {
                // 相 A から持ち越した割れ文字（先頭バイトは未計数）を 1 文字ぶん足して精算する。
                // その継続バイトは次窓の先頭に来るが、下の「文字先頭カウント」は継続バイトを
                // 数えないので、二重計上にならない。
                if !carry.is_empty() {
                    total_chars += 1;
                    carry.clear();
                }
                let (consumed, nl_found, extra) = {
                    let buf = self
                        .reader
                        .fill_buf()
                        .context("Failed to read workspace file line")?;
                    if buf.is_empty() {
                        break; // EOF
                    }
                    saw_any = true;
                    match memchr::memchr(b'\n', buf) {
                        Some(pos) => (pos + 1, true, count_char_starts(&buf[..pos])),
                        None => (buf.len(), false, count_char_starts(buf)),
                    }
                };
                self.reader.consume(consumed);
                total_chars += extra;
                if nl_found {
                    return Ok(Some(self.take_line(text, total_chars)));
                }
                continue;
            }

            // 相 A: 窓をデコードして `text` を埋める。
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
            // `bytes` を妥当な UTF-8 プレフィックス＋（不正シーケンス｜末尾の未完バイト）へ分解して
            // 進める。ポイントは [`std::str::Utf8Error::error_len`] で 2 種を分けること:
            //   - `None`（末尾で入力が尽きた未完文字）→ その < 4 バイトだけ carry して次窓と繋ぐ
            //   - `Some(len)`（確定的な不正シーケンス）→ U+FFFD 1 文字ぶん数えて `len` バイト飛ばし、
            //     **残りを続けて処理する**。これをやらないと、先頭が不正バイト（`0xFF` や孤立継続
            //     バイト）のとき `valid_up_to()` が恒久的に 0 になり、carry に 1 行全体が溜まって
            //     しまう（#616 が消したはずの「行全体をメモリに載せる」状態の退行）。
            let mut rest: &[u8] = &bytes;
            loop {
                match std::str::from_utf8(rest) {
                    Ok(valid) => {
                        for ch in valid.chars() {
                            if total_chars < self.max_chars {
                                text.push(ch);
                            }
                            total_chars += 1;
                        }
                        break;
                    }
                    Err(e) => {
                        let vu = e.valid_up_to();
                        let valid = std::str::from_utf8(&rest[..vu])
                            .expect("valid_up_to is a char boundary");
                        for ch in valid.chars() {
                            if total_chars < self.max_chars {
                                text.push(ch);
                            }
                            total_chars += 1;
                        }
                        match e.error_len() {
                            // 末尾の未完文字（< 4 バイト）。次窓の先頭バイトと繋げる。
                            None => {
                                carry.extend_from_slice(&rest[vu..]);
                                break;
                            }
                            // 確定的な不正シーケンス。置換文字 1 つとして数えて先へ進む。
                            Some(len) => {
                                if total_chars < self.max_chars {
                                    text.push('\u{FFFD}');
                                }
                                total_chars += 1;
                                rest = &rest[vu + len..];
                            }
                        }
                    }
                }
            }
            if nl_found {
                return Ok(Some(self.take_line(text, total_chars)));
            }
        }
        // EOF。何も読めていなければ行は無い。末尾に改行の無い最終行は 1 行として返す。割れ文字が
        // carry に残っていれば（不正な末尾）1 文字ぶん精算する。
        if !saw_any {
            return Ok(None);
        }
        if !carry.is_empty() {
            total_chars += 1;
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
mod tests;
