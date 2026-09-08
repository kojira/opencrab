//! 1 instance = 1 UDS connection。hello / bind ack / said / say / activity だけ。
//! 切断後は指数 backoff で再接続し、hello 再送で open binding を replay する。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, Mutex, Notify};

use super::wire::{
    err_frame, hello_frame_with_operations, invoke_ok_frame, ok_frame, parse_frame_bytes,
    read_frame, said_frame_with_author_label, say_reply_target, say_text, write_json, Activity,
    Attachment, Bind, CoreMsg, FrameError, Invoke, Say, TurnFailed, WireResponse,
};

include!("client/state_api.rs");
include!("client/transport.rs");
include!("client/handlers.rs");

#[cfg(test)]
mod completed_tests {
    include!("client/tests.rs");
}
