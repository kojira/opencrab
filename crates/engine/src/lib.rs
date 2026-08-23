//! engine — 推論の seam と、テスト用の差し替え実装。
//!
//! 本物の LLM は今回入れない（詳細§01）。`ScriptedEngine` は決まった応答を返し、
//! 設計が守ると言った性質を「揺らぎ抜き」で測るためのもの。呼び出し回数も数える。
//!
//! ついでに core の外界向き seam（`ToolHost`・`Notifier`）のテスト用実装も置く。
//! 本番ではこれらは plugd が実装する。

use opencrab_port::*;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

/// 1 回の `infer()` が返すもの。`gate` があればそこで待ってから返す（テストで割り込みを挟むため）。
pub struct Step {
    pub effects: Vec<EffectSpec>,
    pub tool_calls: Vec<ToolCallSpec>,
    pub done: bool,
    pub failure: Option<String>,
    /// 断片の到着を模す。各要素だけ sleep してから 1 つ断片を流す（詳細§05）。
    /// 断片を流さず gate で止めれば「止まった推論」、間隔がアイドル上限より短ければ「長いが切られない生成」。
    pub chunks: Vec<Duration>,
    pub gate: Option<Arc<Notify>>,
    pub entered: Option<Arc<Notify>>,
}

impl Step {
    pub fn cont() -> Step {
        Step {
            effects: vec![],
            tool_calls: vec![],
            done: false,
            failure: None,
            chunks: vec![],
            gate: None,
            entered: None,
        }
    }
    pub fn done() -> Step {
        Step {
            effects: vec![],
            tool_calls: vec![],
            done: true,
            failure: None,
            chunks: vec![],
            gate: None,
            entered: None,
        }
    }
    /// この推論は失敗する（EngineError を返す）。ターンが失敗で終わり、それでも記録が書かれることの確認に使う。
    pub fn fail() -> Step {
        Self::fail_with("scripted failure")
    }
    /// 指定した本文の EngineError を返す。失敗理由が turn_records まで逐語で届く検査に使う。
    pub fn fail_with(detail: &str) -> Step {
        Step {
            effects: vec![],
            tool_calls: vec![],
            done: false,
            failure: Some(detail.to_string()),
            chunks: vec![],
            gate: None,
            entered: None,
        }
    }
    /// 意図的な無発話を明示する。意味的な空出力を作る `done()` とは区別する。
    pub fn no_reply() -> Step {
        Self::say_done("NO_REPLY")
    }
    /// 意図的無発話を返しつつターンを継続する。割り込みfixtureなど、空の `cont()` を使えない場面用。
    pub fn no_reply_cont() -> Step {
        let mut step = Self::no_reply();
        step.done = false;
        step
    }
    pub fn say_done(text: &str) -> Step {
        Step {
            effects: vec![EffectSpec::say(text)],
            tool_calls: vec![],
            done: true,
            failure: None,
            chunks: vec![],
            gate: None,
            entered: None,
        }
    }
    /// 各 `gap` だけ待ってから断片を 1 つ流す、を繰り返してから結果を返す（詳細§05）。
    /// 総時間が長くても、gap がアイドル上限より短ければ切られないことの確認に使う。
    pub fn with_chunks(mut self, gaps: Vec<Duration>) -> Step {
        self.chunks = gaps;
        self
    }
    pub fn with_effect(mut self, e: EffectSpec) -> Step {
        self.effects.push(e);
        self
    }
    pub fn with_tool(mut self, name: &str) -> Step {
        let id = format!("call-{}", self.tool_calls.len());
        self.tool_calls.push(ToolCallSpec {
            id,
            name: name.to_string(),
            args: serde_json::json!({}),
        });
        self
    }
    pub fn with_tool_args(mut self, name: &str, args: serde_json::Value) -> Step {
        let id = format!("call-{}", self.tool_calls.len());
        self.tool_calls.push(ToolCallSpec {
            id,
            name: name.to_string(),
            args,
        });
        self
    }
    pub fn gated(mut self, gate: Arc<Notify>, entered: Arc<Notify>) -> Step {
        self.gate = Some(gate);
        self.entered = Some(entered);
        self
    }
}

#[derive(Clone)]
pub struct ScriptedEngine {
    steps: Arc<Mutex<VecDeque<Step>>>,
    calls: Arc<AtomicU64>,
    contexts: Arc<Mutex<Vec<String>>>,
    /// 各 infer に渡された `Context.system`（人格＋場の枠づけ＋文法前文＋メニュー）。core が system を
    /// どう組むかをテストが検査するため。rendered（可変部）とは別に控える。
    systems: Arc<Mutex<Vec<String>>>,
    histories: Arc<Mutex<Vec<String>>>,
    /// 各 infer に渡された `Context.tools` の名前（平文ツール行の検査用）。`emits_tool_calls=false`
    /// のとき core がネイティブ道具宣言を空にすることを測る。
    tools_seen: Arc<Mutex<Vec<Vec<String>>>>,
    /// 各 infer に渡された `Context.throttle`（返答の絞り・DESIGN-attention §2）。高消費の着火作者の
    /// ターンで core が絞りを組む（max_tokens・努力ヒント）ことをテストが検査するため。
    throttles: Arc<Mutex<Vec<Option<Throttle>>>>,
    /// この engine がネイティブ道具を出せるか（既定 true）。テストが `false` に切り替えて、
    /// core が本文へツールメニューを描き `Context.tools` を空にする経路を測る。
    emits: Arc<AtomicBool>,
    /// この engine が画像を受けるか（既定 true・DESIGN-images §6）。テストが `false` に切り替えて、
    /// core が `core-look` をメニューから落とす経路を測る。
    accepts: Arc<AtomicBool>,
    /// 各 infer で `ctx.history` の tool_result に入っていた画像パートの media_type（DESIGN-images §4）。
    /// core-look の成功で画像がプロバイダの形（ImageBytes）で会話に入ることをテストが検査するため
    /// （テキストは `histories`、画像はここ——混ぜない）。
    image_media_types: Arc<Mutex<Vec<Vec<String>>>>,
    /// 予算の物差しにする実効モデル名（§06・既定 `"scripted"`）。テストハーネスはこの名前で
    /// store に context_window を登録し、core が起動時に会話予算を確定する。
    model: Arc<str>,
}

impl Default for ScriptedEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptedEngine {
    pub fn new() -> ScriptedEngine {
        ScriptedEngine {
            steps: Arc::new(Mutex::new(VecDeque::new())),
            calls: Arc::new(AtomicU64::new(0)),
            contexts: Arc::new(Mutex::new(vec![])),
            systems: Arc::new(Mutex::new(vec![])),
            histories: Arc::new(Mutex::new(vec![])),
            tools_seen: Arc::new(Mutex::new(vec![])),
            throttles: Arc::new(Mutex::new(vec![])),
            emits: Arc::new(AtomicBool::new(true)),
            accepts: Arc::new(AtomicBool::new(true)),
            image_media_types: Arc::new(Mutex::new(vec![])),
            model: Arc::from("scripted"),
        }
    }

    /// 実効モデル名を差し替える（予算の物差し・§06）。テストが「別モデルの context_window で
    /// 別の会話予算になる」ことや「未登録モデルは fail loud」を測るのに使う。
    pub fn with_model(mut self, model: &str) -> ScriptedEngine {
        self.model = Arc::from(model);
        self
    }

    pub fn push(&self, s: Step) {
        self.steps.lock().unwrap().push_back(s);
    }

    pub fn call_count(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }

    /// ネイティブ道具を出す／出さないを切り替える（平文専用 engine を模す）。
    pub fn set_emits_tool_calls(&self, v: bool) {
        self.emits.store(v, Ordering::SeqCst);
    }

    /// 画像を受ける／受けないを切り替える（DESIGN-images §6・core-look の広告経路を測る）。
    pub fn set_accepts_images(&self, v: bool) {
        self.accepts.store(v, Ordering::SeqCst);
    }

    /// これまでの各 infer で `Context.tools` に入っていた道具名（呼ばれた順）。テストの検査用。
    pub fn tools_seen(&self) -> Vec<Vec<String>> {
        self.tools_seen.lock().unwrap().clone()
    }

    /// これまでの各 infer に渡された文脈（rendered＝最初の user テキスト）。テストが中身を検査するため。
    pub fn contexts(&self) -> Vec<String> {
        self.contexts.lock().unwrap().clone()
    }

    pub fn last_context(&self) -> Option<String> {
        self.contexts.lock().unwrap().last().cloned()
    }

    /// これまでの各 infer に渡された system（人格＋場の枠づけ＋文法前文＋メニュー）。テストが構成を検査するため。
    pub fn systems(&self) -> Vec<String> {
        self.systems.lock().unwrap().clone()
    }

    pub fn last_system(&self) -> Option<String> {
        self.systems.lock().unwrap().last().cloned()
    }

    /// これまでの各 infer で `ctx.history` に入っていた道具の結果（tool_result の中身）を連結したもの。
    /// 「結果が次の推論の会話に、テキストではなくプロバイダの形で入る」ことを検査するため（§05）。
    pub fn histories(&self) -> Vec<String> {
        self.histories.lock().unwrap().clone()
    }

    /// 各 infer で会話の tool_result に入っていた画像パートの media_type（DESIGN-images §4）。
    /// core-look の成功で画像がプロバイダの形（ImageBytes）で入ることの検査に使う。
    pub fn image_media_types(&self) -> Vec<Vec<String>> {
        self.image_media_types.lock().unwrap().clone()
    }

    /// これまでの各 infer に渡された `Context.throttle`（返答の絞り・DESIGN-attention §2）。
    pub fn throttles(&self) -> Vec<Option<Throttle>> {
        self.throttles.lock().unwrap().clone()
    }

    pub fn last_throttle(&self) -> Option<Throttle> {
        self.throttles.lock().unwrap().last().copied().flatten()
    }
}

#[async_trait::async_trait]
impl Engine for ScriptedEngine {
    fn emits_tool_calls(&self) -> bool {
        self.emits.load(Ordering::SeqCst)
    }

    fn accepts_images(&self) -> bool {
        self.accepts.load(Ordering::SeqCst)
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn infer(&self, ctx: &Context, chunks: &ChunkSink) -> Result<InferOutput, EngineError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.contexts.lock().unwrap().push(ctx.rendered.clone());
        self.systems.lock().unwrap().push(ctx.system.clone());
        self.tools_seen
            .lock()
            .unwrap()
            .push(ctx.tools.iter().map(|t| t.name.clone()).collect());
        self.throttles.lock().unwrap().push(ctx.throttle);
        // ターン内会話に積まれた道具の結果（tool_result の中身）を控える（§05 の検査用）。
        let mut hist = String::new();
        let mut imgs: Vec<String> = vec![];
        for m in &ctx.history {
            for b in &m.content {
                if let Block::ToolResult { content, .. } = b {
                    // マルチパート（DESIGN-images §4）: テキストは hist へ、画像は media_type を控える。
                    for p in content {
                        match p {
                            Part::Text(t) => {
                                hist.push_str(t);
                                hist.push('\n');
                            }
                            Part::ImageBytes { media_type, .. } => imgs.push(media_type.clone()),
                        }
                    }
                }
            }
        }
        self.image_media_types.lock().unwrap().push(imgs);
        self.histories.lock().unwrap().push(hist);
        // ロックを await をまたいで持たない。
        let step = self.steps.lock().unwrap().pop_front();
        match step {
            Some(s) => {
                if let Some(e) = &s.entered {
                    e.notify_one();
                }
                // 断片を流す（同じ口・詳細§05）。間隔だけ待ってから 1 つ流す。
                for gap in &s.chunks {
                    tokio::time::sleep(*gap).await;
                    chunks.chunk();
                }
                if let Some(g) = &s.gate {
                    g.notified().await;
                }
                if let Some(detail) = s.failure {
                    return Err(EngineError(detail));
                }
                Ok(InferOutput {
                    effects: s.effects,
                    tool_calls: s.tool_calls,
                    done: s.done,
                })
            }
            // 台本を使い切ったら「終わり」を返さない。書き忘れをテストの緑で隠さず、はっきり落とす（詳細§12）。
            None => panic!("ScriptedEngine: 台本が尽きた（想定外の infer 呼び出し）"),
        }
    }
}

/// テスト用のゲートツール実行。ツールごとに「即返す／指定時間眠って返す」を設定でき、
/// 呼ばれた回数を数える（勝手な再実行が無いことの確認に使う）。
#[derive(Clone)]
pub struct ScriptedToolHost {
    behaviors: Arc<Mutex<HashMap<String, ToolBehavior>>>,
    invokes: Arc<Mutex<Vec<String>>>,
    /// 呼ばれた各ツールの引数（名前 → 最後の args）。平文ツール行の引数導出（位置引数・1 行 JSON）が
    /// 線を渡って壊れないことの検査に使う。
    args: Arc<Mutex<HashMap<String, serde_json::Value>>>,
}

#[derive(Clone)]
enum ToolBehavior {
    Immediate(String),
    Sleep { dur: Duration, result: String },
}

impl Default for ScriptedToolHost {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptedToolHost {
    pub fn new() -> ScriptedToolHost {
        ScriptedToolHost {
            behaviors: Arc::new(Mutex::new(HashMap::new())),
            invokes: Arc::new(Mutex::new(vec![])),
            args: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    pub fn set_immediate(&self, name: &str, result: &str) {
        self.behaviors
            .lock()
            .unwrap()
            .insert(name.into(), ToolBehavior::Immediate(result.into()));
    }
    pub fn set_slow(&self, name: &str, dur: Duration, result: &str) {
        self.behaviors.lock().unwrap().insert(
            name.into(),
            ToolBehavior::Sleep {
                dur,
                result: result.into(),
            },
        );
    }
    pub fn invoke_count(&self, name: &str) -> usize {
        self.invokes
            .lock()
            .unwrap()
            .iter()
            .filter(|n| *n == name)
            .count()
    }
    /// そのツールが最後に呼ばれたときの引数（無ければ None）。平文ツール行の引数導出の検査用。
    pub fn last_args(&self, name: &str) -> Option<serde_json::Value> {
        self.args.lock().unwrap().get(name).cloned()
    }
}

#[async_trait::async_trait]
impl ToolHost for ScriptedToolHost {
    async fn invoke_route(
        &self,
        _route: &GateRoute,
        call: &ToolCallSpec,
    ) -> Result<String, ToolError> {
        self.invokes.lock().unwrap().push(call.name.clone());
        self.args
            .lock()
            .unwrap()
            .insert(call.name.clone(), call.args.clone());
        let beh = self.behaviors.lock().unwrap().get(&call.name).cloned();
        match beh {
            Some(ToolBehavior::Immediate(r)) => Ok(r),
            Some(ToolBehavior::Sleep { dur, result }) => {
                tokio::time::sleep(dur).await;
                Ok(result)
            }
            // 知らないツールは失敗させる。近いものに寄せない（詳細§15）。
            None => Err(ToolError(format!("unknown tool: {}", call.name))),
        }
    }
}

/// テスト用の shell 実行（本番の tokio::process の代わり）。呼ばれた `argv`・`cwd` を記録し、
/// 設定した結果（即返し／指定時間眠って返す／失敗）を返す。**argv を素通しで記録する**ので、
/// 直接 exec の検証（`; rm` 入り引数が 1 要素のまま渡ること）に使える。既定（未設定）は argv を
/// 空白で連結して返す（`echo` 相当の決定的な結果——切り離し→決着の連鎖の観測に十分）。
#[derive(Clone, Default)]
pub struct ScriptedShellHost {
    /// 呼ばれた各実行の argv（順番どおり）。直接 exec の検証に使う。
    runs: Arc<Mutex<Vec<Vec<String>>>>,
    /// 呼ばれた各実行の cwd（subject ごとの作業領域が渡ることの検証に使う）。
    cwds: Arc<Mutex<Vec<String>>>,
    behavior: Arc<Mutex<Option<ShellBehavior>>>,
}

#[derive(Clone)]
enum ShellBehavior {
    Immediate(String),
    Sleep { dur: Duration, result: String },
    Fail(String),
}

impl ScriptedShellHost {
    pub fn new() -> ScriptedShellHost {
        ScriptedShellHost::default()
    }
    /// 即返す結果を設定する。
    pub fn set_output(&self, result: &str) {
        *self.behavior.lock().unwrap() = Some(ShellBehavior::Immediate(result.into()));
    }
    /// 指定時間眠ってから返す（切り離し・上限・停止の検証に使う）。
    pub fn set_slow(&self, dur: Duration, result: &str) {
        *self.behavior.lock().unwrap() = Some(ShellBehavior::Sleep {
            dur,
            result: result.into(),
        });
    }
    /// 失敗を返す（非0終了に相当）。
    pub fn set_fail(&self, msg: &str) {
        *self.behavior.lock().unwrap() = Some(ShellBehavior::Fail(msg.into()));
    }
    /// 最後に実行された argv（無ければ None）。直接 exec の検証用。
    pub fn last_argv(&self) -> Option<Vec<String>> {
        self.runs.lock().unwrap().last().cloned()
    }
    /// 最後に実行された cwd（無ければ None）。
    pub fn last_cwd(&self) -> Option<String> {
        self.cwds.lock().unwrap().last().cloned()
    }
    /// 実行された回数（勝手な再実行が無いことの確認に使う）。
    pub fn run_count(&self) -> usize {
        self.runs.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl ShellHost for ScriptedShellHost {
    async fn run(&self, argv: &[String], cwd: &str) -> Result<String, ToolError> {
        self.runs.lock().unwrap().push(argv.to_vec());
        self.cwds.lock().unwrap().push(cwd.to_string());
        let beh = self.behavior.lock().unwrap().clone();
        match beh {
            Some(ShellBehavior::Immediate(r)) => Ok(r),
            Some(ShellBehavior::Sleep { dur, result }) => {
                tokio::time::sleep(dur).await;
                Ok(result)
            }
            Some(ShellBehavior::Fail(m)) => Err(ToolError(m)),
            // 既定: argv を空白で連結して返す（echo 相当・決定的）。
            None => Ok(argv.join(" ")),
        }
    }
}

/// テスト用の fetch（DESIGN-images §3 の seam の fake）。URL ごとに「(content-type, bytes) を返す」か
/// 「失敗する」を設定でき、fetch された URL を順に記録する（取得先・回数が観測できる——迂回が無いことの
/// 確認に使う）。未設定の URL は失敗（近いものへ寄せない）。
/// 設定した取得結果: Ok((content-type, bytes)) か Err(理由)。
type FakeResponse = Result<(Option<String>, Vec<u8>), String>;

#[derive(Clone, Default)]
pub struct FakeFetcher {
    responses: Arc<Mutex<HashMap<String, FakeResponse>>>,
    fetched: Arc<Mutex<Vec<String>>>,
}

impl FakeFetcher {
    pub fn new() -> FakeFetcher {
        FakeFetcher::default()
    }
    /// その URL の取得結果（content-type と実バイト）を設定する。
    pub fn set(&self, url: &str, content_type: Option<&str>, bytes: Vec<u8>) {
        self.responses.lock().unwrap().insert(
            url.to_string(),
            Ok((content_type.map(|s| s.to_string()), bytes)),
        );
    }
    /// その URL の取得を失敗させる（404・上限超過などを模す・fail loud の検証に使う）。
    pub fn set_fail(&self, url: &str, msg: &str) {
        self.responses
            .lock()
            .unwrap()
            .insert(url.to_string(), Err(msg.to_string()));
    }
    /// これまでに fetch された URL（順番どおり）。取得先・回数の観測用。
    pub fn fetched(&self) -> Vec<String> {
        self.fetched.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Fetcher for FakeFetcher {
    async fn fetch(&self, url: &str) -> Result<Fetched, FetchError> {
        self.fetched.lock().unwrap().push(url.to_string());
        let r = self.responses.lock().unwrap().get(url).cloned();
        match r {
            Some(Ok((content_type, bytes))) => Ok(Fetched {
                content_type,
                bytes,
            }),
            Some(Err(m)) => Err(FetchError(m)),
            None => Err(FetchError(format!("未設定の URL: {url}"))),
        }
    }
}

/// 通知を記録するだけの Notifier。本番の plugd の代わり。
#[derive(Clone, Default)]
pub struct RecordingNotifier {
    pub notices: Arc<Mutex<Vec<Notice>>>,
}

impl RecordingNotifier {
    pub fn new() -> RecordingNotifier {
        RecordingNotifier::default()
    }
    pub fn all(&self) -> Vec<Notice> {
        self.notices.lock().unwrap().clone()
    }
    pub fn count_progress(&self) -> usize {
        self.notices
            .lock()
            .unwrap()
            .iter()
            .filter(|n| matches!(n, Notice::ActivityProgress { .. }))
            .count()
    }
}

impl Notifier for RecordingNotifier {
    fn notify(&self, n: Notice) {
        self.notices.lock().unwrap().push(n);
    }
}

/// テスト用の文脈予算の物差し（1 文字 = 1 トークン）。本番の o200k とは違い、内部の
/// トークン値に結合しない安定した物差しなので、予算テストは実トークンではなくこれを使う——
/// 「予算 N で 1 件表示・M 件省略」といった閾値の意味が o200k の内部実装に左右されず保たれる。
#[derive(Clone, Copy, Debug, Default)]
pub struct CharCounter;

impl TokenCounter for CharCounter {
    fn count(&self, s: &str) -> usize {
        s.chars().count()
    }
}
