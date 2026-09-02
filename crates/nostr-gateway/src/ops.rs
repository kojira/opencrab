//! Nostr gateway の DI 能力宣言と invoke handler（DESIGN-DI-EXTENSION §9.2 / §9A）。
//!
//! hello に載せる能力宣言（reply/reaction/repost/follow/unfollow/kind0/upload/resolve）と、core からの
//! invoke を nostaro CLI 実行へ写す handler。短縮参照(uN/eN/cN)は core が解決済みで payload に入る:
//! `event` は発端 origin（`nostr:event:v1:...`）、`user`/`ref` は pubkey か origin。gateway は origin から
//! event_id を導いて nostaro を叩く（層分離・§9.3）。秘密鍵は env(`NOSTARO_SECRET_KEY`) のみ。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::process::Command;

use opencrab_gate_client::{InvokeHandler, InvokeOutcome};

use crate::post::event_id_from_origin;
use crate::secret::SECRET_ENV;

const OP_TIMEOUT: Duration = Duration::from_secs(30);

/// hello の `operations` 配列（§9.2・確定スキーマ）。全 write は callback なし・非同期。引数は
/// セッション局所短縮参照（`event`=e番号 / `user`=u番号）。core は値の platform 意味を解釈しない。
pub fn operation_declarations() -> Value {
    // class: 投稿・操作系は sub-engine へ出さない（not_exposed）。イベント/話者を参照する op は
    // conversation_bound、agent 全体の設定系は agent_bound（DI-02 の既存規則に対応）。
    let conv = |sub: &str, sharing: &str| json!({"sub_engine": sub, "sharing": sharing});
    let decl = |name: &str, desc: &str, input: Value, class: Value| {
        json!({
            "name": name,
            "description": desc,
            "input_schema": input,
            "output_schema": null,
            "callback_schema": null,
            "class": class,
        })
    };
    let str_prop = |desc: &str| json!({"type": "string", "description": desc});
    // 短縮参照フィールド（uN/eN/cN）。core はこの標示 field だけを実 ID へ解決する（レビュー要望）。
    let ref_prop =
        |desc: &str| json!({"type": "string", "description": desc, "format": "short-ref"});
    // 配列は name の UTF-8 昇順（follow < kind0 < reaction < reply < repost < resolve < unfollow < upload）。
    json!([
        decl(
            "follow",
            "指定ユーザーをフォローする。user に会話で見えている u番号を渡す。",
            json!({"type": "object", "required": ["user"], "properties": {"user": ref_prop("フォロー対象の短縮参照（例 u2）")}}),
            conv("not_exposed", "agent_bound"),
        ),
        decl(
            "kind0",
            "自分のプロフィール(kind:0)を設定する。指定した項目だけ更新される。",
            json!({"type": "object", "properties": {
                "name": str_prop("ユーザー名"),
                "display_name": str_prop("表示名"),
                "about": str_prop("自己紹介"),
                "picture": str_prop("アイコン画像URL")
            }}),
            conv("not_exposed", "agent_bound"),
        ),
        decl(
            "reaction",
            "イベントにリアクションする。event に会話の e番号、emoji に絵文字（省略可）。結果は返らず、この呼び出しの後に再度呼び出されることはない（撃ちっぱなし・再開なし）。N 件のリアクションが必要なら、この 1 応答に N 個の reaction 呼び出しを置く。",
            json!({"type": "object", "required": ["event"], "properties": {
                "event": ref_prop("対象イベントの短縮参照（例 e7）"),
                "emoji": str_prop("リアクション絵文字（省略時は既定）")
            }}),
            conv("not_exposed", "conversation_bound"),
        ),
        decl(
            "reply",
            "イベントに返信する。event に会話の e番号、text に返信本文。結果は返らず、この呼び出しの後に再度呼び出されることはない（撃ちっぱなし・再開なし）。N 件の返信が必要なら、この 1 応答に N 個の reply 呼び出しを置く。",
            json!({"type": "object", "required": ["event", "text"], "properties": {
                "event": ref_prop("返信先イベントの短縮参照（例 e7）"),
                "text": str_prop("返信本文")
            }}),
            conv("not_exposed", "conversation_bound"),
        ),
        decl(
            "repost",
            "イベントをリポストする。event に会話の e番号。結果は返らず、この呼び出しの後に再度呼び出されることはない（撃ちっぱなし・再開なし）。N 件のリポストが必要なら、この 1 応答に N 個の repost 呼び出しを置く。",
            json!({"type": "object", "required": ["event"], "properties": {"event": ref_prop("対象イベントの短縮参照（例 e7）")}}),
            conv("not_exposed", "conversation_bound"),
        ),
        decl(
            "resolve",
            "u番号/e番号の完全な生JSONを取得する（会話で省略された全文の参照）。",
            json!({"type": "object", "required": ["ref"], "properties": {"ref": ref_prop("u番号またはe番号の短縮参照（例 u2 / e7）")}}),
            conv("not_exposed", "conversation_bound"),
        ),
        decl(
            "unfollow",
            "指定ユーザーのフォローを解除する。user に会話で見えている u番号を渡す。",
            json!({"type": "object", "required": ["user"], "properties": {"user": ref_prop("解除対象の短縮参照（例 u2）")}}),
            conv("not_exposed", "agent_bound"),
        ),
        decl(
            "upload",
            "workspace のファイルを Blossom へアップロードし URL を得る。path に相対パス。",
            json!({"type": "object", "required": ["path"], "properties": {"path": str_prop("アップロードするファイルの相対パス")}}),
            conv("not_exposed", "agent_bound"),
        ),
    ])
}

/// nostaro 実行結果（三結果へ写す前段）。
enum Run {
    Ok(String),
    Rejected,
    Indeterminate,
}

/// Nostr の invoke handler。nostaro CLI を実行し三結果を返す。
pub struct NostrInvokeHandler {
    nostaro_bin: PathBuf,
    config_path: PathBuf,
    secret: Option<String>,
    /// QC ハーネス（dry-run）: publish 系 invoke（reply/reaction/repost/...）を nostaro を
    /// spawn せず本文・種別を `DRY_RUN_LOG_TARGET` へ残して成功 ack する。read（resolve）は
    /// 副作用が無いので dry-run でも通常経路（外部依存が無ければ Rejected）。
    dry_run: bool,
}

impl NostrInvokeHandler {
    pub fn new(
        nostaro_bin: PathBuf,
        config_path: PathBuf,
        secret: Option<String>,
        dry_run: bool,
    ) -> Self {
        Self {
            nostaro_bin,
            config_path,
            secret,
            dry_run,
        }
    }

    /// `publishes=true` は relay/外部へ書く op（reply/reaction/repost/follow/unfollow/kind0/upload）。
    /// nostaro の publish は「1 relay でも受理で Ok（exit 0）／全 relay 未受理で非ゼロ」（実確認:
    /// client.rs check_publish_output は success 空でだけ bail）。よって非ゼロ=確定 publish なしだが、
    /// 「どの relay も応答しなかった」場合は実際には保存された可能性が残る＝受理成否不明。二重投稿
    /// 防止を優先し、write op の非ゼロは Indeterminate にする（§5.3・捏造しない）。read（resolve）は
    /// 副作用が無いので非ゼロ=Rejected。spawn 失敗は外部 I/O 0 なので双方 Rejected。
    async fn run(&self, argv: &[String], publishes: bool) -> Run {
        let mut cmd = Command::new(&self.nostaro_bin);
        cmd.arg("--config")
            .arg(&self.config_path)
            .args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(s) = &self.secret {
            cmd.env(SECRET_ENV, s);
        }
        match tokio::time::timeout(OP_TIMEOUT, cmd.output()).await {
            Ok(Ok(out)) if out.status.success() => {
                Run::Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
            }
            Ok(Ok(out)) => {
                tracing::warn!(code = ?out.status.code(), publishes, "nostaro op non-zero exit");
                if publishes {
                    Run::Indeterminate
                } else {
                    Run::Rejected
                }
            }
            // spawn 失敗 = 外部 I/O 0（nostaro が起動していない=何も投稿していない）。
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "nostaro spawn failed");
                Run::Rejected
            }
            // timeout = 受理成否不明（kill 前に投稿済みかもしれない）。捏造しない。
            Err(_) => {
                tracing::warn!("nostaro op timeout");
                Run::Indeterminate
            }
        }
    }
}

fn str_field<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

/// origin または 64hex pubkey を受ける。resolve 用に種別を判定する。
fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn run_to_outcome(run: Run) -> InvokeOutcome {
    match run {
        Run::Ok(stdout) => {
            // 生 JSON があればそのまま、無ければ文字列として返す（resolve は生JSON・write は要約）。
            let value = serde_json::from_str::<Value>(&stdout).unwrap_or(Value::String(stdout));
            InvokeOutcome::Ok(value)
        }
        Run::Rejected => InvokeOutcome::Rejected,
        Run::Indeterminate => InvokeOutcome::Indeterminate,
    }
}

#[async_trait]
impl InvokeHandler for NostrInvokeHandler {
    // #900: reply/reaction/repost は発話クラス（ユーザーに見える発言）。follow/kind0/resolve/
    // unfollow/upload は照会・操作クラスなので false（沈黙判定に影響させない）。
    fn is_utterance(&self, operation: &str) -> bool {
        matches!(operation, "reply" | "reaction" | "repost")
    }

    async fn handle(&self, _binding_id: &str, operation: &str, payload: &Value) -> InvokeOutcome {
        // QC ハーネス（dry-run）: invoke を nostaro を spawn せず処理する。publish 系（発話
        // reply/reaction/repost や書き込み）は本文・種別を残して成功 ack し、実配線 E2E で
        // 配送を観測できる。read（resolve・照会クラス）は生 JSON 取得の代わりに短縮参照を
        // 返し、settle→resume が結果を読む経路（DI-08 非回帰）を観測できる。
        if self.dry_run {
            if operation == "resolve" {
                let reference = str_field(payload, "ref").unwrap_or_default();
                return InvokeOutcome::Ok(
                    json!({"dry_run": true, "kind": "resolve", "ref": reference}),
                );
            }
            let body = match operation {
                "reply" => str_field(payload, "text").unwrap_or_default(),
                "reaction" => str_field(payload, "emoji").unwrap_or("+"),
                _ => "",
            };
            tracing::info!(
                target: crate::post::DRY_RUN_LOG_TARGET,
                kind = operation,
                body = %body,
                "DRY_RUN invoke (not published)"
            );
            return InvokeOutcome::Ok(Value::Null);
        }
        // 入力欠落・不正は確定拒否（外部 I/O 0）。argv は "--" でオプション終端して positional を渡す。
        let dash = || "--".to_string();
        let argv: Vec<String> = match operation {
            "reply" => {
                let (Some(origin), Some(text)) =
                    (str_field(payload, "event"), str_field(payload, "text"))
                else {
                    return InvokeOutcome::Rejected;
                };
                let Some(id) = event_id_from_origin(origin) else {
                    return InvokeOutcome::Rejected;
                };
                vec!["reply".into(), dash(), id, text.to_string()]
            }
            "reaction" => {
                let Some(origin) = str_field(payload, "event") else {
                    return InvokeOutcome::Rejected;
                };
                let Some(id) = event_id_from_origin(origin) else {
                    return InvokeOutcome::Rejected;
                };
                let mut a = vec!["react".into(), dash(), id];
                if let Some(emoji) = str_field(payload, "emoji") {
                    a.push(emoji.to_string());
                }
                a
            }
            "repost" => {
                let Some(origin) = str_field(payload, "event") else {
                    return InvokeOutcome::Rejected;
                };
                let Some(id) = event_id_from_origin(origin) else {
                    return InvokeOutcome::Rejected;
                };
                vec!["repost".into(), dash(), id]
            }
            "follow" | "unfollow" => {
                let Some(user) = str_field(payload, "user") else {
                    return InvokeOutcome::Rejected;
                };
                vec![operation.to_string(), dash(), user.to_string()]
            }
            "kind0" => {
                let mut a = vec!["profile".to_string(), "set".to_string()];
                let mut any = false;
                for (field, flag) in [
                    ("name", "--name"),
                    ("display_name", "--display-name"),
                    ("about", "--about"),
                    ("picture", "--picture"),
                ] {
                    if let Some(v) = str_field(payload, field) {
                        a.push(flag.to_string());
                        a.push(v.to_string());
                        any = true;
                    }
                }
                if !any {
                    return InvokeOutcome::Rejected;
                }
                a
            }
            "upload" => {
                let Some(path) = str_field(payload, "path") else {
                    return InvokeOutcome::Rejected;
                };
                vec!["upload".into(), dash(), path.to_string()]
            }
            "resolve" => {
                let Some(reference) = str_field(payload, "ref") else {
                    return InvokeOutcome::Rejected;
                };
                if let Some(id) = event_id_from_origin(reference) {
                    vec!["get".into(), dash(), id]
                } else if is_hex64(reference) {
                    vec![
                        "profile".into(),
                        "show".into(),
                        "-p".into(),
                        reference.to_string(),
                    ]
                } else {
                    return InvokeOutcome::Rejected;
                }
            }
            // 宣言外は正常な core からは来ない。fail-closed。
            _ => return InvokeOutcome::Rejected,
        };
        // resolve は read（副作用なし）。他は relay/外部へ書く（二重投稿防止のため非ゼロ=Indeterminate）。
        let publishes = operation != "resolve";
        run_to_outcome(self.run(&argv, publishes).await)
    }
}

/// config path（post 用 config を共用: relays のみ・鍵は env）。
pub fn ops_config_path(socket: &Path, instance_id: &str) -> PathBuf {
    crate::post::post_config_path(socket, instance_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_are_sorted_and_eight() {
        let decls = operation_declarations();
        let arr = decls.as_array().unwrap();
        assert_eq!(arr.len(), 8);
        let names: Vec<&str> = arr.iter().map(|d| d["name"].as_str().unwrap()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "name UTF-8 昇順（§3.1）");
        assert_eq!(
            names,
            vec!["follow", "kind0", "reaction", "reply", "repost", "resolve", "unfollow", "upload"]
        );
        // 全 callback なし。
        for d in arr {
            assert!(d["callback_schema"].is_null(), "第一段は callback なし");
        }
        for name in ["reaction", "reply", "repost"] {
            let description = arr
                .iter()
                .find(|d| d["name"] == name)
                .and_then(|d| d["description"].as_str())
                .unwrap();
            // #920: 発話は fire-and-forget（結果は返らず再呼び出しされない）を事実文で明示。
            assert!(
                description.contains(
                    "結果は返らず、この呼び出しの後に再度呼び出されることはない（撃ちっぱなし・再開なし）。"
                ),
                "#920: {name} 説明文に fire-and-forget の事実文が無い: {description}"
            );
        }
        // #920: reply は「N 件必要なら N 個をこの応答に置く」を事実文で明示。
        let reply_desc = arr
            .iter()
            .find(|d| d["name"] == "reply")
            .and_then(|d| d["description"].as_str())
            .unwrap();
        assert!(
            reply_desc.contains("N 個の reply 呼び出しを置く"),
            "#920: reply 説明文に N 件並置の事実文が無い: {reply_desc}"
        );
    }

    /// DESIGN-PROMPT-INVENTORY-2026-09-03 §1-4 受け入れ（TDD・赤先行）。
    ///
    /// 発話 op（reaction / reply / repost）の説明文を「命令・強英文」から「事実」へ書換
    /// （§2B B4/B5 統合・§3.7）。観測境界は `operation_declarations()` の各 description
    /// （ops_projection.rs:158 が verbatim 投影する実体）。旧命令断片は count==0、書換後の
    /// 全文と一致で pin する。現 tip（9a6af850）では旧強英文が残るため **赤**。
    ///
    /// 期待文字列: reply は §3.7 逐語。reaction / repost は §2B「同型」規則で機械導出
    /// （B1 維持＋固定 merge 文＋op 別 fact 文）。test-review で設計意図との一致を確認する。
    #[test]
    fn utterance_descriptions_are_factual_rewrite_red() {
        let decls = operation_declarations();
        let arr = decls.as_array().unwrap();
        let desc = |name: &str| -> String {
            arr.iter()
                .find(|d| d["name"] == name)
                .and_then(|d| d["description"].as_str())
                .unwrap_or_else(|| panic!("op が無い: {name}"))
                .to_string()
        };

        // 旧命令・強英文・旧 JP B3 は 0 件（否定側・count）。
        for name in ["reaction", "reply", "repost"] {
            let d = desc(name);
            for frag in [
                "This call returns nothing and you will NOT be invoked again after it",
                "you will NOT be invoked again",
                "結果は返らない（撃ちっぱなし・再開はされない）",
                "1回の応答",
                "呼び直す必要はない",
            ] {
                assert_eq!(
                    d.matches(frag).count(),
                    0,
                    "{name}: 旧命令/強英文が残存: {frag:?}\n{d}"
                );
            }
        }
        for (name, old_n) in [
            ("reaction", "put N reaction calls in THIS response"),
            ("reply", "put N reply calls in THIS response"),
            ("repost", "put N repost calls in THIS response"),
        ] {
            assert_eq!(
                desc(name).matches(old_n).count(),
                0,
                "{name}: 旧 N 件並置命令が残存\n{}",
                desc(name)
            );
        }

        // 書換後の全文と一致（§2B 新文・事実形）。
        assert_eq!(
            desc("reaction"),
            "イベントにリアクションする。event に会話の e番号、emoji に絵文字（省略可）。結果は返らず、この呼び出しの後に再度呼び出されることはない（撃ちっぱなし・再開なし）。N 件のリアクションが必要なら、この 1 応答に N 個の reaction 呼び出しを置く。",
            "reaction 説明文が §2B 新文と一致しない"
        );
        assert_eq!(
            desc("reply"),
            "イベントに返信する。event に会話の e番号、text に返信本文。結果は返らず、この呼び出しの後に再度呼び出されることはない（撃ちっぱなし・再開なし）。N 件の返信が必要なら、この 1 応答に N 個の reply 呼び出しを置く。",
            "reply 説明文が §2B 新文と一致しない"
        );
        assert_eq!(
            desc("repost"),
            "イベントをリポストする。event に会話の e番号。結果は返らず、この呼び出しの後に再度呼び出されることはない（撃ちっぱなし・再開なし）。N 件のリポストが必要なら、この 1 応答に N 個の repost 呼び出しを置く。",
            "repost 説明文が §2B 新文と一致しない"
        );
    }
}
