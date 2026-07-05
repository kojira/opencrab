# Discord VC 対話（STT / TTS）

エージェントがボイスチャンネル（VC）に参加して音声で対話する機能。

## 話者分離の仕組み

Discord の音声はユーザーごとに独立した RTP ストリーム（SSRC）で届き、
SSRC ↔ ユーザー ID の対応はゲートウェイイベントで通知される。つまり
**「誰の発話か」はプロトコルレベルで確定**しており、声紋推定のような
誤認の余地がない。実装はユーザーごとに独立した発話セグメンタを持ち、
無音 800ms（または最長 15 秒）で発話を区切って STT に渡す。

文字起こし結果は「そのユーザーからのテキストメッセージ」として通常の
会話パイプライン（whitelist・セッションロック・履歴・NO_REPLY 規約）に
そのまま乗る。エージェントの返信はテキスト送信と同時に TTS で読み上げられる。

```
[VC] user A ──SSRC A──┐
[VC] user B ──SSRC B──┤ 20ms tick → ユーザー別セグメンタ → 無音で確定
                      │ → 48kHz stereo → 16kHz mono WAV → STT
                      └→ IncomingMessage(sender=そのユーザー) → 通常の会話処理
                                                     ↓ 返信
                            VC 再生 ← TTS（エージェント別の声）←┘
```

## セットアップ

### 1. config.toml

```toml
[voice]
enabled = true

[voice.stt]
provider = "openai"        # OpenAI 互換 /v1/audio/transcriptions
model = "whisper-1"
api_key_env = "OPENAI_API_KEY"
language = "ja"
# ローカル Whisper（faster-whisper-server / LocalAI 等）を使う場合:
# base_url = "http://localhost:8000/v1"

[voice.tts]
provider = "voicevox"      # "voicevox"（ローカル・無料） or "openai"
# base_url = "http://localhost:50021"
default_voice = "3"        # VOICEVOX スタイル ID

# エージェントごとに声を分ける
[voice.tts.agent_voices]
crab = "3"                 # ずんだもん ノーマル
rabomi = "1"               # 四国めたん あまあま
```

VOICEVOX を使う場合は [VOICEVOX ENGINE](https://voicevox.hiroshiba.jp/) を
起動しておく（`docker run -p 50021:50021 voicevox/voicevox_engine:cpu-latest` など）。
スタイル ID 一覧は `curl http://localhost:50021/speakers | jq` で確認できる。

### 2. Discord 側

- Bot に VC の **Connect / Speak** 権限が必要。
- STT 結果の注入先テキストチャンネルがエージェントの whitelist に
  入っていること（通常のテキスト会話と同じゲート）。

### 3. 使い方

whitelist 済みチャンネルで owner / trusted user がエージェントに依頼する:

> 「VC 〈チャンネル名 or ID〉に入って」

エージェントが `join_voice_channel` を呼び、以降 VC 内の発話が文字起こし
されてそのチャンネルの会話として流れ、返信が読み上げられる。
「VC から出て」で `leave_voice_channel`。

## 挙動の補足

- VC セッションはテキストチャンネルに紐づく。**そのチャンネルでの
  エージェント返信はトリガーが音声かテキストかに関係なくすべて読み上げる**
  （設計意図: VC 参加中はチャンネルの会話を音声でも追える）。
- 連続する返信はキュー再生され、重なって混ざることはない。

## 制約（v1）

- 共有（TOML）ゲートウェイのみ対応。per-agent 専用ゲートウェイ（#40）の
  VC 参加は未配線（`manager.rs` に None を渡している）。
- 1 ギルドにつき同時 1 VC セッション。
- Bot ユーザーの発話は文字起こししない（エージェント同士の TTS を拾い
  合う無限ループ防止）。エージェント同士の音声会話をやる場合は、この
  ガードの置き換え（発話者タグ付きで注入し、返信条件をプロンプトで制御）が必要。
- 読み上げはコードブロック・URL・メンションを除去し 400 字で打ち切る。
