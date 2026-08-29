# nostaro ↔ OpenCrab インターフェース契約

OpenCrab の Nostr sub-gateway（`crates/nostr`）は、Nostr プロトコルの実処理を自作 CLI
**nostaro** に subprocess で委譲する。本書はその
呼び出し契約と、そのために nostaro 側へ必要な**汎用的な**改造を定義する。

OpenCrab 側は本契約の I/O をモックしてユニットテスト済み（`crates/nostr`）。実機で
繋ぐには nostaro 側に下記の改造が必要（`--config` / `pubkey` / `vanity --json` /
`watch` フィルタ+`--json`。いずれも一般の Nostr ツールとして有用な機能）。

## 前提: 鍵の隔離

OpenCrab はエージェント毎に一意な config を使い、秘密鍵の共有事故を防ぐ。全 nostaro
呼び出しに `--config <path>` を付ける:

```
data/agents/<agent_id>/nostr/config.toml
```

config.toml は `relays` / `default_relays` / `blossom_server` を持つ。OpenCrab が起動時に
DB の per-agent 設定からこのファイルを materialize する。**#620 以降、`secret_key` 行は書か
ない**（下記「秘密鍵の at-rest 暗号化」）。

## 秘密鍵の at-rest 暗号化と実行時注入（#620）

秘密鍵をエージェントの読める範囲（config.toml・生成鍵ファイル・DB）に平文で置かず、
暗号化して保管し、実行時にシステム側から env で渡す。

- **保管**: DB の本鍵も生成鍵ファイルも `enc:v1:<base64>` の暗号文（XChaCha20-Poly1305）。
  config.toml には鍵行を書かない。よって設定を確認する通常の操作で平文の鍵は目に入らない。
- **注入**: OpenCrab は nostaro を spawn するとき **env `NOSTARO_SECRET_KEY`** に復号した鍵を
  載せる。nostaro は env を config の `secret_key` より優先で読む（空/空白は未設定扱い）。
  本鍵は本設定 config で、生成鍵（`from` 送信）は本設定 config を `--config` に使いつつ env で
  生成鍵を渡す（鍵混同を避けるため送信種別ごとに別の鍵を env に載せる）。
- **マスターキー**: 暗号/復号の鍵は環境変数 **`OPENCRAB_SECRET_MASTER_KEY`**（base64 32B）。
  OpenCrab 起動直後に読んで即座に自プロセスの環境から消す（エージェントのシェルに継承させ
  ない）。生成・バックアップ・紛失時の扱いは [README の `.env` 節](../README.md) を参照。
  **マスターキーを失うと全 Nostr identity が復号不能**になる（DB とキーは二つで一組）。
- **fail-closed**: Nostr が設定済みのエージェントが 1 つ以上ある構成でマスターキーが
  欠落 / 不正形式 / 既存暗号文と不一致なら、区切り線バナー（`error`）を出して **Nostr を起動
  しない**（送信も受信も止まる）。プロセス全体は止めない（Discord 等は動く）。Nostr 未設定の
  構成はマスターキー無しでも通常起動する。
- **移行**: 起動時に 1 回・冪等（`enc:` 済みはスキップ）。既存の平文 DB 本鍵・生成鍵ファイルを
  暗号化し、平文の from-config を削除し、config.toml の `secret_key` 行を落とす。

### デプロイ順序（重要）

**nostaro バイナリの更新が先、OpenCrab の起動（移行）が後。** 逆順にすると、鍵行なし config を
読んだ旧 nostaro が env を解さず「鍵が無い」で失敗し、Nostr の送受信が全停止する。OpenCrab を
起動する前に `OPENCRAB_SECRET_MASTER_KEY` を設定しておくこと。

### nostaro 側の改造（env 優先読み）

`keys_from_config()` が env `NOSTARO_SECRET_KEY` を config の `secret_key` より優先で読む
（空/空白のみは未設定扱いにして config へフォールバック）。config スキーマ自体は不変
（`secret_key` は Option のまま。素の CLI 運用・`nostaro init` は変わらない）。

### 改造 1: `--config <path>` グローバルフラグ（汎用）

現状 config は `~/.nostaro/config.toml` にハードコード。任意パスを指定できる
`--config <path>`（または `NOSTARO_CONFIG` env）を全サブコマンドに追加する。
指定時はそのファイルを config として読む。複数アカウントの運用一般に有用。

### 改造 1b: `nostaro pubkey`（必須・汎用）

`nostaro --config <p> pubkey` で、その config の**公開鍵（hex）を stdout に出力**する。
OpenCrab は起動時にこれを取得し、**自分の投稿を受信ループでスキップ**する（自己返信
無限ループ＋LLM 支出の防止）。取得できないと OpenCrab はゲートウェイを**起動しない**
（fail-closed）ので、実機接続にはこのコマンドが必須。

### 改造 1c: `nostaro vanity --json`（鍵生成・汎用）

`nostaro vanity --json [--prefix=<bech32>]` で**新規鍵を生成**し、1 行の JSON を stdout に
出力する。OpenCrab はこれをエージェントの Nostr アイデンティティ生成に使う（ダッシュボード /
`POST /api/agents/{id}/nostr/generate`）。

```
nostaro vanity --json [--prefix=<bech32prefix>]
```

- `--prefix` は npub（`npub1...`）の `1` 以降に前置される bech32 文字列。省略/空なら通常の
  ランダム鍵を即返す。
- **config 非依存**: 既存 config/秘密鍵を読まず、純粋に生成するだけ（OpenCrab は生成時に
  `--config` を付けない）。
- 出力（stdout, 1 行 JSON）:

```json
{ "nsec": "nsec1...", "npub": "npub1...", "pubkey": "<hex pubkey>" }
```

- `nsec` は必須。`npub`/`pubkey` は任意（あれば表示・自己ループ防止に使える）。
- 進捗を出す場合は stderr へ（OpenCrab は stdout の最後の JSON 行を採用する）。

OpenCrab 側の防御: prefix は bech32 charset かつ最大 3 文字に制限してから渡す（探索コストは
`32^len` で増えるため、同期リクエストがハングしないよう保守的に制限）。nostaro 側は通常の
生成でよい。

### 引数の渡し方（OpenCrab 側の防御・nostaro は通常のパーサでよい）

OpenCrab は positional を取るサブコマンド（post/reply/zap/upload）で **`--`
（オプション終端）を挟み**、watch のフラグ値は **`--flag=value` の = 形式**で渡す。
target/text/recipient は受信イベント/モデル由来（`-` 始まりの値がフラグに化ける
引数インジェクションを防ぐため）。nostaro 側は `--` と `--flag=value` を通常どおり
解釈すればよい（特別対応不要）。

## 送信（既存サブコマンドをそのまま利用）

OpenCrab のツール（`nostr_*`）は既存 CLI を呼ぶだけ。改造不要:

| OpenCrab ツール | nostaro 呼び出し |
|---|---|
| `nostr_post`   | `nostaro --config <p> post "<text>"` |
| `nostr_reply`  | `nostaro --config <p> reply <target> "<text>"` |
| `nostr_zap`    | `nostaro --config <p> zap <recipient> <amount> -m "<message>"` |
| `nostr_upload` | `nostaro --config <p> upload <path>` |

> **#514: DM 送信は禁止**。`nostr_dm` ツールは撤去し、`nostaro dm` を叩く経路
> （送信メソッド・`nostr_run dm` passthrough）も塞いだ。DM は暗号化されていても秘密鍵が
> 漏れた時点で過去に遡って全部読めるため、その前提ごと無くす（オーナー決定）。private な
> 話は Nostr でせず、Discord の DM か指定チャンネルを使う。
>
> **#699（2026-08-19 オーナー裁定）: `nostr_run event` は許可**。任意 kind の publish
> （kind:40 のパブリックチャット作成等）は正当用途があり、塞ぐ不自由が利益を上回っていた。
> `event -k 4` で DM kind を生発行できる理論上の迂回は残るが、DM として機能させるには
> 暗号化まで自前でイベントを組む必要があり実用的でない（受容）。「便利な暗号化 DM」の
> 経路である `dm` の deny は維持する。
>
> **2026-08-30 オーナー裁定: `nostr_run post` / `reply` は拒否**。gateway がある世界で
> 投稿・返信を nostaro passthrough から行うのはトークンの無駄で、gateway の存在意義に反する。
> 投稿・返信は通常の返話（say）一本。`react` / `repost` / `zap` / `profile` / `upload` /
> `get` / `timeline` / `search` 等は触らない（gateway に代替経路が無いものを塞ぐと外形が減る）。

成功時 exit 0・stdout に結果（投稿なら note id / upload なら URL）を出す前提。

### identity 切替（`nostr_switch_identity`・owner/trusted 限定）

`nostr_switch_identity(npub=...)` で、`nostr_generate_key` で生成した鍵をゲートウェイの
**本鍵**に採用する。#620 以降、`config.toml` に鍵行は書かない。OpenCrab 側は、新 pubkey を
**生成鍵経由**（生成鍵を env `NOSTARO_SECRET_KEY` で注入して `pubkey` を引く・`cli.rs` の
`pubkey_from`）で取り直して自己返信スキップ用に更新し、その後で DB の secret_key を新鍵の
暗号文へ差し替える。本鍵プロバイダは DB を読むため、DB 更新の**前**に生成鍵経由で新 pubkey を
取得・検証する（`config.toml` は鍵行なしで relays のみ再生成する）。**watch は鍵非依存なので
プロセス再起動は不要**。owner/trusted のターンでのみ実行される（外部ユーザーによるなりすまし
乗っ取りを防ぐ）。nostaro 側の改造は不要。

### マルチ identity 送信（`from` 指定）

送信ツールの任意 `from`（npub）を指定すると、本鍵ではなく **`nostr_generate_key` で
生成した鍵**で送信できる。#620 以降、**一時 from-config（平文の鍵ファイル）は作らない**。
OpenCrab 側は `--config` に**鍵行なしの本設定 `config.toml`**（relays/blossom を継承）を
そのまま使い、生成鍵は env `NOSTARO_SECRET_KEY` で注入して上表と同じサブコマンドを呼ぶ。
共有点（`command_with_config`）には鍵を差さず、本鍵経路と生成鍵経路で**別々の鍵**を env に
載せる（鍵混同を防ぐ）。nostaro 側の改造は不要（env の鍵で送信するだけ）。`from` に使えるのは、
そのエージェントが生成した鍵（`generated-keys/<npub>.nsec` が存在するもの）に限る。生成鍵
ファイルは暗号文（`enc:v1:…`）で保管し、送信時にサーバ内で復号して env に載せる。

## 受信（`watch` の汎用改造が必要）

現状の `watch` は Discord webhook 専用でフィルタが弱い。OpenCrab は次の形で spawn し、
**stdout の JSONL** を読む:

```
nostaro --config <p> watch --json \
  --relay wss://yabu.me --relay wss://r.kojira.io \
  [--author <npub|hex>]... [--keyword <kw>]... --kind <n>...
```

### 改造 2: `watch` にフィルタフラグ追加（汎用）

`watch` の購読条件は **p タグ（mention-only）／keyword／author の 3 つ**で、これを
`--match` でどう結合するかが決まる（nostaro issue #6 以降）。

- `--mention-only` / `--no-mention-only`: **既定は mention-only = true**（`--json` でも
  効く）。監視対象は `--npub` 未指定なら自分自身なので、既定で**自分宛の p タグ**
  （メンション・リプライ・リアクション・DM）が購読される。両方を同時に指定すると
  パースエラー。
- `--match any|all`: 3 条件の結合方法。**既定は `any`（OR）**。`all` は AND
  （`--author X --keyword foo --match all` = 「X が書いた foo を含む投稿」）。
  **絞り条件が 1 つも無いときだけ全通し**になる。
- `--author <npub|hex>`（複数可）: この author の投稿を拾う（`any` では OR 条件、
  `all` では排他スコープ）。
- `--keyword <kw>`（複数可）: content にいずれかを含む投稿を拾う（同上）。keyword は
  リレー側で絞れないのでローカル一致判定。
- `--kind <n>`（複数可）: 指定 kind のみ。**未指定時の `--json` 既定は kind:1 + kind:7**
  （旧版は kind:1 のみ）。OpenCrab は `effective_kinds()` で必ず 1 つ以上の `--kind` を
  明示するのでこの既定には依存しない。**#514: `effective_kinds()` は DM の kind（4 / 1059）を
  必ず除外する**ので、DM をリレーへ要求することはない（設定・DB に混じっていても外す）。
  書き込み側（`apply_nostr_settings`）でも保存前に落とし、受信ループでも破棄する多層防御。
- `--relay <url>`（複数可）: **このフラグで指定したリレーのみに接続**する
  （config の `relays`/`default_relays` は使わない）。OpenCrab は許可リレーを
  明示フラグで渡すため、config 由来の別リレーに繋がせない。
- 自分のイベントは `--json` でも**除外される**（旧版の json には自己除外が無かった）。
  OpenCrab 側も受信ループで `nostaro pubkey` 一致をスキップしており、二重の防御になる。

### OpenCrab が渡すフラグ（受信セマンティクスの契約 / #278）

| フラグ | OpenCrab の扱い |
|---|---|
| `--json` | 常に渡す |
| `--match=any` | **常に渡す**（既定と同値だが明示する） |
| `--mention-only` / `--no-mention-only` | **どちらも渡さない**（nostaro の既定 true に委ねる） |
| `--relay` | `effective_relays()` を全て明示 |
| `--kind` | `effective_kinds()`（設定が空なら `1`）を全て明示 |
| `--author` / `--keyword` | **運用者が設定したときだけ**渡す（OpenCrab は自動設定しない） |

**`--match=any` を選ぶ理由**: nostaro では mention-only も 1 つの条件なので、`all` にすると
「自分宛（p タグ）**かつ** keyword 一致」という AND になる。運用者が keyword を設定して
いるエージェントでは、(a) 本文に keyword を含まない e/p タグだけの返信が落ち（#271 で
直した事象そのもの）、(b) 本文が暗号文/絵文字である kind:4・1059（DM）や kind:7
（リアクション）は keyword に一致しえず全部落ち、(c)「自分宛でない keyword 一致投稿を
拾う」という keyword 監視の意図も落ちる。`any` なら「自分宛は必ず届く＋運用者が明示した
分が上乗せされる」となり、旧挙動を狭めない。既定と同値でも明示するのは、nostaro 側の既定
が将来また変わっても OpenCrab の受信が黙って変わらないようにするため。

**トレードオフ**: `any` では authors と keywords を**両方**設定したエージェントの結合が
旧版の AND から OR に変わる（受信量が増える）。「使うかどうかはエージェントに決めさせる」
方針に沿って `--match` をエージェント/運用者が選べるようにするのは #275 の範囲。

**購読が無制限にならない担保**: `--no-mention-only` を渡さない限り p タグ条件が必ず効くので、
OpenCrab のフィルタ設定が空でも購読は「自分宛のみ」＝**最も狭い**。逆に keywords を足すほど
（nostaro が keyword 用に kind 全体の購読を張るぶん）広くなる。したがって OpenCrab 側に
「フィルタが空なら起動拒否」というガードは**置かない**（旧版のガードは、旧 nostaro の json が
mention-only を無視して kind:1 を全件購読していたから必要だった）。

### 改造 3: `watch --json`（汎用）

`--json` 指定時、Discord webhook を送らず、**マッチしたイベント1件を1行の JSON**として
stdout に出力する（JSONL）。スキーマ（`crates/nostr/src/event.rs` の `NostrEvent`）:

```json
{
  "id": "<hex event id>",
  "pubkey": "<hex pubkey>",
  "npub": "npub1...",          // 任意
  "note_id": "note1...",        // 任意（返信対象指定に使う）
  "author_name": "owner",      // 任意（プロフィール由来）
  "created_at": 1700000000,
  "kind": 1,
  "content": "本文",
  "tags": [["p", "..."], ["e", "..."]]
}
```

- 未知フィールドがあっても OpenCrab 側は無視する（前方互換）。
- JSON でない行（ログ等）は OpenCrab がスキップするので、進捗ログを stderr に出すのは自由。
- プロセスは stdin クローズ / SIGTERM で終了する（OpenCrab が停止時に kill）。

## OpenCrab 側の動作（参考）

- `crates/nostr`: config/event/CLI ラッパー/送信アクション（本 PR）。
- Phase 1b: per-agent マネージャ（watch を spawn し JSONL を読む → `RunRequest` →
  `run_agent_response` → 返信）、`/api/agents/{id}/nostr`、ダッシュボード UI。
