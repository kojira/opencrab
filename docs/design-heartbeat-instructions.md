# design-heartbeat-instructions

ハートビート時にエージェントが「今この瞬間、何をするか」を判断するための **指示（heartbeat instructions）** を、ハードコードされた1行プロンプトから、エージェント単位・チャンネル単位で設定可能・更新可能なものへ拡張する設計。

バージョン: v1（2026-06-02）

> **更新（#439 / #456 / #455）**: ハートビートは**セッション単位**の設定
> （`session_heartbeat_config`）へ一本化され（旧 agent/channel スコープは廃止）、発火は
> **中央スケジューラ**が握る（永続アンカー・設定時起点・即時反映・`next_fire_at` 照会・
> 再起動をまたぐ位相保存）。`set_my_heartbeat`/`get_my_heartbeat` は**現在のセッション**に
> 対して設定・照会し、`scope` 引数は廃止された。**per-agent 定時実行（#455・cron/@every）は
> 同じ中央スケジューラの時刻源に載る**（別ループを作らない）。詳細は
> [[design-agent-schedules]] と `config-precedence.md`。

関連:
- [[design-agent-instructions]] — `agents.instructions` の設計とOwner限定の `update_instructions` アクション。本設計はこれを強く踏襲する。
- [[design-async-instructions]] — システムプロンプトへの英語ブロック注入パターン。
- [[design-bot-loop-prevention]] — Bot同士の無限ループ問題。ハートビート発言が引き金になりうるため、本設計の安全要件に直結する。
- [[design-openclaw-import]] — OpenClawワークスペースからのインポート。現状 `HEARTBEAT.md` は**除外**されている。
- [[design-memory-rollup-v2]] — ハートビートDreamモード（選択的忘却）。ハートビート指示の拡張先候補。

---

## 1. 現状の挙動と限界

### 1.1 現状の挙動

ハートビートは `opencrab_core::heartbeat::heartbeat_loop`（`crates/core/src/heartbeat.rs`）がプライム秒間隔（既定29秒、`config.toml` の `agent.heartbeat_interval_secs`）で発火し、`crates/server/src/main.rs` の `make_heartbeat_callback()` が返すコールバックを毎tick呼ぶ。

1tickの処理（`main.rs`、`make_heartbeat_callback()` 内）:

1. `list_heartbeat_channels()` で `discord_channel_config` から `heartbeat_enabled = 1` のチャンネルを取得。
2. チャンネルごとに `heartbeat_interval_secs`（per-channel override）の経過を確認。
3. チャンネルごとのハートビートセッションを取得/作成。
4. **ハードコードされた日本語プロンプト**をセッションログ（`log_type = "system"`）に挿入:

   ```text
   [ハートビート] チャンネル「{channel_name}」で今この瞬間、自律的に何をするか
   判断してください。SPEAK/LEARN/IDLEから選んでください。SPEAKの場合は
   'SPEAK: <メッセージ>'の形式で一言。発言は30分に1回以下が望ましい。
   ```

5. `build_agent_context()`（`crates/server/src/process.rs`）でシステムプロンプトを構築し、`run_agent_response()` でLLMを呼ぶ。
6. 応答テキストを素朴にパースして決定を抽出（`response_text.contains("SPEAK:")` → `Speak`、`contains("LEARN")` → `Learn`、それ以外 → `Idle`）。
7. `Speak` ならDiscordへ投稿、`Learn` なら `memory_curated`（category=`reflection`）へ反省を書き込み、`Idle` は何もしない。
8. `insert_heartbeat_log()` で `heartbeat_log` テーブルに決定を記録。

`HeartbeatDecision` は `Speak(String) / Learn / Idle / ManageSkills{..}`（`crates/core/src/heartbeat.rs`）。`ManageSkills` はenumには存在するがコールバック側では生成されていない。

エージェントの `agents.instructions`（`crates/db/src/queries.rs` の `AgentRow`）は `build_agent_context()` 内で `## Instructions` セクションとしてシステムプロンプトに常時注入されるため、**ハートビートにも自動的に適用される**。ただしこれは通常会話と共通の指示であり、ハートビート専用の指示ではない。

### 1.2 限界

- **ハートビート専用指示が存在しない。** 「今この瞬間どう振る舞うか」はハードコードされた1行に固定されており、エージェントごと・チャンネルごとに変えられない。
- **挙動の選択肢が固定。** SPEAK/LEARN/IDLEのみ。`ManageSkills` やDreamモード（[[design-memory-rollup-v2]]）など、ハートビートで行いたい自律行動を増やす導線がない。
- **発言頻度・トーン・話題の方針が調整できない。** 「30分に1回以下」もハードコードで、チャンネル特性（雑談 vs 業務）に合わせられない。
- **無限ループのリスク。** [[design-bot-loop-prevention]] の通り、ハートビート発言はBot同士のループの引き金になりうる。チャンネル単位で「Botしかいなければ黙る」等の方針を持てない。
- **OpenClawの `HEARTBEAT.md` を活かせない。** [[design-openclaw-import]] では `HEARTBEAT.md` は明示的にインポート除外。OpenClaw資産が捨てられている。
- **可観測性が弱い。** プロンプトを変えるにはコード変更とビルドが必要で、運用中の調整ができない。

---

## 2. 提案するユーザー向け挙動

オーナー（信頼できるユーザー）は、Discordでの自然言語の依頼、またはダッシュボードから、以下を設定できる:

- **グローバル（エージェント単位）ハートビート指示**: 「ハートビートのときは、新しい話題があるときだけ話して。雑談は1時間に1回まで。Botしかいないチャンネルでは黙る。」
- **チャンネル単位の上書き**: 特定チャンネルでだけ「ここでは業務連絡のみ。雑談禁止。」のように上書き。
- **読み出し**: エージェント／チャンネルの現在のハートビート指示を確認できる。

エージェント自身は、**オーナーから明示的に承認された文脈でのみ** 自分のハートビート指示を更新できる（例: オーナーが「これからハートビートでは○○して」と言い、エージェントがそれを `update_heartbeat_instructions` で永続化する）。任意の会話・グループチャット・サブタスク中には更新できない。

ランタイムは、ハートビートのtickごとに「グローバル指示 + 該当チャンネルの上書き」を合成してプロンプトに注入する。指示が未設定なら、現状と同じ既定文言にフォールバックする（後方互換）。

---

## 3. ストレージの選択

要求された3案を、本リポジトリの既存モデルに照らして比較する。

### 3.1 案A: ワークスペースの `HEARTBEAT.md`

各エージェントのワークスペース（`data/agents/{agent_id}/workspace/HEARTBEAT.md`）にMarkdownで置く。

- **利点**: OpenClawの `HEARTBEAT.md` と1:1対応。エージェント自身が既存のワークスペースツール（`ws_read/ws_write/ws_edit`）で読み書きできる。Gitやファイルで履歴を追いやすい。
- **欠点**: チャンネル単位の上書きをファイルで表現しづらい。`ws_write` は通常のツールとして任意の会話中に呼べてしまうため、**Owner限定の権限境界をファイル書き込みに被せにくい**（[[design-agent-instructions]] が `instructions` をファイルでなくDB＋Owner限定アクションにしたのと同じ理由）。ダッシュボードからの編集・APIアクセスがファイルI/O経由になり、トランザクション性・一覧性が弱い。

### 3.2 案B: DBバックの指示

`agents` テーブルに `heartbeat_instructions` カラム、`discord_channel_config` に `heartbeat_instructions` カラムを追加。

- **利点**: [[design-agent-instructions]] の `agents.instructions` と完全に同じパターン。Owner限定アクション・ダッシュボードUI・API・移行のすべてが既存設計を流用できる。チャンネル単位の上書きが `discord_channel_config`（既にper-channelの `heartbeat_enabled` / `heartbeat_interval_secs` を持つ）に自然に収まる。権限境界（誰が書けるか）をアクション層で一元的に強制できる。
- **欠点**: OpenClawの `HEARTBEAT.md` を直接の真実ソースにはできない（インポート時に変換が要る）。エージェントがファイルとして直接編集する体験は提供しない。

### 3.3 案C: ハイブリッド（推奨）

**DBを真実のソース（source of truth）とし、`HEARTBEAT.md` をインポート/エクスポートのインターフェースとする。**

- DBに `agents.heartbeat_instructions`（グローバル）と `discord_channel_config.heartbeat_instructions`（チャンネル上書き）を持つ。ランタイム・権限・UIはすべてDBを見る。
- OpenClawワークスペースの `HEARTBEAT.md` は **インポート時に `agents.heartbeat_instructions` へ変換**（[[design-openclaw-import]] の `AGENTS.md → instructions` と同じ写像）。
- 必要に応じて、**Owner操作によるエクスポート**でDBの指示を `data/agents/{agent_id}/workspace/HEARTBEAT.md` に書き出せる（人間が編集しGitに残すための逃げ道）。エクスポートは任意機能でPhase 4送り。

### 3.4 推奨と根拠

**案C（ハイブリッド、DBが真実のソース）を推奨する。**

根拠:

1. **権限境界を被せやすい。** ハートビート指示はエージェントの自律行動の根幹であり、`instructions` と同様にプロンプトインジェクション・不正改変のリスクがある（[[design-agent-instructions]]）。DB＋Owner限定アクションなら、`crates/actions/src/bridge.rs` の既存フィルタ（`owner_only_actions`）に1行追加するだけで境界を強制できる。ファイル書き込み（`ws_write`）は任意会話中に呼べるため、この境界を作るのが難しい。
2. **チャンネル上書きが自然に収まる。** `discord_channel_config` は既に `heartbeat_enabled` / `heartbeat_interval_secs` をper-channelで持っており（`ChannelConfigRow`、`crates/db/src/queries.rs`）、`heartbeat_instructions` を足すのは最小の拡張。ファイルでチャンネル別を表現するより素直。
3. **既存パターンの再利用。** [[design-agent-instructions]] のDBカラム追加・API・UI・移行がほぼそのまま流用でき、実装コストとレビューコストが小さい。
4. **OpenClaw資産を捨てない。** `HEARTBEAT.md` をインポートの入口として扱うことで、OpenClawの設定を活かす。エクスポートでファイル編集の逃げ道も残せる。

つまり「DBを正、`HEARTBEAT.md` を境界（import/export）」というハイブリッドが、本リポジトリのワークスペース/インポートモデルに最も整合する。

---

## 4. スコープ

### 4.1 グローバル（エージェント単位）ハートビート指示

`agents` テーブルに以下を追加:

```sql
ALTER TABLE agents ADD COLUMN heartbeat_instructions TEXT NOT NULL DEFAULT '';
```

- `AgentRow` / `AgentPatch`（`crates/db/src/queries.rs`）に `heartbeat_instructions: String` / `Option<String>` を追加。`upsert_agent` / `get_agent` / `apply_agent_patch` を更新。
- 空文字なら「未設定」とみなし、ランタイムは既定文言（§1.1のハードコード文言）にフォールバック。

### 4.2 チャンネル単位の上書き

`discord_channel_config` テーブルに以下を追加:

```sql
ALTER TABLE discord_channel_config ADD COLUMN heartbeat_instructions TEXT NOT NULL DEFAULT '';
```

- `ChannelConfigRow` に `heartbeat_instructions: String` を追加。`upsert_channel_config` / `get_channel_config_for_agent` / `list_heartbeat_channels` 等の読み書きを更新。
- 解決ルール（既存の `execute_list_channels` のagent優先ロジックに倣う）:
  1. `(channel_id, agent_id)` の行に `heartbeat_instructions` があればそれを**上書き**として使う。
  2. なければ `(channel_id, "")`（グローバルチャンネル設定）。
  3. それも空なら、エージェントの `agents.heartbeat_instructions`。
  4. それも空なら、既定文言。
- 合成方針: チャンネル上書きは「エージェント指示に追記」か「完全置換」かを選べると便利だが、v1では **「エージェント指示 + チャンネル上書きを連結（チャンネル側が後）」** を既定とし、完全置換は未解決事項（§13）に回す。

### 4.3 OpenClawワークスペースからのimport/export

- **Import**: [[design-openclaw-import]] の除外リストから `HEARTBEAT.md` を外し、`HEARTBEAT.md`（全文）→ `agents.heartbeat_instructions` に写像する。`AGENTS.md → instructions` と同じ扱い。スキャン応答（dryrun）に `heartbeat: { found: bool, length: int }` を追加。
- **Export（任意・Phase 4）**: Owner操作で `agents.heartbeat_instructions` を `data/agents/{agent_id}/workspace/HEARTBEAT.md` に書き出す。ワークスペース外への書き込みは既存の `send_file` 同様にパス検証（canonicalize → workspace_root配下チェック）を行う。

---

## 5. ランタイムでの読み込みと注入

`crates/server/src/main.rs` の `make_heartbeat_callback()` を、ハードコードプロンプトを使う代わりに**指示解決関数**を呼ぶ形に変える。

### 5.1 指示の解決

新規ヘルパー（`crates/server/src/process.rs` か新規 `heartbeat_prompt.rs`）:

```rust
/// ハートビートtickのプロンプト本文を解決する。
/// 優先順位: channel(agent) override → channel(global) override → agent global → 既定文言。
fn resolve_heartbeat_instructions(
    conn: &Connection,
    agent_id: &str,
    channel: &ChannelConfigRow,
) -> String { /* §4.2の解決ルール */ }
```

### 5.2 プロンプトへの注入

- 解決した指示を、現状ハードコード文言を入れているのと同じ箇所（セッションログへ `log_type = "system"` で挿入する箇所）に入れる。テンプレートは:

  ```text
  [ハートビート] チャンネル「{channel_name}」。{resolved_instructions}
  出力形式: SPEAK/LEARN/IDLE のいずれか。SPEAKの場合のみ 'SPEAK: <メッセージ>'。
  ```

  → **出力形式の規約（SPEAK/LEARN/IDLE）はランタイムが固定**し、指示部分（方針・頻度・トーン・話題）だけを設定可能にする。これによりパーサ（§1.1の `contains("SPEAK:")`）を壊さない。
- `agents.heartbeat_instructions` を**システムプロンプト側**にも `## Heartbeat Behavior` セクションとして出すかは選択肢。ただし通常会話のtickでこのセクションが出ると混乱するため、**v1ではハートビートtickのときだけ**（`build_agent_context()` に`is_heartbeat: bool` を渡す、または注入はセッションログ側のみに留める）に限定する。シンプルさ優先で**セッションログ側への注入のみ**を推奨。
- [[design-async-instructions]] の知見に従い、出力形式の規約文は英語でも可だが、既存文言が日本語かつエージェントが日本語ペルソナのため、規約は日本語のまま、指示本文はオーナーが書いた言語をそのまま使う。
- **更新（#501）**: 上記の「セッションログ側への注入のみ」は撤回した。指示文はこのテンプレートで整形（`scheduler::format_heartbeat_prompt`）し、**その tick のシステムプロンプトへ注入する**（会話ログには積まない）。理由と実測は §14 を参照。
- **更新（#515・記録側）**: 出力規約に **IDLE の短い理由**を足した（`IDLE: <理由>`）。エージェントの応答は毎ターン HB 専用セッションへ `speech`（`speaker_id=agent_id`）として残っており（`heartbeat_turn::turn` が `result.response` を書く）、これが「自分が何をした/しなかったか」の記録になる。以前は `IDLE` の 1 語しか返させていなかったため記録が空同然（本番実測: `speech` 平均 14 バイト）で、見送りの理由が一切残らなかった。規約で理由を求めることで、記録に判断が載る。
  - **理由は機構が生成しない**（#501 の再来防止）: `format_heartbeat_prompt` は「自分の言葉で」「定型文の繰り返しは避けて」と**促すだけ**で、理由の文面は毎ターン LLM が文脈から書く。定型文をこちらで注入すると同じ文面が何百件も並んで判断を歪めるため（#501 の再現形）、記録は文脈依存に変わり続ける形にして構造的に反復を防ぐ。新しい保存先は作らない（既存の `speech` 行を使う）。
  - **パーサの追従**: 理由が自由文になったので、`parse_heartbeat_decision` の LEARN 判定を「応答全体に LEARN を含むか」から**決定行の先頭語**へ絞った。`IDLE: 直前に LEARN した` のように理由へ LEARN の語が混じるだけで Learn へ化け、内省メモ書き込みが誤発火するのを防ぐ。理由が無い素の `IDLE`（規約非遵守）でも決定は Idle・記録は「IDLE」1 語が残り壊れない。
  - **長さ**: 別途の切り詰めは入れない。会話再構成（`build_heartbeat_conversation_string`）が読み出し側で budget により上限を掛けるため、記録を短く切っても文脈予算は守られる一方、切ると「エージェント自身の言葉」を欠く。規約で「1〜2 行で簡潔に」と促すに留める。

### 5.3 キャッシュ/性能

- 解決はtickごとにDB読み出し1〜2回（既に `list_heartbeat_channels` で全チャンネル取得済みなので、`heartbeat_instructions` をその行に含めれば追加クエリ不要）。`agents.heartbeat_instructions` のみ別途取得が必要。プロンプトキャッシュ（[[design-prompt-cache]]）への影響は、指示が変わらない限りプレフィクスが安定するよう、注入位置を末尾寄りにする。

---

## 6. エージェントによる指示更新（Owner承認時のみ）

[[design-agent-instructions]] の `update_instructions`（Owner限定）と完全に同じ権限モデルを踏襲する。

- エージェントは自分のハートビート指示を**直接DB/ファイルに書けない**。`update_heartbeat_instructions` ゲートウェイアクション経由でのみ更新できる。
- このアクションは `crates/actions/src/bridge.rs` の `owner_only_actions` に登録され、**呼び出し元の `CallerIdentity::Owner`** が成立する文脈（= オーナーのメッセージへの応答中）でのみ実行される。
- グループチャット・サブタスク・他Botからのメッセージへの応答中は、たとえLLMが呼ぼうとしても**アクション層で拒否**される。

---

## 7. ツール／アクション設計

`crates/actions`（および `crates/discord/src/gateway_actions/discord_ops.rs` のパターン）に倣い、`GatewayActionResult` を返すアクションを追加する。

> **実装場所（#157 S3 で更新）**: 両ツールは gateway 非依存層 — `crates/server/src/heartbeat_instructions.rs`（定義は `crates/server/src/system_actions.rs` の `own_definitions()`）にある。Discord ゲートウェイには実装が無い。したがって web / Nostr / REST / heartbeat のターンでも使える。
>
> ただし **`scope="channel"` / `scope="effective"` は非対称**である。`scope="agent"` が触るのは `agents` 行なので全経路で機能するが、チャンネル上書きの行を作るのは Discord のチャンネル運用だけなので、非 Discord 経路では通常「行が無い」応答（`channel` は空文字列、`effective` はエージェント/既定へのフォールバック）になる。テーブル名・カラム名の Discord 依存を解消する改名は #159 の担当。

### 7.1 `update_heartbeat_instructions`（Owner限定・書き込み）

引数:

```json
{
  "scope": "agent | channel",
  "channel_id": "string (scope=channelのとき必須)",
  "guild_id": "string (scope=channel かつ新規行作成時に必要)",
  "instructions": "string (新しいハートビート指示の全文)",
  "reason": "string (変更理由。監査ログに残す)"
}
```

挙動:
- `scope = "agent"`: `agents.heartbeat_instructions` を更新（`apply_agent_patch` 経由）。
- `scope = "channel"`: `discord_channel_config`（`(channel_id, agent_id)`）の `heartbeat_instructions` を更新（`upsert_channel_config` 経由。行がなければ既存設定を尊重して作成）。
- 成功時 `success: true` と更新後のスコープ・文字数・プレビュー（先頭120字）を返す。
- 文字数上限（例: 4000字）を超えたらエラー。

### 7.2 `read_heartbeat_instructions`（読み出し）

引数:

```json
{ "scope": "agent | channel | effective", "channel_id": "string (channel/effectiveのとき)" }
```

挙動:
- `agent`: `agents.heartbeat_instructions` を返す。
- `channel`: 当該チャンネルの上書きのみを返す。
- `effective`: §4.2の解決ルールを適用した「実際に使われる合成結果」を返す（デバッグ・確認用）。
- 読み出しはOwner限定にする必要は低いが、**書き込みと対称にOwner/trusted限定**にしておくと安全（チャンネル設定の漏えい防止）。v1ではtrusted限定。

### 7.3 ワークスペースファイルアクセスとの関係

- 案C採用のため、`ws_read/ws_write` でのファイル直接編集は**真実のソースにしない**。`HEARTBEAT.md` はimport入口／export出口としてのみ扱う。
- これにより「任意会話中に `ws_write` で指示を書き換える」という権限バイパス経路を塞ぐ。

---

## 8. 権限モデルと呼び出し元の同一性要件

- **書き込み（`update_heartbeat_instructions`）**: `CallerIdentity::Owner` 必須。`owner_discord_id`（`DiscordGatewayConfig.owner_discord_id`、`crates/server/src/config.rs`）で判定する既存の仕組みを使う。オーナー判定は「現在処理中のメッセージの送信者がオーナーか」で行い、`bridge.rs` のフィルタで強制する。
- **読み出し（`read_heartbeat_instructions`）**: trusted限定（`trusted_only_actions`）。
- **多人数チャンネル**: 複数人/複数Botが参加する文脈では、オーナー以外の発言を「指示」として受け取らない。アクション層がブロックするため、LLMがそそのかされてもDBは変わらない（[[design-bot-loop-prevention]] の「オーナーの指示のみがtruth」）。
- **サブタスク**: サブタスク実行中（委譲された文脈）は `CallerIdentity::Owner` が成立しない設計とし、書き込みを拒否する。

---

## 9. プロンプトインジェクションとグループチャット安全性

- **指示はコンテンツでなく設定。** ハートビート指示は、他者の発言テキストからは決して更新されない。更新経路はOwner限定アクションのみ（§6, §8）。
- **出力形式はランタイム固定。** SPEAK/LEARN/IDLEの規約と「SPEAK時のみ投稿」はコード側で固定し、指示本文では変えられない。よって「全部SPEAKしろ」のような指示が来ても、頻度・トーンの方針は変えられるが、投稿の出力契約は壊れない。
- **ループ防止との連携。** チャンネル上書きで「このチャンネルにBotしかいないなら必ずIDLE」「『止まる』をテキストで書くな」等を表現できるようにし、[[design-bot-loop-prevention]] の対策をデータ駆動で適用可能にする。Bot判定（`msg.author.bot`）が会話履歴に見えることが前提。
- **長さ制限・サニタイズ。** 指示は最大文字数（例4000字）でクランプ。制御文字を除去。注入時はテンプレートの区切りを明示し、指示本文がテンプレート規約を上書きできないよう、規約をテンプレートの**後ろ**に置く。

---

## 10. ログ・監査・バージョニング

- **監査ログ**: `update_heartbeat_instructions` の各実行を専用テーブルに記録する。[[design-agent-instructions]] には明示的な監査テーブルがなかったが、本設計では指示の改変履歴を追えるようにする:

  ```sql
  CREATE TABLE heartbeat_instructions_audit (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      agent_id TEXT NOT NULL,
      scope TEXT NOT NULL,            -- 'agent' | 'channel'
      channel_id TEXT,                -- scope=channelのとき
      caller_identity TEXT NOT NULL,  -- 'owner' 等
      caller_discord_id TEXT,
      old_value TEXT,
      new_value TEXT,
      reason TEXT,
      created_at TEXT NOT NULL
  );
  ```

- **ハートビート決定ログ**: 既存の `heartbeat_log`（`insert_heartbeat_log`）に、どの指示（agent/channel/default）が使われたかを `result_json` に含める（例: `{"source": "channel", "channel_id": "..."}`）。
- **バージョニング**: v1では明示的なバージョン番号は持たず、`heartbeat_instructions_audit` の `old_value/new_value` で履歴を再構成できる（[[design-memory-rollup-v2]] の watermark的な軽量履歴）。完全な版管理は未解決事項。

---

## 11. 現状のハードコードプロンプトからの移行計画

1. **後方互換のフォールバック**: `agents.heartbeat_instructions` / チャンネル上書きが空のとき、ランタイムは現状と完全に同じ既定文言を使う。既存エージェントは設定変更なしで現状維持。
2. **既定文言の定数化**: §1.1のハードコード文言を `const DEFAULT_HEARTBEAT_INSTRUCTIONS: &str` として切り出し、フォールバックに使う。これにより「ハードコード」を1箇所に集約。
3. **段階導入**: カラム追加（DEFAULT ''）→ 解決関数 → アクション → UI → import の順（§12）。各段階で既存ハートビートが壊れないことをテストで確認。
4. **既存データのバックフィル**: 不要（DEFAULT '' で「未設定＝既定文言」になる）。OpenClaw由来エージェントは再インポート時に `HEARTBEAT.md` から埋まる。

---

## 12. 実装計画（小さなフェーズ分割）

- **Phase 1 — スキーマ＋既定文言の集約（破壊なし）**
  - `agents` / `discord_channel_config` にカラム追加（migration）。
  - `AgentRow/AgentPatch/ChannelConfigRow` と関連クエリ更新。
  - 既定文言を定数化。`make_heartbeat_callback()` を「カラムが空なら既定文言」に変更（挙動は現状と同一）。
  - 完了条件: 既存ハートビートが従来通り動く。
- **Phase 2 — 解決＋注入**
  - `resolve_heartbeat_instructions()` を実装し、§4.2の優先順位で合成。
  - `make_heartbeat_callback()` で解決結果を注入。出力形式規約はランタイム固定。
  - `heartbeat_log` に使用ソースを記録。
- **Phase 3 — Owner限定アクション＋監査**
  - `update_heartbeat_instructions` / `read_heartbeat_instructions` を実装。
  - `bridge.rs` の `owner_only_actions` / `trusted_only_actions` に登録。
  - `heartbeat_instructions_audit` テーブルと記録処理。
- **Phase 4 — ダッシュボードUI＋import/export**
  - SoulタブにグローバルのTextarea、チャンネル設定UIに上書きTextarea。
  - GET/PUT `/api/agents/{id}`（または soul）に `heartbeat_instructions` を含める。
  - [[design-openclaw-import]] の除外から `HEARTBEAT.md` を外し import 写像を追加。任意でexport。

---

## 13. テスト計画

### 13.1 Rust ユニットテスト（`crates/db`, `crates/server`, `crates/core`）

- `T-1.1` `upsert_agent` → `get_agent` で `heartbeat_instructions` がラウンドトリップする。
- `T-1.2` `apply_agent_patch` で `heartbeat_instructions` のみ更新でき、他フィールドが変わらない。空文字とNone（未指定）の区別。
- `T-1.3` `ChannelConfigRow` の `heartbeat_instructions` が `upsert_channel_config`/取得でラウンドトリップ。
- `T-2.1` `resolve_heartbeat_instructions` の優先順位: channel(agent) > channel(global) > agent global > default の4ケース。
- `T-2.2` すべて空 → 既定文言を返す（後方互換）。
- `T-2.3` エージェント指示 + チャンネル上書きの連結順序（チャンネルが後）。
- `T-2.4` 4000字超のクランプ、制御文字除去。
- `T-3.1` `update_heartbeat_instructions`: `CallerIdentity::Owner` 以外で拒否される（bridge フィルタ）。
- `T-3.2` Owner時は成功し、`heartbeat_instructions_audit` に old/new/reason が記録される。
- `T-3.3` `read_heartbeat_instructions` の `effective` が解決結果と一致。
- `T-4.1` import: `HEARTBEAT.md` → `agents.heartbeat_instructions` の写像。除外リストから外れていること。

### 13.2 Discord / 手動E2E

- `E-1` 既定状態（未設定）でハートビートが従来通りSPEAK/LEARN/IDLEを出す。
- `E-2` オーナーがDiscordで「ハートビートでは新しい話題のときだけ話して」と依頼 → エージェントが `update_heartbeat_instructions(scope=agent)` を呼ぶ → 以降のtickで頻度が下がる。
- `E-3` オーナー以外（別ユーザー/別Bot）が同じ依頼 → アクションが拒否され、DBが変わらない（`heartbeat_instructions_audit` に記録なし、ログに拒否）。
- `E-4` チャンネル上書きで「業務連絡のみ・雑談禁止」を設定 → 当該チャンネルだけ挙動が変わり、他チャンネルは不変。
- `E-5` [[design-bot-loop-prevention]] シナリオ: 「Botしかいなければ黙る」上書きで、Bot同士のチャンネルでIDLEになりループが起きない。
- `E-6` import: OpenClawワークスペース（`HEARTBEAT.md` 入り）をスキャン→実行し、`heartbeat_instructions` が埋まる。
- `E-7` `read_heartbeat_instructions(scope=effective)` の出力が実際のtickプロンプトと一致することを `heartbeat_log` で突き合わせる。

---

## 14. 未解決事項とリスク

- **連結 vs 置換**: チャンネル上書きを「エージェント指示に追記」とするか「完全置換」とするか。v1は連結。置換モードのフラグ（`channel_override_mode`）は将来課題。
- **システムプロンプト注入（#501 で確定）**: v1 は指示文を**セッションログ（`system`/`speaker_id='heartbeat'`）へ挿入し会話経由で渡していた**が、毎 tick 同一文面が積まれ、履歴には「同じ指示 → IDLE」の対が何十回も並んでいた（本番の heartbeat セッションで指示文 192 件）。エージェントから見ると「同じ指示が何十回も書かれ、どれが今回か分からない」状態で、容量ではなく**挙動を歪めていた**（オーナー裁定 2026-08）。#501 で方針を変更し、**指示文はその tick のシステムプロンプトへ 1 度だけ注入する**（`scheduler::format_heartbeat_prompt` で整形 → `HeartbeatTarget::instructions_prompt` に載せ → `heartbeat_turn::build_context` が system プロンプトへ）。継続ターン（#440）にも sink 経由で同じ指示が伝わり、`[subtask_completed:…]` マーカーと併せて載る（出力形式の規約行は指示文側に一本化）。
  - **会話には積まない**: 書き込み側（`scheduler.rs`）はセッションログ挿入をやめた。既存 DB に積まれた分は **DB を書き換えず**、会話再構成（`process.rs::retain_conversation_logs`）で `speaker_id='heartbeat'` の `system` 行を**全件除外**する。subtask 完了本文（`system` かつ `speaker_id=None`, #404/#405）は `speaker_id` で区別して残す。
  - **効果（本番コピー・full 経路の実測）**: 会話組み立て 421,990 → 315,137 トークン（−106,853 / 約 25%）。system プロンプトは指示文 1 件ぶん +452 トークン。差し引き約 −106,401 トークン/ターン。
- **完全なバージョン管理**: 監査テーブルで履歴再構成は可能だが、ロールバックUIやdiff表示は未設計。
- **export時の競合**: `HEARTBEAT.md` をexportした後、人間がファイル編集してもDBには反映されない（DBが正）。再importしない限り無視されることを明示する必要がある。
- **出力契約の拡張**: 将来 `ManageSkills` やDreamモード（[[design-memory-rollup-v2]]）をハートビート決定に加える場合、出力形式規約とパーサを拡張する必要があり、指示本文との責務分離を再設計する。
- **オーナー判定の文脈依存**: `CallerIdentity::Owner` が「現在のメッセージ送信者」に依存するため、非同期にツール結果が戻ってくる文脈（[[design-async-instructions]] / [[design-message-loop-v3]]）でオーナー判定が維持されるかの確認が必要。
- **多エージェント同一チャンネル**: 同じチャンネルに複数エージェントがいる場合、`(channel_id, agent_id)` 単位の上書きで分離できるが、UIでの見せ方は未設計。
