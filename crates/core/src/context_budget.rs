//! 文脈予算とモデル pricing ゲート（#412 / #518 手順 3〜4）。
//!
//! `model_pricing` の `context_window` からターンごとの会話予算を算出し、未登録モデルを
//! 設定時に弾く。server / gateway の型に依存しない純粋ロジックなので core に置く。
//! 呼び出し元は `server::process`（既存パスを保つ再エクスポート）。

/// `provider:model` 形式（またはモデル名のみ）を pricing 参照用に分割する。
pub fn split_llm_model_spec(full: &str) -> (&str, &str) {
    if let Some(i) = full.find(':') {
        (&full[..i], &full[i + 1..])
    } else {
        ("", full)
    }
}

/// context_window が不明な場合のデフォルト予算（トークン数）。
const DEFAULT_CONTEXT_BUDGET_TOKENS: usize = 100_000;

/// provider ごとの「会話予算の保守的な天井」（トークン数）。`None` は天井なし（#535）。
///
/// # なぜ天井が要るか
/// [`compute_context_budget`] は `model_pricing.context_window × compaction_ratio` を
/// 返すだけで、その値が**実際にバックエンドへ送れる量**を超えていないかを一切見ていなかった。
/// 2026-08-10、`chatgpt` の `context_window` を 400,000 → 1,050,000（API カタログ値）へ
/// 上げたところ、予算 = 1,050,000 × 0.5 = 525,000 になり、実際の履歴（約 465,000）が
/// 予算内に収まってコンパクションが働かず、毎リクエストが Codex OAuth 経路の実上限で
/// 拒否され、heartbeat が数時間サイレントに停止した（`llm_logs.error_body` に
/// "Your input exceeds the context window of this model." が 07-25〜08-10 で 54 件残って
/// いたが誰も見ていなかった）。天井が無いと `model_pricing` を触るたびに再発する。
///
/// # 350,000 の由来（後から検証できるように）
/// `chatgpt` provider が叩く **Codex OAuth 経路の実測上限は 371,678 ok / 約 371,864 fail
/// （±186）**。そこから出力予約（`max_output_tokens`）・system prompt・tool 定義ぶんを
/// 引いた**保守値**が 350,000。**正確な上限を当てることが目的ではなく、100% 失敗する
/// 予算を返さない天井が存在することが目的**なので、余裕を多めに取ってある。実上限が
/// 変わったらこの数字を更新する。
///
/// # provider 粒度であって経路粒度ではない（将来の落とし穴・必読）
/// 実上限は本来 `(provider, 認証経路)` の組で決まる。ここが provider 粒度で済んでいるのは、
/// **現状 `chatgpt` provider = Codex OAuth 経路が 1:1 だから**にすぎない。将来 `chatgpt`
/// provider から**真の API 経路（カタログ 1,050,000）**を使う口を足したら、この天井は
/// その経路を過剰に絞る。そのときは経路を区別できる形へ持ち替えること。静的値が実測から
/// ズレ始めたら「拒否レスポンスから学習して下げる」動的方式（#535 の次段）へ移す。
fn backend_budget_ceiling(provider: &str) -> Option<usize> {
    match provider {
        "chatgpt" => Some(350_000),
        _ => None,
    }
}

/// 未登録モデルを設定しようとしたときのエラーメッセージ（#412）。
///
/// **登録方法まで書く。** 拒否だけして先へ進む手段を示さないと、「設定できないが
/// どうすれば設定できるかも分からない」で止まる。
///
/// **フロントの導線がこの文言に依存している（#482）。** `web/src/pages/AgentOverview.tsx`
/// の `UNREGISTERED_MARKER`（= `"has no context_window registered in model_pricing"`）と
/// 正規表現 `/model "([^"]+)"/` が、このメッセージから「未登録である」ことと spec を
/// 拾って、その場に登録フォームを出す。**この文言を変えるなら AgentOverview.tsx も
/// 直せ。** さもないと導線が黙って出なくなり、運用者は curl 直叩きに戻る。
/// 契約は `missing_message_keeps_frontend_link_contract` テストで固定している。
pub fn model_context_window_missing_message(spec: &str) -> String {
    format!(
        "model \"{spec}\" has no context_window registered in model_pricing. \
         Register it first: PUT /api/llm/model-pricing with body \
         {{\"provider\": \"...\", \"model\": \"...\", \"input_price_per_1m\": 0.0, \
         \"output_price_per_1m\": 0.0, \"context_window\": <max tokens>}}. \
         Current registrations: GET /api/llm/model-pricing."
    )
}

/// `model_pricing` を引くときのキー。**投入 API の保存キーと同じ正規化**（両端の
/// 空白を落とす）を掛ける。
///
/// 揃えないと「登録したのに未登録と言われる」が起きる。投入側（`PUT
/// /api/llm/model-pricing`）は trim して保存するので、参照側だけ生のまま引くと、
/// 両端に空白のある spec が gate も実行時の予算計算も外す。
fn model_pricing_key(provider: &str, model: &str) -> (String, String) {
    (provider.trim().to_string(), model.trim().to_string())
}

/// `provider:model` 形式の spec を比較用に正規化する（#412）。
///
/// `model_pricing` を引くキーと同じ正規化を通すので、**両端の空白が付いた / 外れた
/// だけの spec は「同じ」**と判定される。生文字列で比べると、実際には同じモデルを
/// 指しているのに「変わった」ことになる。
pub fn normalize_model_spec(spec: &str) -> String {
    let (provider, model) = split_llm_model_spec(spec);
    let (provider, model) = model_pricing_key(provider, model);
    format!("{provider}:{model}")
}

/// 文脈予算に使える `context_window` か（#412）。
///
/// `None` はもちろん、**0 以下も未登録扱い**にする。0 では予算が消え、負では
/// `as usize` で桁違いの値へ巻き上がって上限が事実上無くなる。投入 API は `<= 0` を
/// 弾くので通常は作れないが、読み出し側でも倒しておく。この PR の主題は
/// 「`model_pricing` に変な行を作らせない」であって、変な行を信じることではない。
fn usable_context_window(row: &opencrab_db::queries::ModelPricingRow) -> Option<i32> {
    row.context_window.filter(|w| *w > 0)
}

/// `provider:model` 形式の spec が `model_pricing` に `context_window` を持つ行を
/// 持つか検証する（#412）。
///
/// 文脈予算は `context_window × compaction_ratio` で決まる。行が無いと
/// [`compute_context_budget`] が既定値へ落ち、「データが無い」と「その値だと決めた」
/// が区別できなくなる。**壊れた状態を作れなくする**ため、モデルを設定する瞬間に弾く。
///
/// DB 参照に失敗した場合も Err にする（fail-closed）。登録されていることを確認
/// できていない以上、通してはいけない。
pub fn ensure_model_context_window_registered(
    conn: &rusqlite::Connection,
    spec: &str,
) -> Result<(), String> {
    let (provider, model) = split_llm_model_spec(spec);
    let (provider, model) = model_pricing_key(provider, model);
    match opencrab_db::queries::get_model_pricing(conn, &provider, &model) {
        // 使えない `context_window`（NULL / 0 以下）は行があっても未登録扱い。
        // gate と実行時で判定が食い違うと「設定は通ったのに予算だけ既定へ落ちる」。
        Ok(Some(p)) if usable_context_window(&p).is_some() => Ok(()),
        Ok(_) => Err(model_context_window_missing_message(spec)),
        Err(e) => Err(format!(
            "failed to look up model_pricing for \"{spec}\": {e}"
        )),
    }
}

/// エージェントのモデルを**新しく設定するとき**だけ、`model_pricing` の登録を
/// 要求する（#412）。
///
/// 既に入っている値をそのまま送り直す更新（識別情報だけを編集する PUT など）は
/// 素通しする。ここで既存値まで弾くと、登録前から動いているエージェントが
/// 名前ひとつ変えられなくなる。
///
/// 空文字 / 未指定は「グローバル既定に従う」であってモデルの指定ではないので、
/// これも対象外（既定側は config のホットリロードで検証する）。
/// `effective_model_for_agent` が空を弾いて `default_model` へ落とすため、
/// 空文字がそのまま実効モデルになることはない。
///
/// `agents.model` を書き換える経路（`PUT`/`PATCH /api/agents/{id}` と
/// `configure_self` ツール）はすべてここを通す。
pub fn check_agent_model_change(
    conn: &rusqlite::Connection,
    existing: Option<&opencrab_db::queries::AgentRow>,
    new_model: Option<&str>,
) -> Result<(), String> {
    let Some(new_model) = new_model.filter(|m| !m.is_empty()) else {
        return Ok(());
    };
    if existing.and_then(|a| a.model.as_deref()) == Some(new_model) {
        return Ok(());
    }
    ensure_model_context_window_registered(conn, new_model)
}

/// 同じ (provider, model) の WARN を 1 度だけに絞る（#412）。
///
/// [`compute_context_budget`] は全ターンで通るため、そのまま出すと登録が済むまで
/// 同じ 1 行がログを埋める。登録されれば分岐自体に来なくなるので、解除は要らない。
/// キーは実際に使われた spec の数だけで、増え続けることはない。
fn warn_once_for_model(kind: &'static str, provider: &str, model: &str) -> bool {
    use std::sync::{LazyLock, Mutex};
    /// 既に WARN を出した (種別, provider, model)。
    type WarnedModels = std::collections::HashSet<(&'static str, String, String)>;
    static WARNED: LazyLock<Mutex<WarnedModels>> =
        LazyLock::new(|| Mutex::new(WarnedModels::new()));
    match WARNED.lock() {
        Ok(mut seen) => seen.insert((kind, provider.to_string(), model.to_string())),
        // 抑止のための状態が壊れたなら、黙らせるより出す方を選ぶ。
        Err(_) => true,
    }
}

/// context_budget_tokens を呼び出し元で計算するヘルパー。
/// model_pricing の context_window と compaction_ratio から予算を算出する。
///
/// 行が引けないときは既定値へ落とすが、**黙って落とさない**（#412）。設定時に
/// [`ensure_model_context_window_registered`] で弾くので通常は到達しないが、弾く前に
/// 入った既存データが残りうる。実行中のエージェントを止めないため WARN に留め、
/// 毎ターン同じ行が出ないよう (provider, model) ごとに 1 度だけ出す。
pub fn compute_context_budget(
    conn: &rusqlite::Connection,
    provider: &str,
    model: &str,
    compaction_ratio: f64,
) -> usize {
    let (provider, model) = model_pricing_key(provider, model);
    // #541: lookup 失敗も未登録も、既定 context_window に倒して**同じ出口**（下の予算計算
    // ＋天井）へ合流させる。以前は lookup 失敗だけ early return で `backend_budget_ceiling`
    // を通らず、未登録経路と非対称だった（実害は無いが構造の穴）。WARN は経路ごとに別 kind
    // で 1 回ずつ出す（`lookup_failed` / `unregistered`）。
    let context_window = match opencrab_db::queries::get_model_pricing(conn, &provider, &model) {
        Ok(row) => match row.as_ref().and_then(usable_context_window) {
            Some(w) => w as usize,
            None => {
                if warn_once_for_model("unregistered", &provider, &model) {
                    tracing::warn!(
                        provider = %provider,
                        model = %model,
                        "no usable context_window in model_pricing (missing row, NULL, or \
                         non-positive); falling back to default \
                         ({DEFAULT_CONTEXT_BUDGET_TOKENS}). Register it with \
                         PUT /api/llm/model-pricing so the budget matches the real model."
                    );
                }
                DEFAULT_CONTEXT_BUDGET_TOKENS
            }
        },
        Err(e) => {
            if warn_once_for_model("lookup_failed", &provider, &model) {
                tracing::warn!(
                    provider = %provider,
                    model = %model,
                    "model_pricing lookup failed; falling back to default context window \
                     ({DEFAULT_CONTEXT_BUDGET_TOKENS}): {e}"
                );
            }
            DEFAULT_CONTEXT_BUDGET_TOKENS
        }
    };
    let budget = ((context_window as f64) * compaction_ratio) as usize;
    // #535: 予算が provider の実上限（保守天井）を超えていたら頭を抑える。
    // 天井を超える予算はコンパクションが働かず、送っても 100% 拒否される。
    // 噛んだときだけ WARN 1 回（毎ターン通る経路なので抑止する）。
    match backend_budget_ceiling(&provider) {
        Some(ceiling) if budget > ceiling => {
            if warn_once_for_model("over_backend_ceiling", &provider, &model) {
                tracing::warn!(
                    provider = %provider,
                    model = %model,
                    budget,
                    ceiling,
                    "context budget ({budget}) exceeds the conservative backend ceiling \
                     ({ceiling}); capping. The configured model_pricing.context_window implies \
                     a budget larger than what the backend accepts, so requests would be \
                     rejected. Lower model_pricing.context_window, or raise the ceiling in \
                     backend_budget_ceiling if the real limit changed."
                );
            }
            ceiling
        }
        _ => budget,
    }
}

/// 未登録モデルを設定時に弾き、実行時は既定へ落ちても止めない（#412）。
#[cfg(test)]
mod model_context_window_gate_tests {
    use super::{compute_context_budget, ensure_model_context_window_registered};

    fn register(conn: &rusqlite::Connection, provider: &str, model: &str, window: Option<i32>) {
        opencrab_db::queries::upsert_model_pricing(
            conn,
            &opencrab_db::queries::ModelPricingRow {
                provider: provider.to_string(),
                model: model.to_string(),
                input_price_per_1m: 0.0,
                output_price_per_1m: 0.0,
                context_window: window,
            },
        )
        .unwrap();
    }

    #[test]
    fn registered_model_passes() {
        let conn = opencrab_db::init_memory().unwrap();
        register(&conn, "p1", "m1", Some(200_000));
        assert!(ensure_model_context_window_registered(&conn, "p1:m1").is_ok());
    }

    #[test]
    fn unregistered_model_is_rejected_with_how_to_register() {
        let conn = opencrab_db::init_memory().unwrap();
        let err = ensure_model_context_window_registered(&conn, "p1:m1").unwrap_err();
        // 拒否するだけでなく、登録先を必ず示す。
        assert!(err.contains("model_pricing"), "{err}");
        assert!(err.contains("/api/llm/model-pricing"), "{err}");
    }

    /// フロントの導線（`web/src/pages/AgentOverview.tsx`）はこのメッセージ文言に
    /// 結合している（#482）。`UNREGISTERED_MARKER` と spec 抽出の正規表現
    /// `/model "([^"]+)"/` が拾える形を契約として固定する。ここが変わると導線が
    /// 黙って出なくなるので、変えるならフロント側も直すこと。
    #[test]
    fn missing_message_keeps_frontend_link_contract() {
        let msg = super::model_context_window_missing_message("chatgpt:gpt-5.6-terra");
        // フロントの marker（AgentOverview.tsx の UNREGISTERED_MARKER と一致）。
        assert!(
            msg.contains("has no context_window registered in model_pricing"),
            "{msg}"
        );
        // フロントの正規表現 `model "([^"]+)"` が spec を拾える引用形。
        assert!(msg.contains("model \"chatgpt:gpt-5.6-terra\""), "{msg}");
    }

    /// 行はあるが `context_window` が NULL の場合も未登録扱い。
    /// 予算を決められない以上、単価だけ入っていても通してはいけない。
    #[test]
    fn row_without_context_window_is_rejected() {
        let conn = opencrab_db::init_memory().unwrap();
        register(&conn, "p1", "m1", None);
        assert!(ensure_model_context_window_registered(&conn, "p1:m1").is_err());
    }

    /// provider を持たない spec は provider="" として引く（`split_llm_model_spec` の規約）。
    #[test]
    fn bare_model_spec_uses_empty_provider() {
        let conn = opencrab_db::init_memory().unwrap();
        register(&conn, "", "m1", Some(123));
        assert!(ensure_model_context_window_registered(&conn, "m1").is_ok());
        assert!(ensure_model_context_window_registered(&conn, "p1:m1").is_err());
    }

    /// `context_window` が 0 以下の行は**未登録扱い**。
    ///
    /// 0 なら予算が消え、負なら `as usize` で桁違いの値へ巻き上がって上限が事実上
    /// 無くなる（文脈予算の上限そのものが無意味になる）。投入 API は `<= 0` を弾くが、
    /// 読み出し側でも倒す。gate と実行時で判定が食い違わないよう**両方**で同じ扱い。
    #[test]
    fn non_positive_context_window_is_treated_as_unregistered() {
        for bad in [0, -1, -200_000] {
            let conn = opencrab_db::init_memory().unwrap();
            register(&conn, "p1", "m1", Some(bad));
            assert!(
                ensure_model_context_window_registered(&conn, "p1:m1").is_err(),
                "context_window={bad} は未登録扱いのはず"
            );
            assert_eq!(
                compute_context_budget(&conn, "p1", "m1", 0.5),
                super::DEFAULT_CONTEXT_BUDGET_TOKENS / 2,
                "context_window={bad} で既定へ落ちるはず"
            );
        }
    }

    /// spec の比較用正規化は、参照キーと同じ trim を通す。
    /// 空白が付いた / 外れただけの spec は「同じ」。
    #[test]
    fn spec_normalization_ignores_surrounding_whitespace() {
        use super::normalize_model_spec as norm;
        assert_eq!(norm(" p1 : m1 "), norm("p1:m1"));
        assert_eq!(norm("p1:m1\n"), norm("p1:m1"));
        assert_ne!(norm("p1:m1"), norm("p1:m2"));
        // model 側の `:` は分割しない（最初の `:` だけが区切り）。
        assert_eq!(norm(" p1 : a/b:c "), "p1:a/b:c");
    }

    /// 投入側（`PUT /api/llm/model-pricing`）は trim して保存する。参照側だけ生のまま
    /// 引くと「登録したのに未登録と言われる」になるので、両端の空白は落として揃える。
    #[test]
    fn lookup_ignores_surrounding_whitespace() {
        let conn = opencrab_db::init_memory().unwrap();
        register(&conn, "p1", "m1", Some(200_000));
        assert!(ensure_model_context_window_registered(&conn, " p1 : m1 ").is_ok());
        // 実行時の予算計算も同じ正規化で引く（gate は通ったのに予算だけ既定へ落ちない）。
        assert_eq!(compute_context_budget(&conn, " p1 ", " m1 ", 0.5), 100_000);
    }

    /// 同じ (provider, model) の WARN は 1 度だけ。毎ターン通る経路なので、
    /// 抑止が効かないと登録が済むまでログが同じ 1 行で埋まる。
    #[test]
    fn warn_is_emitted_once_per_model() {
        assert!(super::warn_once_for_model(
            "k",
            "warn-test-p",
            "warn-test-m"
        ));
        assert!(!super::warn_once_for_model(
            "k",
            "warn-test-p",
            "warn-test-m"
        ));
        // 別のモデルは別枠（1 つ出したら以後全部黙る、ではない）。
        assert!(super::warn_once_for_model(
            "k",
            "warn-test-p",
            "warn-test-m2"
        ));
    }

    #[test]
    fn budget_uses_registered_context_window() {
        let conn = opencrab_db::init_memory().unwrap();
        register(&conn, "p1", "m1", Some(200_000));
        assert_eq!(compute_context_budget(&conn, "p1", "m1", 0.5), 100_000);
    }

    /// 未登録でも**実行は止めない**。既定値の compaction_ratio 倍で走り続ける
    /// （WARN は出るが、稼働中のエージェントを落とすのは方針ではない）。
    #[test]
    fn budget_falls_back_to_default_when_unregistered() {
        let conn = opencrab_db::init_memory().unwrap();
        assert_eq!(
            compute_context_budget(&conn, "p1", "m1", 0.5),
            super::DEFAULT_CONTEXT_BUDGET_TOKENS / 2
        );
    }

    /// #535 の障害そのものの再現：`chatgpt` の `context_window` を 1,050,000（API カタログ値）
    /// にしたとき、予算 = 1,050,000 × 0.5 = 525,000 になり、実上限（Codex OAuth 約 371,864）を
    /// 超える。**この設定で毎リクエストが拒否され heartbeat が数時間止まった。** 天井
    /// （350,000）で頭を抑え、100% 失敗する予算を返さないことを固定する。
    #[test]
    fn chatgpt_catalog_window_is_capped_to_backend_ceiling() {
        let conn = opencrab_db::init_memory().unwrap();
        register(&conn, "chatgpt", "gpt-5.6-sol", Some(1_050_000));
        // 素の計算なら 525,000。天井で 350,000 に丸められる。
        assert_eq!(
            compute_context_budget(&conn, "chatgpt", "gpt-5.6-sol", 0.5),
            350_000
        );
    }

    /// 天井の無い provider は従来どおり `context_window × ratio`（頭を抑えない）。
    #[test]
    fn provider_without_ceiling_is_unchanged() {
        let conn = opencrab_db::init_memory().unwrap();
        register(&conn, "p1", "m1", Some(1_050_000));
        assert_eq!(compute_context_budget(&conn, "p1", "m1", 0.5), 525_000);
    }

    /// 天井以下の予算は素通り（現状の運用値 350,000 × 0.5 = 175,000 は変わらない）。
    /// 天井は「超えたときだけ」効く。
    #[test]
    fn chatgpt_budget_below_ceiling_passes_through() {
        let conn = opencrab_db::init_memory().unwrap();
        register(&conn, "chatgpt", "gpt-5.6-sol", Some(350_000));
        assert_eq!(
            compute_context_budget(&conn, "chatgpt", "gpt-5.6-sol", 0.5),
            175_000
        );
    }

    /// 天井が噛んだときの WARN は (provider, model) ごとに 1 回だけ。毎ターン通る経路
    /// なので、抑止が効かないとログが埋まる。`over_backend_ceiling` の kind で固定する。
    ///
    /// #541: 以前は `warn_once_for_model` を直接叩くだけで（`warn_is_emitted_once_per_model`
    /// と重複気味・通るだけ）、`compute_context_budget` の kind 文字列がドリフトしても
    /// 気付けなかった。ここでは**天井超えの設定で `compute_context_budget` を実際に回し**、
    /// (1) 予算が天井で頭打ちになること、(2) 実経路が `"over_backend_ceiling"` の kind で
    /// warn_once を 1 回叩いたこと（直後の直接呼びが抑止される＝既に登録済み）を見る。
    /// kind 文字列がずれれば下の `assert!(!…)` が true に戻って落ちる。
    #[test]
    fn ceiling_warn_is_emitted_once() {
        let conn = opencrab_db::init_memory().unwrap();
        // 天井超えの設定（1,050,000 × 0.5 = 525,000 > chatgpt 天井 350,000）。model 名は
        // このテスト固有にして、warn_once のグローバル抑止キーが他テストと衝突しないようにする。
        register(&conn, "chatgpt", "ceiling-warn-once-m", Some(1_050_000));

        // (1) 実経路が天井で頭を抑える（＝over_backend_ceiling の分岐を通っている）。
        assert_eq!(
            compute_context_budget(&conn, "chatgpt", "ceiling-warn-once-m", 0.5),
            350_000
        );
        // (2) 同じ (kind, provider, model) の WARN を実経路が既に 1 回出したので、直後の
        //     直接呼びは抑止される。compute_context_budget が実際に "over_backend_ceiling"
        //     の kind で warn_once を叩いたことの証明（kind がドリフトすればここが true）。
        assert!(!super::warn_once_for_model(
            "over_backend_ceiling",
            "chatgpt",
            "ceiling-warn-once-m"
        ));
        // 2 回目の compute も同じ天井値（挙動は不変・WARN は既に抑止済み）。
        assert_eq!(
            compute_context_budget(&conn, "chatgpt", "ceiling-warn-once-m", 0.5),
            350_000
        );
    }
}
