use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use serde_json::{json, Value};
use syn::{
    visit::{self, Visit},
    Attribute, Expr, ExprCall, ExprLit, ExprMethodCall, File, ItemFn, Lit, Meta,
};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Route {
    path: String,
    methods: Vec<String>,
    activation: String,
    source: String,
}

fn cfg_text(attrs: &[Attribute]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for attr in attrs {
        if attr.path().is_ident("cfg") {
            let Meta::List(list) = &attr.meta else {
                return Err("cfg attribute is not a list".to_string());
            };
            out.push(format!("cfg({})", list.tokens));
        }
    }
    Ok(out)
}

fn activation(parts: &[String]) -> String {
    if parts.is_empty() {
        "always".to_string()
    } else {
        parts.join(" && ")
    }
}

fn call_name(expr: &Expr) -> Option<String> {
    let Expr::Call(ExprCall { func, .. }) = expr else {
        return None;
    };
    let Expr::Path(path) = func.as_ref() else {
        return None;
    };
    Some(
        path.path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>()
            .join("::"),
    )
}

fn route_methods(expr: &Expr, methods: &mut BTreeSet<String>) -> Result<(), String> {
    match expr {
        Expr::Call(_) => {
            let name =
                call_name(expr).ok_or_else(|| "route method is not a named call".to_string())?;
            let method = name.rsplit("::").next().unwrap_or_default();
            match method {
                "get" | "post" | "put" | "patch" | "delete" | "head" | "options" | "trace"
                | "connect" => {
                    methods.insert(method.to_ascii_uppercase());
                    Ok(())
                }
                _ => Err(format!("unsupported route method constructor: {name}")),
            }
        }
        Expr::MethodCall(call) => {
            route_methods(&call.receiver, methods)?;
            let method = call.method.to_string();
            match method.as_str() {
                "get" | "post" | "put" | "patch" | "delete" | "head" | "options" | "trace"
                | "connect" => {
                    methods.insert(method.to_ascii_uppercase());
                    Ok(())
                }
                _ => Err(format!("unsupported chained route method: {method}")),
            }
        }
        _ => Err("route method expression is not a call or method chain".to_string()),
    }
}

struct RouteVisitor {
    conditions: Vec<String>,
    routes: Vec<Route>,
    errors: Vec<String>,
    source: String,
}

impl<'ast> Visit<'ast> for RouteVisitor {
    fn visit_expr_block(&mut self, node: &'ast syn::ExprBlock) {
        let before = self.conditions.len();
        match cfg_text(&node.attrs) {
            Ok(parts) => self.conditions.extend(parts),
            Err(error) => self.errors.push(error),
        }
        visit::visit_expr_block(self, node);
        self.conditions.truncate(before);
    }

    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        if node.method == "route" {
            if node.args.len() != 2 {
                self.errors.push(format!(
                    "{}: .route must have exactly two arguments",
                    self.source
                ));
                return;
            }
            let path_expr = &node.args[0];
            let Expr::Lit(ExprLit {
                lit: Lit::Str(path),
                ..
            }) = path_expr
            else {
                self.errors.push(format!(
                    "{}: route path is not a string literal",
                    self.source
                ));
                return;
            };
            let mut methods = BTreeSet::new();
            if let Err(error) = route_methods(&node.args[1], &mut methods) {
                self.errors
                    .push(format!("{} {}: {error}", self.source, path.value()));
                return;
            }
            self.routes.push(Route {
                path: path.value(),
                methods: methods.into_iter().collect(),
                activation: activation(&self.conditions),
                source: self.source.clone(),
            });
        }
        visit::visit_expr_method_call(self, node);
    }
}

fn parse_file(path: &Path) -> Result<File, String> {
    let source = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    syn::parse_file(&source).map_err(|e| format!("{}: Rust parse failed: {e}", path.display()))
}

fn collect_function_routes(
    repo: &Path,
    relative: &str,
    function: &str,
    inherited_condition: Option<&str>,
) -> Result<Vec<Route>, String> {
    let file = parse_file(&repo.join(relative))?;
    let matching: Vec<&ItemFn> = file
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Fn(item_fn) if item_fn.sig.ident == function => Some(item_fn),
            _ => None,
        })
        .collect();
    if matching.len() != 1 {
        return Err(format!(
            "{relative}: expected exactly one function named {function}, found {}",
            matching.len()
        ));
    }
    let item = matching[0];
    let mut conditions = cfg_text(&item.attrs)?;
    if let Some(condition) = inherited_condition {
        conditions.push(condition.to_string());
    }
    let mut visitor = RouteVisitor {
        conditions,
        routes: vec![],
        errors: vec![],
        source: format!("{relative}::{function}"),
    };
    visitor.visit_block(&item.block);
    if !visitor.errors.is_empty() {
        return Err(visitor.errors.join("\n"));
    }
    Ok(visitor.routes)
}

fn assert_feature_merge(
    repo: &Path,
    target_suffix: &str,
    expected_feature: &str,
) -> Result<(), String> {
    let file = parse_file(&repo.join("crates/server/src/lib.rs"))?;
    let function = file
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(item_fn) if item_fn.sig.ident == "create_router" => Some(item_fn),
            _ => None,
        })
        .ok_or_else(|| "create_router not found".to_string())?;

    struct MergeVisitor {
        conditions: Vec<String>,
        found: Vec<(String, String)>,
    }
    impl<'ast> Visit<'ast> for MergeVisitor {
        fn visit_expr_block(&mut self, node: &'ast syn::ExprBlock) {
            let before = self.conditions.len();
            if let Ok(parts) = cfg_text(&node.attrs) {
                self.conditions.extend(parts);
            }
            visit::visit_expr_block(self, node);
            self.conditions.truncate(before);
        }
        fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
            if node.method == "merge" && node.args.len() == 1 {
                if let Some(name) = call_name(&node.args[0]) {
                    self.found.push((name, activation(&self.conditions)));
                }
            }
            visit::visit_expr_method_call(self, node);
        }
    }
    let mut visitor = MergeVisitor {
        conditions: vec![],
        found: vec![],
    };
    visitor.visit_block(&function.block);
    if visitor.found.len() != 2 {
        return Err(format!(
            "create_router merge topology changed: expected exactly 2 router merges, found {}",
            visitor.found.len()
        ));
    }
    let matches: Vec<_> = visitor
        .found
        .iter()
        .filter(|(name, _)| name.ends_with(target_suffix))
        .collect();
    if matches.len() != 1 {
        return Err(format!(
            "expected exactly one merge target ending {target_suffix}, found {}",
            matches.len()
        ));
    }
    let expected = format!("cfg(feature = \"{expected_feature}\")");
    if matches[0].1 != expected {
        return Err(format!(
            "merge {target_suffix} activation changed: expected {expected:?}, got {:?}",
            matches[0].1
        ));
    }
    Ok(())
}

fn collect_routes(repo: &Path) -> Result<Vec<Route>, String> {
    assert_feature_merge(repo, "nostr_routes", "nostr")?;
    assert_feature_merge(repo, "opencrab_web_gateway::routes", "web")?;

    let mut routes =
        collect_function_routes(repo, "crates/server/src/lib.rs", "create_router", None)?;
    routes.extend(collect_function_routes(
        repo,
        "crates/server/src/lib.rs",
        "nostr_routes",
        None,
    )?);
    routes.extend(collect_function_routes(
        repo,
        "crates/web-gateway/src/http.rs",
        "routes",
        Some("cfg(feature = \"web\")"),
    )?);
    routes.sort();

    let mut seen = BTreeSet::new();
    for route in &routes {
        if !seen.insert(route.path.clone()) {
            return Err(format!(
                "duplicate route path was not merged: {}",
                route.path
            ));
        }
    }
    Ok(routes)
}

fn route_json(route: Route) -> Value {
    json!({
        "path": route.path,
        "methods": route.methods,
        "activation": route.activation,
        "source": route.source,
    })
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("baseline-l1: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "cannot resolve repository root".to_string())?;
    let output = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| repo.join("baseline/l1/opencrab-l1.json"));

    let routes = collect_routes(repo)?;
    let route_count = routes.len();
    let document = json!({
        "schema_version": 1,
        "scope": "opencrab-external-shape-l1",
        "http": {
            "route_count": route_count,
            "routes": routes.into_iter().map(route_json).collect::<Vec<_>>(),
            "uncollected": []
        },
        "tools": opencrab_server::baseline_l1::collect_tools(),
        "fixed_responses": opencrab_server::baseline_l1::collect_responses().await?,
    });
    let bytes = serde_json::to_vec_pretty(&document)
        .map_err(|e| format!("JSON serialization failed: {e}"))?;
    let parent = output
        .parent()
        .ok_or_else(|| format!("output has no parent: {}", output.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let mut with_newline = bytes;
    with_newline.push(b'\n');
    fs::write(&output, with_newline).map_err(|e| format!("{}: {e}", output.display()))?;
    println!("wrote {} ({} HTTP routes)", output.display(), route_count);
    Ok(())
}
