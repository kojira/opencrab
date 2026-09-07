use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serenity::all::{CreateActionRow, CreateModal};

use anyhow::{Context as AnyhowContext, Result};
use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use serenity::all::{
    ChannelId, Client, Context, CreateInteractionResponse, CreateInteractionResponseMessage,
    EventHandler, GatewayIntents, Interaction, Message as SerenityMessage, MessageId, ReactionType,
    Ready,
};
use serenity::http::Http;

use opencrab_gateway::{Channel, IncomingMessage, MessageSource, Sender};

/// A2UI Form 用モーダル応答（ボタン押下時に `CreateInteractionResponse::Modal` で返す）。
#[derive(Clone)]
pub struct A2uiFormModalSpec {
    pub modal_custom_id: String,
    pub title: String,
    pub components: Vec<CreateActionRow>,
}

/// `custom_id` とクリックユーザー ID を受け取り、モーダルを出すべきときだけ `Some` を返す。
pub type A2uiFormModalResolver = Arc<dyn Fn(&str, &str) -> Option<A2uiFormModalSpec> + Send + Sync>;

/// Discordのコンポーネントインタラクション（ボタンクリック等）のデータ。
///
/// serenityのInteraction型からplatform-agnosticなデータに変換したもの。
/// discord crateのイベントループで実際の処理を行う。
#[derive(Debug, Clone)]
pub struct ComponentInteractionData {
    /// custom_id (例: "interaction:{uuid}:{action_name}")
    pub custom_id: String,
    /// ボタンをクリックしたユーザーのID
    pub user_id: String,
    /// ボタンをクリックしたユーザー名
    pub user_name: String,
    /// チャンネルID
    pub channel_id: String,
    /// ギルドID (DMの場合は空)
    pub guild_id: String,
    /// メッセージID
    pub message_id: String,
    /// セレクトメニューの選択値（StringSelect時のみ）
    pub selected_values: Option<Vec<String>>,
    /// モーダルSubmitのフィールド値（custom_id → value）
    pub modal_values: Option<Vec<(String, String)>>,
    /// インタラクション種別
    pub interaction_kind: InteractionKind,
}

/// コンポーネントインタラクションの種別
#[derive(Debug, Clone, PartialEq)]
pub enum InteractionKind {
    Button,
    SelectMenu,
    ModalSubmit,
}

/// Discordゲートウェイ
///
/// serenityクレートを使用してDiscord Botとしてメッセージの送受信を行う。
/// Cargo feature `discord` を有効にすることで利用可能になる。
///
/// # プラグイン分離
///
/// このモジュールは `#[cfg(feature = "discord")]` で条件付きコンパイルされる。
/// `discord` featureを有効にしない限り、serenityクレートは依存関係に含まれず、
/// 本体のビルドに一切影響しない。
///
/// # 使い方
///
/// ```ignore
/// let gateway = DiscordGateway::new("your-bot-token");
/// gateway.start().await?;
///
/// // メッセージ受信（ブロッキング）
/// let msg = gateway.recv().await?;
///
/// // チャンネルにテキスト送信
/// gateway.send_to_channel(channel_id, "Hello!").await?;
/// ```
pub struct DiscordGateway {
    token: String,
    rx: Mutex<mpsc::Receiver<IncomingMessage>>,
    tx: mpsc::Sender<IncomingMessage>,
    http: Arc<Http>,
    shard_manager: Mutex<Option<Arc<serenity::gateway::ShardManager>>>,
    /// [`Self::shutdown`] が呼ばれたら真。client タスク終了時に「意図した停止」か
    /// 「接続死（fail-loud すべき）」かを見分けるのに使う（#337）。監視タスクと
    /// `shutdown` の両方から触るので `Arc<AtomicBool>`。
    shutting_down: Arc<AtomicBool>,
    /// A2UIコンポーネントインタラクション受信チャンネル
    interaction_rx: Mutex<mpsc::Receiver<ComponentInteractionData>>,
    interaction_tx: mpsc::Sender<ComponentInteractionData>,
    /// Form トリガーボタン → モーダル応答（未設定時は従来どおり UpdateMessage ACK のみ）
    form_modal_resolver: Option<A2uiFormModalResolver>,
    /// Voice (songbird) マネージャ。start() 時に serenity クライアントへ登録される。
    /// 受信 PCM をデコードする DecodeMode::Decode で構築する（VC 対話の STT 用）。
    voice: Arc<songbird::Songbird>,
}

impl DiscordGateway {
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_form_modal_resolver(token, None)
    }

    /// `form_modal_resolver` を渡すと、Form に紐づくボタン押下時にモーダルで応答する。
    pub fn with_form_modal_resolver(
        token: impl Into<String>,
        form_modal_resolver: Option<A2uiFormModalResolver>,
    ) -> Self {
        let token = token.into();
        let (tx, rx) = mpsc::channel(256);
        let (interaction_tx, interaction_rx) = mpsc::channel(64);
        let http = Arc::new(Http::new(&token));
        Self {
            token,
            rx: Mutex::new(rx),
            tx,
            http,
            shard_manager: Mutex::new(None),
            shutting_down: Arc::new(AtomicBool::new(false)),
            interaction_rx: Mutex::new(interaction_rx),
            interaction_tx,
            form_modal_resolver,
            voice: songbird::Songbird::serenity_from_config(
                songbird::Config::default().decode_mode(songbird::driver::DecodeMode::Decode),
            ),
        }
    }

    /// serenityのHTTPクライアントへの参照を返す（管理API用）
    pub fn http(&self) -> &Arc<Http> {
        &self.http
    }

    /// Voice (songbird) マネージャを返す（VC 参加・受信・再生用）。
    pub fn voice(&self) -> Arc<songbird::Songbird> {
        self.voice.clone()
    }

    /// client タスクの接続死検知を再武装する（#337 NIT-2）。
    ///
    /// 同一インスタンスを shutdown → 再 start した場合、`shutdown()` が立てた
    /// `shutting_down` フラグをここで倒しておかないと、再 start 後に client タスクが
    /// 接続死しても監視タスクが「意図した停止」と誤認して二度と鳴らなくなる（恒久沈黙）。
    /// `start()` の冒頭で必ず呼ぶ。
    fn rearm_client_death_detection(&self) {
        self.shutting_down.store(false, Ordering::SeqCst);
    }

    /// Bot接続を開始する（バックグラウンドタスクとして起動）
    pub async fn start(&self) -> Result<()> {
        // #337 NIT-2: 前回 shutdown 分のフラグを倒し、接続死検知を再武装する。
        self.rearm_client_death_detection();

        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT
            | GatewayIntents::GUILDS
            | GatewayIntents::GUILD_VOICE_STATES;

        let handler = DiscordHandler {
            tx: self.tx.clone(),
            interaction_tx: self.interaction_tx.clone(),
            self_user_id: tokio::sync::OnceCell::new(),
            form_modal_resolver: self.form_modal_resolver.clone(),
        };

        let mut client = Client::builder(&self.token, intents)
            .event_handler(handler)
            .voice_manager_arc(self.voice.clone())
            .await
            .context("Failed to create Discord client")?;

        let shard_manager = client.shard_manager.clone();
        {
            let mut sm = self.shard_manager.lock().await;
            *sm = Some(shard_manager);
        }

        // #337: client タスクの**終了そのもの**を監視して fail-loud にする。
        //
        // 以前は `if let Err(e) = client.start().await` で ERROR を 1 行出すだけで、
        // タスクが終わっても（`Ok` 正常終了・`Err` 致命エラーどちらも）誰にも
        // エスカレーションされなかった。致命エラー（4004 invalid token /
        // 4014 disallowed intents など復旧不能）で接続が死んでも、受信転送側の
        // stall 検知（message_loop の `warn_inbound_stalled`）は `recv()` が `Err` を
        // 返すことが発火条件で、その `tx` は本構造体が保持しているため `Err` にならず、
        // 「起動ログは出る → 以後メッセージが永久に来ない → 警告ゼロ」というサイレント
        // 停止になっていた。タスクが終わったら接続は死んでいるので、ここで表面化させる。
        //
        // `shutdown()` 由来の意図した停止（`shutting_down` == true）では鳴らさない。
        let shutting_down = self.shutting_down.clone();
        tokio::spawn(async move {
            let outcome = match client.start().await {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("error: {e}"),
            };
            crate::owner_warning::warn_discord_client_task_exited(
                shutting_down.load(Ordering::SeqCst),
                &outcome,
            );
        });

        info!("Discord gateway starting...");
        Ok(())
    }

    /// メッセージを受信する（ブロッキング）
    ///
    /// Discordからメッセージが届くまで待機する。
    /// 受信ループから呼ぶことを想定。
    pub async fn recv(&self) -> Result<IncomingMessage> {
        let mut rx = self.rx.lock().await;
        rx.recv().await.context("Discord gateway channel closed")
    }

    /// 指定チャンネルにテキストメッセージを送信する。
    ///
    /// 送信した Discord メッセージの id を返す（分割送信時は**最後のチャンク**の id）。
    /// 「発言終わり」リアクション（#431）が、そのターンで自分が最後に投稿したメッセージを
    /// 特定するのに使う。id が不要な呼び出し側は戻り値を無視してよい。
    pub async fn send_to_channel(&self, channel_id: u64, text: &str) -> Result<Option<u64>> {
        let mut last_id = None;
        // Discord APIの文字数制限（2000文字）
        if text.len() <= 2000 {
            let msg = ChannelId::new(channel_id)
                .say(&self.http, text)
                .await
                .context("Failed to send message to Discord channel")?;
            last_id = Some(msg.id.get());
        } else {
            // 長いメッセージは分割送信
            for chunk in split_message(text, 2000) {
                let msg = ChannelId::new(channel_id)
                    .say(&self.http, &chunk)
                    .await
                    .context("Failed to send message chunk to Discord channel")?;
                last_id = Some(msg.id.get());
            }
        }
        Ok(last_id)
    }

    /// 指定メッセージにUnicode絵文字のリアクションを付ける。
    ///
    /// LLM が受信メッセージを読んだ（ターン文脈に含めた）ことを示す 👀 などに使う。
    /// 呼び出し側は失敗を非致命的に扱うこと（権限不足・削除済みメッセージ等で失敗しうる）。
    pub async fn add_reaction(&self, channel_id: u64, message_id: u64, emoji: &str) -> Result<()> {
        ChannelId::new(channel_id)
            .create_reaction(
                &self.http,
                MessageId::new(message_id),
                ReactionType::Unicode(emoji.to_string()),
            )
            .await
            .context("Failed to add reaction to Discord message")?;
        Ok(())
    }

    /// 指定チャンネルに「入力中...」インジケーターを送信する
    pub async fn start_typing(&self, channel_id: u64) -> Result<()> {
        ChannelId::new(channel_id)
            .broadcast_typing(&self.http)
            .await
            .context("Failed to broadcast typing indicator")?;
        Ok(())
    }

    /// コンポーネントインタラクション（ボタンクリック等）を受信する。
    ///
    /// Discordからのinteraction_createイベントがあるまで待機する。
    pub async fn recv_interaction(&self) -> Result<ComponentInteractionData> {
        let mut rx = self.interaction_rx.lock().await;
        rx.recv()
            .await
            .context("Discord interaction channel closed")
    }

    /// Botをシャットダウンする
    pub async fn shutdown(&self) {
        // #337: client タスクの終了を「意図した停止」と見分けさせるため、
        // shutdown_all() で client.start() を返させる**前に**フラグを立てる。
        // これで監視タスクは終了を検知しても fail-loud を鳴らさない。
        self.shutting_down.store(true, Ordering::SeqCst);
        let sm = self.shard_manager.lock().await;
        if let Some(ref manager) = *sm {
            manager.shutdown_all().await;
            info!("Discord gateway shut down");
        }
    }
}

/// Discordの2000文字制限に合わせてメッセージを分割する
///
/// 長さは文字数（コードポイント数）で数え、長い行は文字境界で分割する。
/// バイト境界での分割はマルチバイトUTF-8（日本語等）を破壊するため行わない。
/// 空のチャンクは生成しない（Discordは空メッセージを400で拒否する）。
fn split_message(text: &str, max_len: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0usize;

    for line in text.lines() {
        let line_chars = line.chars().count();

        // 1行が制限を超える場合は文字境界でさらに分割
        if line_chars > max_len {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
                current_chars = 0;
            }
            let mut piece = String::new();
            let mut piece_chars = 0usize;
            for ch in line.chars() {
                piece.push(ch);
                piece_chars += 1;
                if piece_chars == max_len {
                    chunks.push(std::mem::take(&mut piece));
                    piece_chars = 0;
                }
            }
            if !piece.is_empty() {
                chunks.push(piece);
            }
            continue;
        }

        if current_chars + line_chars + 1 > max_len && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        if !current.is_empty() {
            current.push('\n');
            current_chars += 1;
        }
        current.push_str(line);
        current_chars += line_chars;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// 受信メッセージの Sender を構築する。
///
/// **投稿者が bot かどうかで作り分けない。** 無限ループを止めるのは `is_own_message`
/// （自分自身の投稿の除外）であって、bot フラグではない。bot を別扱いすると
/// エージェント同士が会話できなくなる。
fn build_sender(author_id: u64, author_name: &str, avatar_url: String) -> Sender {
    Sender::new(author_id.to_string(), author_name).with_avatar(avatar_url)
}

/// 自分自身の投稿か（無限ループを防ぐ唯一の判定）。
///
/// `self_user_id` は `ready` イベントで確定する自分の Discord user id。**他の bot
/// （＝他エージェント）は自分ではない**ので通す — それが会話。
fn is_own_message(self_user_id: Option<u64>, author_id: u64) -> bool {
    self_user_id == Some(author_id)
}

/// subtask lifecycle webhook の「機械通知」投稿か（**自分のものに限らず**、同形式の
/// lifecycle 通知全般を対象にする）。
///
/// 背景（自己受信ループの芽）: subtask lifecycle webhook（#175 の opt-in）が
/// **会話チャンネルへ**向いていると、`build_started_messages` /
/// `build_terminal_message` / `build_progress_message` が出した生ペイロードが
/// チャンネルに投稿される。その投稿者 id は **webhook の id** であって bot の
/// user id ではないため、`is_own_message`（bot user id 一致のみ）を素通りして
/// メッセージイベントとして受信され、機械通知を会話として処理してしまう。
///
/// webhook id からは「自分が管理する webhook か」を（DB 照合なしには）判定できないため、
/// ここでは**発信元を問わず**、この bot が出すのと同形式の lifecycle 通知を落とす。
/// 会話に混ぜたくない機械通知という点で扱いは同じで、副作用も無害側（正当な会話は
/// webhook 由来でない）に倒れる。判定は 2 条件の AND:
/// - webhook 由来（`webhook_id` あり）。人間や他 bot の**通常発言**は `webhook_id` が
///   無いので、本文がたまたま `**subtask ` を含んでいても落とさない。
/// - 本文の**先頭行**が lifecycle payload ヘッダ（`<emoji> **subtask <status>**`）。
///   3 つの builder が出す固定書式に共通する唯一のトークンが `**subtask ` で、
///   これらは webhook でしか配送されない。人間側の連携ツール等の**別種**の
///   webhook 投稿はこの形にならないので通す（正当な webhook 連携の外形を保つ）。
///
/// activity webhook（`tool_call_*`）は別チャンネル前提の別機能なのでスコープ外。
fn is_subtask_lifecycle_webhook_post(webhook_id: Option<u64>, content: &str) -> bool {
    webhook_id.is_some() && content_is_subtask_lifecycle_payload(content)
}

/// 本文が subtask lifecycle payload（`build_*` が出すヘッダ）か。
///
/// ヘッダは常に 1 行目で `<emoji> **subtask <status>**` の形。判定を 1 行目に
/// 限定して、本文中に `**subtask ` を引用しただけの投稿の巻き添えを避ける。
///
/// `pub(crate)`: 書式の出所（`gateway_actions::webhook` の `build_*`）側で、
/// 実出力がこの判定に一致し続けることを回帰テストで固定するため公開する。
pub(crate) fn content_is_subtask_lifecycle_payload(content: &str) -> bool {
    content
        .lines()
        .next()
        .is_some_and(|first| first.contains("**subtask "))
}

/// Discord添付ファイルが画像かどうかを判定する
fn is_image_attachment(a: &serenity::model::channel::Attachment) -> bool {
    a.content_type
        .as_deref()
        .map(|ct| ct.starts_with("image/"))
        .unwrap_or(false)
        || (a.width.is_some() && a.height.is_some())
}

/// 添付ファイルの注記／画像パート生成に必要な最小情報の射影。
///
/// serenity の `Attachment` はテストから組み立てられない（非公開/非exhaustive）ため、
/// 本文組み立てのロジックをこの型の上に閉じてテスト可能にしている。
#[derive(Debug, Clone, PartialEq)]
struct AttachmentInfo {
    filename: String,
    content_type: Option<String>,
    size: u32,
    url: String,
    is_image: bool,
}

/// 注記に埋め込むフィールド（ファイル名 / content_type）の最大文字数。
/// 切り詰めたことが分かるよう末尾 1 文字は省略記号に使う。
const MAX_NOTE_FIELD_CHARS: usize = 100;

/// 注記へ埋め込む前にフィールドを無害化する。
///
/// #272: この変更で画像の filename が**初めてプロンプト本文に到達する**ようになった
/// （従来は `ContentPart::Image.alt` にしか入らず `extract_discord_content` が捨てていた）。
/// 会話文字列は `[{speaker}] [{ts}]:\n{content}` を `\n` で連結して作るため、改行を含む
/// ファイル名で**偽の発話行を注入**できてしまう。他の制御文字も注記の見た目を壊す。
/// 非画像側も同じ関数を通す（同型のリスクを従来から持っているので揃えて塞ぐ）。
///
/// #521: 防御ロジックは Nostr の受信アンカーと共有する [`opencrab_core::injection`] に集約。
/// ここは Discord の上限 `MAX_NOTE_FIELD_CHARS` を渡す薄いラッパ。
fn sanitize_note_field(s: &str) -> String {
    opencrab_core::injection::sanitize_embedded_field(s, MAX_NOTE_FIELD_CHARS)
}

impl AttachmentInfo {
    fn from_serenity(a: &serenity::model::channel::Attachment) -> Self {
        Self {
            filename: a.filename.clone(),
            content_type: a.content_type.clone(),
            size: a.size,
            url: a.url.clone(),
            is_image: is_image_attachment(a),
        }
    }

    /// 本文テキストへ焼き込む注記行。
    ///
    /// #272: 画像添付だけ注記が無いと `session_logs.content` に「画像があった」痕跡が
    /// 一切残らず、次ターン以降の履歴が「画像に触れていないユーザー発言 + 画像に言及した
    /// 自分の応答」という証拠の非対称になる。モデルはこれを見て「画像など無かったのに
    /// 作話した」と誤って自己否認する。画像も非画像と同じ経路・同じ書式でアンカーを残す。
    /// URL は Discord CDN の署名付きで失効するため**含めない**（必要なのは存在の痕跡のみ）。
    fn note(&self) -> String {
        let name = sanitize_note_field(&self.filename);
        let ct = sanitize_note_field(self.content_type.as_deref().unwrap_or("unknown"));
        if self.is_image {
            format!("[画像添付: {} ({})]", name, ct)
        } else {
            format!("[添付ファイル: {} ({}), {}B]", name, ct, self.size)
        }
    }
}

/// 本文と添付注記を結合した「履歴に残るテキスト」を作る。
/// 注記は添付の並び順を保つ（画像と非画像を混ぜても Discord 上の順序どおり）。
fn build_full_text(content: &str, attachments: &[AttachmentInfo]) -> String {
    let notes: Vec<String> = attachments.iter().map(AttachmentInfo::note).collect();
    if notes.is_empty() {
        content.to_string()
    } else if content.trim().is_empty() {
        // 本文なしの添付のみ（スクショのドラッグ＆ドロップ＝最も普通の画像投稿）。
        // ここで `format!("{content}\n{notes}")` に落とすと `session_logs.content` が
        // 改行始まりになり、`format_single_log` の `"[{}]{}:\n{}"` と合わさって履歴に
        // 空行が 1 本入る。本文が無いなら注記だけを返す。
        notes.join("\n")
    } else {
        format!("{}\n{}", content, notes.join("\n"))
    }
}

/// 受信メッセージの `MessageContent` を組み立てる。
/// 画像は従来どおり `ContentPart::Image` としても載せる（vision 経路は不変）。
/// 本文アンカー（`build_full_text`）と画像パートの**両方**が出る形になる。
fn build_message_content(
    content: &str,
    attachments: &[AttachmentInfo],
) -> opencrab_gateway::MessageContent {
    let full_text = build_full_text(content, attachments);
    let image_parts: Vec<opencrab_gateway::ContentPart> = attachments
        .iter()
        .filter(|a| a.is_image)
        .map(|a| opencrab_gateway::ContentPart::Image {
            url: a.url.clone(),
            alt: Some(a.filename.clone()),
        })
        .collect();

    if image_parts.is_empty() {
        opencrab_gateway::MessageContent::text(&full_text)
    } else {
        // 画像があれば注記も必ず出るので `full_text` は非空。以前あった
        // 「本文が空なら画像パートのみ」の分岐は到達不能になったので削除した
        // （#272: 画像は必ず本文アンカーを伴う、が不変条件）。
        let mut parts = vec![opencrab_gateway::ContentPart::Text(full_text)];
        parts.extend(image_parts);
        opencrab_gateway::MessageContent::Multi(parts)
    }
}

// ==================== Serenity Event Handler ====================

struct DiscordHandler {
    tx: mpsc::Sender<IncomingMessage>,
    interaction_tx: mpsc::Sender<ComponentInteractionData>,
    self_user_id: tokio::sync::OnceCell<u64>,
    form_modal_resolver: Option<A2uiFormModalResolver>,
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn message(&self, ctx: Context, msg: SerenityMessage) {
        // 自分自身のメッセージは無視（無限ループ防止）。
        // ここで弾くのは**自分だけ**。他の bot（他エージェント）は通す。
        if is_own_message(self.self_user_id.get().copied(), msg.author.id.get()) {
            return;
        }

        // subtask lifecycle webhook 投稿（機械通知）は会話として受信しない（自分のものに
        // 限らず同形式のもの全般）。webhook の投稿者 id は bot user id と異なるため上の
        // is_own_message を素通りする。webhook 由来 + lifecycle payload 形式のものだけを
        // 落とし、自己受信ループの芽を断つ（人間/他ツールの正当な webhook 連携は通す）。
        // log の前に落として、機械通知が「受信した」ように記録される QC 混乱の元も消す。
        if is_subtask_lifecycle_webhook_post(msg.webhook_id.map(|id| id.get()), &msg.content) {
            return;
        }

        // 添付ファイルの処理（#272: 画像も本文アンカーを持つ）
        let attachments: Vec<AttachmentInfo> = msg
            .attachments
            .iter()
            .map(AttachmentInfo::from_serenity)
            .collect();
        let image_count = attachments.iter().filter(|a| a.is_image).count();

        // #272 P1: 切り分けに body_len の逆算を強いられたので、受信段で件数を残す
        // （ファイル名/URL は出さない）。
        info!(
            author = %msg.author.name,
            content = %msg.content.chars().take(50).collect::<String>(),
            attachments = attachments.len(),
            images = image_count,
            "Discord message event received"
        );

        let guild_id = msg.guild_id.map(|id| id.to_string()).unwrap_or_default();
        let channel_id = msg.channel_id.to_string();

        let content = build_message_content(&msg.content, &attachments);

        let sender = build_sender(msg.author.id.get(), &msg.author.name, msg.author.face());

        let mut incoming = IncomingMessage::new(
            MessageSource::Discord {
                guild_id,
                channel_id: channel_id.clone(),
            },
            content,
            sender,
        )
        .with_channel(Channel {
            id: channel_id,
            name: msg.channel_id.to_string(),
        })
        .with_metadata("discord_message_id", serde_json::json!(msg.id.to_string()));

        // ギルド情報（チャンネルの場合のみ）
        if let Some(gid) = msg.guild_id {
            if let Some(guild) = ctx.cache.guild(gid) {
                incoming = incoming
                    .with_metadata("guild_name", serde_json::json!(guild.name.clone()))
                    .with_metadata(
                        "guild_icon_url",
                        serde_json::json!(guild.icon_url().unwrap_or_default()),
                    );
                if let Some(channel) = guild.channels.get(&msg.channel_id) {
                    incoming = incoming
                        .with_metadata("channel_name", serde_json::json!(channel.name.clone()));
                }
            }
        }

        if let Err(e) = self.tx.send(incoming).await {
            warn!("Failed to forward Discord message to gateway: {e}");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Component(component) => {
                let custom_id = component.data.custom_id.clone();

                // Only handle our A2UI interactions (format: "interaction:{uuid}:{action}")
                if !custom_id.starts_with("interaction:") {
                    return;
                }

                debug!(
                    custom_id = %custom_id,
                    user = %component.user.name,
                    "Discord component interaction received"
                );

                let guild_id = component
                    .guild_id
                    .map(|id| id.to_string())
                    .unwrap_or_default();
                let channel_id = component.channel_id.to_string();
                let message_id = component.message.id.to_string();
                let user_id = component.user.id.to_string();
                let user_name = component.user.name.clone();

                // Detect interaction kind and extract select values
                use serenity::all::ComponentInteractionDataKind;
                let (interaction_kind, selected_values) = match &component.data.kind {
                    ComponentInteractionDataKind::StringSelect { values } => {
                        (InteractionKind::SelectMenu, Some(values.clone()))
                    }
                    ComponentInteractionDataKind::Button => (InteractionKind::Button, None),
                    _ => (InteractionKind::Button, None),
                };

                // Form トリガー: モーダルで応答（UpdateMessage だとモーダルが開けない）
                if interaction_kind == InteractionKind::Button {
                    if let Some(ref resolver) = self.form_modal_resolver {
                        if let Some(spec) = resolver(&custom_id, &component.user.id.to_string()) {
                            let modal =
                                CreateModal::new(spec.modal_custom_id.clone(), spec.title.clone())
                                    .components(spec.components.clone());
                            let _ = component
                                .create_response(&ctx.http, CreateInteractionResponse::Modal(modal))
                                .await;
                            return;
                        }
                    }
                }

                // ACK with deferred update (prevents "interaction failed" message)
                let _ = component
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::UpdateMessage(
                            CreateInteractionResponseMessage::new(),
                        ),
                    )
                    .await;

                let data = ComponentInteractionData {
                    custom_id,
                    user_id,
                    user_name,
                    channel_id,
                    guild_id,
                    message_id,
                    selected_values,
                    modal_values: None,
                    interaction_kind,
                };

                if let Err(e) = self.interaction_tx.send(data).await {
                    warn!("Failed to forward component interaction: {e}");
                }
            }
            Interaction::Modal(modal) => {
                let custom_id = modal.data.custom_id.clone();

                // Only handle our A2UI modals
                if !custom_id.starts_with("interaction:") {
                    return;
                }

                debug!(
                    custom_id = %custom_id,
                    user = %modal.user.name,
                    "Discord modal submit received"
                );

                let guild_id = modal.guild_id.map(|id| id.to_string()).unwrap_or_default();
                let channel_id = modal.channel_id.to_string();
                let user_id = modal.user.id.to_string();
                let user_name = modal.user.name.clone();

                // Extract modal field values
                let mut modal_values = Vec::new();
                for row in &modal.data.components {
                    for comp in &row.components {
                        if let serenity::all::ActionRowComponent::InputText(input) = comp {
                            modal_values.push((
                                input.custom_id.clone(),
                                input.value.clone().unwrap_or_default(),
                            ));
                        }
                    }
                }

                // ACK the modal submit
                let _ = modal
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::UpdateMessage(
                            CreateInteractionResponseMessage::new(),
                        ),
                    )
                    .await;

                let data = ComponentInteractionData {
                    custom_id,
                    user_id,
                    user_name,
                    channel_id,
                    guild_id,
                    message_id: String::new(), // Modal submits don't have a message_id
                    selected_values: None,
                    modal_values: Some(modal_values),
                    interaction_kind: InteractionKind::ModalSubmit,
                };

                if let Err(e) = self.interaction_tx.send(data).await {
                    warn!("Failed to forward modal submit: {e}");
                }
            }
            _ => return,
        }
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        let self_id = ready.user.id.get();
        let _ = self.self_user_id.set(self_id);
        info!(
            "Discord bot connected as {} (id: {})",
            ready.user.name, ready.user.id,
        );
    }
}

#[cfg(test)]
mod tests;
