# Discord 👀リアクションの付与タイミング修正

- 対象Issue: #964
- 設計契約: DC-964 v0.5
- 状態: 承認済み

## 1. 問題

現在のDiscord gatewayは、ターンの処理開始を示す`activity started`を受けた時点で、対象投稿へ👀を付けている。

しかし`activity started`は、権限確認、会話履歴の構築、LLMへ送るリクエストの構築より前に発生する。そのため現在の👀は「LLMがこの投稿を読む」より早く付いている。

👀の意味を「その投稿を含むプロンプトをLLMへ渡す直前」に一致させる。

## 2. 必須の処理順序

LLM呼び出しは必ず直列で行う。

1. 1個目のプロンプトを構築する。
2. 1個目の対象投稿があれば👀を付ける。
3. 1個目のプロンプトをLLMへ渡す。
4. LLMが1個目の処理結果を返すまで待つ。
5. 1個目の処理結果を会話履歴へ追加する。
6. 待機中に届いた新着投稿を会話履歴へ追加する。
7. 1個目の処理結果と新着投稿を含む2個目のプロンプトを構築する。
8. 2個目で新しく読む対象投稿があれば👀を付ける。
9. 2個目のプロンプトをLLMへ渡す。

1個目の処理結果が返る前に、2個目のプロンプトを構築したり、2個目のLLM呼び出しを開始したりしてはならない。

## 3. 修正方針

### 3.1 `activity started`

- `activity started`は入力中表示の開始だけに使う。
- `activity started`では投稿へ👀を付けない。
- 発端投稿のoriginを`activity started`へ載せない。

### 3.2 LLM呼び出し直前

`SkillEngine`は、次のLLM呼び出しで新しく読む投稿のoriginを小さな一覧として持つ。

- 初回は、発端投稿のoriginを一覧へ入れる。
- 2回目以降は、既存の新着取得処理が実際にプロンプトへ追加した投稿のoriginを同じ一覧へ入れる。
- 各`llm.chat(request).await`の直前に一覧を見る。
- 対象があれば、各originについて既存の`read`通知を1回送る。
- 対象がなければ何もしない。
- 通知後は一覧を空にし、次の呼び出しで同じ投稿へ重複通知しない。

Discord gatewayは既存どおり`read+origin`を受けて、その投稿へ👀を付ける。

## 4. 変更しないもの

次の仕組みは変更しない。

- LLM呼び出しの直列実行
- 1個目の処理結果を2個目のプロンプトへ渡す処理
- 新着投稿の保存・取得・会話への追加方法
- セッションロック
- メッセージqueue
- 権限・owner・trusted user判定
- 投稿者identityと表示名
- 添付ファイル処理
- Discord返信処理
- 🏁、🤐、❌の意味と付与条件
- providerの選択やLLM request形式
- DB schema

新しいscheduler、並列処理、先行prompt構築、再試行機構、会話provenance層は追加しない。

## 5. エラー処理

- LLM requestを構築できず、LLM呼び出しへ到達しない場合は👀を付けない。
- `read`通知やDiscordリアクションの失敗は、既存どおりログへ残し、LLM処理の結果や返信内容を変更しない。
- 同じoriginへの重複通知は既存の重複防止と呼び出しごとの一覧消費で防ぐ。

## 6. 変更範囲

想定する変更箇所:

- `crates/actions/src/run_request.rs`
- `crates/server/src/process/mod.rs`
- `crates/core/src/engine/skill_engine.rs`
- `crates/core/src/engine/skill_engine/run.rs`
- `crates/extgate/src/inbound/turn.rs`
- Extgate conformance test
- Discord QC harness test
- `started+origin`を👀のタイミングとして説明しているコメント・設計文

対象外:

- loop restartの会話再構築方法（別Issue #965。今回の修正へ混ぜない）
- production環境の変更
- DB migration

## 7. テスト

決定的なテストで次を確認する。

1. `activity started`だけでは👀が付かない。
2. 初回LLM呼び出しの直前に、発端投稿へ👀が1回だけ付く。
3. 対象originがないLLM呼び出しでは👀を付けない。
4. 1個目のLLMが処理結果を返す前に、2個目のプロンプトを構築しない。
5. 1個目のLLMが処理結果を返す前に、2個目のLLMを呼び出さない。
6. 2個目のプロンプトには、1個目の完全な処理結果と新着投稿が、この順序で含まれる。
7. 新着投稿は、到着時ではなく、それを含む2個目のLLM呼び出し直前に👀が1回だけ付く。
8. LLM呼び出し前に失敗した場合は👀を付けない。
9. record-only、held、whitelist除外、fold処理、既存リアクションのテストが引き続き合格する。
10. strict clippy、関連crate test、Extgate conformance、Discord QC harness、800行監査が合格する。

## 8. 実機QC

許可されたDiscord channelで、固有の確認文を使って次の順序を確認する。

1. Discord投稿をgatewayが受信する。
2. `activity started`時点では👀を付けない。
3. 対象投稿を含むLLM requestの直前に`read+origin`を通知する。
4. 同じDiscord投稿へ👀が1回付く。
5. LLMが処理結果を返す。
6. Discordへ返信する。

複数回呼び出すケースでは、1個目の結果が2個目のrequestに含まれ、2個目が1個目の完了前に始まっていないこともログで確認する。

## 9. 受入条件

- 👀はターン開始時ではなく、その投稿を含むLLM呼び出しの直前に付く。
- 対象投稿がないLLM呼び出しでは👀を付けない。
- 1個目の処理結果を受け取り、履歴へ追加するまで2個目のプロンプトを構築しない。
- 2個目のプロンプトには1個目の処理結果が含まれる。
- LLM呼び出しは常に1本ずつ直列で行う。
- 既存の会話、queue、権限、添付、返信、他リアクションの挙動を変えない。
