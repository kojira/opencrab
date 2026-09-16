use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::AsyncWrite;

use super::*;

#[test]
fn owner_context_is_exact() {
    assert_eq!(owner_context().caller, SaidCaller::Owner);
    assert!(owner_context().start_turn);
    assert_eq!(owner_context().system_context, None);
    assert_eq!(owner_context().reply_target, None);
    assert_eq!(owner_context().live_inbound_scope, LiveInboundScope::All);
}

#[test]
fn maps_acceptance_and_redacts_wire_details() {
    let id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".to_string();
    let origin = format!("cli:{id}");
    assert_eq!(
        map_said(
            id.clone(),
            origin.clone(),
            Ok(SaidOutcome::Accepted { seq: 4 })
        ),
        Event::Accepted {
            id: id.clone(),
            origin,
            seq: 4
        }
    );
    assert_eq!(
        map_said(
            id.clone(),
            format!("cli:{id}"),
            Ok(SaidOutcome::WireErr {
                code: "private_sql_error".into(),
                detail: Some("secret".into())
            })
        ),
        Event::Error {
            request_id: Some(id),
            code: "gate_error".into(),
            detail: None
        }
    );
}

#[test]
fn maps_all_live_contract_variants() {
    assert!(matches!(
        map_live(LiveEvent::Message {
            delivery_id: "d".into(),
            text: "reply".into(),
            reply_origin: None,
        }),
        Event::Message { .. }
    ));
    assert!(matches!(
        map_live(LiveEvent::Activity {
            activity_id: "a".into(),
            state: "read".into(),
            origin: Some("cli:x".into()),
        }),
        Event::Activity { .. }
    ));
    assert!(matches!(
        map_live(LiveEvent::CompletedNoReply { reply_origin: None }),
        Event::CompletedNoReply { reply_origin: None }
    ));
    assert!(matches!(
        map_live(LiveEvent::TurnFailed {
            reply_origin: "cli:x".into()
        }),
        Event::TurnFailed { .. }
    ));
    assert!(matches!(
        map_live(LiveEvent::Completed { target: "d".into() }),
        Event::Completed { .. }
    ));
}

#[tokio::test]
async fn bounded_output_writer_reports_broken_pipe() {
    assert_eq!(QUEUE_CAPACITY, 32);
    let (writer, reader) = tokio::io::duplex(64);
    drop(reader);
    let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
    tx.send(Output::Event(Event::Closed {
        reason: "requested",
    }))
    .await
    .unwrap();
    drop(tx);
    assert!(output_loop(Frontend::Jsonl, rx, writer, "agent-a".into())
        .await
        .is_err());
}

struct BlockedWriter;

impl AsyncWrite for BlockedWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Poll::Pending
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn output_queue_applies_backpressure_at_32() {
    let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
    let task = tokio::spawn(output_loop(
        Frontend::Jsonl,
        rx,
        BlockedWriter,
        "agent-a".into(),
    ));
    tx.send(Output::Event(Event::Connection { state: "connected" }))
        .await
        .unwrap();
    tokio::task::yield_now().await;
    for _ in 0..QUEUE_CAPACITY {
        tx.try_send(Output::Event(Event::Connection { state: "connected" }))
            .unwrap();
    }
    assert!(matches!(
        tx.try_send(Output::Event(Event::Connection { state: "connected" })),
        Err(mpsc::error::TrySendError::Full(_))
    ));
    task.abort();
}

#[tokio::test]
async fn jsonl_ingress_pauses_without_dropping_after_32() {
    let id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let mut input = String::new();
    for _ in 0..34 {
        input.push_str(&format!(
            "{{\"type\":\"message\",\"id\":\"{id}\",\"text\":\"x\"}}\n"
        ));
    }
    let (ingress_tx, mut ingress_rx) = mpsc::channel(QUEUE_CAPACITY);
    let (output_tx, _output_rx) = mpsc::channel(QUEUE_CAPACITY);
    let (diagnostic_tx, _diagnostic_rx) = mpsc::channel(QUEUE_CAPACITY);
    let emitters = Emitters {
        frontend: Frontend::Jsonl,
        output: output_tx,
        diagnostics: diagnostic_tx,
    };
    let task = tokio::spawn(async move {
        jsonl_input(input.as_bytes(), ingress_tx, emitters).await;
    });
    tokio::task::yield_now().await;
    assert_eq!(ingress_rx.len(), QUEUE_CAPACITY);
    assert!(!task.is_finished());
    let _ = ingress_rx.recv().await;
    tokio::task::yield_now().await;
    assert_eq!(ingress_rx.len(), QUEUE_CAPACITY);
    task.abort();
}

#[test]
fn said_preflight_accounts_for_complete_wire_frame() {
    let context = owner_context();
    assert!(said_fits(
        "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
        "cli:bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        "local-operator",
        &context,
        "hello"
    ));
    assert!(!said_fits(
        "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
        "cli:bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        "local-operator",
        &context,
        &"x".repeat(MAX_FRAME)
    ));
}
