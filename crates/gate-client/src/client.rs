//! 1 instance = 1 UDS connection。hello / bind ack / said / say / activity だけ。
//! 切断後は指数 backoff で再接続し、hello 再送で open binding を replay する。

use std::collections::{HashMap, HashSet};
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
    command_frame, create_binding_frame, err_frame, hello_frame_with_capabilities_and_env,
    interrupt_frame, invoke_ok_frame, ok_frame, parse_frame_bytes, provisional_cancel_frame,
    provisional_frame, read_frame, said_frame_with_context, say_reply_target, say_stream_id,
    say_text, write_json, Activity, Attachment, Bind, CoreMsg, FrameError, Invoke, Partial,
    ProvisionalRequest, ProvisionalResult, SaidCaller, SaidContext, Say, SubtaskLifecycle,
    ToolActivity, TurnFailed, WireResponse,
};

include!("client/live_event.rs");
include!("client/state_api.rs");
include!("client/transport.rs");
include!("client/handlers.rs");
include!("client/provisional.rs");

#[cfg(test)]
mod completed_tests {
    include!("client/tests.rs");
}

#[cfg(test)]
mod partial_tests {
    include!("client/partial_tests.rs");
}

#[cfg(test)]
mod provisional_tests {
    include!("client/provisional_tests.rs");
}

#[cfg(test)]
mod transport_tests {
    include!("client/transport_tests.rs");
}
