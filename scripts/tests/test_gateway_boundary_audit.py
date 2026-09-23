import importlib.util
import pathlib
import unittest

MODULE_PATH = pathlib.Path(__file__).parents[1] / "gateway_boundary_audit.py"
SPEC = importlib.util.spec_from_file_location("gateway_boundary_audit", MODULE_PATH)
AUDIT = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(AUDIT)


class GatewayBoundaryMutationTests(unittest.TestCase):
    def rules(self, files):
        return {finding.rule for finding in AUDIT.audit_texts(files)}

    def test_rejects_normal_gateway_dependency_but_allows_dev_only_qc(self):
        normal = {
            "crates/example-gateway/Cargo.toml": """
[package]
name = "opencrab-example-gateway"
[dependencies]
opencrab-core = { path = "../core" }
"""
        }
        self.assertIn("gateway-production-dependency", self.rules(normal))

        build = {
            "crates/example-gateway/Cargo.toml": """
[package]
name = "opencrab-example-gateway"
[build-dependencies]
opencrab-server = { path = "../server" }
"""
        }
        self.assertIn("gateway-production-dependency", self.rules(build))

        dev_only = {
            "crates/server/Cargo.toml": """
[package]
name = "opencrab-server"
[dev-dependencies]
opencrab-example-gateway = { path = "../example-gateway" }
"""
        }
        self.assertNotIn("gateway-production-dependency", self.rules(dev_only))

    def test_rejects_concrete_platform_symbol_in_shared_production_only(self):
        production = {
            "crates/core/src/example.rs": "pub struct DiscordChannelConfig { pub channel_id: String }\n"
        }
        self.assertIn("shared-concrete-vocabulary", self.rules(production))

        permitted = {
            "crates/core/tests/example.rs": "pub struct DiscordChannelConfig { pub channel_id: String }\n",
            "crates/core/src/example_tests.rs": "pub struct DiscordChannelConfig { pub channel_id: String }\n",
            "crates/db/src/schema/migrations/v999.rs": "const SQL: &str = \"CREATE TABLE agent_discord_config\";\n",
            "crates/core/src/comment.rs": "// Discord channel_id is historical evidence.\n",
        }
        self.assertNotIn("shared-concrete-vocabulary", self.rules(permitted))

    def test_rejects_concrete_gateway_core_sqlite_open(self):
        fixture = {
            "crates/example-gateway/src/daemon.rs": "let core_database_path = args.core_database_path;\nlet db = opencrab_db::Db::open(core_database_path)?;\n"
        }
        self.assertIn("gateway-core-sqlite-open", self.rules(fixture))

    def test_rejects_public_admin_router_merge(self):
        fixture = {
            "crates/server/src/lib.rs": "let public = Router::new().merge(opencrab_extgate::admin_router(state));\n"
        }
        self.assertIn("public-gate-admin", self.rules(fixture))

    def test_unclassified_and_stale_burn_down_entries_fail_closed(self):
        finding = AUDIT.audit_texts(
            {
                "crates/core/src/example.rs":
                    "pub struct DiscordChannelConfig { pub channel_id: String }\n"
            }
        )[0]
        empty = {"entries": [], "coverage": []}
        errors = AUDIT.check_baseline([finding], empty)
        self.assertTrue(
            any(error.startswith("UNCLASSIFIED shared-concrete-vocabulary") for error in errors),
            errors,
        )

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


if __name__ == "__main__":
    unittest.main()
