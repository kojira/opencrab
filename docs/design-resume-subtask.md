# 設計ドキュメント: サブセッション Resume 機能

> TODO #24
> ステータス: **設計中**

---

## 1. 問題定義

現在の設計では、サブエンジンが完了するとJoinHandleが終了してDashMapから削除される。
セッション履歴はDBに残っているが、同じコンテキストで再起動する手段がない。

### 1.1 発生する問題

```
① サブが作業途中でタイムアウト
  → 「前回の続き」を引き継いで再起動できない
  → 最初から全コンテキストを説明し直す必要がある

② steer（追加指示）がサブの完了後に届いた
  → DashMapにJoinHandleがないのでsteerできない
  → 追加指示を反映した再実行ができない

③ 長期タスク（設計→実装→テスト）の途中状態を保ちながら
  人間がレビューして戻ってきた時に継続したい
```

### 1.2 実際の事例

設計ドキュメント修正タスク（opencrab-import-design-fix）で、サブが処理完了後に
「テンポラリエージェント方式への変更」という追加指示をsteerで送ったが、
タイムアウト（5秒）により間に合わなかった。resumeできれば同じコンテキストで継続できた。

---

## 2. 設計

### 2.1 gateway action: `resume_subtask`

```
resume_subtask(
  session_id: String,        // 再開するセッションのID（必須）
  task: Option<String>,      // 追加指示（省略時は「前回の続き」として再開）
  timeout_secs: Option<u32>, // タイムアウト（省略時はデフォルト1800秒）
)
```

### 2.2 処理フロー

```
resume_subtask(session_id, task?) 呼び出し
  ↓
DB: sessions テーブルから session_id で検索
  ↓
DB: session_logs から会話履歴を取得（時系列順）
  ↓
新しいセッションを作成
  - metadata_json に `resumed_from_session_id: session_id` を記録
  - depth は元セッションの depth を引き継ぐ
  ↓
新しいエンジンを起動
  - 既存の会話履歴を初期コンテキストとして渡す
  - task が指定された場合は最後のユーザーメッセージとして追加
  - task が省略された場合は「前回の作業を引き継いで続けてください」を追加
  ↓
新しい subtask_id を生成して DashMap に登録
  ↓
メインセッション履歴に subtask_spawned を記録
  {type: "subtask_resumed", subtask_id, session_id, resumed_from: original_session_id, spawned_at}
```

### 2.3 セッション設計

| 項目 | 方針 |
|------|------|
| session_id | 新しいIDを生成（元のセッションとは別に記録） |
| subtask_id | 新しいIDを生成 |
| resumed_from | 元のsession_idをmetadata_jsonに記録 |
| depth | 元のセッションのdepthを引き継ぐ |
| 会話履歴 | 元のセッションのsession_logsを読み込んで新セッションの初期コンテキストに使用 |

新しいsession_idにすることでクリーンな履歴管理が可能。
`resumed_from_session_id`チェーンを辿ることで継続性を追跡できる。

---

## 3. UIとユースケース

### 3.1 ダッシュボードでのresume

セッション一覧ページで「再開」ボタンを追加:
- 完了・タイムアウトしたセッションを選択して「再開」
- オプションで追加指示テキストを入力できるフォーム

### 3.2 エージェントからの自然言語呼び出し

メインLLMが「前回の続きをお願い」「セッションXXXを再開して」と言われた場合に
`resume_subtask`を呼び出す。

セッション履歴に`subtask_spawned`/`subtask_completed`が記録されているので、
メインLLMはsubtask_idからsession_idを取得してresume_subtaskを呼べる。

### 3.3 典型的なユースケース

```
① タイムアウト後の継続
[メイン]: 「前のタスク（subtask_id: xxx）の続きをお願い」
  → resume_subtask(session_id: yyy)
  → サブが前回コンテキストを引き継いで再開

② レビュー後の追加指示
[メイン]: 「設計を見直した。テンポラリエージェント方式に変更して」
  → resume_subtask(session_id: yyy, task: "5.2節をテンポラリエージェント方式に書き直して")
  → サブが前回コンテキストを知った状態で追加指示を受け取る

③ 長期タスクの分割実行
Day 1: サブがPhase 1実装 → 完了
Day 2: resume_subtask → Phase 2実装を継続
```

---

## 4. 実装ステップ

### Phase 1（最優先）

- [ ] `crates/actions/src/resume_subtask.rs` 新規作成
  - DB検索: sessions + session_logsから履歴取得
  - 新セッション生成（resumed_from_session_id記録）
  - エンジン起動（履歴を初期コンテキストとして渡す）
  - DashMapに登録
- [ ] メインセッション履歴への `subtask_resumed` 記録
- [ ] ゲートウェイアクション登録

### Phase 2（後日）

- [ ] ダッシュボードUI: セッション一覧の「再開」ボタン
- [ ] 追加指示入力フォーム

---

## 5. 既知の制約

| 制約 | 内容 |
|------|------|
| コンテキスト長 | 長大な会話履歴はLLMのコンテキスト上限に引っかかる可能性あり → 最新N件に制限するオプションを提供 |
| depth整合性 | resumeしたサブがさらにサブを生成した場合のdepth管理は元のルールに従う |
| セキュリティ | session_idを知っていれば誰でもresumeできてしまう問題（エージェントのオーナーチェック必須） |
