# lib

| 関数 | 契約 |
|---|---|
| `conversationTitle` | theme が logical id と同じなら「新しい会話」。ID を表示名にしない |
| `uuidV4` | `crypto.getRandomValues` だけの canonical lowercase UUIDv4。`randomUUID` は使わない（secure context 不要）。UI の採番はすべてここ |
| `logScroll` | §7.2c。`shouldFollowLogTail` は下端 80px 以内または自分の送信直後だけ追従。`logScrollBehavior` は `prefers-reduced-motion` なら `instant`、それ以外は `smooth` |
