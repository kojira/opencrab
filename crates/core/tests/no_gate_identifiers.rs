//! R7: 共有層（core の production コード）に gate/SDK 固有の名前が現れない。
//!
//! 守るもの: DESIGN.md §2.2 / design-plugin-architecture.md の「コア（gateway
//! 非依存層）は特定の transport・SDK を知らない」という境界。R4（`scripts/
//! check-deps.sh`）が *依存ツリー* の逆流を止めるのに対し、こちらは *ソース* の
//! レベルで「Discord / Nostr を名指しする識別子や文字列」が core に紛れ込むのを
//! 止める。名指しが 1 つ入ると、上位がそのゲートウェイを特別扱いし始める入口に
//! なる（design-plugin-architecture.md §4 が実際に起きたと記録している事故）。
//!
//! 検査方法は `cargo expand` ではなく `syn` による AST 走査にした。expand は
//! nightly を要し、しかも doc コメントが展開結果へ残って偽陽性を生む。ここでは
//! ソースをそのまま構文木にして、次を確かめる。
//!
//! - **テスト専用の項目**（`#[cfg(test)]` / `#[cfg(all(test, ...))]` 等）の
//!   サブツリーはスキップ。ただし `any(test, ...)` はテスト以外でもコンパイル
//!   されるのでスキップしない（`is_cfg_test` を参照）。
//! - 属性は **doc コメント（`#[doc = "..."]`）だけ**除外し、それ以外の属性
//!   （`#[serde(rename = "...")]` / `#[error("...")]` 等）の中の文字列・識別子
//!   は検査する（本番の挙動・ワイヤ表現にゲート名が焼き込まれるため）。
//! - 識別子と文字列リテラルを見る。
//!
//! こうして production の識別子・文字列に禁止語が無いことを確かめる。
//!
//! **限界（意図的）**: この検査が届くのは AST に現れるトークンに限る。`syn` は
//! `tracing::info!("... discord ...")` や `format!(...)` などマクロ本体を
//! 生トークンのまま保持し、文字列・識別子として展開しない。したがって
//! **マクロ本体に書いたゲート名（ログ文言など）は検出できない**。core は
//! tracing を多用するのでこの穴は実在するが、マクロ内まで見るのは過剰なので
//! 検査の形は変えず、限界として明示する。

use std::path::{Path, PathBuf};

use proc_macro2::{TokenStream, TokenTree};
use quote::ToTokens;
use syn::visit::Visit;
use syn::{Attribute, Meta};

/// 禁止語（小文字・部分一致）。gate/SDK を名指しする語。
const FORBIDDEN: &[&str] = &["discord", "serenity", "songbird", "nostr"];

/// 「この cfg が付いた項目は *テスト時にしかコンパイルされない*」か。
///
/// トップレベルのトークンだけを見ると `#[cfg(all(test, unix))]` を取りこぼす
/// （`all` と Group `(test , unix)` の 2 トークンに割れ、どちらも `"test"` に
/// ならない）ので、cfg 述語を再帰的に評価する。`all` と `any` は意味が逆なので
/// 区別する。
///
/// - `test`（単独）: テスト専用。スキップする。
/// - `all(A, B, ...)`: A かつ B。**どれか一つでも**テスト専用なら全体もテスト
///   専用。スキップする。
/// - `any(A, B, ...)`: A または B。テスト以外でもコンパイルされうるので、
///   **全部**がテスト専用でない限りスキップしない（`any(test, unix)` は unix
///   でも生きる）。
/// - `not(...)` / それ以外: テスト専用を保証できない。スキップしない。
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

/// cfg 述語のトークン列が「テスト時のみ真」を保証するか。
fn cfg_predicate_is_test_only(tokens: TokenStream) -> bool {
    let trees: Vec<TokenTree> = tokens.into_iter().collect();
    predicate_trees_is_test_only(&trees)
}

fn predicate_trees_is_test_only(trees: &[TokenTree]) -> bool {
    match trees {
        // 単独の `test`
        [TokenTree::Ident(id)] => *id == "test",
        // `all(...)` / `any(...)` / `not(...)`: Ident のあとに括弧グループ
        [TokenTree::Ident(op), TokenTree::Group(group)] => {
            let inner: Vec<TokenTree> = group.stream().into_iter().collect();
            let subs = split_top_level_commas(&inner);
            match op.to_string().as_str() {
                // かつ: 一つでもテスト専用なら全体もテスト専用
                "all" => subs.iter().any(|s| predicate_trees_is_test_only(s)),
                // または: 全部テスト専用でなければ保証できない
                "any" => subs.iter().all(|s| predicate_trees_is_test_only(s)),
                // not() や feature = "..." 等はテスト専用を保証しない
                _ => false,
            }
        }
        _ => false,
    }
}

/// トップレベルの `,` で区切って部分述語ごとのトークン列に分ける。
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

/// 語に禁止語が部分一致（小文字化して比較）するなら、その禁止語を返す。
fn hit(word: &str) -> Option<&'static str> {
    let lower = word.to_ascii_lowercase();
    FORBIDDEN.iter().copied().find(|w| lower.contains(w))
}

struct Finder<'a> {
    file: &'a Path,
    violations: Vec<String>,
}

impl<'a> Finder<'a> {
    fn record(&mut self, kind: &str, text: &str, word: &str) {
        self.violations.push(format!(
            "{}: {} \"{}\" に禁止語 \"{}\"",
            self.file.display(),
            kind,
            text,
            word
        ));
    }

    /// 属性の生トークン列を走査する。属性の中身（`#[serde(rename = "...")]` /
    /// `#[error("...")]` 等）は syn の AST では生トークンのままで、`visit_lit_str`
    /// には届かない。ここでトークンを直接見て、文字列リテラルと識別子を検査する。
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
                    if let syn::Lit::Str(s) = syn::Lit::new(lit) {
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

impl<'ast> Visit<'ast> for Finder<'_> {
    // --- `#[cfg(test)]` サブツリーのスキップ ---
    // 属性を持ちうる各ノードで、cfg(test) が付いていれば再帰しない。

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

    // --- 属性 ---
    // 走査対象から外すのは doc コメント（`#[doc = "..."]`）**だけ**。doc には
    // Discord / Nostr が正当に多数出るため除外する。それ以外の属性
    // （`#[serde(rename = "discord")]` / `#[error("... nostr ...")]` 等）は
    // 本番の挙動やワイヤ表現にゲート名を焼き込むので、中の文字列リテラル・
    // 識別子を検査する。属性の中身は生トークンなので専用に走査する
    // （syn の `visit_lit_str` には届かない）。
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
    // 属性を持つ主要な Item 種別だけ判定すれば十分（mod / fn / impl など）。
    // `#[cfg(test)] mod tests { ... }` が最も典型。
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

/// `crates/core/src` 以下の `.rs` を再帰列挙する。
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

#[test]
fn core_production_has_no_gate_identifiers() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);
    files.sort();
    assert!(!files.is_empty(), "core の src に .rs が見つからない");

    let mut violations = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("read {} に失敗: {e}", file.display()));
        let ast = match syn::parse_file(&text) {
            Ok(ast) => ast,
            Err(e) => panic!("{} の構文解析に失敗: {e}", file.display()),
        };
        let mut finder = Finder {
            file,
            violations: Vec::new(),
        };
        finder.visit_file(&ast);
        violations.extend(finder.violations);
    }

    assert!(
        violations.is_empty(),
        "core の production コードに gate/SDK 固有の識別子・文字列が見つかった \
         （production 側に混入した証拠。禁止語を減らさず該当箇所を直すこと）:\n{}",
        violations.join("\n")
    );
}
