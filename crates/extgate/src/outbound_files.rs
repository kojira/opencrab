//! Generic outbound workspace files (`"format": "workspace-file"`).
//!
//! A gateway marks a top-level operation property as a workspace file: either
//! `{"type":"string","format":"workspace-file"}` or
//! `{"type":"array","items":{"type":"string","format":"workspace-file"}}`.
//! Core resolves each agent-supplied path inside the caller agent's workspace,
//! copies the regular file into the core-owned outbox
//! (`<outbox_root>/<instance_id>/<uuid>.bin`, 0600, fsync) and replaces the
//! value with descriptor objects of the inbound `LocalFile` shape:
//! `{id, name, media_type, size, sha256, local_path}` where `local_path` is
//! relative to the outbox root. The gateway reads only the outbox copy.
//!
//! Every failure happens before any delivery row, speech row or invoke frame
//! and removes the copies made so far. Copies are deleted when the call
//! reaches a terminal state; a startup sweep removes leftovers.

use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value};

use crate::file_copy::{copy_regular_file, CopyFailure};
use crate::operations::GatewayOperationDeclaration;
use crate::registry::ExtgateState;

/// Declaration `format` value that marks a workspace-relative file path.
pub const WORKSPACE_FILE_FORMAT: &str = "workspace-file";

const MAX_NAME_CHARS: usize = 255;

/// One outbox copy owned by a pending call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedFile {
    pub path: PathBuf,
    pub name: String,
}

/// A rejected workspace-file value. `Display` is the tool error returned to
/// the model; it never contains host absolute paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundFileError {
    pub field: String,
    pub index: Option<usize>,
    pub reason: &'static str,
}

impl std::fmt::Display for OutboundFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.index {
            Some(index) => write!(
                f,
                "workspace_file_rejected: {}[{}]: {}",
                self.field, index, self.reason
            ),
            None => write!(
                f,
                "workspace_file_rejected: {}: {}",
                self.field, self.reason
            ),
        }
    }
}

enum MarkedShape {
    Single,
    List,
}

fn marked_fields(declaration: &GatewayOperationDeclaration) -> Vec<(String, MarkedShape)> {
    let Some(props) = declaration
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    let is_marked =
        |spec: &Value| spec.get("format").and_then(Value::as_str) == Some(WORKSPACE_FILE_FORMAT);
    props
        .iter()
        .filter_map(|(name, spec)| {
            if is_marked(spec) {
                Some((name.clone(), MarkedShape::Single))
            } else if spec.get("items").is_some_and(is_marked) {
                Some((name.clone(), MarkedShape::List))
            } else {
                None
            }
        })
        .collect()
}

/// Rewrite the declared workspace-file fields of `payload` into outbox
/// descriptors. Payloads without marked values are returned unchanged and no
/// outbox is required.
pub fn stage_workspace_files(
    state: &ExtgateState,
    instance_id: &str,
    agent_id: &str,
    declaration: &GatewayOperationDeclaration,
    payload: &Value,
) -> Result<(Value, Vec<StagedFile>), OutboundFileError> {
    let fields = marked_fields(declaration);
    let Value::Object(map) = payload else {
        return Ok((payload.clone(), Vec::new()));
    };
    let used: Vec<_> = fields
        .into_iter()
        .filter(|(name, _)| map.get(name).is_some_and(|v| !v.is_null()))
        .collect();
    if used.is_empty() {
        return Ok((payload.clone(), Vec::new()));
    }
    let mut stager = Stager::open(state, instance_id, agent_id, &used[0].0)?;
    let mut out = map.clone();
    for (field, shape) in &used {
        let value = &map[field];
        let rewritten = match (shape, value) {
            (MarkedShape::Single, Value::String(path)) => stager.stage(field, None, path),
            (MarkedShape::List, Value::Array(items)) => items
                .iter()
                .enumerate()
                .map(|(index, item)| match item {
                    Value::String(path) => stager.stage(field, Some(index), path),
                    _ => Err(reject(field, Some(index), "must be a string path")),
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            (MarkedShape::Single, _) => Err(reject(field, None, "must be a string path")),
            (MarkedShape::List, _) => Err(reject(field, None, "must be an array of string paths")),
        };
        match rewritten {
            Ok(v) => {
                out.insert(field.clone(), v);
            }
            Err(error) => {
                remove_staged_files(&stager.staged);
                return Err(error);
            }
        }
    }
    Ok((Value::Object(out), stager.staged))
}

/// Remove outbox copies. Idempotent; missing files are ignored.
pub fn remove_staged_files(files: &[StagedFile]) {
    for file in files {
        let _ = std::fs::remove_file(&file.path);
    }
}

/// Startup sweep: remove every leftover outbox copy (no call survives a
/// restart). Symlinks are unlinked, never followed. Returns removed files.
pub fn sweep_attachment_outbox(root: &Path) -> std::io::Result<usize> {
    let mut removed = 0;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            for inner in std::fs::read_dir(&path)? {
                let inner = inner?;
                if !inner.file_type()?.is_dir() {
                    std::fs::remove_file(inner.path())?;
                    removed += 1;
                }
            }
            let _ = std::fs::remove_dir(&path);
        } else {
            std::fs::remove_file(&path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn reject(field: &str, index: Option<usize>, reason: &'static str) -> OutboundFileError {
    OutboundFileError {
        field: field.to_string(),
        index,
        reason,
    }
}

struct Stager {
    workspace: opencrab_core::Workspace,
    instance_dir: PathBuf,
    instance_id: String,
    staged: Vec<StagedFile>,
}

impl Stager {
    fn open(
        state: &ExtgateState,
        instance_id: &str,
        agent_id: &str,
        field: &str,
    ) -> Result<Self, OutboundFileError> {
        let outbox = state
            .attachment_outbox_root()
            .ok_or_else(|| reject(field, None, "attachment outbox is not configured"))?;
        let base = state
            .workspace_base()
            .ok_or_else(|| reject(field, None, "agent workspace is not configured"))?;
        let root = opencrab_core::workspace::resolve_agent_workspace(&base, agent_id)
            .map_err(|_| reject(field, None, "agent workspace is unavailable"))?;
        if !root.is_dir() {
            return Err(reject(field, None, "agent workspace is unavailable"));
        }
        let workspace = opencrab_core::Workspace::from_root(root)
            .map_err(|_| reject(field, None, "agent workspace is unavailable"))?;
        if !is_single_normal_component(instance_id) {
            return Err(reject(field, None, "attachment outbox is unavailable"));
        }
        let instance_dir = outbox.join(instance_id);
        create_private_dir(&instance_dir)
            .map_err(|_| reject(field, None, "attachment outbox is unavailable"))?;
        Ok(Self {
            workspace,
            instance_dir,
            instance_id: instance_id.to_string(),
            staged: Vec::new(),
        })
    }

    fn stage(
        &mut self,
        field: &str,
        index: Option<usize>,
        raw: &str,
    ) -> Result<Value, OutboundFileError> {
        let err = |reason| reject(field, index, reason);
        let relative = Path::new(raw);
        if raw.trim().is_empty() {
            return Err(err("path is empty"));
        }
        if relative.is_absolute() || relative.has_root() {
            return Err(err(
                "absolute paths are not allowed; use a workspace-relative path",
            ));
        }
        if relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(err("path traversal is not allowed"));
        }
        let name = relative
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| valid_name(n))
            .ok_or_else(|| err("file name is invalid"))?
            .to_string();
        let resolved = self
            .workspace
            .resolve_path(raw)
            .map_err(|_| err("path escapes the agent workspace"))?;
        // Require the lexical path to be the canonical path: any symlink in a
        // directory or the leaf is rejected (resolve_path follows links).
        let lexical = relative
            .components()
            .filter(|c| matches!(c, Component::Normal(_)))
            .fold(self.workspace.root().to_path_buf(), |p, c| p.join(c));
        let leaf = lexical
            .symlink_metadata()
            .map_err(|_| err("file not found"))?;
        if leaf.file_type().is_symlink() || resolved != lexical {
            return Err(err("symbolic links are not allowed"));
        }
        if !leaf.file_type().is_file() {
            return Err(err("not a regular file"));
        }
        // Every component is opened with O_NOFOLLOW, closing the check/open race.
        let mut input = crate::secure_path::open_file_no_follow(&lexical)
            .map_err(|_| err("file cannot be opened without following links"))?;
        let id = uuid::Uuid::new_v4().to_string();
        let file_name = format!("{id}.bin");
        let destination = self.instance_dir.join(&file_name);
        let copied = copy_regular_file(&mut input, &destination).map_err(|f| match f {
            CopyFailure::Source => err("not a readable regular file"),
            CopyFailure::Destination => err("attachment outbox write failed"),
        })?;
        self.staged.push(StagedFile {
            path: destination,
            name: name.clone(),
        });
        let mut descriptor = Map::new();
        descriptor.insert("id".into(), Value::String(id));
        descriptor.insert("name".into(), Value::String(name.clone()));
        descriptor.insert(
            "media_type".into(),
            media_type_for(&name).map_or(Value::Null, |m| Value::String(m.to_string())),
        );
        descriptor.insert("size".into(), Value::from(copied.size));
        descriptor.insert("sha256".into(), Value::String(copied.sha256));
        descriptor.insert(
            "local_path".into(),
            Value::String(format!("{}/{file_name}", self.instance_id)),
        );
        Ok(Value::Object(descriptor))
    }
}

fn is_single_normal_component(value: &str) -> bool {
    let mut components = Path::new(value).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && !name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let meta = path.symlink_metadata()?;
            if meta.file_type().is_dir() {
                Ok(())
            } else {
                Err(std::io::Error::from(std::io::ErrorKind::InvalidInput))
            }
        }
        Err(e) => Err(e),
    }
}

fn media_type_for(name: &str) -> Option<&'static str> {
    let ext = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => return None,
    })
}
