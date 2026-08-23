//! app — 設定・起動（詳細§01 の app 層）。core・store・engine・plugd を 1 つの走るプロセスに束ね、
//! 実ソケットでプラグインの接続を受ける。
//!
//! ここは「系の中身」であって、ゲート（プラグイン）ではない。だから core の型を使ってよい
//! （プロトコルの規律が掛かるのはゲート側だけ）。ゲート（web）は別クレート・別プロセスで、
//! 線に載る JSON を自分で組む。
//!
//! app が持つ判断は 2 つだけ:
//!   1. **どんな場を用意するか**（設定）— どの主体を、どの住所に、どの発火方針で置くか。
//!   2. **いつ結ぶか**（起動順・繋ぎ直し）— プラグインが（再）接続したら、その住所へ結び直す。
//!
//! ターンの起こし方・直列化・履歴・権限は core が握る。app は触らない。

mod cursor;
mod provider;
mod shell;
pub use cursor::CursorEngine;
pub use provider::{
    AnthropicProvider, ChatGptProvider, ChatProvider, HttpSseEngine, OpenAiProvider,
};
pub use shell::TokioShellHost;

use opencrab_plugd::Plugd;
use opencrab_port::{
    ChunkSink, Context, EffectSpec, Engine, EngineError, GateKindId, InferOutput, Property, Role,
    Standing, SubjectId, SubjectKind,
};
use opencrab_social_runtime::{Config, ImmediateFrom, Policy, System};
use opencrab_store::Store;
use std::sync::Arc;
use tokio::net::UnixListener;

pub const WEB_GATE: &str = "web";

/// 用意する場 1 つ分の設定（app の判断1・詳細§01）。**ゲート名は直書きせずデータで持つ** — web でも
/// nostr でも同じ 1 つの入口（`Host::provision_place`）が起こす。
#[derive(Clone, Debug)]
pub struct PlaceSpec {
    /// 外界の住所（ゲートの address_form に合う。web は `room:xxx`、nostr は `npub1...`/`filter:...`）。
    pub address: String,
    /// 結ぶゲートの名前（`web` / `nostr` …）。バイナリに直書きしない。
    pub gate: String,
    /// この場に置く主体（エージェント）の表示名（同一性）。ログの著者表示に使う。人格本文とは別。
    pub name: String,
    /// この場に置く主体（エージェント）の人格本文（逐語で system の先頭に載る）。
    pub persona: String,
    /// 発火方針（即応の条件・まとめ窓・無条件の間隔）。`default_subject` は用意した主体で埋めるので不要。
    pub policy: Policy,
    /// 主体の外界の身元 `(gate, external)`（例: `("nostr", "npub1…")`）。名寄せに載り、言及・返信が解決する。
    pub identities: Vec<(String, String)>,
}

/// 設定（JSON）から用意する場の一覧を読む。**壊れ・未知は既定へ倒さず Err**（近いものへ寄せない・§15）。
///
/// 形（`policy` は core の `Policy` の線形と同じ。`default_subject` は書いても無視され、用意した主体で埋まる）:
/// ```json
/// {"places":[
///   {"address":"room:main","gate":"web","name":"web-agent","persona":"あなたは…",
///    "policy":{"immediate":["direct"],"immediate_from":"anyone","batch_window_ms":null,"unconditional_interval_ms":null}},
///   {"address":"filter:kind=1&author=npub1abc","gate":"nostr","name":"エージェントA","persona":"あなたは…",
///    "policy":{"immediate":["mentions_me","replies_to_me"],"immediate_from":"anyone","batch_window_ms":8000,"unconditional_interval_ms":null},
///    "identities":[{"gate":"nostr","external":"npub1abc"}]}
/// ]}
/// ```
pub fn parse_places_config(json: &str) -> Result<Vec<PlaceSpec>, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("invalid config JSON: {e}"))?;
    let arr = v
        .get("places")
        .and_then(|x| x.as_array())
        .ok_or_else(|| "config: `places` array required".to_string())?;
    let mut out = Vec::new();
    for (i, p) in arr.iter().enumerate() {
        let field = |name: &str| -> Result<String, String> {
            p.get(name)
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| format!("places[{i}].{name}: string required"))
        };
        let address = field("address")?;
        let gate = field("gate")?;
        // name（表示名）と persona（人格本文）は別フィールド（統括裁定）。後方互換は取らない——
        // 既存 config も両方を書く（利用者に追従してもらう家風）。欠落は既定へ倒さず Err（§15）。
        let name = field("name")?;
        let persona = field("persona")?;
        let policy_obj = p
            .get("policy")
            .ok_or_else(|| format!("places[{i}].policy: object required"))?;
        // Policy::from_json は immediate_from の欠落・未知を Err にする（緩い方へ倒さない・§15）。
        let policy = Policy::from_json(&policy_obj.to_string())
            .map_err(|e| format!("places[{i}].policy: {e}"))?;
        let mut identities = Vec::new();
        if let Some(ids) = p.get("identities").and_then(|x| x.as_array()) {
            for (j, id) in ids.iter().enumerate() {
                let g = id
                    .get("gate")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| format!("places[{i}].identities[{j}].gate: string required"))?;
                let e = id.get("external").and_then(|x| x.as_str()).ok_or_else(|| {
                    format!("places[{i}].identities[{j}].external: string required")
                })?;
                identities.push((g.to_string(), e.to_string()));
            }
        }
        out.push(PlaceSpec {
            address,
            gate,
            name,
            persona,
            policy,
            identities,
        });
    }
    Ok(out)
}

/// どの推論の口を使うか（設定で選ぶ・フォールバックではない・詳細§15）。
///
/// `OPENCRAB_LLM_PROVIDER` が設定されていれば本物のプロバイダ（`provider::engine_from_env`）、
/// 無ければ `EchoEngine`（揺らぎ無しの差し替え実装）。**echo は本物の失敗時の逃げ道ではない**——
/// 設定で本物を選んだのにプロバイダが失敗すれば、そのターンは失敗で終わる（echo へ戻らない・§15）。
/// 鍵が無くてもここは通る（本物を選んでいなければ echo・選んでいても組めるが実行時に失敗を返す）。
fn select_engine() -> Arc<dyn Engine> {
    match provider::engine_from_env() {
        Ok(Some(engine)) => engine,
        Ok(None) => Arc::new(EchoEngine),
        Err(message) => panic!("{message}"),
    }
}

/// 決まった応答を返す推論の口（差し替え可能・詳細§01）。本物の LLM は今回入れない。
///
/// これは engine seam の正当な実装であって、フォールバックではない — 「無ければ既定で埋める」の類ではなく、
/// 「この系のターンを回すものを、揺らぎ無しの実物で用意する」もの。届いた発話を読んで 1 度だけ発話して終える。
/// 断片の口（`chunk()`）も本物どおり叩く（アイドル上限が総時間上限に化けない形・詳細§05）。
#[derive(Clone, Default)]
pub struct EchoEngine;

/// echo の実効モデル名（予算の物差し・§06）。echo は実 LLM ではないが、系は予算で文脈を組むので
/// 物差しが要る。`KNOWN_MODEL_CONTEXT_WINDOWS` に同名で登録し、本体 claude 系と同じ context_window を差す。
pub const ECHO_MODEL: &str = "echo";

/// 予算の物差し（会話予算 = `context_window × compaction_ratio`・§06）にする既知モデルの context_window。
/// 値は**本番 opencrab の DB `model_pricing` テーブルの実登録値（2026-08-18 実測）**をこの実装へ転記したもの
/// （予算に要るのは context_window だけなので単価は持たない）。**ここに無いモデルを実効にすると起動時に
/// fail loud**（既定値へ寄せない・§15）。新モデルはここへ 1 行足す。
///
/// 注意（drift・ISSUES）: これはオペレータ登録データ（本番 DB）のハードコード複製なので、本番が値を
/// 更新すると乖離する。還元時は本体 DB を単一の真実源にし、この seed 定数を廃す（ISSUES.md 参照）。
pub const KNOWN_MODEL_CONTEXT_WINDOWS: &[(&str, i64)] = &[
    // echo（差し替え実装・実 LLM ではない）。本体 claude 系と同じ 200_000 を物差しにする。
    (ECHO_MODEL, 200_000),
    // Anthropic（本番 model_pricing: claude 系は 200_000）。hermit-shell 橋の haiku もこの id。
    ("claude-opus-5", 200_000),
    ("claude-sonnet-5", 200_000),
    ("claude-haiku-4-5", 200_000),
    // OpenAI 互換（Chat Completions）。
    ("gpt-4o-mini", 128_000),
    // ChatGPT サブスク（本番 model_pricing: gpt-5.6 系は 350_000）。
    ("gpt-5.6", 350_000),
    ("gpt-5.6-sol", 350_000),
    ("gpt-5.6-terra", 350_000),
    ("gpt-5.6-luna", 350_000),
    // Cursor 経由（本番 model_pricing に実登録済み）。
    ("cursor-grok-4.6-high", 500_000),
    // Cursor 経由（Composer 2.5）。Cursor 公式は context window を未公開（2026-08-18 確認・
    // docs/models・models-and-pricing・blog/composer いずれも数値なし）。本番 model_pricing にも未登録。
    // よってこれは能力値ではなく、**意図的に保守的な運用値**（予算は soft limit・小さく始めて実測で
    // 広げる・[[experiments-start-at-smallest-window]]）。公式値が出たら差し替える（統括裁定 2026-08-18）。
    ("composer-2.5", 128_000),
];

/// 既知モデルの context_window を store へ seed する（冪等・boot ごとに呼ぶ）。store が予算の権威
/// （詳細§03）——core は起動時にここへ登録された値で会話予算を確定する。未登録モデルは fail loud。
pub fn seed_model_context_windows(store: &Store) {
    for (model, window) in KNOWN_MODEL_CONTEXT_WINDOWS {
        store
            .register_model_context_window(model, *window)
            .expect("seed model context_window");
    }
}

#[async_trait::async_trait]
impl Engine for EchoEngine {
    fn model(&self) -> &str {
        ECHO_MODEL
    }

    async fn infer(&self, ctx: &Context, chunks: &ChunkSink) -> Result<InferOutput, EngineError> {
        // 断片が流れていることを外へ見せる（詳細§05）。
        chunks.chunk();
        let last = last_said_text(&ctx.rendered);
        let text = match last {
            Some(t) => format!("受け取りました:「{t}」"),
            // 無条件・まとめで文脈に発話が無いこともある。その時は様子見の一言。
            None => "こんにちは。呼んでくれたら答えます。".to_string(),
        };
        Ok(InferOutput {
            effects: vec![EffectSpec::say(text)],
            tool_calls: vec![],
            done: true,
        })
    }
}

/// 文脈（rendered）から、最後の発話の本文を取り出す。`[{seq}] {who}: {text}` の形（core の render_event）。
fn last_said_text(rendered: &str) -> Option<String> {
    for line in rendered.lines().rev() {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with("===")
            || line.starts_with('（')
            || line.starts_with("子 ")
        {
            continue;
        }
        // `[n] who: text` の形だけを拾う。
        if line.starts_with('[') {
            if let Some((_, text)) = line.rsplit_once(": ") {
                if !text.is_empty() {
                    return Some(text.to_string());
                }
            }
        }
    }
    None
}

/// 走るプロセス 1 つ分の取っ手。
#[derive(Clone)]
pub struct Host {
    pub sys: System,
    plugd: Plugd,
}

impl Host {
    /// store（ファイル）と engine を差して系を組み立て、再起動の後始末（startup）まで済ませる。
    /// engine は設定で選ぶ（本物のプロバイダ or `EchoEngine`・`select_engine`）。
    pub fn boot(store: Store) -> Host {
        Host::boot_with_engine(store, select_engine())
    }

    /// engine を明示して組み立てる（テストが本物のプロバイダ実装を差し込むのに使う）。
    pub fn boot_with_engine(store: Store, engine: Arc<dyn Engine>) -> Host {
        Host::boot_with(store, engine, Config::default())
    }

    /// engine と Config を明示して組み立てる（アイドル上限などを短くしたテスト用）。
    pub fn boot_with(store: Store, engine: Arc<dyn Engine>, cfg: Config) -> Host {
        let plugd = Plugd::new();
        // 予算の物差し（§06）を先に登録する——System::new が実効モデルの context_window を引いて
        // 会話予算を確定する（未登録なら fail loud）。冪等なので再起動でも同じ 1 本で足りる。
        seed_model_context_windows(&store);
        // shell（core builtin）の作業領域の基準（DESIGN-shell.md）。deployment の設定（環境変数）。
        // shell は既定で off なので未設定でも動く——使われたときに TokioShellHost が fail loud する。
        let shell_root = std::env::var("OPENCRAB_SHELL_ROOT")
            .ok()
            .map(std::path::PathBuf::from);
        let sys = System::new(
            store,
            engine,
            Arc::new(plugd.clone()) as Arc<dyn opencrab_port::ToolHost>,
            Arc::new(TokioShellHost::new(shell_root)) as Arc<dyn opencrab_port::ShellHost>,
            Arc::new(plugd.clone()) as Arc<dyn opencrab_port::Notifier>,
            // 文脈予算の物差し（§06/§10）。本番は o200k 見積り（本体と同じ物差し・還流のため）。
            Arc::new(opencrab_social_runtime::O200kCounter::new())
                as Arc<dyn opencrab_port::TokenCounter>,
            cfg,
        );
        plugd.attach_system(sys.clone());
        sys.attach_transport(Arc::new(plugd.clone()) as Arc<dyn opencrab_port::Transport>);
        // URL の中身取得の口（DESIGN-images §3）。core-look / core-read が使う。本番は reqwest。
        // これが無いと look/read は fail loud（黙って別動作へ逃げない・§15）。
        sys.attach_fetcher(
            Arc::new(crate::provider::ReqwestFetcher::new()) as Arc<dyn opencrab_port::Fetcher>
        );
        // 再起動の後始末（詳細§11）: 走り残りを中断として閉じ、出来事にし、予定を位相ごと読み直す。
        sys.startup();
        Host { sys, plugd }
    }

    /// 設定から場を 1 つ用意する（app の判断1・詳細§01「どんな場を、どの住所に、どの発火方針で置くか」）。
    /// 既に DB にあれば作り直さない — 再起動を跨いで同じ場・同じ主体を使う。
    ///
    /// 住所・**ゲート名**・主体・発火方針・（Nostr 等の）主体の外界の身元を、すべて **spec のデータ**で
    /// 受ける。ゲート名をここに直書きしない——`web` だろうと `nostr` だろうと同じ 1 本が用意する。
    /// これが「場を起こす」入口で、web に固有の経路ではない（配線漏れの是正・タスク#1）。
    ///
    /// `identities` を与えると、その主体の外界の宛先（例: Nostr の npub）が名寄せに載る。これが無いと
    /// 「自分宛の言及・返信」が解決できず、Nostr のタイムラインで即応（メンション・返信）が起きない。
    pub fn provision_place(&self, spec: &PlaceSpec) -> (i64, SubjectId) {
        let kind = GateKindId::parse(spec.gate.clone())
            .unwrap_or_else(|e| panic!("invalid configured gate kind: {e}"));
        // v15 protocol-1 adapter contract: startup owns compatibility instance creation.
        // Hello lookup is deliberately read-only and will fail loud if this seed is absent.
        self.sys
            .store()
            .seed_compatibility_instance(&kind)
            .expect("seed configured protocol-1 compatibility instance");
        let (place, agent) = match self.find_existing_room(&spec.address) {
            Some(pa) => pa,
            None => {
                let agent = self.sys.create_subject(
                    SubjectKind::Agent,
                    &spec.name,
                    &spec.persona,
                    Standing::Trusted,
                );
                // 宛先が無いときに返すのは、この場の主体（発火方針の default_subject は主体で埋める）。
                let policy = spec.policy.clone().with_default(agent);
                let place = self
                    .sys
                    .create_place(Some(&spec.address), None, &policy, None);
                self.sys.join(place, agent, Role::Participant);
                // 主体の外界の身元を登録する（新規作成時のみ。再起動後は DB に永続している）。
                for (gate, external) in &spec.identities {
                    self.sys.add_identity(agent, gate, external);
                }
                (place, agent)
            }
        };
        // 住所を用意する（設定）。bind はまだ送らない——プラグインが（再）接続した瞬間に core が結び直す
        // （`rebind_gate`・プロトコル§08）。冪等なので、初回も再起動後も同じ 1 本で足りる。
        self.sys
            .provision_channel(place, &spec.gate, &spec.address)
            .expect("configured compatibility gate must be prebound");
        (place, agent)
    }

    /// 1 つの web の場を用意する（設定の既定・テストと手順書の入口）。`provision_place` の web 版の薄い糖衣。
    ///
    /// 発火方針: この場へ直接届いた発話に即応し、宛先が無ければ既定のエージェントが返す。公開の窓口なので
    /// 誰からの発話でも即応の対象にする（入場はゲート側が絞る・基本§03）。
    pub fn provision_web_room(&self, address: &str, name: &str, persona: &str) -> (i64, SubjectId) {
        let spec = PlaceSpec {
            address: address.to_string(),
            gate: WEB_GATE.to_string(),
            name: name.to_string(),
            persona: persona.to_string(),
            policy: Policy::immediate_on(&[Property::Direct]).with_from(ImmediateFrom::Anyone),
            identities: vec![],
        };
        self.provision_place(&spec)
    }

    fn find_existing_room(&self, address: &str) -> Option<(i64, SubjectId)> {
        let store = self.sys.store();
        for place in store.all_open_places().ok()? {
            let row = store.get_place(place).ok()??;
            if row.address.as_deref() != Some(address) {
                continue;
            }
            // その場の Agent の Participant を 1 体拾う。
            let members = store.members(place).ok()?;
            for m in members {
                if m.role != Role::Participant {
                    continue;
                }
                if let Ok(Some(s)) = store.get_subject(m.subject) {
                    if s.kind == SubjectKind::Agent {
                        return Some((place, m.subject));
                    }
                }
            }
        }
        None
    }

    /// 実ソケットで接続を受け、1 本ごとに plugd に回す（プロトコル§00「バイト列であれば運び方は問わない」）。
    /// 別プロセスのプラグインが、この Unix ソケットへ繋いでくる。
    pub async fn serve_unix(&self, listener: UnixListener) -> std::io::Result<()> {
        loop {
            let (stream, _addr) = listener.accept().await?;
            self.plugd.serve(stream);
        }
    }
}

/// 実ソケットを開く。古い残りがあれば消してから bind する（前プロセスの後始末・詳細§11）。
pub fn bind_unix(path: &std::path::Path) -> std::io::Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    UnixListener::bind(path)
}
