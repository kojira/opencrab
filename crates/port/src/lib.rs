//! port — 語彙の型と差し替え可能な seam。依存は tokio(time/sync) と serde_json のみ。
//!
//! 設計（詳細§01）: `port` は語彙の型・プロトコルの型を持ち、他クレートの都合を持たない。
//! 差し替え可能な `Engine`、および core の外界向き seam（`ToolHost`・`Notifier`）の
//! trait をここに置く。core はこれらの trait にだけ依存し、実装（本物の LLM / プラグイン配線）
//! を知らない。

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use tokio::time::Instant;

pub type PlaceId = i64;

/// ゲートの名前（プロトコル§01 `name`）。**文字列リテラルと比較できない型**にしてある。
///
/// これで守るのは詳細§02「core にゲートの名前を書かない」——`if gate == "mastodon"` は
/// `PartialEq<str>` を実装していないので**コンパイルできない**。既知の名前を並べた検査と違い、
/// まだ見ぬ 4 つ目・5 つ目のゲート名でも同じく止まる（検査ではなく型で落ちる）。
///
/// core は名前で振る舞いを変えず、名乗り（`GateSpec`）という値だけを読む。名前は
/// 登録簿の鍵・チャネルの列としてしか使わない。線（JSON）へ載せる／conn を引くために
/// `as_str()` を使うのは plugd の仕事で、そこはゲートを名前で分岐しているわけではない。
///
/// ```compile_fail
/// # use opencrab_port::GateName;
/// let gate = GateName::new("nostr");
/// // core にこう書けないことを型で保証する（まだ見ぬ 4 つ目の名前でも同じく止まる）。
/// if gate == "mastodon" {}
/// ```
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct GateName(String);

impl GateName {
    pub fn new(s: impl Into<String>) -> GateName {
        GateName(s.into())
    }
    /// 線・SQL へ渡すための借用。**比較のためではない**（比較は `GateName` 同士で行う）。
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn parse(s: impl Into<String>) -> Result<GateName, String> {
        let value = s.into();
        let mut chars = value.chars();
        let first = chars
            .next()
            .ok_or_else(|| "empty gate kind id".to_string())?;
        if !first.is_ascii_lowercase()
            || value.len() > 64
            || !chars
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
        {
            return Err(format!("invalid gate kind id: {value}"));
        }
        Ok(GateName(value))
    }
}

impl std::fmt::Display for GateName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Protocol family identifier. `GateName` remains as the protocol-1 facade name.
pub type GateKindId = GateName;

/// Credential/process instance identifier. The textual form is always canonical lowercase UUID.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct GateInstanceId(String);

impl GateInstanceId {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        let bytes = value.as_bytes();
        let valid = bytes.len() == 36
            && bytes.iter().enumerate().all(|(index, byte)| {
                if matches!(index, 8 | 13 | 18 | 23) {
                    *byte == b'-'
                } else {
                    byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)
                }
            });
        if !valid {
            return Err(format!("noncanonical gate instance id: {value}"));
        }
        Ok(Self(value))
    }

    pub fn from_canonical(value: String) -> Self {
        debug_assert!(Self::parse(value.clone()).is_ok());
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for GateInstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OriginScope {
    Instance,
    KindAddress,
}

impl OriginScope {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Instance => "instance",
            Self::KindAddress => "kind_address",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "instance" => Some(Self::Instance),
            "kind_address" => Some(Self::KindAddress),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngressDiscovery {
    Prebound,
    Membership,
}

impl IngressDiscovery {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Prebound => "prebound",
            Self::Membership => "membership",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "prebound" => Some(Self::Prebound),
            "membership" => Some(Self::Membership),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RoutePurpose(String);

impl RoutePurpose {
    pub fn inbound() -> Self {
        Self("inbound".into())
    }
    pub fn outbound() -> Self {
        Self("outbound".into())
    }
    pub fn timed() -> Self {
        Self("timed".into())
    }
    pub fn tool(name: &str) -> Result<Self, String> {
        if name.is_empty()
            || !name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
        {
            return Err(format!("invalid route tool name: {name}"));
        }
        Ok(Self(format!("tool:{name}")))
    }
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if matches!(value.as_str(), "inbound" | "outbound" | "timed") {
            return Ok(Self(value));
        }
        let name = value
            .strip_prefix("tool:")
            .ok_or_else(|| format!("invalid route purpose: {value}"))?;
        Self::tool(name)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Immutable route snapshot used by turn, delivery, operation, and activity seams.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateRoute {
    pub subject_id: SubjectId,
    pub place_id: PlaceId,
    pub kind_id: GateKindId,
    pub instance_id: GateInstanceId,
    pub binding_id: String,
    pub address: String,
    pub connection_epoch: u64,
    pub revision: u64,
    pub purpose: RoutePurpose,
}

pub type Seq = i64;
pub type SubjectId = i64;
pub type ActivityId = i64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Standing {
    Owner,
    Trusted,
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Participant,
    Observer,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubjectKind {
    Human,
    Agent,
}

/// 出来事の種別（閉じた列挙）。ゲートが勝手に増やせない（詳細§04 / 基本§05）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventKind {
    // 外から届くもの
    Said,
    Edited,
    Retracted,
    Reacted,
    UiAction,
    // 効果が確定してログに載るもの
    Spoke,
    Quoted,
    Boosted,
    ReactEffect,
    Amended,
    RetractEffect,
    UiPosted,
    ReadMarked,
    // 系の出来事
    Settled,
    Interrupted,
}

impl EventKind {
    /// 発火方針に照らされるのは発話だけ（詳細§13「再入」）。
    /// 反応・広める・UI・読んだ印・編集・取り消しは照らされない。
    /// Settled/Interrupted は for_subject で紐づく主体のターンを起こす。
    pub fn is_firing(self) -> bool {
        matches!(
            self,
            EventKind::Said | EventKind::Spoke | EventKind::Settled | EventKind::Interrupted
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Said => "said",
            EventKind::Edited => "edited",
            EventKind::Retracted => "retracted",
            EventKind::Reacted => "reacted",
            EventKind::UiAction => "ui_action",
            EventKind::Spoke => "spoke",
            EventKind::Quoted => "quoted",
            EventKind::Boosted => "boosted",
            EventKind::ReactEffect => "react",
            EventKind::Amended => "amended",
            EventKind::RetractEffect => "retract",
            EventKind::UiPosted => "ui",
            EventKind::ReadMarked => "read_mark",
            EventKind::Settled => "settled",
            EventKind::Interrupted => "interrupted",
        }
    }

    pub fn from_wire(s: &str) -> Option<EventKind> {
        Some(match s {
            "said" => EventKind::Said,
            "edited" => EventKind::Edited,
            "retracted" => EventKind::Retracted,
            "reacted" => EventKind::Reacted,
            "ui_action" => EventKind::UiAction,
            "spoke" => EventKind::Spoke,
            "quoted" => EventKind::Quoted,
            "boosted" => EventKind::Boosted,
            "react" => EventKind::ReactEffect,
            "amended" => EventKind::Amended,
            "retract" => EventKind::RetractEffect,
            "ui" => EventKind::UiPosted,
            "read_mark" => EventKind::ReadMarked,
            "settled" => EventKind::Settled,
            "interrupted" => EventKind::Interrupted,
            _ => return None,
        })
    }
}

/// 効果の種別（閉じた列挙・詳細§08）。既定実装を置かない。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum EffectKind {
    Say,
    Quote,
    Boost,
    React,
    Amend,
    Retract,
    Ui,
    ReadMark,
}

impl EffectKind {
    /// プロトコルの線上の名前（プラグインの名乗り `effects` / 効果の `kind`・プロトコル§01/§04）。
    pub fn as_wire(self) -> &'static str {
        match self {
            EffectKind::Say => "say",
            EffectKind::Quote => "quote",
            EffectKind::Boost => "boost",
            EffectKind::React => "react",
            EffectKind::Amend => "amend",
            EffectKind::Retract => "retract",
            EffectKind::Ui => "ui",
            EffectKind::ReadMark => "read_mark",
        }
    }

    /// 線上の名前から。知らない値は None（呼び手が `unknown_enum` を返す・プロトコル§00）。
    /// 近いものへ寄せない・既定へ倒さない。
    pub fn from_wire(s: &str) -> Option<EffectKind> {
        Some(match s {
            "say" => EffectKind::Say,
            "quote" => EffectKind::Quote,
            "boost" => EffectKind::Boost,
            "react" => EffectKind::React,
            "amend" => EffectKind::Amend,
            "retract" => EffectKind::Retract,
            "ui" => EffectKind::Ui,
            "read_mark" => EffectKind::ReadMark,
            _ => return None,
        })
    }

    /// 効果が確定したとき、ログに載る出来事の種別。
    pub fn logged_as(self) -> EventKind {
        match self {
            EffectKind::Say => EventKind::Spoke,
            EffectKind::Quote => EventKind::Quoted,
            EffectKind::Boost => EventKind::Boosted,
            EffectKind::React => EventKind::ReactEffect,
            EffectKind::Amend => EventKind::Amended,
            EffectKind::Retract => EventKind::RetractEffect,
            EffectKind::Ui => EventKind::UiPosted,
            EffectKind::ReadMark => EventKind::ReadMarked,
        }
    }
}

/// 出来事の性質（閉じた列挙・詳細§04）。core が算出する。プラグインは送らない。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum Property {
    MentionsMe,
    RepliesToMe,
    Direct,
}

/// 添付の種別（DESIGN-images §1）。今は画像だけ。動画・音声は来たら足す（core 側だけの変更で済む）。
/// 閉じた列挙——ゲートが勝手に増やせない（未知値は plugd が `unknown_enum` で弾く・§00 の流儀）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttachmentKind {
    Image,
}

impl AttachmentKind {
    pub fn as_wire(self) -> &'static str {
        match self {
            AttachmentKind::Image => "image",
        }
    }
    /// 知らない値は None（呼び手が `unknown_enum` を返す・§00）。近いものへ寄せない。
    pub fn from_wire(s: &str) -> Option<AttachmentKind> {
        match s {
            "image" => Some(AttachmentKind::Image),
            _ => None,
        }
    }
}

/// 出来事に付いた添付（DESIGN-images §1）。ゲートは**拾えたものを全部**そのまま載せる（正規化・
/// 選別・上限はゲートに置かない）。記録するのは URL（参照）だけ——中身は保存しない（§1）。
///
/// `origin_author` は**由来作者**の外界識別子（§5「リポストの罠」）: その URL がどの作者の本文に
/// 由来するか。Nostr のリポスト（kind 6）・引用なら内側の元イベントの作者。信頼はリポストを経由して
/// 継承しないので、core はこの由来作者を信頼リストと突き合わせる。構造的に取れない（生 URL 等）なら
/// `None`——core は**信頼できない扱い**にする（安全側・フォールバックで通さない・§5）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    pub kind: AttachmentKind,
    pub url: String,
    pub origin_author: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Content {
    pub text: Option<String>,
    pub symbol: Option<String>,
}

impl Content {
    pub fn text(s: impl Into<String>) -> Content {
        Content {
            text: Some(s.into()),
            symbol: None,
        }
    }
}

/// engine が出す効果の指定。宛先の場は None なら「いまの場」。
#[derive(Clone, Debug)]
pub struct EffectSpec {
    pub kind: EffectKind,
    pub place: Option<PlaceId>,
    pub target: Option<Seq>,
    pub content: Content,
    pub mentions: Vec<SubjectId>,
    /// 平文アクションを生んだ verb（あれば・平文アクション文法）。core は不透明に扱い、`confirm` が
    /// `OutgoingEffect.verb` へ素通しする。engine が直接出す効果・散文 say では None。
    pub verb: Option<String>,
}

impl EffectSpec {
    pub fn say(text: impl Into<String>) -> EffectSpec {
        EffectSpec {
            kind: EffectKind::Say,
            place: None,
            target: None,
            content: Content::text(text),
            mentions: vec![],
            verb: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolCallSpec {
    /// プロバイダが振った呼び出しの識別子（Anthropic の `tool_use.id`）。結果ブロックを対応づける。
    /// 差し替え engine は合成の id を振る。core は権限判定にだけ使う場面では空でよい。
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

/// ターンの中の会話の話者（§05）。ターン内だけ生き、終われば捨てる（場のログとは別）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsgRole {
    User,
    Assistant,
}

/// tool_result の中身の 1 かけら（DESIGN-images §4「マルチパートの口」）。テキストと画像バイトを
/// **混ぜずに**別ブロックで持つ——core-look が fetch した画像はここへ `ImageBytes` として入り、
/// provider が自分の wire（Anthropic=image ブロック base64・OpenAI=image_url の data URI）へ写す。
/// URL はここに載らない（core が実バイトを持ち、provider に URL を渡さない・§3/§4）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    Text(String),
    /// fetch 済みの画像の**実バイト**（core-look）。`media_type` はラスタの MIME（`image/png` 等・
    /// 実バイト検査で確定したもの・§3）。base64 化は provider が wire へ写すときに 1 度だけ行う。
    ImageBytes {
        media_type: String,
        data: Vec<u8>,
    },
}

impl Part {
    pub fn text(s: impl Into<String>) -> Part {
        Part::Text(s.into())
    }
}

/// 会話の 1 ブロック。プロバイダの形（tool_use / tool_result）で持つ——テキストに混ぜない（§05）。
#[derive(Clone, Debug)]
pub enum Block {
    Text(String),
    /// 道具の呼び出し（assistant 側）。
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// 道具の結果（user 側）。`tool_use_id` で呼び出しと対にする（並行でも対応が付く・§05）。
    /// 中身は**マルチパート**（Text / ImageBytes・DESIGN-images §4）——大半は Text 1 つだが、
    /// core-look の成功は枠書きの Text と画像の ImageBytes が並ぶ。
    ToolResult {
        tool_use_id: String,
        content: Vec<Part>,
        is_error: bool,
    },
}

/// ターン内会話の 1 メッセージ。
#[derive(Clone, Debug)]
pub struct Message {
    pub role: MsgRole,
    pub content: Vec<Block>,
}

/// 推論努力のヒント（返答の絞り・DESIGN-attention §2）。高消費の着火作者への返答ターンで
/// core が下げる。**engine が対応していれば** reasoning effort に写す——長考の資源は出力上限では
/// 絞れないので、これが本命。対応しない engine は無視してよい（落とせる通知と同じ型）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
}

/// このターンの返答の絞り（DESIGN-attention §2）。閾値超えの着火作者の着火ターンでだけ core が
/// 組んで `Context.throttle` に載せる。`None` のターンは絞らない（既定の生成）。**絞られたターンでも
/// 応答自体はする**（無視ではない。無視は元栓の層）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Throttle {
    /// 出力トークン上限（この値まで下げる）。対応する provider は wire の max_tokens へ写す。
    pub max_output_tokens: Option<usize>,
    /// 推論努力のヒント（下げる）。engine が対応していれば reasoning effort に写す。
    pub effort: Effort,
}

/// engine に渡す文脈。`rendered` が「モデルの目に入る」**最初の**テキスト（場のログから 1 度だけ組む）。
/// ターンの中は組み直さず `history` に**積む**（§05）——道具の往復は tool_use/tool_result で対にして入る。
/// 記録用に、実際に入った範囲・切り詰めた範囲・引き継いだ範囲を持つ。
#[derive(Clone, Debug, Default)]
pub struct Context {
    pub place: PlaceId,
    pub subject: SubjectId,
    /// システムプロンプト（人格＋場の枠づけ＋文法前文＋メニュー）。**core が組み、provider は自分の
    /// wire の system スロットへ載せるだけ**（Anthropic=top-level system・OpenAI chat=先頭 role=system・
    /// Responses=instructions）。ターン跨ぎで安定なので、キャッシュ prefix になる（末尾に cache breakpoint）。
    /// Agent 主体のターンでだけ組む（それ以外は空）。空 persona の Agent ターンは core が fail loud にする。
    pub system: String,
    pub rendered: String,
    /// ターンの中で積み上がった会話（最初の user メッセージ＝`rendered` の後に続く分）。
    /// 反復ごとに増えた分だけ足す。ターンが終われば捨てる——場のログへは持ち越さない（§05）。
    pub history: Vec<Message>,
    pub ctx_from_seq: Option<Seq>,
    pub ctx_to_seq: Option<Seq>,
    pub skipped_from_seq: Option<Seq>,
    pub skipped_to_seq: Option<Seq>,
    pub inherit_from_seq: Option<Seq>,
    pub inherit_to_seq: Option<Seq>,
    /// 今回の文脈で新しく読み位置が進む先（=ctx_to_seq）。
    pub newly_read_to: Option<Seq>,
    /// この主体がこの場で使える道具（§09/§10）。core が `check` を通ったものだけを載せる。
    /// 本物のプロバイダはこれを API の道具宣言に写す — 宣言していない道具をモデルは呼べない。
    /// テストの差し替え engine は無視してよい（台本で直接 tool_call を出す）。
    pub tools: Vec<ToolDef>,
    /// このターンの返答の絞り（DESIGN-attention §2）。高消費の着火作者への返答でだけ `Some`。
    /// `None` なら絞らない（既定の生成）。provider は max_tokens / reasoning effort に写す。
    pub throttle: Option<Throttle>,
}

#[derive(Default)]
pub struct InferOutput {
    pub effects: Vec<EffectSpec>,
    pub tool_calls: Vec<ToolCallSpec>,
    pub done: bool,
}

impl InferOutput {
    /// 正規化後の推論出力に、ターンを進める意味のある内容が一つも無いか。
    ///
    /// `done` は provider の終端判断であって内容ではないため参照しない。tool call と Say 以外の効果は
    /// それ自体が有効な一手であり、Say は非空白の本文がある場合だけ内容として数える。
    pub fn is_semantically_empty(&self) -> bool {
        if !self.tool_calls.is_empty() {
            return false;
        }
        self.effects.iter().all(|effect| {
            effect.kind == EffectKind::Say
                && effect
                    .content
                    .text
                    .as_deref()
                    .is_none_or(|text| text.trim().is_empty())
        })
    }
}

#[derive(Debug)]
pub struct EngineError(pub String);

/// 推論の断片が届いたことを外へ伝える口（詳細§05）。
/// 断片が届くたびに `chunk()` を呼ぶ。core はチャンク間のアイドルで上限を掛ける（総時間ではない）。
/// これが無い（1 往復で結果だけ返す）口だと、アイドル上限が総時間上限に化け、長い正当な生成が殺される。
pub struct ChunkSink(tokio::sync::mpsc::UnboundedSender<()>);

impl ChunkSink {
    /// core が用意する。送信側（engine へ渡す）と受信側（アイドルの取り直しに使う）を返す。
    pub fn channel() -> (ChunkSink, tokio::sync::mpsc::UnboundedReceiver<()>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (ChunkSink(tx), rx)
    }
    /// 断片が届いた。core 側のアイドル計測が取り直される。
    pub fn chunk(&self) {
        let _ = self.0.send(());
    }
}

/// 差し替え可能な推論。テストは決まった応答を返す実装を使う（詳細§01）。
///
/// `infer` は結果を返す前に、断片が届くたび `chunks.chunk()` を呼ぶ。
/// 「まだ届いている」が外から見えるので、バイトが流れている限り長い生成は切られない（詳細§05）。
/// テスト用の実装も同じ口を持つ — 止まった推論を作れないと、この上限は検証できない。
#[async_trait::async_trait]
pub trait Engine: Send + Sync {
    async fn infer(&self, ctx: &Context, chunks: &ChunkSink) -> Result<InferOutput, EngineError>;

    /// 予算の物差しにする**実効モデル名**（§06）。会話予算 = この model の `context_window`
    /// （store 登録）× `compaction_ratio`。**既定を置かない**——各 engine が自分の model を名乗る。
    /// 近いものへ寄せる隙を作らないため（§15）。未登録モデルを名乗れば起動時に fail loud する。
    ///
    /// per-engine で解決する（`emits_tool_calls` と同じ流儀）。core は起動時にこの値で予算を確定する。
    fn model(&self) -> &str;

    /// この engine がネイティブな道具呼び出し（`InferOutput.tool_calls`）を出せるか（平文ツール行の設計）。
    /// 既定は `true`（本物のプロバイダ）——道具を `Context.tools` として受け取り `tool_calls` で呼ぶので、
    /// core は本文にツールメニューを描かない。
    ///
    /// `false` を返す engine（ネイティブ道具を持たない・平文専用）には、core が本文へツールメニューを
    /// 描き、`Context.tools` を空にする（宣言しても呼べないものを渡さない）。どちらの engine でも、
    /// モデルが本文に書いた**平文ツール行**は core が解釈して実行する（発話とは別経路）。
    ///
    /// per-engine で解決する（プロセス全体のフラグにしない）。core は文脈を組む時にこの値を読む。
    fn emits_tool_calls(&self) -> bool {
        true
    }

    /// この engine が **画像を tool_result で受け取れるか**（DESIGN-images §6）。既定 `true`
    /// （本物のプロバイダ Anthropic / OpenAI は image ブロックを受ける）。`false`（CursorEngine 等、
    /// CLI が画像を受けない）なら core は `core-look` を**メニューに出さない**——「宣言しても呼べない
    /// ものを渡さない」の既存原則（`emits_tool_calls` と同型）。per-engine で解決する。
    fn accepts_images(&self) -> bool {
        true
    }
}

#[derive(Debug)]
pub struct ToolError(pub String);

/// ゲート種別のツールを実行する seam（本番では plugd が実装）。
/// core は core ツール以外の呼び出しをここへ渡す。今回の範囲ではテスト用の実装だけ。
#[async_trait::async_trait]
pub trait ToolHost: Send + Sync {
    async fn invoke_route(
        &self,
        route: &GateRoute,
        call: &ToolCallSpec,
    ) -> Result<String, ToolError>;
}

/// shell（core builtin `core-shell`）を実行する seam。テストでは fake、実機では tokio::process。
///
/// 設計（DESIGN-shell.md）: コマンド体系をラップしない（モデルが知っている sh の世界をそのまま
/// 使わせる）。core は `argv` を**構造化して**渡し、シェル文字列を組まない——実装は `argv[0]` を
/// 実行ファイル、残りを引数として**直接 exec**する（`sh -c` を経由しない＝注入不可）。パイプ等は
/// エージェントが明示的に `argv=["sh","-c",...]` と書き、それも 1 コマンドとして allowlist 判定される。
///
/// `cwd` は subject ごとの作業領域（core が主体から決める相対トークン・実装がその基準へ根づける）。
/// core はこの seam の背後を知らない——切り離し・退避・停止・上限はすべて既存の背景の機構が担う。
#[async_trait::async_trait]
pub trait ShellHost: Send + Sync {
    async fn run(&self, argv: &[String], cwd: &str) -> Result<String, ToolError>;
}

/// URL の中身を取得する seam（DESIGN-images §3「fetch は seam」）。本番は reqwest、テストは fake。
///
/// **look（画像）と read（本文）で 1 本**——どちらも「URL を引いて (content-type, bytes) を得る」だけを
/// 担う。形式の判定（look の実バイト検査・read の HTML 本文抽出）は core が行い、この seam には置かない
/// （並行実装を作らない）。**取得は core が行う**（プロバイダに URL を渡さない・§3）ので、取得先と回数が
/// すべて系内の記録に残り、プロバイダ側 fetch への迂回が構造的に無い。実装は自分でタイムアウト・サイズ上限を
/// 掛けてよい（core は結果だけ見る）。
#[async_trait::async_trait]
pub trait Fetcher: Send + Sync {
    async fn fetch(&self, url: &str) -> Result<Fetched, FetchError>;
}

/// `Fetcher::fetch` が返す中身。`content_type` は生ヘッダ（`image/png; charset=...` のようにパラメタ付き
/// のこともある——core が主要部だけ見る）。`bytes` は本体そのもの（core が実バイトを検査する）。
#[derive(Clone, Debug)]
pub struct Fetched {
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

/// 取得の失敗（DESIGN-images §3「失敗は fail loud」）。理由をそのまま tool_result に返す（黙って省略しない）。
#[derive(Debug)]
pub struct FetchError(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityKindTag {
    Turn,
    Background,
}

/// core が外界へ出す通知（活動・効果の配送依頼）。本番では plugd が受けてチャネルへ運ぶ。
#[derive(Clone, Debug)]
pub enum Notice {
    RoutedActivityStarted {
        route: GateRoute,
        activity: ActivityId,
        kind: ActivityKindTag,
        label: Option<String>,
    },
    RoutedActivityProgress {
        route: GateRoute,
        activity: ActivityId,
        label: String,
    },
    RoutedActivityEnded {
        route: GateRoute,
        activity: ActivityId,
    },
    ActivityStarted {
        place: PlaceId,
        activity: ActivityId,
        kind: ActivityKindTag,
        label: Option<String>,
    },
    ActivityProgress {
        place: PlaceId,
        activity: ActivityId,
        label: String,
    },
    ActivityEnded {
        place: PlaceId,
        activity: ActivityId,
    },
    /// 確定した効果の配送依頼。チャネルを持たない場では運び先が無い。
    Effect {
        place: PlaceId,
        seq: Seq,
        kind: EffectKind,
    },
}

pub trait Notifier: Send + Sync {
    fn notify(&self, n: Notice);
}

/// 文脈予算を数える物差しの seam。core は会話予算・記憶索引予算・文脈の観測（§06/§10）を
/// **すべてこの 1 本**で数える。片方を文字、片方をトークンで測ると同じ長さでも中身（日本語 /
/// 英数字 / base64）で実効量が数倍ぶれ、「予算内のはずが溢れる／まだ余裕があるのに切る」が
/// 起きる。予算は soft limit（超えたら「省略」と申告する）なので、**近似トークンで十分**。
///
/// 本番は o200k 見積り（`opencrab_social_runtime::O200kCounter`）を差す。厳密なプロバイダ別トークナイザには
/// **依存しない**が、単一の見積りトークナイザには依存する——理由は opencrab 本体（トークン単位・
/// o200k）への還流を綺麗にし、会話と記憶索引を同じ物差しで測るため。別プロバイダの正確な
/// トークナイザが要る日が来たら、この実装を差し替えるだけで済む（core の会計点には触らない）。
///
/// **数える以外の出口を持たない**（`Result` で逃がさない）。数えられないときに文字数へ黙って
/// 戻す等のフォールバックは作らない——本番実装はロード失敗を起動時に fail loud させる。
pub trait TokenCounter: Send + Sync {
    /// `s` の（近似）トークン数。
    fn count(&self, s: &str) -> usize;

    /// `s` の（近似）トークン数が `limit` **以上**か。既定は `count` を使う安全な実装
    /// （`self.count(s) >= limit`）なので、素朴な counter（文字数など）は無改修で意味が保たれる。
    ///
    /// **巨大入力で `count` が重い実装（tiktoken の BPE は 1 pre-token に対し最悪 O(バイト²)・
    /// 486MB 級で GB 確保→OOM）は、これを override して全体をトークナイズせず窓ごとに数え、上限
    /// 到達で即 return する**（判定にしか使わない場面で「全量を数える」を避ける）。override は
    /// **false negative を出さないこと**——真に `limit` 以上の入力を「未満」と取りこぼさない
    /// （窓境界で累計が本来のトークン数を上回るのは可、下回るのは不可）。
    fn count_reaches(&self, s: &str, limit: usize) -> bool {
        self.count(s) >= limit
    }
}

/// 活動の実時間の上限。`Default` を実装しない — 上限の無い活動を作れない（詳細§02-4）。
#[derive(Clone, Copy, Debug)]
pub struct Deadline(pub Instant);

// ---- ゲート（プラグイン）との seam ----
//
// プロトコル（版 1）の型を語彙として持つ。core はこれらの「値」を読むだけで、
// ゲートの名前で振る舞いを変えない（詳細§02「ゲートの違いは、値で渡す」）。
// 線（JSON）の読み書きは plugd が担い、core はここの型でだけ受け渡す。

/// 効果以外にできること（プロトコル§01 `capabilities`）。版 1 では `open` のみ。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum Capability {
    Open,
}

impl Capability {
    pub fn as_wire(self) -> &'static str {
        match self {
            Capability::Open => "open",
        }
    }
    /// 知らない値は None（呼び手が `unknown_enum` を返す・プロトコル§00）。
    pub fn from_wire(s: &str) -> Option<Capability> {
        match s {
            "open" => Some(Capability::Open),
            _ => None,
        }
    }
}

/// ゲートが名乗るツールの定義（プロトコル§01 `tools`）。`params` は JSON Schema（不透明に保持）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// 引数の形を表す JSON Schema。ネイティブなプロバイダはこれを `input_schema` に写す。**平文ツール行
    /// では core がここを軽く読んで content から引数を導出する**が、検証に使うのは `required`（存在検査）と
    /// `enum`（会員判定）だけ——他のキーワード（`pattern`・`minLength`・`format` 等）は**無視する。当てに
    /// しないこと**（`ActionDef.params` と同じ流儀・protocol §01「検証で見るのはこのキーワードだけ」）。
    /// 位置引数（1 行の content）で束ねられるのは `required` がちょうど 1 つの string のときだけ。
    pub params: serde_json::Value,
}

/// ゲートが名乗る平文アクションの定義（hello の加算・平文アクション文法）。
///
/// 平文アクション文法は、本文の各行 `verb:seq:content` を、その場のメニューにある verb だけ
/// アクションに解釈する（それ以外は地の文＝残余 say）。この宣言はそのメニューの 1 項目。
///
/// core が**意味的に読むのは `kind` だけ**（効果配送の経路を決める）。`verb`（＝`name`）は core に
/// とって不透明で、等値比較にしか使わない——名前で分岐しない（`GateName` と同じ思想）。`OutgoingEffect`
/// に素通しして、ゲートが zap→kind-9735 のように出し分ける材料にする。`params` は content の型
/// （enum／自由文 等）を表す JSON Schema で、**パース検証にだけ**使う（違反した行は成立させない）。
///
/// アクションは `tools` とは**別リスト**（効果配送の経路であって ToolHost 実行ではない）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionDef {
    pub name: String,
    pub description: String,
    /// content（1 行の内容枠）の型を表す JSON Schema（不透明に保持）。**v1 で検証に使うのは `enum`
    /// だけ**（会員判定。宣言が無ければどんな文字列でも素通し）——他のキーワードは検証に使わず説明
    /// として扱う（当てにしないこと）。core は content の文法・妥当性（絵文字らしさ・字数等）を判定しない。
    pub params: serde_json::Value,
    /// この verb が生む効果の種別。core が意味的に読む唯一の欄（配送経路を決める）。
    pub kind: EffectKind,
}

/// ゲートの名乗り（プロトコル§01）そのもの。これが core にとってのそのゲートの全部（詳細§02）。
/// core はこの値を機械へ差し込むだけで、`if gate == "..."` を書かない。
#[derive(Clone, Debug)]
pub struct GateSpec {
    pub name: GateName,
    pub protocol: u32,
    /// 住所の書式（RE2 相当・文字列全体一致）。plugd が構文検証済みの正規表現ソース。
    pub address_form: String,
    pub tools: Vec<ToolDef>,
    /// 運べる効果の種別（§01/§04 の閉じた列挙から）。
    pub effects: BTreeSet<EffectKind>,
    pub capabilities: BTreeSet<Capability>,
    /// 名乗る平文アクション（hello の加算・平文アクション文法）。省略時 []（既存ゲートは無改変）。
    /// protocol の版は上げない——古いゲートは actions=[] のまま従来どおり動く。
    pub actions: Vec<ActionDef>,
}

/// Protocol-2 kind declaration. Multiple active instances of one kind must compare equal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateKindSpec {
    pub kind_id: GateKindId,
    pub origin_scope: OriginScope,
    pub address_form: String,
    pub ingress_discovery: IngressDiscovery,
    pub tools: Vec<ToolDef>,
    pub effects: BTreeSet<EffectKind>,
    pub capabilities: BTreeSet<Capability>,
    pub actions: Vec<ActionDef>,
}

impl GateSpec {
    pub fn compatibility_kind_spec(&self) -> GateKindSpec {
        GateKindSpec {
            kind_id: self.name.clone(),
            origin_scope: OriginScope::Instance,
            address_form: self.address_form.clone(),
            ingress_discovery: IngressDiscovery::Prebound,
            tools: self.tools.clone(),
            effects: self.effects.clone(),
            capabilities: self.capabilities.clone(),
            actions: self.actions.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateConnection {
    pub instance_id: GateInstanceId,
    pub revision: u64,
    pub connection_epoch: u64,
    pub spec: GateKindSpec,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressKind {
    Guild,
    Dm,
    Thread,
}

impl AddressKind {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Guild => "guild",
            Self::Dm => "dm",
            Self::Thread => "thread",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MembershipDiscovery {
    pub address_kind: AddressKind,
    pub guild_id: Option<String>,
    pub label: Option<String>,
}

/// 外界へ運ぶ効果の中身（core → plugin・プロトコル§04）。
/// core が確定済みの効果とその宛先（外界の識別子）から組み立て、plugd が線へ載せる。
#[derive(Clone, Debug)]
pub struct OutgoingEffect {
    pub kind: EffectKind,
    pub text: Option<String>,
    pub symbol: Option<String>,
    /// 宛先の外界識別子（返信・反応・引用・取り消しの `target`／`say` の返信先）。
    pub target_origin: Option<String>,
    /// 平文アクションを生んだ verb（あれば・平文アクション文法）。core にとって不透明で、ここは
    /// 素通し——ゲートが同じ kind の中で出し分ける材料にする（zap→kind-9735 等）。散文 say や
    /// 従来の効果では None。
    pub verb: Option<String>,
}

/// 効果の ack（プロトコル§04）。`origin` は外界に新しく作られたものの識別子（§08）。
#[derive(Clone, Debug)]
pub struct DeliveryAck {
    pub delivered: bool,
    pub origin: Option<String>,
}

#[derive(Debug)]
pub struct TransportError(pub String);

/// Result of one canonical routed delivery, classified at the external-acceptance boundary.
pub type LateDeliveryObservation = Pin<Box<dyn Future<Output = Option<Vec<u8>>> + Send + 'static>>;

pub enum TransportDeliveryResult {
    /// The request was rejected before it could reach the external API.
    DefiniteFailure(TransportError),
    /// The gate definitely acknowledged the request.
    DefiniteAck(DeliveryAck),
    /// The request crossed the acceptance boundary but its outcome could not be observed.
    Indeterminate {
        error: TransportError,
        late_observation: Option<LateDeliveryObservation>,
    },
}

/// core → plugin の要求（応答を伴うもの）を運ぶ seam。本番では plugd が実装する。
///
/// 活動（通知・応答なし）は `Notifier` が、ゲートのツール実行は `ToolHost` が担う。
/// ここは「住所を結ぶ／解く」「外に容れ物を作る」「効果を運ぶ」——応答（ack）が要るもの。
/// core はこの trait にだけ依存し、plugd を知らない（詳細§01）。
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    /// Explicit protocol-1 compatibility boundary. Canonical callers use the route methods.
    async fn compat_bind(&self, gate: &GateName, address: &str) -> Result<(), TransportError>;
    async fn compat_unbind(&self, gate: &GateName, address: &str) -> Result<(), TransportError>;
    /// 外に容れ物を作る（§02 `open`）。返ってきた住所に core が新しい場を結ぶ。
    async fn compat_open(
        &self,
        gate: &GateName,
        under: &str,
        hint: Option<&str>,
    ) -> Result<String, TransportError>;
    /// 確定済みの効果を、あるチャネル（gate+address）へ運ぶ（§04）。ack で origin を返し得る。
    async fn compat_deliver_effect(
        &self,
        gate: &GateName,
        address: &str,
        effect: OutgoingEffect,
    ) -> Result<DeliveryAck, TransportError>;

    async fn bind_route(&self, route: &GateRoute) -> Result<(), TransportError>;

    async fn unbind_route(&self, route: &GateRoute) -> Result<(), TransportError>;

    async fn deliver_effect_route(
        &self,
        route: &GateRoute,
        seq: Seq,
        effect: OutgoingEffect,
    ) -> TransportDeliveryResult;
}

/// プラグインから届いた出来事（プロトコル§03）。外界の識別子は文字列のまま core へ渡し、
/// 名寄せ・返信先の解決・external_refs の記録は core が行う（詳細§03/§04）。
/// plugd は線（JSON）をこの値へ写すだけで、判断をしない。
#[derive(Clone, Debug)]
pub struct GateEvent {
    pub kind: EventKind,
    pub address: String,
    /// 著者のそのゲートでの識別子（`author.id`）。
    pub author_external: String,
    pub author_display: Option<String>,
    pub content: Content,
    /// 言及された相手の識別子（そのゲートでの・外界の文字列）。
    pub mentions: Vec<String>,
    /// 返信先の外界識別子（`reply_to`）。
    pub reply_to: Option<String>,
    /// 対象の外界識別子（`target`・said 以外で必須）。
    pub target: Option<String>,
    /// この出来事の外界識別子（`origin`）。無ければ後から反応・返信できない（§03）。
    pub origin: Option<String>,
    /// この出来事に付いた添付（DESIGN-images §1）。ゲートが拾えたものを全部そのまま載せる（判断しない）。
    /// 既定 `[]`——添付を送らない既存ゲートは従来どおり（後方互換）。
    pub attachments: Vec<Attachment>,
    /// Membership-driven discovery metadata. Protocol-1 prebound gates always use `None`.
    pub discovery: Option<MembershipDiscovery>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn say_with(text: Option<&str>) -> EffectSpec {
        EffectSpec {
            kind: EffectKind::Say,
            place: None,
            target: None,
            content: Content {
                text: text.map(str::to_string),
                symbol: None,
            },
            mentions: vec![],
            verb: None,
        }
    }

    #[test]
    fn infer_output_treats_missing_empty_and_whitespace_say_as_empty() {
        for (label, effect) in [
            ("missing", say_with(None)),
            ("empty", say_with(Some(""))),
            ("whitespace", say_with(Some("  \n\t"))),
        ] {
            for done in [false, true] {
                let output = InferOutput {
                    effects: vec![effect.clone()],
                    tool_calls: vec![],
                    done,
                };
                assert!(
                    output.is_semantically_empty(),
                    "{label} Say must be empty regardless of done={done}"
                );
            }
        }
    }

    #[test]
    fn infer_output_without_any_effect_or_tool_call_is_empty() {
        assert!(InferOutput::default().is_semantically_empty());
    }

    #[test]
    fn infer_output_with_tool_call_is_not_empty() {
        let output = InferOutput {
            effects: vec![say_with(Some(" \n"))],
            tool_calls: vec![ToolCallSpec {
                id: "call-1".into(),
                name: "tool".into(),
                args: serde_json::json!({}),
            }],
            done: true,
        };
        assert!(!output.is_semantically_empty());
    }

    #[test]
    fn infer_output_with_non_whitespace_say_or_other_effect_is_not_empty() {
        let text = InferOutput {
            effects: vec![say_with(Some(" answer "))],
            tool_calls: vec![],
            done: true,
        };
        assert!(!text.is_semantically_empty());

        let mut react = say_with(None);
        react.kind = EffectKind::React;
        let effect = InferOutput {
            effects: vec![react],
            tool_calls: vec![],
            done: false,
        };
        assert!(!effect.is_semantically_empty());
    }
}
