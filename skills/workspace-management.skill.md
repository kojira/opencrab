---
name: workspace-management
description: "ワークスペース管理スキル - 自分専用のファイル空間を管理する"
version: 1
actions:
  - ws_read
  - ws_write
  - ws_edit
  - ws_list
  - ws_delete
  - ws_mkdir
---

# ワークスペース管理マニュアル

あなたは自分専用のワークスペースを持っています。ここにメモ、要約、自作スキルなどを自由に保存できます。

## 1. ワークスペースの構造

推奨ディレクトリ構造:
```
workspace/
├── notes/           # メモ・ノート
├── summaries/       # 要約
├── skills/          # 自作スキル
├── drafts/          # 下書き
└── data/            # その他のデータ
```

## 2. ファイル操作

### ws_read(path)
ファイルの内容を読み取ります。

### ws_write(path, content)
ファイルに内容を書き込みます（上書き）。

### ws_edit(path, old_string, new_string)
ファイルの一部を差分編集します。`old_string` は一意である必要があります。

### ws_list(path)
ディレクトリの内容を一覧表示します。

### ws_delete(path)
ファイルまたはディレクトリを削除します。

### ws_mkdir(path)
ディレクトリを作成します。

## 3. 注意事項

- パスはワークスペースルートからの相対パス
- 親ディレクトリは自動作成されます
- ファイルサイズには制限があります（100MB）
- 機密情報の保存には注意してください
