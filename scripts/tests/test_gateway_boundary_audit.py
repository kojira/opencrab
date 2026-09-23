import importlib.util
import pathlib
import tempfile
import textwrap
import unittest

MODULE_PATH = pathlib.Path(__file__).parents[1] / "gateway_boundary_audit.py"
SPEC = importlib.util.spec_from_file_location("gateway_boundary_audit", MODULE_PATH)
AUDIT = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(AUDIT)


class GatewayBoundaryMutationTests(unittest.TestCase):
    def rules(self, files):
        return {finding.rule for finding in AUDIT.audit_texts(files)}

    def findings(self, files, rule):
        return [finding for finding in AUDIT.audit_texts(files) if finding.rule == rule]

    def test_gateway_normal_dependency_is_rejected(self):
        files = {
            "crates/example-gateway/Cargo.toml": """
[package]
name = "opencrab-example-gateway"
[dependencies]
opencrab-core = { path = "../core" }
"""
        }
        self.assertIn("gateway-production-dependency", self.rules(files))

    def test_gateway_build_dependency_is_rejected(self):
        files = {
            "crates/example-gateway/Cargo.toml": """
[package]
name = "opencrab-example-gateway"
[build-dependencies]
opencrab-server = { path = "../server" }
"""
        }
        self.assertIn("gateway-production-dependency", self.rules(files))

    def test_reverse_normal_concrete_gateway_dependency_is_rejected(self):
        files = {
            "crates/server/Cargo.toml": """
[package]
name = "opencrab-server"
[dependencies]
opencrab-example-gateway = { path = "../example-gateway" }
"""
        }
        self.assertIn("platform-production-gateway-dependency", self.rules(files))

    def test_reverse_build_concrete_gateway_dependency_is_rejected(self):
        files = {
            "crates/core/Cargo.toml": """
[package]
name = "opencrab-core"
[build-dependencies]
example-daemon = { package = "opencrab-example-gateway", path = "../example-gateway" }
"""
        }
        self.assertIn("platform-production-gateway-dependency", self.rules(files))

    def test_dev_only_qc_dependency_is_not_production(self):
        files = {
            "crates/server/Cargo.toml": """
[package]
name = "opencrab-server"
[dev-dependencies]
opencrab-example-gateway = { path = "../example-gateway" }
"""
        }
        self.assertNotIn("platform-production-gateway-dependency", self.rules(files))
        self.assertNotIn("unreviewed-gateway-dev-dependency", self.rules(files))
        self.assertIn("reviewed-gateway-dev-dependency", self.rules(files))

    def test_dev_gateway_edge_outside_reviewed_server_qc_scope_is_rejected(self):
        files = {
            "crates/core/Cargo.toml": """
[package]
name = "opencrab-core"
[dev-dependencies]
opencrab-example-gateway = { path = "../example-gateway" }
"""
        }
        self.assertIn("unreviewed-gateway-dev-dependency", self.rules(files))

    def _metadata_workspace(self, dependency_table):
        temp = tempfile.TemporaryDirectory()
        root = pathlib.Path(temp.name)
        (root / "Cargo.toml").write_text(textwrap.dedent("""
            [workspace]
            resolver = "2"
            members = ["crates/server", "crates/example-gateway", "crates/core"]
        """))
        for name in ("server", "example-gateway", "core"):
            crate = root / "crates" / name
            (crate / "src").mkdir(parents=True)
            (crate / "src/lib.rs").write_text("")
        (root / "crates/core/Cargo.toml").write_text(textwrap.dedent("""
            [package]
            name = "opencrab-core"
            version = "0.0.0"
            edition = "2021"
        """))
        (root / "crates/example-gateway/Cargo.toml").write_text(textwrap.dedent("""
            [package]
            name = "opencrab-example-gateway"
            version = "0.0.0"
            edition = "2021"
            [dependencies]
            opencrab-core = { path = "../core" }
        """))
        (root / "crates/server/Cargo.toml").write_text(textwrap.dedent(f"""
            [package]
            name = "opencrab-server"
            version = "0.0.0"
            edition = "2021"
            {dependency_table}
            opencrab-example-gateway = {{ path = "../example-gateway" }}
        """))
        return temp, root

    def test_cargo_metadata_allows_reviewed_server_dev_qc_edge(self):
        temp, root = self._metadata_workspace("[dev-dependencies]")
        with temp:
            production, dev_only = AUDIT.cargo_metadata_evidence(root)
        self.assertNotIn(
            ("platform-production-gateway-dependency", "crates/server/Cargo.toml", "opencrab-example-gateway"),
            production,
        )
        self.assertIn("opencrab-server -> opencrab-example-gateway", dev_only)
        self.assertIn(
            ("gateway-production-dependency", "crates/example-gateway/Cargo.toml", "opencrab-core"),
            production,
        )

    def test_cargo_metadata_rejects_server_gateway_normal_edge(self):
        temp, root = self._metadata_workspace("[dependencies]")
        with temp:
            production, _ = AUDIT.cargo_metadata_evidence(root)
        self.assertIn(
            ("platform-production-gateway-dependency", "crates/server/Cargo.toml", "opencrab-example-gateway"),
            production,
        )

    def test_cargo_metadata_rejects_server_gateway_build_edge(self):
        temp, root = self._metadata_workspace("[build-dependencies]")
        with temp:
            production, _ = AUDIT.cargo_metadata_evidence(root)
        self.assertIn(
            ("platform-production-gateway-dependency", "crates/server/Cargo.toml", "opencrab-example-gateway"),
            production,
        )

    def test_schema_identifier_mutation_is_rejected(self):
        files = {"crates/db/src/schema/sql/full.rs": 'const SQL: &str = "CREATE TABLE session_watch (id TEXT)";\n'}
        self.assertIn("shared-concrete-schema", self.rules(files))

    def test_platform_dto_field_mutation_is_rejected(self):
        files = {"crates/db/src/queries/trusted_users.rs": "pub struct ExternalUser { pub platform: String }\n"}
        self.assertIn("shared-platform-dto", self.rules(files))

    def test_platform_dto_qualified_lowercase_type_is_rejected(self):
        files = {
            "crates/db/src/queries/trusted_users.rs":
                "pub struct ExternalUser { pub platform: serde_json::Value }\n"
        }
        self.assertIn("shared-platform-dto", self.rules(files))

    def test_concrete_route_mutation_is_rejected(self):
        files = {"crates/server/src/lib.rs": 'router.route("/api/agents/{id}/trusted-users", get(handler));\n'}
        self.assertIn("shared-concrete-route", self.rules(files))

    def test_gateway_kind_branch_mutation_is_rejected(self):
        files = {"crates/core/src/dispatch.rs": 'if gateway_kind == "matrix" { select_adapter(); }\n'}
        self.assertIn("shared-gateway-name-branch", self.rules(files))

    def test_gateway_kind_non_equality_branch_is_rejected(self):
        for operator in ("!=", "<", "<=", ">", ">="):
            with self.subTest(operator=operator):
                files = {
                    "crates/core/src/dispatch.rs":
                        f'if request.gateway_kind {operator} "matrix" {{ select_adapter(); }}\n'
                }
                self.assertIn("shared-gateway-name-branch", self.rules(files))

    def test_borrowed_qualified_gateway_name_match_is_rejected(self):
        files = {
            "crates/core/src/dispatch.rs":
                'match &request.gateway_name { "matrix" => select_adapter(), _ => fallback() }\n'
        }
        self.assertIn("shared-gateway-name-branch", self.rules(files))

    def test_qualified_operation_name_match_is_rejected(self):
        files = {
            "crates/extgate/src/dispatch.rs":
                'match request.operation_name.as_str() { "send" => utter(), _ => fallback() }\n'
        }
        self.assertIn("shared-gateway-name-branch", self.rules(files))

    def test_operation_name_branch_mutation_is_rejected(self):
        files = {"crates/extgate/src/dispatch.rs": 'if operation_name == "matrix_send" { utter(); }\n'}
        self.assertIn("shared-gateway-name-branch", self.rules(files))

    def test_session_watch_singular_query_symbol_is_rejected(self):
        files = {"crates/db/src/queries/session_watches.rs": "pub fn get_session_watch(id: i64) {}\n"}
        self.assertIn("shared-concrete-schema", self.rules(files))

    def test_session_watches_plural_camel_symbol_is_rejected(self):
        files = {"crates/db/src/queries/session_watches.rs": "pub struct SessionWatchesResult;\n"}
        self.assertIn("shared-concrete-schema", self.rules(files))

    def test_shared_concrete_vocabulary_excludes_nonproduction_sources(self):
        permitted = {
            "crates/core/tests/example.rs": "pub struct DiscordChannelConfig { pub channel_id: String }\n",
            "crates/core/src/example_tests.rs": "pub struct DiscordChannelConfig { pub channel_id: String }\n",
            "crates/core/src/example_e2e/fixture.rs": "pub struct DiscordChannelConfig { pub channel_id: String }\n",
            "crates/db/src/schema/migrations/v999.rs": 'const SQL: &str = "CREATE TABLE agent_discord_config";\n',
            "crates/core/src/comment.rs": "// Discord channel_id is historical evidence.\n",
        }
        self.assertFalse(self.rules(permitted))

    def test_legitimate_generic_platform_value_and_kind_branch_are_not_broadly_banned(self):
        permitted = {
            "crates/core/src/catalog.rs":
                'let platform = descriptor.platform();\n'
                'if kind == "tool" { index(platform); }\n'
                'if request.kind != "tool" { fallback(); }\n'
                'match &request.name { "tool" => index(platform), _ => fallback() }\n'
        }
        self.assertFalse(self.rules(permitted))

    def test_gateway_core_path_acceptance_is_rejected(self):
        files = {"crates/example-gateway/src/config.rs": "pub core_path: PathBuf,\n"}
        self.assertIn("gateway-core-path", self.rules(files))

    def test_direct_aliased_sqlite_open_is_inventoried(self):
        files = {
            "crates/example-gateway/src/store.rs":
                "use rusqlite::Connection as Sqlite;\nfn open(path: &Path) { Sqlite::open(path); }\n"
        }
        self.assertIn("gateway-db-open", self.rules(files))

    def test_grouped_arbitrary_sqlite_alias_open_is_inventoried(self):
        files = {
            "crates/example-gateway/src/store.rs":
                "use rusqlite::{Connection as Conn, OpenFlags};\n"
                "fn open(path: &Path) { Conn::open(path); }\n"
        }
        self.assertIn("gateway-db-open", self.rules(files))

    def test_sqlite_connection_type_alias_open_is_inventoried(self):
        files = {
            "crates/example-gateway/src/store.rs":
                "type StoreConn = rusqlite::Connection;\n"
                "fn open(path: &Path) { StoreConn::open(path); }\n"
        }
        self.assertIn("gateway-db-open", self.rules(files))

    def test_pub_crate_sqlite_connection_type_alias_open_is_inventoried(self):
        files = {
            "crates/example-gateway/src/store.rs":
                "pub(crate) type StoreConn = rusqlite::Connection;\n"
                "fn open(path: &Path) { StoreConn::open(path); }\n"
        }
        self.assertIn("gateway-db-open", self.rules(files))

    def test_pub_in_sqlite_connection_type_alias_chain_is_inventoried(self):
        files = {
            "crates/example-gateway/src/store.rs":
                "pub(in crate::store) type InnerConn = rusqlite::Connection;\n"
                "pub(super) type StoreConn = InnerConn;\n"
                "fn open(path: &Path) { StoreConn::open(path); }\n"
        }
        self.assertIn("gateway-db-open", self.rules(files))

    def test_helper_mediated_core_store_open_is_rejected(self):
        files = {
            "crates/example-gateway/src/daemon.rs":
                "fn run(args: Args) { CoreStore::connect(args.core_path); }\n"
        }
        self.assertIn("gateway-core-store-open", self.rules(files))

    def test_arbitrary_core_import_is_not_mislabeled_as_sqlite_open(self):
        files = {"crates/example-gateway/src/crypto.rs": "use opencrab_core::secret_box;\n"}
        self.assertFalse(self.rules(files))

    def test_gateway_owned_store_open_is_explicit_inventory_not_silent(self):
        files = {
            "crates/example-gateway/src/daemon.rs":
                "fn run(config: Config) { GatewayStore::open(&config.database_path); }\n"
        }
        findings = self.findings(files, "gateway-db-open")
        self.assertEqual(1, len(findings), findings)
        errors = AUDIT.check_baseline(findings, {"entries": [], "coverage": []})
        self.assertTrue(any(error.startswith("UNCLASSIFIED gateway-db-open") for error in errors), errors)

    @staticmethod
    def _public_listener(factory_call):
        return textwrap.dedent(f"""
            async fn main() {{
                let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
                let app = {factory_call};
                axum::serve(listener, app).await.unwrap();
            }}
        """)

    @staticmethod
    def _six_operation_router(name):
        return textwrap.dedent(f"""
            fn {name}() -> Router {{
                Router::new()
                    .route("/api/gate-instances/{{instance_id}}", get(get_one).put(put_one).delete(delete_one))
                    .route("/api/gate-instances/{{instance_id}}/revisions", post(post_revision))
                    .route("/api/gate-bindings/{{binding_id}}", put(put_binding).delete(delete_binding))
            }}
        """)

    def test_public_reachability_rejects_renamed_admin_factory(self):
        files = {
            "crates/server/src/main.rs": self._public_listener("public_router()"),
            "crates/server/src/lib.rs": self._six_operation_router("renamed_control_plane")
                + "fn public_router() -> Router { renamed_control_plane() }\n",
        }
        self.assertIn("public-gate-admin-reachable", self.rules(files))

    def test_public_reachability_rejects_direct_six_operation_registration(self):
        files = {
            "crates/server/src/main.rs": self._public_listener("public_router()"),
            "crates/server/src/lib.rs": self._six_operation_router("public_router"),
        }
        self.assertIn("public-gate-admin-reachable", self.rules(files))

    def test_public_reachability_traces_direct_served_factory_expression(self):
        files = {
            "crates/server/src/main.rs": textwrap.dedent("""
                async fn main() {
                    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
                    axum::serve(listener, public_router()).await.unwrap();
                }
            """),
            "crates/server/src/lib.rs": self._six_operation_router("protected_controls")
                + "fn public_router() -> Router { protected_controls() }\n",
        }
        self.assertIn("public-gate-admin-reachable", self.rules(files))

    def test_public_reachability_traces_mutation_and_alias_into_serve(self):
        files = {
            "crates/server/src/main.rs": textwrap.dedent("""
                async fn main() {
                    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
                    let mut app = public_router();
                    app = app.merge(protected_controls());
                    let served = app;
                    axum::serve(listener, served).await.unwrap();
                }
            """),
            "crates/server/src/lib.rs": self._six_operation_router("protected_controls")
                + "fn public_router() -> Router { Router::new() }\n",
        }
        self.assertIn("public-gate-admin-reachable", self.rules(files))

    def test_public_reachability_traces_function_item_alias(self):
        files = {
            "crates/server/src/main.rs": textwrap.dedent("""
                async fn main() {
                    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
                    let factory = protected_controls;
                    let app = factory();
                    axum::serve(listener, app).await.unwrap();
                }
            """),
            "crates/server/src/lib.rs": self._six_operation_router("protected_controls"),
        }
        self.assertIn("public-gate-admin-reachable", self.rules(files))

    def test_public_reachability_traces_qualified_function_item_alias(self):
        files = {
            "crates/server/src/main.rs": textwrap.dedent("""
                async fn main() {
                    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
                    let factory = crate::protected_controls;
                    let app = factory();
                    axum::serve(listener, app).await.unwrap();
                }
            """),
            "crates/server/src/lib.rs": self._six_operation_router("protected_controls"),
        }
        self.assertIn("public-gate-admin-reachable", self.rules(files))

    def test_public_reachability_ignores_mutation_after_served_alias(self):
        files = {
            "crates/server/src/main.rs": textwrap.dedent("""
                async fn main() {
                    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
                    let app = public_router();
                    let served = app.clone();
                    axum::serve(listener, served).await.unwrap();
                    let _unused = app.merge(protected_controls());
                }
            """),
            "crates/server/src/lib.rs": self._six_operation_router("protected_controls")
                + "fn public_router() -> Router { Router::new() }\n",
        }
        self.assertNotIn("public-gate-admin-reachable", self.rules(files))

    def test_public_reachability_rejects_public_merge(self):
        files = {
            "crates/server/src/main.rs": self._public_listener("public_router()"),
            "crates/server/src/lib.rs": self._six_operation_router("protected_controls")
                + "fn public_router() -> Router { Router::new().merge(protected_controls()) }\n",
        }
        self.assertIn("public-gate-admin-reachable", self.rules(files))

    def test_protected_uds_only_admin_router_is_allowed(self):
        files = {
            "crates/server/src/admin.rs": self._six_operation_router("admin_router")
                + "async fn serve_admin() { serve_uds(admin_router()).await; }\n",
            "crates/server/src/main.rs": self._public_listener("Router::new()"),
        }
        self.assertFalse(self.rules(files))

    def test_unclassified_and_stale_burn_down_entries_fail_closed(self):
        finding = AUDIT.audit_texts(
            {"crates/db/src/schema/sql/full.rs": 'const SQL: &str = "CREATE TABLE session_watch";\n'}
        )[0]
        empty = {"entries": [], "coverage": []}
        errors = AUDIT.check_baseline([finding], empty)
        self.assertTrue(any(error.startswith("UNCLASSIFIED") for error in errors), errors)

        stale_entry = {
            "rule": finding.rule,
            "path": finding.path,
            "line": finding.line + 1,
            "snippet": finding.snippet,
            "classification": "production-violation",
            "violation": "V11",
            "owner_stage": "S3",
            "expires_when": "remove fixture debt",
        }
        errors = AUDIT.check_baseline([], {"entries": [stale_entry], "coverage": []})
        self.assertTrue(any(error.startswith("STALE baseline entry") for error in errors), errors)

    def test_generated_baseline_requires_explicit_line_review(self):
        finding = AUDIT.audit_texts(
            {"crates/db/src/schema/sql/full.rs": 'const SQL: &str = "CREATE TABLE session_watch";\n'}
        )[0]
        document = AUDIT.baseline_document([finding])
        errors = AUDIT.check_baseline([finding], document)
        self.assertIn("baseline review status must be line-by-line-reviewed", errors)
        document["review"]["status"] = "line-by-line-reviewed"
        errors = AUDIT.check_baseline([finding], document)
        self.assertFalse(any("baseline review" in error for error in errors), errors)

    @staticmethod
    def _reviewed_document_for(finding):
        document = AUDIT.baseline_document([finding])
        document["review"]["status"] = "line-by-line-reviewed"
        return document

    def test_baseline_cannot_relabel_production_db_open_as_valid_store(self):
        finding = AUDIT.audit_texts({
            "crates/example-gateway/src/store.rs":
                "fn open(path: &Path) { GatewayStore::open(path); }\n"
        })[0]
        document = self._reviewed_document_for(finding)
        document["entries"][0]["classification"] = "valid-gateway-owned-store"
        errors = AUDIT.check_baseline([finding], document)
        self.assertTrue(any("classification mismatch" in error for error in errors), errors)

    def test_valid_gateway_db_open_requires_exact_finding_identity(self):
        approved_sites = (
            (
                "crates/nostr-gateway/src/store.rs",
                66,
                "let conn = Connection::open(path)",
            ),
            (
                "crates/nostr-gateway/src/daemon.rs",
                115,
                "let mut gateway_store = GatewayStore::open(&config.database_path)?;",
            ),
        )
        for path, approved_line, snippet in approved_sites:
            with self.subTest(path=path):
                source = "\n" * (approved_line - 1) + snippet + "\n" + snippet + "\n"
                findings = self.findings({path: source}, "gateway-db-open")
                self.assertEqual([approved_line, approved_line + 1], [item.line for item in findings])
                approved, moved = (AUDIT._metadata_for(item) for item in findings)
                self.assertEqual("valid-gateway-owned-store", approved[0])
                self.assertEqual("production-violation", moved[0])
                document = self._reviewed_document_for(findings[0])
                errors = AUDIT.check_baseline([findings[1]], document)
                self.assertTrue(any(error.startswith("UNCLASSIFIED") for error in errors), errors)
                self.assertTrue(any(error.startswith("STALE baseline entry") for error in errors), errors)

    def test_baseline_violation_must_match_normative_metadata(self):
        finding = AUDIT.audit_texts({
            "crates/example-gateway/src/store.rs":
                "fn open(path: &Path) { GatewayStore::open(path); }\n"
        })[0]
        document = self._reviewed_document_for(finding)
        document["entries"][0]["violation"] = "V16"
        errors = AUDIT.check_baseline([finding], document)
        self.assertTrue(any("violation mismatch" in error for error in errors), errors)

    def test_baseline_owner_must_match_normative_metadata(self):
        finding = AUDIT.audit_texts({
            "crates/example-gateway/src/store.rs":
                "fn open(path: &Path) { GatewayStore::open(path); }\n"
        })[0]
        document = self._reviewed_document_for(finding)
        document["entries"][0]["owner_stage"] = "S11"
        errors = AUDIT.check_baseline([finding], document)
        self.assertTrue(any("owner_stage mismatch" in error for error in errors), errors)

    def test_baseline_expiry_must_match_normative_metadata(self):
        finding = AUDIT.audit_texts({
            "crates/example-gateway/src/store.rs":
                "fn open(path: &Path) { GatewayStore::open(path); }\n"
        })[0]
        document = self._reviewed_document_for(finding)
        document["entries"][0]["expires_when"] = "never"
        errors = AUDIT.check_baseline([finding], document)
        self.assertTrue(any("expires_when mismatch" in error for error in errors), errors)


if __name__ == "__main__":
    unittest.main()
