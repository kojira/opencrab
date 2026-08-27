# lib

| 関数 | 契約 |
|---|---|
| `conversationTitle` | theme が logical id と同じなら「新しい会話」。ID を表示名にしない |
| `uuidV4` | `crypto.getRandomValues` だけの canonical lowercase UUIDv4。`randomUUID` は使わない（secure context 不要）。UI の採番はすべてここ |
