impl NostaroCli {
    /// 一発実行系（post/reply/dm/zap/upload）を既定 timeout 付きで走らせ stdout を返す。
    async fn run(&self, cmd: Command) -> Result<String> {
        self.run_with_timeout(cmd, self.timeout).await
    }

    /// 指定 timeout でコマンドを走らせ stdout を返す（vanity 等の長時間処理用）。
    async fn run_with_timeout(&self, mut cmd: Command, timeout: Duration) -> Result<String> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| anyhow::anyhow!("nostaro timed out after {}s", timeout.as_secs()))?
            .context("failed to run nostaro")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // config パース失敗時、nostaro は config 先頭行（`secret_key = "nsec1..."`）を
            // stderr にエコーする。そのまま anyhow エラー/ログへ載せると平文 nsec が漏れる
            // ため、秘密材料をマスクしてから載せる（#262 セキュリティ所見）。
            anyhow::bail!(
                "nostaro failed ({}): {}",
                output.status,
                mask_secrets(stderr.trim())
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// `nostaro post -- "<text>"` — 新規ノート投稿。
    ///
    /// 全ての positional 引数の前に `--`（オプション終端）を置く。target/text/recipient
    /// 等はモデル/受信イベント由来で、`-` 始まりの値をフラグと誤解釈させない
    /// （引数インジェクション対策）。
    pub async fn post(&self, agent_id: &str, text: &str, from: Option<&str>) -> Result<String> {
        let mut cmd = self.command_for(agent_id, from)?;
        cmd.arg("post").arg("--").arg(text);
        self.run(cmd).await
    }

    /// `nostaro reply -- <target> "<text>"` — 返信。target は note1.../hex id。
    pub async fn reply(
        &self,
        agent_id: &str,
        target: &str,
        text: &str,
        from: Option<&str>,
    ) -> Result<String> {
        let mut cmd = self.command_for(agent_id, from)?;
        cmd.arg("reply").arg("--").arg(target).arg(text);
        self.run(cmd).await
    }

    // #514: DM 送信メソッド（`nostaro dm send`）は撤去した。DM は秘密鍵漏洩で過去に
    // 遡って全部読めるため送信禁止。`nostr_dm` ツールの撤去に加え、送信の実プリミティブ
    // 自体をここから消し、`nostr_run dm` passthrough も `PASSTHROUGH_DENIED_SUBCOMMANDS`
    // で塞いでいる（送信の全経路を封じる）。

    /// `nostaro zap -m <message> -- <recipient> <amount>`。
    /// `-m` は positional の前（`--` 後に置くと value 扱いされるため）。
    pub async fn zap(
        &self,
        agent_id: &str,
        recipient: &str,
        amount: u64,
        message: Option<&str>,
        from: Option<&str>,
    ) -> Result<String> {
        let mut cmd = self.command_for(agent_id, from)?;
        cmd.arg("zap");
        if let Some(m) = message.filter(|s| !s.is_empty()) {
            // 値も = 形式で確実に束ねる（`-` 始まりのメッセージ対策）。
            cmd.arg(format!("-m={m}"));
        }
        cmd.arg("--").arg(recipient).arg(amount.to_string());
        self.run(cmd).await
    }

    /// `nostaro upload -- <path>` — Blossom アップロード。返り値は URL。
    pub async fn upload(&self, agent_id: &str, path: &str, from: Option<&str>) -> Result<String> {
        let mut cmd = self.command_for(agent_id, from)?;
        cmd.arg("upload").arg("--").arg(path);
        self.run(cmd).await
    }

    /// `nostr_run`（server-own passthrough / #268）で**拒否**するサブコマンド。
    ///
    /// - `init`: 鍵の作成/上書き。鍵管理は opencrab の `nostr_generate_key` /
    ///   `nostr_switch_identity` に閉じる（passthrough から鍵をいじらせない）。
    /// - `watch`: 受信は per-agent ゲートウェイ管理が担う。passthrough から
    ///   `--no-mention-only` 等で無制限 watch を上げさせない（かつ長時間ブロックを避ける）。
    /// - `relay`: リレー設定の真実源は opencrab の DB（`agent_nostr_config`）で、config.toml は
    ///   materialize で毎回上書きされる。passthrough から `relay add/remove` すると config.toml
    ///   だけ書き換わって DB と desync し、次の gateway start / switch_identity で黙って揮発する。
    ///   よってリレー管理は opencrab の DB 経路（configure_nostr / ダッシュボード）に閉じる。
    ///   壊れている（揮発する）機能を塞ぐので**非劣化ではない**。
    /// - `dm`（#514）: DM 送信は禁止。DM は暗号化されていても秘密鍵が漏れた時点で過去に
    ///   遡って全部読めるため、その前提ごと無くす（オーナー決定）。`nostr_dm` ツールの撤去
    ///   だけでは `nostr_run dm send` から通ってしまう（passthrough は inner ツール名を
    ///   隠しても能力は塞げない / #306）ので、送信のもう一方の経路であるここも塞ぐ。
    ///   `nostr_run dm ...` は拒否される。private な話は Discord の DM か指定チャンネルへ。
    /// - `event` は**許可**（#699・オーナー裁定 2026-08-19）: 任意 kind の publish は
    ///   パブリックチャット作成（kind:40）・投稿（kind:42）など正当用途があり、塞ぐ不自由が
    ///   利益を上回っていた。`event -k 4` で DM kind を生発行できる理論上の迂回は残るが、
    ///   DM として機能させるには**暗号化まで自前でイベントを組む**必要があり実用的でない
    ///   （#514 が塞ぎたかった「便利な暗号化 DM」の経路は `dm` サブコマンドで、そちらの
    ///   deny は維持する）。
    ///
    /// これ以外のサブコマンドは**そのまま nostaro に委ねる**（Nostr 仕様の判断は
    /// opencrab で再実装せず nostaro に委譲する＝非劣化）。
    pub const PASSTHROUGH_DENIED_SUBCOMMANDS: &'static [&'static str] =
        &["init", "watch", "relay", "dm"];

    /// nostaro サブコマンドを**薄く passthrough 実行**する（#268）。
    ///
    /// `nostaro --config <data/agents/{agent_id}/nostr/config.toml> <subcommand> [args]` を
    /// **構造化引数**で起動する（シェル文字列を組まないので注入不可）。守るのは 2 点だけ:
    ///
    /// 1. **鍵のエージェント間混同防止**: config は常に `agent_id` のもの
    ///    （[`base_command`](Self::base_command)）。`--config` を args で上書きさせない。
    /// 2. **nsec 隠蔽**: agent は nsec を引数に持たない前提に加え、`init` を拒否して鍵の
    ///    作成/上書きを塞ぎ、stdout / エラー出力の双方を [`mask_secrets`] に通す。
    ///
    /// `init`/`watch`/`relay`/`dm` は拒否し、それ以外（event 含む）は素通しする。config.toml 未
    /// materialize（鍵未採用）なら nostaro を spawn せず明示エラーを返す。
    pub async fn run_passthrough(
        &self,
        agent_id: &str,
        subcommand: &str,
        args: &[String],
    ) -> Result<String> {
        let sub = subcommand.trim();
        if sub.is_empty() {
            anyhow::bail!("subcommand が空です");
        }
        // deny: 鍵の作成/上書き（init）・無制限受信（watch）・リレー編集（relay）・
        // DM 送信（dm / #514）。event（任意 kind publish）は #699 のオーナー裁定で許可。
        // relay は config.toml だけ書き換えて DB(agent_nostr_config) と desync し次の
        // gateway start / switch_identity で揮発するため塞ぐ。
        if Self::PASSTHROUGH_DENIED_SUBCOMMANDS.contains(&sub) {
            anyhow::bail!(
                "nostr_run では '{sub}' は実行できません（init は nostr_generate_key / \
                 nostr_switch_identity に、watch はゲートウェイ管理に閉じています。リレー設定は \
                 opencrab 側（configure_nostr / ダッシュボード）で管理してください。dm は #514 で \
                 禁止です — DM は秘密鍵漏洩で過去に遡って読めるため扱いません。private な話は \
                 Discord の DM か指定チャンネルを使ってください）"
            );
        }
        // `--config` の上書きを封じる（config は常にあなた自身の鍵設定＝鍵混同防止を回避
        // させない）。それ以外のフラグは nostaro にそのまま委ねる。
        if args
            .iter()
            .any(|a| a == "--config" || a.starts_with("--config="))
        {
            anyhow::bail!(
                "--config は指定できません（config は常にあなた自身の Nostr 鍵設定を使います）"
            );
        }
        // config.toml が無い＝まだ本鍵を採用していない。nostaro を spawn せず明示エラー。
        let config_path = Self::agent_config_path(agent_id)?;
        if !config_path.exists() {
            anyhow::bail!(
                "Nostr の鍵がまだ採用されていません（config.toml が未生成）。先に \
                 nostr_switch_identity で鍵を採用してください"
            );
        }
        let mut cmd = self.base_command(agent_id)?;
        cmd.arg(sub);
        for a in args {
            cmd.arg(a);
        }
        // `run` は失敗時に stderr を mask する。成功時の stdout も念のため mask を通す
        // （config を表示しうる系のサブコマンドで万一 nsec が混じっても伏せる / 多層防御 #263）。
        self.run(cmd).await.map(|out| mask_secrets(&out))
    }

    /// `nostaro pubkey` — このエージェント（config）の公開鍵（hex）を返す。
    /// 自分の投稿への自己返信ループを防ぐために使う。
    pub async fn pubkey(&self, agent_id: &str) -> Result<String> {
        let mut cmd = self.base_command(agent_id)?;
        cmd.arg("pubkey");
        self.run(cmd).await
    }

    /// **生成鍵**（`from` npub）の公開鍵（hex）を返す（#620・identity 切替）。
    ///
    /// identity 切替では DB の本鍵を新鍵へ更新する**前**に新 pubkey が要る。本鍵プロバイダは
    /// DB を読むため `pubkey()` では旧鍵の pubkey が返る。ここは生成鍵を env で注入して
    /// nostaro に引かせるので、**新鍵が実際に使えること（検証）と新 pubkey の取得**を DB
    /// 更新前に同時に行える。
    pub async fn pubkey_from(&self, agent_id: &str, from_npub: &str) -> Result<String> {
        let mut cmd = self.generated_key_command(agent_id, from_npub)?;
        cmd.arg("pubkey");
        self.run(cmd).await
    }

    /// このエージェント自身の**フォローリスト（kind:3）**を取得し、照合キーの集合で返す（#698）。
    ///
    /// 未信頼作者の元栓（[`crate::manager`] のゲート）は「フォロイー ∪ owner 以外はターンを
    /// 着火させず store にも入れない」を実現する。その**フォロイー集合の権威データ**がこれ。
    ///
    /// `nostaro following --out <file> --out-format json`（npub 引数なし＝自分自身）で取得する。
    /// `following` は JSON をファイルにしか吐かない（stdout はサマリのみ）ので、per-agent の
    /// nostr ディレクトリ（config.toml と同じ場所）へ書かせて読み返す。返すのは
    /// [`crate::pubkey::follow_key`] で寄せた照合キーの集合で、ゲート側も同じ関数でキーを作る。
    ///
    /// **フォールバックを持たない**（#698 の要求）: 取得できなければ `Err` を返す。呼び出し側は
    /// これを「黙って全通しへ倒す」のではなく、起動中止（[`spawn_agent_gateway`]）または前回値
    /// 保持（定期更新）で **うるさく失敗**させる。空のフォローリスト（0 フォロー）は正当な
    /// 成功（空集合）で、エラーではない。
    pub async fn fetch_following(
        &self,
        agent_id: &str,
    ) -> Result<std::collections::HashSet<String>> {
        // nostaro は cwd=workspace で走る（#299）ので、`--config` と同じく `--out` も**絶対化**して
        // 「child が書く場所」と「ここで読む場所」を一致させる。current_dir が取れない degrade 時は
        // child も process cwd を継承するので、相対のままで両者一致する（plan_cwd_and_config と同じ筋）。
        let out_rel = Self::agent_nostr_dir(agent_id)?.join("following.json");
        let out_path = match std::env::current_dir() {
            Ok(cwd) => absolutize_with(&out_rel, &cwd),
            Err(_) => out_rel,
        };
        // 親ディレクトリを用意（base_command も config 親を作るが、out を絶対化した経路でも確実に）。
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        // 前回の残骸を読み違えないよう、実行前に消す（following がファイルを上書きするが、
        // 取得が途中失敗したときに古い内容を「成功」と誤読しないための保険）。
        let _ = std::fs::remove_file(&out_path);
        let mut cmd = self.base_command(agent_id)?;
        cmd.arg("following")
            .arg(format!("--out={}", out_path.display()))
            .arg("--out-format=json");
        // stdout はサマリのみ（本体は out ファイル）。失敗時は run が stderr を mask する。
        self.run(cmd).await.context(
            "nostaro following の実行に失敗しました（フォローリスト取得。#698 の元栓は \
             フォローリストを権威データにするため、取得不能のまま全通しへは倒しません）",
        )?;
        let raw = std::fs::read_to_string(&out_path).with_context(|| {
            format!(
                "following の JSON 出力を読めませんでした: {}",
                out_path.display()
            )
        })?;
        parse_following_json(&raw)
    }

    /// `nostaro vanity --json [--prefix=<p>]` — 新規鍵を生成して返す。
    ///
    /// prefix は npub の `npub1` 以降に前置される bech32 文字列。空なら通常のランダム鍵。
    /// **config を読まない**（新規鍵生成なので既存 nsec に依存しない）ため `--config` は
    /// 付けない。探索が終わらないよう prefix を検証し、長めの vanity_timeout を使う。
    pub async fn vanity(&self, prefix: &str) -> Result<GeneratedKey> {
        let prefix = prefix.trim().to_lowercase();
        validate_vanity_prefix(&prefix)?;
        // 同時実行を 1 に絞る（各生成は最大 vanity_timeout ぶん nostaro プロセスを
        // 抱える。並列に溢れさせない = DoS/資源枯渇の防止）。生成は ≤3 文字で通常
        // 即時なので、待ちは実質発生しない。
        let _permit = self
            .vanity_gate
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("vanity ゲートが閉じています"))?;
        // config 非依存なので base_command は使わず素で組む。
        let mut cmd = Command::new(&self.binary_path);
        cmd.kill_on_drop(true);
        cmd.arg("vanity").arg("--json");
        if !prefix.is_empty() {
            // `-` 始まりはあり得ない（bech32 charset 検証済み）が = 形式で束ねる。
            cmd.arg(format!("--prefix={prefix}"));
        }
        let out = self.run_with_timeout(cmd, self.vanity_timeout).await?;
        parse_generated_key(&out)
    }

    /// per-agent の nostaro config.toml を DB 由来のリレーから materialize する。
    ///
    /// #620: **secret_key 行はもう書かない**（鍵は実行時に env で注入する）。config には
    /// relays/default_relays/blossom だけを書く。エージェントがこの config を読んでも平文の
    /// 鍵は目に入らない。relays は送信（post/reply）が publish するリレー。受信は watch の
    /// フラグで別途明示する。partial read で誤ったリレーへ繋がせないよう、書き込みは従来どおり
    /// アトミックにする（[`write_secret_file`]）。
    pub fn materialize_config(
        agent_id: &str,
        relays: &[String],
        blossom_server: Option<&str>,
    ) -> Result<PathBuf> {
        let path = Self::agent_config_path(agent_id)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create nostr dir: {}", parent.display()))?;
        }
        // TOML 文字列を壊す/追記させない文字（`"` `\` 改行）を除去する（値は URL 前提なので
        // 本来含まれない。防御的にサニタイズ）。
        let esc = |s: &str| {
            s.chars()
                .filter(|c| !matches!(c, '"' | '\\' | '\n' | '\r'))
                .collect::<String>()
        };
        let relay_list = relays
            .iter()
            .map(|r| format!("\"{}\"", esc(r)))
            .collect::<Vec<_>>()
            .join(", ");
        // nostaro は `relays` と `default_relays` の**両方**を必須フィールドとして要求する
        // （どちらか一方だけだと `missing field ...` で config パースが失敗し、post/watch/pubkey
        // など Nostr 全操作が止まる。#262）。opencrab は送信/受信リレーを常にフラグで明示する
        // ため両者は同値でよい。secret_key は Option なので省略しても parse できる（鍵は env）。
        let mut toml = format!("relays = [{relay_list}]\ndefault_relays = [{relay_list}]\n");
        if let Some(b) = blossom_server.filter(|s| !s.is_empty()) {
            toml.push_str(&format!("blossom_server = \"{}\"\n", esc(b)));
        }
        write_secret_file(&path, &toml)?;
        Ok(path)
    }

    /// LLM が生成した鍵の nsec を**サーバ内に 0600 で保存**する（LLM には返さない）。
    ///
    /// 保存先は per-agent の `data/agents/{id}/nostr/generated-keys/<npub>.nsec`。
    /// ファイル名は npub（無ければ pubkey）を bech32/hex 文字に限定して安全化する
    /// （パストラバーサル/インジェクション防止）。返り値は保存パス。
    pub fn save_generated_key(&self, agent_id: &str, key: &GeneratedKey) -> Result<PathBuf> {
        let dir = Self::agent_nostr_dir(agent_id)?.join("generated-keys");
        // 失敗経路でも鍵の**所在（保存先パス）をエラーに載せない**（#241）。成功経路は
        // 返り値の PathBuf をツール層（`nostr_generate_key`）が捨てて npub だけ返す＝所在を
        // LLM に渡さない。だが失敗経路の `with_context` がパスを載せると、その保護が失敗時
        // だけ破れ、エラーがツール結果としてそのままエージェントへ渡る。所在は**サーバログ
        // にだけ**残し、返すエラーは「失敗した」事実のみにする（運用者はログで所在を追える）。
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(dir = %dir.display(), error = %format!("{e}"), "generated key: 保存先ディレクトリの作成に失敗");
            anyhow::bail!("鍵の保存先ディレクトリの作成に失敗しました");
        }
        // ファイル名は英数字のみ（bech32/hex は満たす）。空や異物は fallback。
        let stem = sanitize_key_stem(&key.npub);
        let stem = if stem.is_empty() {
            let hexed = sanitize_key_stem(&key.pubkey);
            if hexed.is_empty() {
                "key".to_string()
            } else {
                hexed
            }
        } else {
            stem
        };
        let path = dir.join(format!("{stem}.nsec"));
        // #620: 平文ではなく暗号文（`enc:v1:…`）で保存する（エージェントが `../nostr/
        // generated-keys/*.nsec` を読んでも平文の鍵が目に入らない）。マスターキー未注入
        // （テスト）は平文フォールバック。
        let material = self.encrypt_secret(&key.nsec)?;
        // `write_secret_file` のエラーには path が Context として載るため、ここで握り、
        // 所在はログにだけ残して、返すエラーからは落とす（上と同じ理由 = #241）。
        if let Err(e) = write_secret_file(&path, &material) {
            tracing::warn!(path = %path.display(), error = %format!("{e:#}"), "generated key: 鍵ファイルの保存に失敗");
            anyhow::bail!("生成した鍵の保存に失敗しました");
        }
        Ok(path)
    }

    /// watch 用の Command を組む（spawn はループ側が行い、stdout の JSONL を読む）。
    ///
    /// リレー/フィルタは**必ずフラグで明示**して渡す（config の default に依存しない
    /// ＝指定リレー以外へ繋がせない）。`--json` で JSONL を stdout に出させる。
    ///
    /// ## 条件の結合は `--match=any` で固定する（#278）
    ///
    /// nostaro の `watch` は p タグ（mention-only）／keyword／author の 3 条件を
    /// `--match any|all` で結合する。opencrab は **`any`（OR）を明示的に渡す**。既定と
    /// 同じ値だが、**受信セマンティクスを argv に焼く**ことで nostaro 側の既定が将来
    /// 変わっても opencrab の受信が黙って変わらないようにする。
    ///
    /// `all`（AND）を選ばない理由は、nostaro では **mention-only も 1 つの条件**であり、
    /// `all` にすると「自分宛（p タグ）**かつ** keyword 一致」になるからである。運用者が
    /// keyword を設定しているエージェントでは、
    ///
    /// - 本文に keyword を含まない e/p タグだけの返信が落ちる（#271 で直したい事象そのもの）、
    /// - 本文が暗号文/絵文字である kind:4・1059（DM）や kind:7（リアクション）が
    ///   keyword 一致しえないので全部落ちる、
    /// - 「自分宛でない keyword 一致投稿を拾う」という運用者の意図（keyword 監視）も落ちる、
    ///
    /// という三重の劣化になる。`any` なら「自分宛は必ず届く（#271）＋ 運用者が明示した
    /// keyword/author の分が上乗せされる」となり、旧挙動を狭めない。
    ///
    /// なお `--mention-only` は nostaro 側の既定 true に委ね、**`--no-mention-only` は
    /// 絶対に渡さない**（渡すと p タグ条件が消えて全ノート購読になりうる）。この不変条件は
    /// `test_watch_command_never_disables_mention_only` で固定している。
    pub fn build_watch_command(&self, agent_id: &str, config: &NostrConfig) -> Result<Command> {
        let mut cmd = self.base_command(agent_id)?;
        cmd.arg("watch").arg("--json");
        // 条件の結合方法（OR）。既定と同値でも明示して契約を argv に残す（#278）。
        cmd.arg("--match=any");
        // フラグ値は `--flag=value` の = 形式で束ねる（`-` 始まりの author/keyword を
        // 別フラグと誤解釈させない = 引数インジェクション対策）。
        for relay in config.effective_relays() {
            cmd.arg(format!("--relay={relay}"));
        }
        for author in &config.filter.authors {
            cmd.arg(format!("--author={author}"));
        }
        for keyword in &config.filter.keywords {
            cmd.arg(format!("--keyword={keyword}"));
        }
        for kind in config.effective_kinds() {
            cmd.arg(format!("--kind={kind}"));
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(cmd)
    }
}

