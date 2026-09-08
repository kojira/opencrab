// ---------- LLM Router builder ----------

/// 既知の provider 形式（`type`）の一覧。**単一の出所**にして、`build_llm_router` の
/// match アームと未知 type エラーの文言がずれないようにする（11 個目の形式を足すときの
/// drift 防止）。ここへ 1 語足したら `build_llm_router` に同名の match アームを足すこと。
/// この対応は `every_known_provider_type_dispatches` テストで機械的に固定している。
const KNOWN_PROVIDER_TYPES: &[&str] = &[
    "openai",
    "anthropic",
    "google",
    "openrouter",
    "ollama",
    "llamacpp",
    "codex",
    "cursor",
    "acp",
    "chatgpt",
];

/// Build an LlmRouter from the LLM config section.
/// Only providers with non-empty API keys (or local providers) are registered.
pub fn build_llm_router(config: &LlmConfig) -> Result<LlmRouter> {
    let mut router = LlmRouter::new();

    for (name, pconfig) in &config.providers {
        // 形式（type）でクライアント実装を選ぶ。セクションキー `name` は「名乗り名」
        // （接続先の実体・ルーティングキー）で、`type` は「形式」。`type` 省略時は
        // セクションキーをそのまま形式名として使う解決規則なので、既存セクションは
        // 無編集で従来どおり動き、編集が要るのは形式名と異なる名前を付けたいとき
        // （例: hermit を openai 形式で喋らせる）だけ。
        let provider_type = if pconfig.provider_type.is_empty() {
            name.as_str()
        } else {
            pconfig.provider_type.as_str()
        };

        let provider: Option<Arc<dyn LlmProvider>> = match provider_type {
            "openai" => {
                if pconfig.api_key.is_empty() {
                    None
                } else {
                    let mut p = OpenAiProvider::new(&pconfig.api_key).with_name(name.as_str());
                    if !pconfig.base_url.is_empty() {
                        p = p.with_base_url(&pconfig.base_url);
                    }
                    if !pconfig.organization.is_empty() {
                        p = p.with_org_id(&pconfig.organization);
                    }
                    // GPT-5 系 / o シリーズを使うときの reasoning_effort（任意）。
                    if !pconfig.reasoning_effort.is_empty() {
                        p = p.with_reasoning_effort(&pconfig.reasoning_effort);
                    }
                    Some(Arc::new(p))
                }
            }
            "anthropic" => {
                if pconfig.api_key.is_empty() {
                    None
                } else {
                    let mut p = AnthropicProvider::new(&pconfig.api_key).with_name(name.as_str());
                    if !pconfig.base_url.is_empty() {
                        p = p.with_base_url(&pconfig.base_url);
                    }
                    Some(Arc::new(p))
                }
            }
            "google" => {
                if pconfig.api_key.is_empty() {
                    None
                } else {
                    let mut p = GoogleProvider::new(&pconfig.api_key).with_name(name.as_str());
                    if !pconfig.base_url.is_empty() {
                        p = p.with_base_url(&pconfig.base_url);
                    }
                    Some(Arc::new(p))
                }
            }
            "openrouter" => {
                if pconfig.api_key.is_empty() {
                    None
                } else {
                    let mut p = OpenRouterProvider::new(&pconfig.api_key).with_name(name.as_str());
                    if !pconfig.base_url.is_empty() {
                        p = p.with_base_url(&pconfig.base_url);
                    }
                    if !pconfig.app_name.is_empty() {
                        p = p.with_title(&pconfig.app_name);
                    }
                    if !pconfig.site_url.is_empty() {
                        p = p.with_referer(&pconfig.site_url);
                    }
                    Some(Arc::new(p))
                }
            }
            "ollama" => {
                let mut p = OllamaProvider::new().with_name(name.as_str());
                if !pconfig.base_url.is_empty() {
                    p = p.with_base_url(&pconfig.base_url);
                }
                Some(Arc::new(p))
            }
            "llamacpp" => {
                let mut p = LlamaCppProvider::new().with_name(name.as_str());
                if !pconfig.base_url.is_empty() {
                    p = p.with_base_url(&pconfig.base_url);
                }
                Some(Arc::new(p))
            }
            "codex" => {
                let mut p = opencrab_llm::CodexProvider::new().with_name(name.as_str());
                if !pconfig.default_model.is_empty() {
                    p = p.with_default_model(&pconfig.default_model);
                }
                if !pconfig.binary_path.is_empty() {
                    p = p.with_codex_path(&pconfig.binary_path);
                }
                if !pconfig.sandbox.is_empty() {
                    p = p.with_sandbox(&pconfig.sandbox);
                }
                if !pconfig.working_dir.is_empty() {
                    p = p.with_working_dir(&pconfig.working_dir);
                }
                if pconfig.timeout_secs > 0 {
                    p = p.with_timeout_secs(pconfig.timeout_secs);
                }
                // reasoning effort の上書き（gpt-5.6 系の既定 high を下げる等）。
                if !pconfig.reasoning_effort.is_empty() {
                    p = p.with_reasoning_effort(&pconfig.reasoning_effort);
                }
                if !pconfig.models.is_empty() {
                    let extra: Vec<(String, u32)> = pconfig
                        .models
                        .iter()
                        .map(|m| (m.clone(), 200_000u32))
                        .collect();
                    p = p.with_extra_models(extra);
                }
                Some(Arc::new(p))
            }
            "cursor" => {
                let mut p = opencrab_llm::CursorProvider::new().with_name(name.as_str());
                if !pconfig.default_model.is_empty() {
                    p = p.with_default_model(&pconfig.default_model);
                }
                if !pconfig.binary_path.is_empty() {
                    p = p.with_binary_path(&pconfig.binary_path);
                }
                // サンドボックスを config から配線（codex と同じ形。cursor の値は
                // "enabled" | "disabled"）。空なら既定の最安全側（enabled）を維持。
                // 読取専用モード（--plan）は常時有効で、これはその上の多層防御層。
                if !pconfig.sandbox.is_empty() {
                    p = p.with_sandbox(&pconfig.sandbox);
                }
                // working_dir は配線しない（#682）。cursor は chat_completion 毎に空の
                // 一時 cwd を内部生成するため、外から cwd を指定させない（実データを
                // 持ち込むと native 読取が権限ゲートを迂回する）。
                if pconfig.timeout_secs > 0 {
                    p = p.with_timeout_secs(pconfig.timeout_secs);
                }
                // config に api_key があれば CURSOR_API_KEY として渡す。
                // 無ければ `cursor-agent login` 済みのアンビエント認証に任せる。
                if !pconfig.api_key.is_empty() {
                    p = p.with_api_key(&pconfig.api_key);
                }
                if !pconfig.models.is_empty() {
                    let extra: Vec<(String, u32)> = pconfig
                        .models
                        .iter()
                        .map(|m| (m.clone(), 200_000u32))
                        .collect();
                    p = p.with_extra_models(extra);
                }
                Some(Arc::new(p))
            }
            "acp" => {
                // ACP（Agent Client Protocol）エージェントを JSON-RPC/stdio で駆動する。
                // 起動コマンド/引数はエージェント毎に異なるため binary_path + args で指定。
                let mut p = opencrab_llm::AcpProvider::new().with_name(name.as_str());
                if !pconfig.default_model.is_empty() {
                    p = p.with_default_model(&pconfig.default_model);
                }
                if !pconfig.binary_path.is_empty() {
                    p = p.with_binary_path(&pconfig.binary_path);
                }
                if !pconfig.args.is_empty() {
                    p = p.with_args(pconfig.args.clone());
                }
                if !pconfig.working_dir.is_empty() {
                    p = p.with_working_dir(&pconfig.working_dir);
                }
                if pconfig.timeout_secs > 0 {
                    p = p.with_timeout_secs(pconfig.timeout_secs);
                }
                if !pconfig.models.is_empty() {
                    let extra: Vec<(String, u32)> = pconfig
                        .models
                        .iter()
                        .map(|m| (m.clone(), 200_000u32))
                        .collect();
                    p = p.with_extra_models(extra);
                }
                Some(Arc::new(p))
            }
            "chatgpt" => {
                let mut p = ChatGptProvider::new().with_name(name.as_str());
                if !pconfig.auth_file.is_empty() {
                    p = p.with_auth_file(&pconfig.auth_file);
                }
                if !pconfig.base_url.is_empty() {
                    p = p.with_base_url(&pconfig.base_url);
                }
                if !pconfig.default_model.is_empty() {
                    p = p.with_default_model(&pconfig.default_model);
                }
                if !pconfig.reasoning_effort.is_empty() {
                    p = p.with_reasoning_effort(&pconfig.reasoning_effort);
                }
                // 長考ターン（reasoning_effort の高い体）が既定 60 秒の read timeout を
                // 超えて error → リトライを繰り返さないよう、config から伸ばせる（#433）。
                if pconfig.timeout_secs > 0 {
                    p = p.with_timeout_secs(pconfig.timeout_secs);
                }
                p = p.with_include_encrypted_content(pconfig.include_reasoning_encrypted_content);
                Some(Arc::new(p))
            }
            other => {
                // 未知の形式は起動を止める（黙って落とさない）。旧実装はここで
                // `None` を返してスキップしていたが、形式名の typo や rename の
                // 取りこぼしは設定バグであり、黙って provider を落とすと agents が
                // 実行時に遠く離れた場所で失敗する。fail loudly。
                anyhow::bail!(
                    "provider '{name}' has unknown type '{other}'. Known types: {}. \
                     Set `type = \"<one of these>\"` in [llm.providers.{name}] \
                     (bonsai 等の別名は形式名と別に付ける).",
                    KNOWN_PROVIDER_TYPES.join(", ")
                );
            }
        };

        if let Some(p) = provider {
            // ルーティングキーはセクションキー（名乗り名）を単一の代入点として渡す。
            // router は provider.name() を読まないので、キーと名乗り名は構造的に
            // 乖離しえない（二重命名を規約でなく構造で解消）。
            router.register_provider(name.clone(), p);
        }
    }

    // default_provider が定義されていなければ起動を止める（chain / alias と対称）。
    // これは bare model（`provider:` を含まない model 名）の解決先という LIVE な参照で、
    // rename を取りこぼすと「起動は通るが実効モデルが宙に浮き、fallback chain へ黙って
    // 誤ルートする」——本 PR が塞いだ欠陥クラスの生き残りになる。定義済みセクションかで
    // 判定する（chain / alias と同じ理由: 認証キー未設定でスキップされただけの provider を
    // 既定に据えた構成は壊さない）。
    //
    // ただし次の 2 つは「未設定」として通す（ここで弾かない）:
    //   - default_provider が空文字＝**既定を明示的に置かない**構成。bare model を許さず
    //     全モデルを `provider:model` で指定する運用で、空文字を providers と照合すると常に
    //     外れて誤って弾く。
    //   - provider を 1 つも定義していない構成。空の LlmConfig（テストのスタブ）や、DB
    //     オーバーライドで全 provider を enabled=false にした実効設定（`apply_llm_overrides`
    //     が providers を空にする・`reload_router` 経路）は、空の router を返すのが正で、
    //     既定名が宙に浮くのは「そもそも何も設定されていない」ことの帰結にすぎない。
    // 捕まえたいのは「provider は在るのに既定名だけがどのセクションにも無い」＝ rename 取りこぼし。
    if !config.default_provider.is_empty()
        && !config.providers.is_empty()
        && !config.providers.contains_key(&config.default_provider)
    {
        anyhow::bail!(
            "default_provider '{p}' has no [llm.providers.{p}] section. It is the \
             fallback for bare model names, so it must point to a defined provider. \
             If you are disabling '{p}' (e.g. from the dashboard), change \
             `default_provider` in the config TOML to another defined provider first, \
             then restart — default_provider lives in the TOML only and cannot be \
             changed at runtime, so the dashboard cannot repoint or override it.",
            p = config.default_provider,
        );
    }
    // Set default provider
    router.set_default_provider(&config.default_provider);

    // fallback.chain / aliases が「定義されていない provider」を指していたら起動を
    // 止める。旧実装は chain を黙って filter し、alias は無検証で登録していたが、
    // これは未知 type の無言スキップと同じ欠陥クラス（rename の取りこぼしや typo が
    // 黙って無効化され、実行時に誤ルーティングとして遠くで顕在化する）。
    //
    // 判定は「セクションが定義されているか（config.providers に居るか）」で行う。
    // ルーティング登録済みかで判定しないのは、認証キー未設定でスキップされただけの
    // provider（例: 環境変数未設定の anthropic）を chain に書いた構成を壊さないため。
    // そうした provider は定義済みとして通し、実行時は router 側が get で拾えなければ
    // 次の候補へ graceful に進む。捕まえたいのは「どのセクションにも無い名前」。
    for provider_name in &config.fallback.chain {
        if !config.providers.contains_key(provider_name) {
            anyhow::bail!(
                "fallback.chain references undefined provider '{provider_name}'. \
                 Add a [llm.providers.{provider_name}] section or remove it from \
                 the chain."
            );
        }
    }
    if !config.fallback.chain.is_empty() {
        router.set_fallback_chain(config.fallback.chain.clone());
    }

    // Set model aliases
    for (alias, acfg) in &config.aliases {
        if !config.providers.contains_key(&acfg.provider) {
            anyhow::bail!(
                "alias '{alias}' targets undefined provider '{provider}'. \
                 Add a [llm.providers.{provider}] section or fix the alias.",
                provider = acfg.provider,
            );
        }
        let target = format!("{}:{}", acfg.provider, acfg.model);
        router.add_model_mapping(alias, target);
    }

    info!(
        providers = ?router.provider_names(),
        "LLM router configured"
    );

    Ok(router)
}

