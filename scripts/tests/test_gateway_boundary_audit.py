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

    def test_concrete_route_mutation_is_rejected(self):
        files = {"crates/server/src/lib.rs": 'router.route("/api/agents/{id}/trusted-users", get(handler));\n'}
        self.assertIn("shared-concrete-route", self.rules(files))

    def test_gateway_kind_branch_mutation_is_rejected(self):
        files = {"crates/core/src/dispatch.rs": 'if gateway_kind == "matrix" { select_adapter(); }\n'}
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
                'let platform = descriptor.platform();\nif kind == "tool" { index(platform); }\n'
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


if __name__ == "__main__":
    unittest.main()
