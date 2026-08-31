//! #852: gateway 共有層（extgate / gate-client の production）に platform 特化分岐や
//! gate/SDK 固有の識別子が現れないことの static audit。
//!
//! 背景: DI 原則「gateway 語彙を共有経路に持ち込まない」の static audit は元々
//! `crates/core/tests/no_gate_identifiers.rs`（R7）と #851 の
//! `di_generic_mechanism_has_no_platform_vocab`（extgate の DI 中核 3 ファイル）
//! だけを見ていた。そのため extgate 全体・gate-client（どちらも gateway 共有層）に
//! 残る `if kind_id == "nostr"` 形の platform 分岐を機械検出できなかった。Nostr 単独
//! 時代は他 platform が無く緑のまま通っていたが、これは「上位がそのゲートウェイを
//! 特別扱いし始める入口」（design-plugin-architecture.md §4 の事故）を再び開く。
//!
//! この audit は走査範囲を **extgate/src と gate-client/src の production 全体**へ広げ、
//! 次の 2 つを機械検査する:
//!
//! 1. `kind_id == "<platform>"` / `"<platform>" == kind_id` 形の platform 特化等値分岐
//!    （`==` / `!=`）。runtime 文字列一般ではなく `kind_id` との等値比較に的を絞る。
//! 2. gate/SDK を名指しする識別子・文字列（`nostr` / `discord` / `serenity` /
//!    `songbird` / `telegram` / `slack` の部分一致）。
//!
//! 検査は `crates/core/tests/no_gate_identifiers.rs` と同じ `syn` の AST 走査で行う
//! （expand は nightly を要し doc コメントが偽陽性を生むため使わない）。除外は同テストと
//! 同じ:
//! - `#[cfg(test)]` サブツリー（test fixture）。
//! - doc コメント（`#[doc = "..."]` / `//!` / `///`）。
//! - マクロ本体の生トークン（`syn` が展開しないため届かない。既知の限界）。
//!
//! これらに加え、**まだ generic 化できていない Nostr profile ファイル**は
//! `ALLOWLIST` に「なぜ generic にできないか」を 1 行添えて登録する。allowlist に載る
//! ファイルは走査対象外になるが、それ以外の共有ファイルは厳格に検査される（新たな
//! platform 語彙が generic 面へ紛れ込めば fail する）。allowlist の各行は phase-2 の
//! profile module 切り出しで解消される TODO を兼ねる。

use std::path::{Path, PathBuf};

use proc_macro2::{TokenStream, TokenTree};
use quote::ToTokens;
use syn::visit::Visit;
use syn::{Attribute, BinOp, Expr, ExprBinary, Lit, Meta};

/// 禁止語（小文字・部分一致）。gate/SDK を名指しする語。`no_gate_identifiers.rs` の
/// FORBIDDEN と揃える（`web` は webhook/website 等に部分一致するので入れない。`web`
/// profile の platform 分岐は下の `kind_id == "web"` 検査が拾う）。
const FORBIDDEN: &[&str] = &[
    "discord", "serenity", "songbird", "nostr", "telegram", "slack",
];

/// まだ generic 化できていない Nostr profile ファイル。`(相対パス, なぜ generic に
/// できないか)`。ここに載るファイルは走査対象外になる。各理由は phase-2 の profile
/// module 切り出しで解消される（TODO）。
///
/// 分類の内訳（#852 短報）:
/// - inbound.rs / completion.rs: **原理的には profile dispatch で汎化可能**な
///   `kind_id == "nostr"` 分岐を持つが、汎化は挙動に触れるため audit 専用の本 PR の
///   対象外（phase-2）。
/// - registry.rs / bundle.rs / lib.rs: Nostr の wire 形式・admit 状態機械・platform 別
///   ID 解決など **本質的に platform 固有**な実装。あるべき姿は profile module/crate 側
///   への配置（phase-2）。
const ALLOWLIST: &[(&str, &str)] = &[
    (
        "extgate/src/inbound.rs",
        "Nostr 受信の wire 形式（[NOSTRGATE/V1]）解釈・watch 束ね・prompt 整形を持つ未分離の Nostr profile。kind_id 分岐は phase-2 で profile dispatch へ。",
    ),
    (
        "extgate/src/completion.rs",
        "resume ターンの LiveInboundScope 選択に Nostr 固有の話者スコープ分岐が残る。profile dispatch 化は phase-2。",
    ),
    (
        "extgate/src/registry.rs",
        "Nostr profile hook（NostrSaidAdmit / nostr_workspace / nostr_relay / NostrBundleAdmit 等）を保持。profile 抽象化は phase-2。",
    ),
    (
        "extgate/src/bundle.rs",
        "Nostr バンドル admit の状態機械と nostr_bundle_state テーブル。Nostr profile 実装。phase-2 で profile 側へ。",
    ),
    (
        "extgate/src/lib.rs",
        "co-agent 解決の platform 分岐（TRUSTED_PLATFORM_DISCORD/NOSTR → resolve_agent_by_*）。DB 側の platform 別 ID 列に依存し registry 化は phase-2。",
    ),
];

// ---------------------------------------------------------------------------
// cfg(test) 判定（`no_gate_identifiers.rs` から移植。挙動を揃える）。
// ---------------------------------------------------------------------------

fn is_cfg_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("cfg") {
            return false;
        }
        match &attr.meta {
            Meta::List(list) => cfg_predicate_is_test_only(list.tokens.clone()),
            _ => false,
        }
    })
}

fn cfg_predicate_is_test_only(tokens: TokenStream) -> bool {
    let trees: Vec<TokenTree> = tokens.into_iter().collect();
    predicate_trees_is_test_only(&trees)
}

fn predicate_trees_is_test_only(trees: &[TokenTree]) -> bool {
    match trees {
        [TokenTree::Ident(id)] => *id == "test",
        [TokenTree::Ident(op), TokenTree::Group(group)] => {
            let inner: Vec<TokenTree> = group.stream().into_iter().collect();
            let subs = split_top_level_commas(&inner);
            match op.to_string().as_str() {
                "all" => subs.iter().any(|s| predicate_trees_is_test_only(s)),
                "any" => subs.iter().all(|s| predicate_trees_is_test_only(s)),
                _ => false,
            }
        }
        _ => false,
    }
}

fn split_top_level_commas(trees: &[TokenTree]) -> Vec<Vec<TokenTree>> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for tt in trees {
        if let TokenTree::Punct(p) = tt {
            if p.as_char() == ',' {
                out.push(std::mem::take(&mut cur));
                continue;
            }
        }
        cur.push(tt.clone());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

// ---------------------------------------------------------------------------
// 検出本体。
// ---------------------------------------------------------------------------

fn hit(word: &str) -> Option<&'static str> {
    let lower = word.to_ascii_lowercase();
    FORBIDDEN.iter().copied().find(|w| lower.contains(w))
}

/// `expr` が `kind_id`（path 末尾 or field 名）を指すなら true。
fn is_kind_id_operand(expr: &Expr) -> bool {
    match expr {
        Expr::Path(p) => p
            .path
            .segments
            .last()
            .is_some_and(|seg| seg.ident == "kind_id"),
        Expr::Field(f) => match &f.member {
            syn::Member::Named(id) => id == "kind_id",
            syn::Member::Unnamed(_) => false,
        },
        Expr::Reference(r) => is_kind_id_operand(&r.expr),
        Expr::Paren(p) => is_kind_id_operand(&p.expr),
        _ => false,
    }
}

/// `expr` が文字列リテラルなら its value を返す。
fn as_str_lit(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Lit(l) => match &l.lit {
            Lit::Str(s) => Some(s.value()),
            _ => None,
        },
        Expr::Reference(r) => as_str_lit(&r.expr),
        Expr::Paren(p) => as_str_lit(&p.expr),
        _ => None,
    }
}

struct Finder {
    rel: String,
    violations: Vec<String>,
}

impl Finder {
    fn record(&mut self, kind: &str, text: &str, word: &str) {
        self.violations.push(format!(
            "{}: {} \"{}\" に禁止語 \"{}\"",
            self.rel, kind, text, word
        ));
    }

    /// 属性の生トークン列（`#[serde(rename = "...")]` 等）を走査する。
    fn scan_attr_tokens(&mut self, stream: TokenStream) {
        for tt in stream {
            match tt {
                TokenTree::Ident(id) => {
                    let text = id.to_string();
                    if let Some(word) = hit(&text) {
                        self.record("属性の識別子", &text, word);
                    }
                }
                TokenTree::Literal(lit) => {
                    if let Lit::Str(s) = Lit::new(lit) {
                        let text = s.value();
                        if let Some(word) = hit(&text) {
                            self.record("属性の文字列リテラル", &text, word);
                        }
                    }
                }
                TokenTree::Group(group) => self.scan_attr_tokens(group.stream()),
                TokenTree::Punct(_) => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for Finder {
    // --- `#[cfg(test)]` サブツリーのスキップ ---
    fn visit_item(&mut self, node: &'ast syn::Item) {
        if item_is_cfg_test(node) {
            return;
        }
        syn::visit::visit_item(self, node);
    }

    fn visit_impl_item(&mut self, node: &'ast syn::ImplItem) {
        if impl_item_is_cfg_test(node) {
            return;
        }
        syn::visit::visit_impl_item(self, node);
    }

    fn visit_trait_item(&mut self, node: &'ast syn::TraitItem) {
        if trait_item_is_cfg_test(node) {
            return;
        }
        syn::visit::visit_trait_item(self, node);
    }

    // 関数本体内の `#[cfg(test)]` ブロック（`if cfg!(...)` ではなく item 属性）も
    // Stmt 経由で item として訪れるので上の visit_item が拾う。

    // --- platform 特化等値分岐: `kind_id == "<platform>"` ---
    fn visit_expr_binary(&mut self, node: &'ast ExprBinary) {
        if matches!(node.op, BinOp::Eq(_) | BinOp::Ne(_)) {
            let pair = if is_kind_id_operand(&node.left) {
                as_str_lit(&node.right)
            } else if is_kind_id_operand(&node.right) {
                as_str_lit(&node.left)
            } else {
                None
            };
            if let Some(platform) = pair {
                self.violations.push(format!(
                    "{}: platform 特化分岐 `kind_id == \"{}\"`（共有経路に platform 語彙。profile dispatch へ）",
                    self.rel, platform
                ));
            }
        }
        syn::visit::visit_expr_binary(self, node);
    }

    // --- 属性 ---
    fn visit_attribute(&mut self, node: &'ast Attribute) {
        if node.path().is_ident("doc") {
            return;
        }
        self.scan_attr_tokens(node.meta.to_token_stream());
    }

    // --- 識別子 ---
    fn visit_ident(&mut self, node: &'ast proc_macro2::Ident) {
        let text = node.to_string();
        if let Some(word) = hit(&text) {
            self.record("識別子", &text, word);
        }
    }

    // --- 文字列リテラル ---
    fn visit_lit_str(&mut self, node: &'ast syn::LitStr) {
        let text = node.value();
        if let Some(word) = hit(&text) {
            self.record("文字列リテラル", &text, word);
        }
    }
}

fn item_is_cfg_test(node: &syn::Item) -> bool {
    let attrs: &[Attribute] = match node {
        syn::Item::Const(i) => &i.attrs,
        syn::Item::Enum(i) => &i.attrs,
        syn::Item::Fn(i) => &i.attrs,
        syn::Item::Impl(i) => &i.attrs,
        syn::Item::Mod(i) => &i.attrs,
        syn::Item::Static(i) => &i.attrs,
        syn::Item::Struct(i) => &i.attrs,
        syn::Item::Trait(i) => &i.attrs,
        syn::Item::Type(i) => &i.attrs,
        syn::Item::Union(i) => &i.attrs,
        syn::Item::Use(i) => &i.attrs,
        _ => return false,
    };
    is_cfg_test(attrs)
}

fn impl_item_is_cfg_test(node: &syn::ImplItem) -> bool {
    let attrs: &[Attribute] = match node {
        syn::ImplItem::Const(i) => &i.attrs,
        syn::ImplItem::Fn(i) => &i.attrs,
        syn::ImplItem::Type(i) => &i.attrs,
        _ => return false,
    };
    is_cfg_test(attrs)
}

fn trait_item_is_cfg_test(node: &syn::TraitItem) -> bool {
    let attrs: &[Attribute] = match node {
        syn::TraitItem::Const(i) => &i.attrs,
        syn::TraitItem::Fn(i) => &i.attrs,
        syn::TraitItem::Type(i) => &i.attrs,
        _ => return false,
    };
    is_cfg_test(attrs)
}

/// 1 ファイル分のソースを走査して違反メッセージを返す（pure。単体テストしやすい形）。
fn scan_source(rel: &str, text: &str) -> Vec<String> {
    let ast = syn::parse_file(text).unwrap_or_else(|e| panic!("{rel} の構文解析に失敗: {e}"));
    let mut finder = Finder {
        rel: rel.to_string(),
        violations: Vec::new(),
    };
    finder.visit_file(&ast);
    finder.violations
}

/// `dir` 以下の `.rs` を再帰列挙する。
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {} に失敗: {e}", dir.display()));
    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// `crates/` を基準にした相対パス（`extgate/src/foo.rs` の形）。allowlist 照合に使う。
fn rel_from_crates(crates_dir: &Path, path: &Path) -> String {
    path.strip_prefix(crates_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[test]
fn gateway_shared_layer_has_no_platform_branch() {
    // extgate の manifest は `crates/extgate`。その親が `crates/`。
    let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ dir")
        .to_path_buf();

    let mut files = Vec::new();
    collect_rs_files(&crates_dir.join("extgate/src"), &mut files);
    collect_rs_files(&crates_dir.join("gate-client/src"), &mut files);
    files.sort();
    assert!(
        !files.is_empty(),
        "gateway 共有層の src に .rs が見つからない"
    );

    // allowlist の各エントリが実在ファイルを指すことを保証（腐った allowlist を防ぐ）。
    for (rel, _why) in ALLOWLIST {
        assert!(
            crates_dir.join(rel).is_file(),
            "ALLOWLIST の項目 {rel} が実在しない（リネーム後の腐り。更新すること）"
        );
    }

    let mut violations = Vec::new();
    for file in &files {
        let rel = rel_from_crates(&crates_dir, file);
        if ALLOWLIST.iter().any(|(a, _)| *a == rel) {
            continue;
        }
        let text = std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("read {} に失敗: {e}", file.display()));
        violations.extend(scan_source(&rel, &text));
    }

    assert!(
        violations.is_empty(),
        "gateway 共有層（allowlist 外）に platform 特化分岐 / gate 固有語彙が見つかった \
         （generic 面に混入した証拠。禁止語を減らさず該当箇所を profile へ寄せるか、\
         正当なら理由付きで ALLOWLIST に登録すること）:\n{}",
        violations.join("\n")
    );
}

// ---------------------------------------------------------------------------
// audit 自体の自己検査: 意図的に platform 分岐を足した fixture で fail するか。
// ---------------------------------------------------------------------------

#[test]
fn detector_flags_kind_id_platform_branch() {
    let src = r#"
        fn handle(kind_id: &str) -> u32 {
            if kind_id == "discord" { 1 } else { 0 }
        }
    "#;
    let v = scan_source("fixture.rs", src);
    assert!(
        v.iter().any(|m| m.contains("kind_id == \"discord\"")),
        "kind_id == \"discord\" を検出できていない: {v:?}"
    );
}

#[test]
fn detector_flags_kind_id_field_and_web_platform() {
    // field 形（`row.kind_id`）と、FORBIDDEN に無い "web" も等値分岐なら拾う。
    let src = r#"
        struct Row { kind_id: String }
        fn f(row: &Row) -> bool { row.kind_id == "web" }
    "#;
    let v = scan_source("fixture.rs", src);
    assert!(
        v.iter().any(|m| m.contains("kind_id == \"web\"")),
        "row.kind_id == \"web\" を検出できていない: {v:?}"
    );
}

#[test]
fn detector_flags_platform_identifier() {
    let src = r#"
        fn f() { let nostr_token = compute(); let _ = nostr_token; }
    "#;
    let v = scan_source("fixture.rs", src);
    assert!(
        v.iter().any(|m| m.contains("nostr")),
        "nostr_token 識別子を検出できていない: {v:?}"
    );
}

#[test]
fn detector_ignores_doc_comments_and_tests() {
    // doc コメント・cfg(test) 内は platform 語彙があっても素通り。
    let src = r#"
        //! DESIGN-NOSTRGATE のモジュール doc。
        /// nostr / discord のことを説明する doc。
        fn clean() -> u32 { 0 }

        #[cfg(test)]
        mod tests {
            fn helper() { let discord_id = "x"; let _ = discord_id; }
            fn kb(kind_id: &str) -> bool { kind_id == "nostr" }
        }
    "#;
    let v = scan_source("fixture.rs", src);
    assert!(
        v.is_empty(),
        "doc コメント / cfg(test) を誤検出している: {v:?}"
    );
}
