//! 実リレー（WebSocket・NIP-01）への薄い口。フレームの組み立てと受信メッセージの分類だけ。
//!
//! 接続の生死・再購読・応答待ちの管理は main が持つ（状態を握るのはゲート本体）。ここは main と
//! tests/relay.rs の両方が同じ 1 実装を使えるように、接続と純粋なフレーム関数を出すだけにする。

use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// リレーへ接続する（wss:// は rustls + webpki-roots で張る）。
pub async fn connect(url: &str) -> Result<Ws, String> {
    let (ws, _resp) = connect_async(url).await.map_err(|e| e.to_string())?;
    Ok(ws)
}

/// 購読を張る: `["REQ", <subid>, <filter>]`。
pub fn req_frame(subid: &str, filter: &Value) -> Message {
    Message::Text(json!(["REQ", subid, filter]).to_string())
}

/// 購読を閉じる: `["CLOSE", <subid>]`。
pub fn close_frame(subid: &str) -> Message {
    Message::Text(json!(["CLOSE", subid]).to_string())
}

/// 発行する: `["EVENT", <event>]`。
pub fn event_frame(event: &Value) -> Message {
    Message::Text(json!(["EVENT", event]).to_string())
}

/// リレーからの受信メッセージ（NIP-01）。知らない形は Other にして落とさず記録に回せるようにする。
pub enum RelayMsg {
    Event { subid: String, event: Value },
    Ok { id: String, ok: bool, msg: String },
    Eose { subid: String },
    Notice(String),
    Closed { subid: String, msg: String },
    Other(String),
}

/// テキストフレームを NIP-01 のメッセージへ分類する。
pub fn parse_relay(text: &str) -> RelayMsg {
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return RelayMsg::Other(text.to_string()),
    };
    let arr = match v.as_array() {
        Some(a) => a,
        None => return RelayMsg::Other(text.to_string()),
    };
    let t = arr.first().and_then(|x| x.as_str()).unwrap_or("");
    let s = |i: usize| {
        arr.get(i)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    };
    match t {
        "EVENT" => RelayMsg::Event {
            subid: s(1),
            event: arr.get(2).cloned().unwrap_or(Value::Null),
        },
        "OK" => RelayMsg::Ok {
            id: s(1),
            ok: arr.get(2).and_then(|x| x.as_bool()).unwrap_or(false),
            msg: s(3),
        },
        "EOSE" => RelayMsg::Eose { subid: s(1) },
        "NOTICE" => RelayMsg::Notice(s(1)),
        "CLOSED" => RelayMsg::Closed {
            subid: s(1),
            msg: s(2),
        },
        _ => RelayMsg::Other(text.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_nip01_shaped() {
        let f = req_frame("sub1", &json!({"kinds": [1]}));
        assert_eq!(
            f.into_text().unwrap().as_str(),
            r#"["REQ","sub1",{"kinds":[1]}]"#
        );
        assert_eq!(
            close_frame("sub1").into_text().unwrap().as_str(),
            r#"["CLOSE","sub1"]"#
        );
    }

    #[test]
    fn parses_event_ok_eose() {
        match parse_relay(r#"["EVENT","s",{"id":"a"}]"#) {
            RelayMsg::Event { subid, event } => {
                assert_eq!(subid, "s");
                assert_eq!(event["id"], json!("a"));
            }
            _ => panic!("expected event"),
        }
        match parse_relay(r#"["OK","idhex",true,"ok"]"#) {
            RelayMsg::Ok { id, ok, msg } => {
                assert_eq!(id, "idhex");
                assert!(ok);
                assert_eq!(msg, "ok");
            }
            _ => panic!("expected ok"),
        }
        match parse_relay(r#"["EOSE","s"]"#) {
            RelayMsg::Eose { subid } => assert_eq!(subid, "s"),
            _ => panic!("expected eose"),
        }
        match parse_relay("not json") {
            RelayMsg::Other(_) => {}
            _ => panic!("expected other"),
        }
    }
}
