//! cursor — 平文専用の推論を、`Engine` の seam の向こうに置く（詳細§01・§05）。
//!
//! `HttpSseEngine`（同クレート `provider`）は HTTPS で SSE を流すプロバイダの共通転送だが、cursor は
//! **HTTP ではなく子プロセス**（`cursor-agent` CLI）なので、その転送には乗らない——`Engine` を直に実装する。
//! ネイティブな道具呼び出しを持たない**平文専用**の頭（`emits_tool_calls()==false`）として繋ぐ:
//! core は本文にツールメニューを描き `Context.tools` を空にするので、この engine は tool 宣言を送らず、
//! 単一プロンプト（system＋会話）を渡して本文を受け取るだけでよい。
//!
//! **呼び出し契約は本体 opencrab #674/#682 と同じ**（移植であって共有ではない・core の型は 1 つも持ち込まない）:
//! infer 毎に空の一時 cwd を作り、その中に deny だけを置いた `.cursor/cli.json` を書き、
//! `--plan --sandbox <値> --trust` で起動する。効いている防御は 2 つ（本体 #682 実測の再掲）:
//!
//! 1. **deny cli.json**: 一時 cwd の `.cursor/cli.json` で Read/Write/Shell/WebFetch/WebSearch/Mcp を deny
//!    する（[`CURSOR_DENY_CONFIG`]）。`version` キーは付けない（project 版は schema エラー・本体 #682 実測）。
//! 2. **空の専用 cwd**: infer 毎に空の [`tempfile::TempDir`] を作り CLI の cwd にする（RAII で削除・孤児を
//!    残さない）。実 workspace を cwd として露出させない。
//!
//! `--plan`（読取専用）で cursor-agent 自身の write/shell を封じ、`--sandbox`（既定 enabled）を重ね、
//! `--trust` で信頼確認プロンプトのハングを避ける（`--force`/`--yolo` は使わない＝危険操作を承認なしで
//! 走らせない）。**塞げていない穴**（本体 #682 でオーナー裁定により受容）: native grep/glob は同梱 `rg`
//! 直呼びで cli.json/`--sandbox` の管轄外で、絶対パスを与えれば空 cwd の外も読める。機構で塞げるのは OS
//! sandbox だけだが複雑さを避けて不採用とし、任意パス読取のリスクは受容する。`Grep(**)` を deny に列挙
//! しないのは、効かないものを載せて「効く」と誤認させないため（本体 #682 実測で否定済み）。
//!
//! **断片を流す口**（§05）: `--output-format stream-json` で起動し、届いた NDJSON 行ごとに `chunks.chunk()`
//! を叩く。cursor-agent は生成中に `thinking`(delta) を流すので、長い生成の間もアイドル計測が取り直され、
//! 「長いが止まっていない生成」は切られない。**最終テキストは終端の `{"type":"result",...}` の `result`**
//! を権威とする（本体の `--output-format json` が返すのと同じ本文。途中の `assistant`/`thinking` 行は
//! liveness のためだけに読む）。1 往復で結果だけ返す形にしない（アイドル上限が総時間上限に化けないため）。
//!
//! **落とし方の境界**（§15・家風 fail loud）: spawn 失敗・stdout 読取失敗・stream-json の壊れ行・本文内の
//! `error` イベント・result 空での失敗印・result イベント無しでの終了は、すべて `EngineError` を返す
//! （core は死なない）。近いものへ寄せない・既定値で埋めない・黙って再試行しない。**総時間では切らない**
//! （アイドルは core の `infer_with_idle_cap` が握る）——内部タイムアウトは持たない。止まった生成は chunk()
//! が来ず、core が上限で infer future を捨て、`kill_on_drop` が子プロセスを確実に kill する（孤児を残さない）。
//!
//! 子プロセスの env は最小化する（[`minimal_env`]）。親 env（他プロバイダのトークン類）を継承させず、
//! `PATH` / `HOME` と、設定時のみ `CURSOR_API_KEY` だけを渡す。認証は `CURSOR_API_KEY` か
//! `cursor-agent login` 済みのアンビエント認証のどちらでも動く。

use opencrab_port::{
    Block, ChunkSink, Context, EffectSpec, Engine, EngineError, InferOutput, MsgRole, Part,
};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

/// 一時 cwd に置く `.cursor/cli.json`（プロジェクト単位の permission 設定）の中身。deny が allow に
/// 優先する。write/shell/read/net/mcp を封じる。**grep / glob は deny が効かないので列挙しない**
/// （効かないものを載せて誤認させないため・本体 #682 実測）。`version` キーは付けない（project 版は
/// schema エラーで弾かれる・本体 #682 実測）。
const CURSOR_DENY_CONFIG: &str = r#"{"permissions":{"allow":[],"deny":["Read(**)","Write(**)","Shell(**)","WebFetch(**)","WebSearch(**)","Mcp(**)"]}}"#;

/// 平文専用の推論の口（`cursor-agent` CLI・子プロセス）。infer 毎に stateless に起動する。
pub struct CursorEngine {
    /// 実行するバイナリ（インストールによりゆれる: `cursor-agent` / `cursor` / `agent`）。
    binary: String,
    /// 予算の物差しにする実効モデル名（§06）。store 登録の context_window で会話予算が決まる。
    model: String,
    /// `--sandbox` の値（"enabled" | "disabled"）。既定は最安全側 "enabled"。`--plan` と直交する多層防御。
    sandbox: String,
    /// 設定時に `CURSOR_API_KEY` として渡す。None なら `cursor-agent login` 済みのアンビエント認証に任せる。
    api_key: Option<String>,
}

impl CursorEngine {
    pub fn new(
        model: impl Into<String>,
        binary: impl Into<String>,
        sandbox: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        CursorEngine {
            binary: binary.into(),
            model: model.into(),
            sandbox: sandbox.into(),
            api_key: api_key.filter(|k| !k.trim().is_empty()),
        }
    }

    /// コマンドと、その cwd に使う空の一時ディレクトリを組み立てる。返した `TempDir` は**呼び出し側が
    /// 子プロセス完了まで保持**する（drop でディレクトリごと削除され、孤児を残さない）。
    fn build_command(&self, prompt: &str) -> Result<(Command, tempfile::TempDir), EngineError> {
        // 空の専用 cwd を作り、deny 設定だけを置く（実 workspace を cwd として露出させない・cli.json 置き場）。
        let cwd = tempfile::TempDir::new()
            .map_err(|e| EngineError(format!("cursor-agent temp cwd: {e}")))?;
        write_deny_config(cwd.path())?;

        let mut cmd = Command::new(resolve_binary(&self.binary));
        // アイドル上限やドロップ時に子プロセスを確実に kill（孤児 agent を残さない）。
        cmd.kill_on_drop(true);
        cmd.arg("-p") // print / headless
            .arg("--output-format")
            .arg("stream-json") // 断片を流す（liveness）。最終本文は終端の result イベント。
            .arg("--model")
            .arg(&self.model)
            // 推論専用（#674）: 読取専用モードで cursor-agent 自身の write/shell を封じる。
            // `--force`/`--yolo` は使わない（危険操作を承認なしで走らせない）。
            .arg("--plan")
            .arg("--sandbox")
            .arg(&self.sandbox)
            // 信頼確認プロンプトでハングしないよう workspace を信頼する（--force を外したので必須）。
            // --trust はディレクトリ信頼のみで write/shell 許可は与えない（それは --plan が封じる）。
            .arg("--trust")
            // プロンプトは positional で渡す（stdin 待ちハング回避）。
            .arg(prompt);

        // 子プロセスの env を最小化する（親 env の他プロバイダのトークン類を継承させない）。
        cmd.env_clear();
        for (key, value) in minimal_env(self.api_key.as_deref()) {
            cmd.env(key, value);
        }
        cmd.current_dir(cwd.path());

        Ok((cmd, cwd))
    }
}

#[async_trait::async_trait]
impl Engine for CursorEngine {
    fn model(&self) -> &str {
        &self.model
    }

    /// 平文専用（ネイティブ道具呼び出しを出せない）。core は本文へツールメニューを描き `Context.tools`
    /// を空にする。この engine は tool 宣言を送らず、本文の平文ツール行を core が解釈して実行する。
    fn emits_tool_calls(&self) -> bool {
        false
    }

    /// CLI（cursor-agent）は画像を受けない——core は core-look をメニューに出さない（DESIGN-images §6）。
    fn accepts_images(&self) -> bool {
        false
    }

    async fn infer(&self, ctx: &Context, chunks: &ChunkSink) -> Result<InferOutput, EngineError> {
        let prompt = build_cursor_prompt(ctx);

        // `_cwd` は子プロセス完了まで保持する（drop で一時 cwd ごと削除される）。
        let (mut cmd, _cwd) = self.build_command(&prompt)?;
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| EngineError(format!("cursor-agent spawn: {e}")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| EngineError("cursor-agent: no stdout pipe".into()))?;
        let mut stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| EngineError("cursor-agent: no stderr pipe".into()))?;
        // stderr は並行して吸い出す（読まないとパイプが埋まって子が停まり得るため）。
        let stderr_task = tokio::spawn(async move {
            let mut s = String::new();
            let _ = stderr_pipe.read_to_string(&mut s).await;
            s
        });

        let mut lines = BufReader::new(stdout).lines();
        // 終端 result イベントの本文・失敗印。stream-json は必ず result で終わる（本体 #684 の形）。
        let mut result_text: Option<String> = None;
        let mut result_is_error = false;
        let mut result_seen = false;
        // 本文内で届いた失敗（`{"type":"error"}`）。握り潰さず、ループ後に EngineError に写す（§15）。
        let mut stream_error: Option<String> = None;

        // 断片を読む。行が届くたび chunk()（アイドルの計測が取り直される・§05）。
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|e| EngineError(format!("cursor-agent stdout: {e}")))?
        {
            chunks.chunk();
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // stream-json は行区切りの完全 JSON。壊れ行は握り潰さず失敗（§15・家風 fail loud）。
            let v: serde_json::Value = serde_json::from_str(line)
                .map_err(|e| EngineError(format!("cursor-agent stream JSON invalid: {e}")))?;
            match v.get("type").and_then(|x| x.as_str()) {
                // 終端イベント: 最終本文・失敗印・（usage も此処に載るが seam に口が無い・下の注記）。
                Some("result") => {
                    result_seen = true;
                    result_is_error = v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
                    result_text = v
                        .get("result")
                        .and_then(|r| r.as_str())
                        .map(|s| s.to_string());
                }
                // 200 応答相当の本文内で届いた失敗。握り潰さない（§15）。
                Some("error") => {
                    let msg = v
                        .get("message")
                        .and_then(|x| x.as_str())
                        .or_else(|| v.pointer("/error/message").and_then(|x| x.as_str()))
                        .unwrap_or("unknown error");
                    stream_error = Some(msg.to_string());
                }
                // system / user / thinking / assistant は liveness のためだけに読む（chunk 済み）。
                _ => {}
            }
        }

        let status = child
            .wait()
            .await
            .map_err(|e| EngineError(format!("cursor-agent wait: {e}")))?;
        let stderr = stderr_task.await.unwrap_or_default();

        // 本文内の失敗は失敗として返す（近いものへ寄せない・握り潰さない・§15）。
        if let Some(msg) = stream_error {
            return Err(EngineError(format!("cursor-agent stream error: {msg}")));
        }

        // 最終本文を決める。result イベントが権威（本体 `--output-format json` の `result` と同じ本文）。
        let text = match (result_seen, result_text) {
            (true, Some(t)) => {
                if t.trim().is_empty() {
                    // 空本文: 失敗印があれば失敗、無ければ「発話なし」（§15・偽の本文を作らない）。
                    if !status.success() || result_is_error {
                        return Err(EngineError(format!(
                            "cursor-agent failed (exit {status}): {}",
                            stderr.trim()
                        )));
                    }
                    String::new()
                } else {
                    // 本文はある。失敗印付き（非ゼロ / is_error）でも本文を捨てない（本体と同じ姿勢）——
                    // result に本文がある = モデルの実出力があるということ。
                    t
                }
            }
            // result が空文字（`"result":""`）で来た場合も上と同様に扱う。
            (true, None) => {
                if !status.success() || result_is_error {
                    return Err(EngineError(format!(
                        "cursor-agent failed (exit {status}): {}",
                        stderr.trim()
                    )));
                }
                String::new()
            }
            // result イベントが来なかった。成功終了でも stream-json の契約破り（必ず result で終わる）
            // → 偽の本文を作らず fail loud（§15）。
            (false, _) => {
                return Err(EngineError(format!(
                    "cursor-agent produced no result event (exit {status}): {}",
                    stderr.trim()
                )));
            }
        };

        // 本文があれば say にする（core が平文アクション/ツール行を解釈する・NO_REPLY 含む）。
        // 空なら効果なし（§08: 出す効果はある分だけ）。ネイティブ道具は出さない（平文専用）。
        let effects = if text.trim().is_empty() {
            vec![]
        } else {
            vec![EffectSpec::say(text)]
        };
        // cursor CLI は 1 往復で完結する stateless 呼び出し——ターンは常に完了（平文ツールの往復は
        // core が決着イベントで別ターンとして回す・§07。ここで done=false にはしない）。
        Ok(InferOutput {
            effects,
            tool_calls: vec![],
            done: true,
        })
    }
}

/// 文脈を cursor CLI の**単一プロンプト**に組む。cursor CLI は system ロールを持たないので、
/// core が組んだ `system`（人格＋場の枠づけ＋文法前文＋メニュー）を**先頭**に置き、順序で system の
/// 役割を代替する。続けて場のログから 1 度組んだ会話（`rendered`）、さらにターン内で積み上がった
/// `history` を平文で連結する。
///
/// 平文専用 engine では `history` は通常空（平文ツールは決着イベントで別ターンになり、native tool
/// ループを回さないため）だが、`done=false` の反復に備えて忠実に平文化しておく。
fn build_cursor_prompt(ctx: &Context) -> String {
    let mut p = String::new();
    if !ctx.system.is_empty() {
        p.push_str(&ctx.system);
        p.push_str("\n\n");
    }
    p.push_str(&ctx.rendered);
    for m in &ctx.history {
        let who = match m.role {
            MsgRole::Assistant => "assistant",
            MsgRole::User => "user",
        };
        for b in &m.content {
            match b {
                Block::Text(t) => {
                    p.push_str(&format!("\n\n[{who}]\n{t}"));
                }
                // 平文専用 engine では通常出ないが、忠実に平文化する（テキストに混ぜて渡すしかない）。
                Block::ToolUse { name, input, .. } => {
                    p.push_str(&format!("\n\n[{who} tool_use: {name}]\n{input}"));
                }
                Block::ToolResult {
                    content, is_error, ..
                } => {
                    let tag = if *is_error {
                        "tool_result error"
                    } else {
                        "tool_result"
                    };
                    // マルチパート（DESIGN-images §4）。平文 CLI は画像を受けない（accepts_images=false で
                    // core-look は来ない）——テキストパートだけを平文化する。
                    let text: String = content
                        .iter()
                        .filter_map(|part| match part {
                            Part::Text(t) => Some(t.as_str()),
                            Part::ImageBytes { .. } => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    p.push_str(&format!("\n\n[{tag}]\n{text}"));
                }
            }
        }
    }
    p
}

/// 一時 cwd に `.cursor/cli.json`（deny 設定）を書き出す。cursor-agent はプロジェクト単位の permission を
/// このパスから読む。
fn write_deny_config(cwd: &Path) -> Result<(), EngineError> {
    let dir = cwd.join(".cursor");
    std::fs::create_dir_all(&dir)
        .map_err(|e| EngineError(format!("cursor-agent .cursor dir: {e}")))?;
    std::fs::write(dir.join("cli.json"), CURSOR_DENY_CONFIG)
        .map_err(|e| EngineError(format!("cursor-agent cli.json: {e}")))?;
    Ok(())
}

/// `binary` を spawn 用に解決する。ディレクトリ付き相対パス（例 `bin/cursor-agent`）は、child の cwd を
/// 一時ディレクトリへ切り替えると解決できなくなるため、サーバー cwd 基準で絶対パス化しておく。単なる
/// コマンド名（PATH 検索）や絶対パスはそのまま返す。
fn resolve_binary(path: &str) -> PathBuf {
    let p = Path::new(path);
    if path.contains('/') && p.is_relative() {
        std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
    } else {
        p.to_path_buf()
    }
}

/// cursor-agent 子プロセスに渡す最小 env を組み立てる。`env_clear` で親 env を捨てた上でこれだけを渡し、
/// 他プロバイダのトークン類を継承させない。
///
/// - `PATH`: バイナリ / node ランタイムの解決に必須
/// - `HOME`: cursor-agent の launcher が `$HOME` を参照し、無いと即死する。アンビエント認証の資格情報も
///   HOME 配下にある
/// - `CURSOR_API_KEY`: api_key を指定したときだけ渡す。未指定なら `cursor-agent login` 済みのアンビエント
///   認証（HOME 配下）に任せる
fn minimal_env(api_key: Option<&str>) -> Vec<(&'static str, String)> {
    let mut env: Vec<(&'static str, String)> = Vec::new();
    if let Ok(path) = std::env::var("PATH") {
        env.push(("PATH", path));
    }
    if let Ok(home) = std::env::var("HOME") {
        env.push(("HOME", home));
    }
    if let Some(key) = api_key {
        env.push(("CURSOR_API_KEY", key.to_string()));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_port::Message;

    fn ctx_with(system: &str, rendered: &str) -> Context {
        Context {
            system: system.to_string(),
            rendered: rendered.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn model_is_the_configured_name() {
        let e = CursorEngine::new("cursor-grok-4.6-high", "cursor-agent", "enabled", None);
        assert_eq!(e.model(), "cursor-grok-4.6-high");
    }

    #[test]
    fn plaintext_engine_does_not_emit_tool_calls() {
        let e = CursorEngine::new("cursor-grok-4.6-high", "cursor-agent", "enabled", None);
        assert!(!e.emits_tool_calls());
    }

    #[test]
    fn empty_api_key_is_treated_as_absent() {
        let e = CursorEngine::new("m", "cursor-agent", "enabled", Some("  ".to_string()));
        assert!(e.api_key.is_none());
        let e2 = CursorEngine::new("m", "cursor-agent", "enabled", Some("sk-x".to_string()));
        assert_eq!(e2.api_key.as_deref(), Some("sk-x"));
    }

    /// 呼び出し契約: 推論専用のコマンドライン（危険フラグ無し・stream-json・--plan/--sandbox/--trust・
    /// プロンプトは末尾 positional・モデルは長形式 --model）。
    #[test]
    fn build_command_is_inference_only_and_streaming() {
        let e = CursorEngine::new("cursor-grok-4.6-high", "cursor-agent", "enabled", None);
        let (cmd, _cwd) = e.build_command("[System]\nhi").expect("build_command");
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        // 危険フラグは存在してはならない。
        assert!(
            !args
                .iter()
                .any(|a| a == "--force" || a == "--yolo" || a == "-f"),
            "cursor は推論専用: --force/--yolo を含めてはならない: {args:?}"
        );
        assert!(args.iter().any(|a| a == "--plan"), "--plan 必須: {args:?}");
        assert!(
            args.iter().any(|a| a == "--trust"),
            "--trust 必須: {args:?}"
        );
        let sb = args
            .iter()
            .position(|a| a == "--sandbox")
            .expect("--sandbox");
        assert_eq!(args.get(sb + 1).map(String::as_str), Some("enabled"));
        assert!(args.iter().any(|a| a == "-p"));
        // 断片を流すため stream-json（1 往復の json ではない）。
        let of = args
            .iter()
            .position(|a| a == "--output-format")
            .expect("--output-format");
        assert_eq!(args.get(of + 1).map(String::as_str), Some("stream-json"));
        // モデルは長形式 --model（この CLI 版は -m を受け付けない・本体 #674）。
        assert!(!args.iter().any(|a| a == "-m"), "-m は無効: {args:?}");
        let m = args.iter().position(|a| a == "--model").expect("--model");
        assert_eq!(
            args.get(m + 1).map(String::as_str),
            Some("cursor-grok-4.6-high")
        );
        // プロンプトは末尾の positional。
        assert_eq!(args.last().map(String::as_str), Some("[System]\nhi"));
    }

    /// sandbox の値は設定から差し替えられ、コマンドラインに反映される。最弱でも --plan は残る。
    #[test]
    fn build_command_sandbox_override_keeps_plan() {
        let e = CursorEngine::new("auto", "cursor-agent", "disabled", None);
        let (cmd, _cwd) = e.build_command("hi").expect("build_command");
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let sb = args
            .iter()
            .position(|a| a == "--sandbox")
            .expect("--sandbox");
        assert_eq!(args.get(sb + 1).map(String::as_str), Some("disabled"));
        assert!(
            args.iter().any(|a| a == "--plan"),
            "sandbox=disabled でも --plan は残らねばならない: {args:?}"
        );
    }

    /// build_command は空の一時 cwd を作り、その中に deny 設定（`.cursor/cli.json`）だけを置く。
    /// deny の中身が [`CURSOR_DENY_CONFIG`] と一致し version を含まず、grep 系を列挙しない（本体 #682）。
    #[test]
    fn build_command_creates_empty_cwd_with_deny_config() {
        let e = CursorEngine::new("auto", "cursor-agent", "enabled", None);
        let (cmd, cwd) = e.build_command("hi").expect("build_command");
        assert_eq!(cmd.as_std().get_current_dir(), Some(cwd.path()));

        let cli_json = std::fs::read_to_string(cwd.path().join(".cursor").join("cli.json"))
            .expect(".cursor/cli.json が読めること");
        assert_eq!(cli_json, CURSOR_DENY_CONFIG);
        assert!(!cli_json.contains("version"));
        assert!(cli_json.contains(r#""allow":[]"#));
        for tool in [
            "Read(**)",
            "Write(**)",
            "Shell(**)",
            "WebFetch(**)",
            "WebSearch(**)",
            "Mcp(**)",
        ] {
            assert!(cli_json.contains(tool), "deny に {tool} が無い: {cli_json}");
        }
        for absent in ["Grep", "list_dir", "codebase_search", "Glob"] {
            assert!(
                !cli_json.contains(absent),
                "deny 効果のない {absent} を列挙してはならない: {cli_json}"
            );
        }
        // cwd 直下は `.cursor` 以外に何も無い（空 cwd の担保）。
        let entries: Vec<String> = std::fs::read_dir(cwd.path())
            .expect("read_dir")
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec![".cursor".to_string()]);
    }

    /// RAII: 返した TempDir を drop すると一時 cwd がディレクトリごと消える（孤児を残さない）。
    #[test]
    fn temp_cwd_is_removed_on_drop() {
        let e = CursorEngine::new("auto", "cursor-agent", "enabled", None);
        let (_cmd, cwd) = e.build_command("hi").expect("build_command");
        let path = cwd.path().to_path_buf();
        assert!(path.exists());
        drop(cwd);
        assert!(
            !path.exists(),
            "drop 後は一時 cwd が削除されていなければならない"
        );
    }

    /// env 最小化: 許可キー（PATH/HOME/CURSOR_API_KEY）以外を渡さない。CURSOR_API_KEY は指定時のみ。
    #[test]
    fn minimal_env_only_allows_expected_keys() {
        let env = minimal_env(None);
        for (k, _) in &env {
            assert!(*k == "PATH" || *k == "HOME", "予期しない env キー: {k}");
        }
        assert!(!env.iter().any(|(k, _)| *k == "CURSOR_API_KEY"));

        let env2 = minimal_env(Some("sk-test-123"));
        assert_eq!(
            env2.iter()
                .find(|(k, _)| *k == "CURSOR_API_KEY")
                .map(|(_, v)| v.as_str()),
            Some("sk-test-123")
        );
    }

    /// プロンプト組み: system が先頭、続けて rendered。system が空なら付けない（線に空 system を載せない）。
    #[test]
    fn prompt_puts_system_first_then_rendered() {
        let ctx = ctx_with("PERSONA-AND-MENU", "[1] alice: hi");
        let p = build_cursor_prompt(&ctx);
        assert!(p.starts_with("PERSONA-AND-MENU"));
        assert!(p.contains("[1] alice: hi"));
        assert!(p.find("PERSONA").unwrap() < p.find("alice").unwrap());

        let ctx2 = ctx_with("", "[1] alice: hi");
        let p2 = build_cursor_prompt(&ctx2);
        assert_eq!(p2, "[1] alice: hi");
    }

    /// 実 CLI 統合テスト（`#[ignore]`: CI に cursor-agent が無い）。stream-json 経路が通しで動くこと:
    /// 1. `infer` が say 効果に本文を載せて返る（`--output-format stream-json` の終端 result を拾う）
    /// 2. 断片（`chunk()`）が複数回叩かれる（thinking delta 等で liveness が流れる＝1 往復ではない）
    /// 3. done=true・ネイティブ tool_calls は空（平文専用）
    /// 4. 読取専用: 絶対パスのマーカーを「作れ」と指示しても作られない（--plan + deny が効いている）
    ///
    ///   `cargo test -p opencrab-app --lib cursor_real_cli -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "requires cursor-agent CLI + auth (grok), makes 1 API call"]
    async fn cursor_real_cli_streams_and_is_read_only() {
        use opencrab_port::EffectKind;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("pwned_by_cursor.txt");
        let engine = CursorEngine::new("cursor-grok-4.6-high", "cursor-agent", "enabled", None);

        let ctx = ctx_with(
            "You are a probe. Answer concisely.",
            &format!(
                "Create a file named {} containing PWNED, then reply with exactly the word DONE.",
                marker.display()
            ),
        );

        let (sink, mut rx) = ChunkSink::channel();
        // 断片を数える受信側（core の infer_with_idle_cap 相当の観測）。
        let count = Arc::new(AtomicU64::new(0));
        let count2 = count.clone();
        let drain = tokio::spawn(async move {
            while rx.recv().await.is_some() {
                count2.fetch_add(1, Ordering::SeqCst);
            }
        });

        let out = engine
            .infer(&ctx, &sink)
            .await
            .expect("infer should succeed");
        drop(sink);
        let _ = drain.await;

        // 1. say に本文が載る。
        let say = out
            .effects
            .iter()
            .find(|e| e.kind == EffectKind::Say)
            .expect("expected a say effect");
        let text = say.content.text.clone().unwrap_or_default();
        assert!(!text.trim().is_empty(), "empty say body");
        eprintln!("cursor said: {text:?}");

        // 2. 断片が複数回流れた（1 往復ではない）。
        let fired = count.load(Ordering::SeqCst);
        assert!(
            fired >= 2,
            "expected multiple chunks (streaming), got {fired}"
        );
        eprintln!("chunks fired: {fired}");

        // 3. done=true・ネイティブ tool_calls は空。
        assert!(out.done);
        assert!(out.tool_calls.is_empty());

        // 4. 読取専用: マーカーは作られていない。
        assert!(
            !marker.exists(),
            "read-only violated: cursor created {}",
            marker.display()
        );
    }

    /// history（ターン内会話）は rendered の後に平文で続く（assistant 本文・tool_result を忠実化）。
    #[test]
    fn prompt_appends_history_after_rendered() {
        let mut ctx = ctx_with("SYS", "[1] alice: read the file");
        ctx.history = vec![
            Message {
                role: MsgRole::Assistant,
                content: vec![Block::Text("ws_read::notes.txt".to_string())],
            },
            Message {
                role: MsgRole::User,
                content: vec![Block::ToolResult {
                    tool_use_id: "call-0".to_string(),
                    content: vec![opencrab_port::Part::text("file body")],
                    is_error: false,
                }],
            },
        ];
        let p = build_cursor_prompt(&ctx);
        assert!(p.contains("ws_read::notes.txt"));
        assert!(p.contains("file body"));
        // rendered が history より前。
        assert!(p.find("alice").unwrap() < p.find("file body").unwrap());
    }
}
