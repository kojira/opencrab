# crates/discord-gate

`opencrab-discord-gate` は Discord I/O（text + voice RTP）と protocol 2 変換だけを持つ。core / store には依存しない。

## 契約

- 1 bot token = 1 instance = 1 process。token は `OPENCRAB_GATE_TOKEN`。argv / log に出さない。
- hello は protocol 2 / `kind_id=discord` / `origin_scope=kind_address` / `ingress_discovery=membership`。
- 接続: hello → Discord REST current-user → Gateway READY → `ready`。boot 失敗は hello のあと `failed(code)`。
- ingress は Message Create → `said`。VC STT 成功も同じ `said`（`metadata.source=discord_voice`、origin 無し）。新 EventKind は無い。
- discovery は v15 §2.2。intents は `GUILDS | GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT | GUILD_VOICE_STATES`。
- effect は `say` / `react` / `ui`。`say` のあと、voice 活性 place なら TTS（`maybe_speak`）。`ui create` の renderer はこの slice に無いので fail loud。
- join/leave は gate operation。hello `tools` と `DeclaredDiscordOperations()` は 8 件（§8.1 の 6 + `join_voice_channel` / `leave_voice_channel`）。
- `m: tool` は plugd の現行経路（`address` + `caller`）。権限は grant/standing → `VoiceCaller`。songbird join/leave は子の中。
- `read` は送らない。bind / unbind は受け取っても membership では core が送らない。

## 起動

launcher が `present && enabled && secret 非空` の **dedicated** discord instance を列挙し、`opencrab-gate-runner <db> <uuid> <sock> opencrab-discord-gate` を exec する。secret 空は 1 行ログして未起動。`shared:*` は token 非空でも起動拒否し、§8 未実装を #793 で追跡する。token は argv / log に出さない。

runner は `OPENCRAB_VOICE_CONFIG_B64`（override>default）と、dedicated label から復号した `OPENCRAB_GATE_OWNER_AGENT_ID` を子へ渡す。子は store を読まない。STT/TTS キーは env 名参照のまま。enabled 構築失敗は警告して `voice=None`（起動は止めない）。

## モックで見られること / 実 Discord QC だけ

モック: 権限順、VoiceTick（20ms / 無音 800ms / 最大 15s / 48k→16k）、scripted STT/TTS HTTP、said wire の `source=discord_voice`。

実 Discord QC のみ: songbird 実 join/leave、実 RTP VoiceTick、実 VC TTS 再生、実 OpenAI/VOICEVOX、複数ギルド同時 VC、再 join 時の receiver 張り替え。
