/// 相対パスを `base`（＝プロセス cwd）基準で絶対化する（存在は要求しない）。
///
/// nostaro は**エージェント workspace を cwd にして**起動する（#299）ので、プロセス cwd
/// 基準で組まれた `data/agents/{id}/nostr/config.toml` のような相対パスをそのまま渡すと
/// workspace 配下を探して見失う。spawn 前にここで絶対化して基準ズレを断つ。
///
/// 基準は呼び出し側（[`NostaroCli::plan_cwd_and_config`]）が**1 回だけ**取得して渡す。
/// 取得できない場合は絶対化も cwd 固定も行わない（両方見送り）ので、ここでは fallback を
/// 持たない。
fn absolutize_with(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    base.join(path)
}

/// 鍵ファイル名の stem を英数字のみに安全化する（bech32 npub / hex pubkey は満たす）。
/// パストラバーサル/インジェクション防止。空文字は呼び出し側で fallback/拒否する。
fn sanitize_key_stem(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}

/// nostaro の stderr/エラー文字列から秘密材料をマスクする。
///
/// config パース失敗時、nostaro は config 先頭行（`secret_key = "nsec1..."`）を stderr に
/// エコーする。これを anyhow エラーやログへ載せると平文の秘密鍵が漏れるため、載せる前に
/// **多層防御**で伏せる（#262）:
/// - 第1層: `secret_key` を含む行は行ごと伏せる。nostaro は行番号ガター付き
///   （`1 | secret_key = "..."`）でエコーするため `starts_with` では発火しない。
///   `contains` にして、ガター付き行も、nsec でない hex 秘密（`secret_key = "<64hex>"`）も
///   行ごと落とす。潰しすぎても秘密漏れ側には倒れない。
/// - 第2層: 文字列中の任意の `nsec1...`（bech32）トークンを伏せ字へ置換する。
///
/// `secret_key` を含まない診断行（`missing field ...` / `TOML parse error` 等）は残す。
/// regex 依存を持ち込まないよう手書きで処理する。
fn mask_secrets(input: &str) -> String {
    let mut lines: Vec<String> = input
        .lines()
        .map(|line| {
            if line.contains("secret_key") {
                // ガター（`1 | `）等の前置きは残し、`secret_key` 以降の値部分だけを伏せる。
                // これで行番号など診断に有用な前置きを保ちつつ、秘密値は確実に落とす。
                let idx = line.find("secret_key").unwrap();
                format!("{}secret_key = \"<redacted>\"", &line[..idx])
            } else {
                line.to_string()
            }
        })
        .collect();
    for line in &mut lines {
        *line = redact_nsec_tokens(line);
    }
    lines.join("\n")
}

/// 文字列中の `nsec1<bech32...>` トークンをすべて `nsec1<redacted>` に置換する。
fn redact_nsec_tokens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if s[i..].starts_with("nsec1") {
            out.push_str("nsec1<redacted>");
            i += "nsec1".len();
            // bech32 データ部（小文字英数字）を読み飛ばして落とす。
            while i < bytes.len() && (bytes[i] as char).is_ascii_alphanumeric() {
                i += 1;
            }
        } else {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// 一意な temp path 用のプロセス内カウンタ（同一 pid の並行書き込みでの temp 衝突防止）。
static SECRET_TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 秘密（nsec 等）を含むファイルを**作成時から 0600**で、かつ**アトミックに**書く。
///
/// 一意な temp（同ディレクトリ・0600）へ書いてから `rename` で差し替える。これにより
/// 読み手（nostaro）が**部分書き込みの config を絶対に読まない**（partial read → 既定
/// リレーへ publish のような事故を防ぐ）。同一 config への並行書き込みも、最終パスは
/// 常に完全なファイルを指す（内容は決定的で同一）。unix 以外は通常書き込み。
fn write_secret_file(path: &std::path::Path, contents: &str) -> Result<()> {
    let n = SECRET_TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(), n));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("failed to open secret temp: {}", tmp.display()))?;
        f.write_all(contents.as_bytes())
            .with_context(|| format!("failed to write secret temp: {}", tmp.display()))?;
        f.sync_all().ok();
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&tmp, contents)
            .with_context(|| format!("failed to write secret temp: {}", tmp.display()))?;
    }
    // アトミックに差し替え。失敗したら temp を掃除する。
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("failed to place secret file: {}", path.display()));
    }
    Ok(())
}

/// `nostaro vanity --json` の stdout から鍵を取り出す。進捗ログが混ざりうるので
/// **最後の JSON 行**（`{` 始まり）を採用する。`{"nsec","npub","pubkey"}` を想定。
///
/// **重要**: エラーメッセージに stdout / JSON 行を絶対に載せない。それらは nsec 平文を
/// 含みうる（例: `--json` 非対応版が生鍵を吐く / JSON 破損）。載せると 500 応答やログに
/// 秘密鍵が漏れる。失敗時は固定文言のみを返す。
fn parse_generated_key(stdout: &str) -> Result<GeneratedKey> {
    let line = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.starts_with('{'))
        .ok_or_else(|| anyhow::anyhow!("nostaro vanity: JSON 出力を解釈できません"))?;
    let v: serde_json::Value = serde_json::from_str(line)
        .map_err(|_| anyhow::anyhow!("nostaro vanity: JSON を解釈できません"))?;
    let get = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .unwrap_or_default()
            .to_string()
    };
    let nsec = get("nsec");
    if nsec.is_empty() {
        anyhow::bail!("nostaro vanity: nsec が空です");
    }
    Ok(GeneratedKey {
        nsec,
        npub: get("npub"),
        pubkey: get("pubkey"),
    })
}

/// `nostaro following --out-format json` の JSON（`{"count":N,"users":[{"hex","npub"}]}`）を
/// 照合キーの集合へ変換する（#698）。
///
/// 各 user の `hex`（無ければ `npub`）を [`crate::pubkey::follow_key`] で寄せて集める。
/// **フォールバックを持たない**: JSON 全体が壊れていれば `Err`（呼び出し側が fail-loud）。
/// ただし個々のエントリで hex/npub が両方欠けている行は**その 1 件だけ**飛ばす（壊れた 1 行
/// で全フォロイーを落として全通しへ倒すより安全側 / 空も 0 件の正当な成功と同じ扱い）。
fn parse_following_json(raw: &str) -> Result<std::collections::HashSet<String>> {
    let v: serde_json::Value = serde_json::from_str(raw)
        .context("nostaro following: JSON を解釈できません（#698 フォローリスト取得）")?;
    let users = v
        .get("users")
        .and_then(|u| u.as_array())
        .ok_or_else(|| anyhow::anyhow!("nostaro following: JSON に users 配列がありません"))?;
    let mut set = std::collections::HashSet::with_capacity(users.len());
    for u in users {
        let raw_key = u
            .get("hex")
            .and_then(|x| x.as_str())
            .or_else(|| u.get("npub").and_then(|x| x.as_str()))
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(k) = raw_key {
            set.insert(crate::pubkey::follow_key(k));
        }
    }
    Ok(set)
}

