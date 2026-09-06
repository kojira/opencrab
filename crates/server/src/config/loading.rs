// ---------- Config loading ----------

/// Load config from a TOML file, expanding `${VAR}` placeholders with env vars.
pub fn load_config(path: &str) -> Result<AppConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path))?;

    let expanded = expand_env_vars(&raw);

    let mut config: AppConfig =
        toml::from_str(&expanded).with_context(|| "Failed to parse config TOML")?;

    // owner は入口で正規化する。`.env` からのコピペで前後に空白が混ざると、
    // `api::is_owner_id`（trim 済み比較）を通る経路では owner と認識されるのに、
    // 生比較が残っている下位 crate（form/modal、ボタン操作）だけ無言で拒否される。
    // 判定述語を下位 crate へ移す整理は #174。
    let owner = config.gateway.discord.owner_discord_id.trim();
    if owner.len() != config.gateway.discord.owner_discord_id.len() {
        config.gateway.discord.owner_discord_id = owner.to_string();
    }

    // dispatch の kill switch は環境変数で上書きできる（`.env` だけで切り戻せるように）。
    if let Some(v) = auto_dispatch_from_env() {
        config.subtask.auto_dispatch = v;
    }

    Ok(config)
}

/// Replace `${VAR_NAME}` patterns with corresponding environment variable values.
/// Unknown variables are replaced with empty strings.
///
/// **単一パス走査**（#171）: 入力を左から右へ一度だけ走り、置換した値は**再走査しない**。
/// 旧実装は毎回先頭から `find` し直して展開結果も再解釈していたため、以下の実測済み
/// 失敗モードがあった。単一パスにすることで、上限や反復回数の管理なしに全て解消する。
///
/// - **起動ハング**: 値が自分自身を参照する形（値に `${SAME_VAR}` を含む）だと置換が
///   収束せず無限ループになっていた。再走査しないので、置換値に `${...}` が現れても
///   もう展開されず、必ず有限で終わる。
/// - **設定行の無言消失**: `}` を全体から探すため、`${` に対応する `}` が同じ行に無いと
///   ずっと後方の `}`（別の設定行）まで飲み込み、間の行が丸ごと消えていた。ここでは
///   `}` の探索を**同じ行の中（次の `\n` まで）**に限定し、閉じが無ければ `${` を
///   リテラルとして残すので、後続行は保存される。
/// - **原因の追えないパースエラー**: 値に `"` / 改行 / `${` が含まれると TOML を壊すが、
///   どの変数が原因か分からなかった。そうした値を検出したら**変数名を添えて `warn!`** する
///   （ロガーは `load_config` 呼び出し前に初期化済み。`hot_reload` 経路でも初期化済み）。
///
/// この関数は「向き」を狭める方向にのみ働く（再帰展開という以前は暗黙にできていた挙動を
/// 止める）。設定値に環境変数参照をネストさせる運用は無い（値は token / ID 等）。
pub(crate) fn expand_env_vars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    loop {
        let start = match rest.find("${") {
            Some(pos) => pos,
            None => {
                out.push_str(rest);
                break;
            }
        };
        out.push_str(&rest[..start]);
        // `${` の直後から。`}` は**同じ行の中だけ**で探す（別行を飲み込まない = 行消失防止）。
        let after = &rest[start + 2..];
        let line_end = after.find('\n').unwrap_or(after.len());
        match after[..line_end].find('}') {
            None => {
                // 同じ行に閉じ `}` が無い → 展開せず `${` をリテラルとして残し、続きを走査する。
                warn!(
                    "config: `${{` に同じ行で対応する `}}` が無いためリテラルとして残します（設定の記法ミスの可能性）"
                );
                out.push_str("${");
                rest = after;
            }
            Some(close) => {
                let var_name = &after[..close];
                let value = std::env::var(var_name).unwrap_or_default();
                if value.contains('"') || value.contains('\n') || value.contains("${") {
                    warn!(
                        var = %var_name,
                        "config: 環境変数の値に TOML を壊す/再展開を誘発しうる文字（\" ・改行・${{）が含まれます。リテラルとして（再展開せず）差し込みます"
                    );
                }
                out.push_str(&value);
                // 置換値は out へ入れたきり再走査しない（単一パス）。`}` の次から続ける。
                rest = &after[close + 1..];
            }
        }
    }
    out
}

