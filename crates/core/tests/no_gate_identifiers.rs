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
//! ソースをそのまま構文木にして、
//!   - `#[cfg(test)]` が付いた項目のサブツリーはスキップ（テストコードは対象外）
//!   - 属性（`#[doc]` = doc コメント、`#[cfg]` など）は走査しない
//!     （doc コメントに Discord / Nostr が多数出てくるため）
//!   - 識別子と文字列リテラルだけを見る
//! という形で、production の識別子・文字列に禁止語が無いことだけを確かめる。

use std::path::{Path, PathBuf};

use syn::visit::Visit;
use syn::{Attribute, Meta};

/// 禁止語（小文字・部分一致）。gate/SDK を名指しする語。
const FORBIDDEN: &[&str] = &["discord", "serenity", "songbird", "nostr"];

/// `#[cfg(test)]`（`#[cfg(all(test, ...))]` 等も含む）が付いているか。
fn is_cfg_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("cfg") {
            return false;
        }
        match &attr.meta {
            // cfg(...) は Meta::List。トークン列に `test` が含まれていれば
            // テスト専用項目とみなす。
            Meta::List(list) => list
                .tokens
                .clone()
                .into_iter()
                .any(|tt| tt.to_string() == "test"),
            _ => false,
        }
    })
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

    // --- 属性は走査対象から外す ---
    // doc コメント（`#[doc = "..."]`）や `#[cfg]` の中身に禁止語があっても
    // 見ない。ここを no-op にすることで属性配下へは一切降りない。
    fn visit_attribute(&mut self, _node: &'ast Attribute) {}

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
