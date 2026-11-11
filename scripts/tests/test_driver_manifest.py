import base64
import contextlib
import hashlib
import importlib.util
import io
import json
import os
import re
import shutil
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).parents[2]
spec = importlib.util.spec_from_file_location("driver_manifest", ROOT / "scripts" / "driver_manifest.py")
dm = importlib.util.module_from_spec(spec)
spec.loader.exec_module(dm)

REDIS_FEATURES = ["navigator", "query.arrow_c_data", "query.redis_key", "query.redis_command", "mutation.set_ttl"]


def inspect_json(driver="redis", features=None, abi=2, schemes=None):
    """Shape printed by `inspect-driver <dylib>` (capabilities use the wire's snake_case)."""
    features = REDIS_FEATURES if features is None else features
    schemes = ["redis", "rediss"] if schemes is None else schemes
    return {
        "ok": True,
        "abi_version": abi,
        "driver": driver,
        "capabilities": {
            "abi_version": abi,
            "driver": driver,
            "display_name": driver.title(),
            "uri_schemes": schemes,
            "features": features,
            "editors": [],
            "navigator_kinds": [],
            "single_connection_pool": True,
        },
    }


class BuildEntryTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.zip = Path(self.tmp.name) / "libdbengine_redis-arm64.dylib.zip"
        self.zip.write_bytes(b"zip-bytes")

    def entry(self, inspect=None, **overrides):
        kwargs = dict(
            version="0.1.0-dev.abcd1234",
            zip_path=self.zip,
            url="file:///x/libdbengine_redis-arm64.dylib.zip",
            arch="arm64",
            team_id="LOCAL_UNSIGNED",
            license="MIT",
        )
        kwargs.update(overrides)
        return dm.build_entry(inspect or inspect_json(), **kwargs)

    def test_fields_come_from_capabilities(self):
        entry = self.entry(inspect_json(features=["navigator", "query.sql", "mutation.batch"], driver="mysql", schemes=["mysql"]))
        self.assertEqual(entry["id"], "mysql")
        self.assertEqual(entry["displayName"], "Mysql")
        self.assertEqual(entry["appAbiVersion"], 2)
        self.assertEqual(entry["uriSchemes"], ["mysql"])
        self.assertEqual(entry["features"], ["navigator", "query.sql", "mutation.batch"])

    def test_feature_list_is_not_filtered_or_reordered(self):
        features = ["z.last", "navigator", "a.first"]
        self.assertEqual(self.entry(inspect_json(features=features))["features"], features)

    def test_artifact_checksum_and_size_are_of_the_zip(self):
        artifact = self.entry()["artifacts"][0]
        self.assertEqual(artifact["sha256"], hashlib.sha256(b"zip-bytes").hexdigest())
        self.assertEqual(artifact["sizeBytes"], len(b"zip-bytes"))
        self.assertEqual(artifact["arch"], "arm64")

    def test_schema_matches_host_manifest_entry(self):
        source = (ROOT / "rust-engine/crates/engine-driver-host/src/manifest.rs").read_text()
        body = re.search(r"pub struct DriverManifestEntry \{(.*?)\n\}", source, re.S).group(1)
        fields = re.findall(r"pub (\w+):", body)
        camel = {re.sub(r"_(\w)", lambda m: m.group(1).upper(), name) for name in fields}
        self.assertEqual(set(self.entry()), camel)
        artifact_body = re.search(r"pub struct DriverArtifact \{(.*?)\n\}", source, re.S).group(1)
        artifact_fields = {re.sub(r"_(\w)", lambda m: m.group(1).upper(), n) for n in re.findall(r"pub (\w+):", artifact_body)}
        self.assertEqual(set(self.entry()["artifacts"][0]), artifact_fields)

    def test_rejects_wrong_abi(self):
        with self.assertRaisesRegex(dm.ManifestError, "ABI"):
            self.entry(inspect_json(abi=1))

    def test_rejects_missing_required_feature(self):
        with self.assertRaisesRegex(dm.ManifestError, "query.redis_scan"):
            self.entry(require=["navigator", "query.redis_scan"])

    def test_required_features_present_pass(self):
        self.entry(require=["navigator", "query.redis_key"])

    def test_rejects_duplicate_features_and_empty_schemes(self):
        with self.assertRaisesRegex(dm.ManifestError, "duplicates"):
            self.entry(inspect_json(features=["navigator", "navigator"]))
        with self.assertRaisesRegex(dm.ManifestError, "uri_schemes"):
            self.entry(inspect_json(schemes=[]))

    def test_rejects_missing_capabilities(self):
        with self.assertRaisesRegex(dm.ManifestError, "capabilities"):
            self.entry({"ok": True})

    def test_cli_entry_roundtrip(self):
        inspect_path = Path(self.tmp.name) / "inspect.json"
        inspect_path.write_text(json.dumps(inspect_json()))
        out = Path(self.tmp.name) / "redis-manifest-entry.json"
        code = dm.main([
            "entry", "--inspect", str(inspect_path), "--version", "1.2.3", "--zip", str(self.zip),
            "--url", "file:///z.zip", "--arch", "arm64", "--team-id", "T", "--license", "MIT",
            "--require", '["navigator"]', "--out", str(out),
        ])
        self.assertEqual(code, 0)
        written = json.loads(out.read_text())
        self.assertEqual(written["features"], REDIS_FEATURES)
        self.assertEqual(written["version"], "1.2.3")

    def test_cli_entry_fails_on_stale_required_feature(self):
        inspect_path = Path(self.tmp.name) / "inspect.json"
        inspect_path.write_text(json.dumps(inspect_json()))
        out = Path(self.tmp.name) / "entry.json"
        code = dm.main([
            "entry", "--inspect", str(inspect_path), "--version", "1", "--zip", str(self.zip),
            "--url", "file:///z.zip", "--arch", "arm64", "--team-id", "T", "--license", "MIT",
            "--require", '["query.page_lease"]', "--out", str(out),
        ])
        self.assertEqual(code, 1)
        self.assertFalse(out.exists())


class MergeTests(unittest.TestCase):
    def test_sorted_by_id_replaces_same_id_and_keeps_tools(self):
        existing = {"drivers": [{"id": "redis", "version": "old"}], "tools": [{"id": "t"}]}
        entries = [{"id": "sqlite", "version": "1"}, {"id": "redis", "version": "new"}, {"id": "mongodb", "version": "1"}]
        manifest = dm.merge_entries(entries, existing, now="2026-10-08T00:00:00Z")
        self.assertEqual([d["id"] for d in manifest["drivers"]], ["mongodb", "redis", "sqlite"])
        self.assertEqual(manifest["drivers"][1]["version"], "new")
        self.assertEqual(manifest["tools"], [{"id": "t"}])
        self.assertEqual(manifest["generatedAt"], "2026-10-08T00:00:00Z")

    def test_defaults_tools_and_timestamp(self):
        manifest = dm.merge_entries([{"id": "a"}])
        self.assertEqual(manifest["tools"], [])
        self.assertTrue(manifest["generatedAt"].endswith("Z"))

    def test_cli_merge_writes_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            first = Path(tmp) / "b-manifest-entry.json"
            second = Path(tmp) / "a-manifest-entry.json"
            first.write_text(json.dumps({"id": "b"}))
            second.write_text(json.dumps({"id": "a"}))
            out = Path(tmp) / "drivers-manifest.json"
            self.assertEqual(dm.main(["merge", "--out", str(out), str(first), str(second)]), 0)
            self.assertEqual([d["id"] for d in json.loads(out.read_text())["drivers"]], ["a", "b"])

    def test_cli_merge_without_entries_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(dm.main(["merge", "--out", str(Path(tmp) / "m.json")]), 1)


class PackagingScriptTests(unittest.TestCase):
    """Guards the shell glue against drifting back to hand-written feature lists / ABI 1 builds."""

    scripts = ROOT / "rust-engine" / "scripts"

    def test_every_driver_is_built_with_abi_v2_except_v2_only_sqlite(self):
        registry = (self.scripts / "driver-registry.sh").read_text()
        for crate in ("mongo", "postgres", "mysql", "mariadb", "redis"):
            block = re.search(rf"dbengine-{crate}\)\n(.*?);;", registry, re.S).group(1)
            self.assertIn("--features abi-v2", block, crate)
        for v2_only in ("sqlite", "clickhouse"):
            block = re.search(rf"dbengine-{v2_only}\)\n(.*?);;", registry, re.S).group(1)
            self.assertNotIn("abi-v2", block, v2_only)

    def test_clickhouse_is_registered_and_packaged(self):
        registry = (self.scripts / "driver-registry.sh").read_text()
        self.assertRegex(registry, r'ALL_DRIVER_IDS="[^"]*\bclickhouse\b[^"]*"')
        package = (self.scripts / "package-driver.sh").read_text()
        block = re.search(r"clickhouse\)\n(.*?);;", package, re.S).group(1)
        self.assertIn("query.arrow_c_data", block)
        self.assertIn("operation.cancel", block)
        for script in ("dev-drivers.sh", "build-driver.sh"):
            self.assertIn("clickhouse", (self.scripts / script).read_text(), script)

    def test_build_driver_never_lets_cargo_strip_and_uses_strip_x(self):
        build = (self.scripts / "build-driver.sh").read_text()
        self.assertIn("CARGO_PROFILE_RELEASE_STRIP=none", build)
        self.assertIn('strip -x "${OUT}"', build)

    def test_package_driver_has_no_hardcoded_manifest_features(self):
        package = (self.scripts / "package-driver.sh").read_text()
        self.assertNotIn("FEATURES=", package.replace("REQUIRED_FEATURES=", ""))
        self.assertIn("verify-driver.sh", package)
        self.assertIn("driver_manifest.py", package)


HTTPS = "https://example.com/libdbengine_redis-arm64.dylib.zip"


class SchemaV2Tests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.zip = Path(self.tmp.name) / "d.zip"
        self.zip.write_bytes(b"zip")

    def entry(self, driver="redis", url="file:///d.zip", team_id="", **overrides):
        kwargs = dict(version="1.0.0", zip_path=self.zip, url=url, arch="arm64", team_id=team_id, license="MIT")
        kwargs.update(overrides)
        return dm.build_entry(inspect_json(driver=driver), **kwargs)

    def test_every_entry_carries_the_catalog_fields(self):
        entry = self.entry(published_at="2026-10-08T12:00:00Z")
        self.assertEqual(entry["category"], "key_value")
        self.assertTrue(entry["description"] and "\n" not in entry["description"])
        self.assertRegex(entry["minAppVersion"], r"^\d+(\.\d+)*$")
        self.assertEqual(entry["releaseNotesUrl"], f"{dm.DEFAULT_SOURCE_REPO}/releases/tag/redis-v1.0.0")
        self.assertEqual(entry["publishedAt"], "2026-10-08T12:00:00Z")
        self.assertEqual(entry["teamId"], "", "ad-hoc dev dylibs have no Team ID")

    def test_categories_per_driver(self):
        expected = {
            "postgresql": "sql", "mysql": "sql", "mariadb": "sql", "sqlite": "sql",
            "mongodb": "nosql", "redis": "key_value", "clickhouse": "analytics",
        }
        self.assertEqual({k: v[0] for k, v in dm.DRIVER_META.items()}, expected)
        for driver, category in expected.items():
            self.assertEqual(self.entry(driver)["category"], category)
        # An unknown driver needs an explicit category.
        with self.assertRaisesRegex(dm.ManifestError, "category"):
            self.entry("newdb")
        self.assertEqual(self.entry("newdb", category="sql")["category"], "sql")
        with self.assertRaisesRegex(dm.ManifestError, "category"):
            self.entry(category="graph")

    def test_field_validation(self):
        with self.assertRaisesRegex(dm.ManifestError, "dotted numeric"):
            self.entry(min_app_version="1.x")
        with self.assertRaisesRegex(dm.ManifestError, "RFC 3339"):
            self.entry(published_at="yesterday")
        with self.assertRaisesRegex(dm.ManifestError, "one line"):
            self.entry(description="a\nb")

    def test_artifact_urls_are_https_or_development_file_urls(self):
        self.assertEqual(self.entry(url=HTTPS, team_id="ABCDE12345")["artifacts"][0]["url"], HTTPS)
        with self.assertRaisesRegex(dm.ManifestError, "Team ID"):
            self.entry(url=HTTPS, team_id="")
        with self.assertRaisesRegex(dm.ManifestError, "https"):
            self.entry(url="http://example.com/x.zip", team_id="ABCDE12345")

    def test_merge_writes_schema_v2_and_a_strictly_increasing_timestamp(self):
        first = dm.merge_entries([{"id": "a"}], now="2026-10-08T12:00:00Z")
        self.assertEqual(first["schemaVersion"], 2)
        self.assertEqual(first["generatedAt"], "2026-10-08T12:00:00Z")
        # The clock did not advance (or went back): still strictly greater.
        for now in ("2026-10-08T12:00:00Z", "2026-10-08T11:00:00Z"):
            second = dm.merge_entries([{"id": "b"}], first, now=now)
            self.assertEqual(second["generatedAt"], "2026-10-08T12:00:01Z")
        later = dm.merge_entries([{"id": "b"}], first, now="2027-01-01T00:00:00Z")
        self.assertEqual(later["generatedAt"], "2027-01-01T00:00:00Z")
        self.assertRegex(dm.merge_entries([{"id": "a"}])["generatedAt"], r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$")

    def test_cli_merge_never_goes_backwards_on_rewrite(self):
        entry = Path(self.tmp.name) / "a-manifest-entry.json"
        entry.write_text(json.dumps(self.entry()))
        out = Path(self.tmp.name) / "drivers-manifest.json"
        out.write_text(json.dumps({"generatedAt": "2999-01-01T00:00:00Z"}))
        self.assertEqual(dm.main(["merge", "--out", str(out), str(entry)]), 0)
        written = json.loads(out.read_text())
        self.assertEqual(written["generatedAt"], "2999-01-01T00:00:01Z")
        self.assertEqual(written["schemaVersion"], 2)

    def test_release_merge_refuses_dev_entries(self):
        entry = Path(self.tmp.name) / "a-manifest-entry.json"
        entry.write_text(json.dumps(self.entry()))
        out = Path(self.tmp.name) / "m.json"
        self.assertEqual(dm.main(["merge", "--release", "--out", str(out), str(entry)]), 1)
        self.assertFalse(out.exists())
        entry.write_text(json.dumps(self.entry(url=HTTPS, team_id="ABCDE12345")))
        self.assertEqual(dm.main(["merge", "--release", "--out", str(out), str(entry)]), 0)


class TargetTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.lib = Path(self.tmp.name) / "libdbengine_redis.so"
        self.lib.write_bytes(b"elf")

    def entry(self, **overrides):
        kwargs = dict(version="1.0.0", zip_path=self.lib, url=HTTPS, license="MIT")
        kwargs.update(overrides)
        return dm.build_entry(inspect_json(), **kwargs)

    def artifact(self, **overrides):
        return self.entry(**overrides)["artifacts"][0]

    def test_target_is_emitted_and_arch_stays_for_old_readers(self):
        artifact = self.artifact(target="macos-aarch64", team_id="ABCDE12345")
        self.assertEqual((artifact["target"], artifact["arch"]), ("macos-aarch64", "arm64"))
        artifact = self.artifact(target="macos-x86_64", team_id="ABCDE12345")
        self.assertEqual((artifact["target"], artifact["arch"]), ("macos-x86_64", "x86_64"))
        artifact = self.artifact(target="macos-universal", team_id="ABCDE12345")
        self.assertEqual((artifact["target"], artifact["arch"]), ("macos-universal", "universal"))

    def test_non_macos_arch_never_matches_an_old_macos_reader(self):
        for target in ("linux-x86_64", "linux-aarch64", "windows-x86_64", "windows-aarch64"):
            artifact = self.artifact(target=target)
            self.assertEqual((artifact["target"], artifact["arch"]), (target, target))

    def test_legacy_arch_maps_to_a_macos_target(self):
        self.assertEqual(self.artifact(arch="arm64", team_id="T")["target"], "macos-aarch64")
        self.assertEqual(self.artifact(arch="universal", team_id="T")["target"], "macos-universal")

    def test_target_defaults_to_the_build_host(self):
        self.assertEqual(self.artifact(url="file:///x.so")["target"], dm.host_target())
        self.assertRegex(dm.host_target(), r"^(macos|linux|windows)-(aarch64|x86_64)$")

    def test_unknown_target_or_arch_is_rejected(self):
        with self.assertRaisesRegex(dm.ManifestError, "unknown target"):
            self.entry(target="linux-universal")
        with self.assertRaisesRegex(dm.ManifestError, "unknown target"):
            self.entry(target="freebsd-x86_64")
        with self.assertRaisesRegex(dm.ManifestError, "unknown --arch"):
            self.entry(arch="riscv")

    def test_an_ad_hoc_release_takes_a_macos_build_without_a_team_id(self):
        # An explicit target: without one the entry is for the host, which needs no Team ID off macOS.
        mac = dict(url=HTTPS, team_id="", target="macos-aarch64")
        with self.assertRaisesRegex(dm.ManifestError, "Team ID"):
            self.entry(**mac)
        with mock.patch.dict(os.environ, {"TABLEX_AD_HOC_SIGNING": "1"}):
            self.assertEqual(self.entry(**mac)["teamId"], "")
        with mock.patch.dict(os.environ, {"TABLEX_AD_HOC_SIGNING": "0"}):
            with self.assertRaisesRegex(dm.ManifestError, "Team ID"):
                self.entry(**mac)

    def test_team_id_is_only_required_for_macos_builds(self):
        self.entry(target="linux-x86_64", team_id="")
        self.entry(target="windows-aarch64", team_id="")
        with self.assertRaisesRegex(dm.ManifestError, "Team ID"):
            self.entry(target="macos-aarch64", team_id="")

    def test_cli_target_flag_and_legacy_arch_flag(self):
        inspect_path = Path(self.tmp.name) / "inspect.json"
        inspect_path.write_text(json.dumps(inspect_json()))
        base = ["entry", "--inspect", str(inspect_path), "--version", "1", "--zip", str(self.lib),
                "--url", HTTPS, "--license", "MIT"]
        out = Path(self.tmp.name) / "entry.json"
        self.assertEqual(dm.main(base + ["--target", "linux-x86_64", "--out", str(out)]), 0)
        self.assertEqual(json.loads(out.read_text())["artifacts"][0]["target"], "linux-x86_64")
        self.assertEqual(dm.main(base + ["--arch", "arm64", "--team-id", "T", "--out", str(out)]), 0)
        self.assertEqual(json.loads(out.read_text())["artifacts"][0]["target"], "macos-aarch64")

    def test_merge_folds_per_target_entries_of_one_driver(self):
        mac = self.entry(target="macos-aarch64", team_id="ABCDE12345")
        linux = self.entry(target="linux-x86_64")
        windows = self.entry(target="windows-x86_64")
        manifest = dm.merge_entries([linux, mac, windows], now="2026-10-08T12:00:00Z")
        [driver] = manifest["drivers"]
        self.assertEqual([a["target"] for a in driver["artifacts"]], ["linux-x86_64", "macos-aarch64", "windows-x86_64"])
        self.assertEqual(driver["teamId"], "ABCDE12345", "the macOS Team ID survives a team-less first entry")
        dm.validate_release(manifest)

    def test_merge_rejects_duplicate_targets_and_version_skew(self):
        with self.assertRaisesRegex(dm.ManifestError, "two artifacts for target"):
            dm.merge_entries([self.entry(target="linux-x86_64"), self.entry(target="linux-x86_64")])
        with self.assertRaisesRegex(dm.ManifestError, "disagree on version"):
            dm.merge_entries([self.entry(target="linux-x86_64"), self.entry(target="windows-x86_64", version="2.0.0")])

    def test_release_needs_a_team_id_only_with_a_macos_artifact(self):
        linux_only = dm.merge_entries([self.entry(target="linux-x86_64")], now="2026-10-08T12:00:00Z")
        dm.validate_release(linux_only)
        old_mac = dm.merge_entries([self.entry(target="macos-aarch64", team_id="T")], now="2026-10-08T12:00:00Z")
        old_mac["drivers"][0]["teamId"] = ""
        with self.assertRaisesRegex(dm.ManifestError, "Team ID"):
            dm.validate_release(old_mac)
        legacy = {"id": "x", "artifacts": [{"arch": "arm64", "url": HTTPS}], "teamId": ""}
        self.assertEqual(dm.artifact_target(legacy["artifacts"][0]), "macos-aarch64")


class ToolEntryTests(unittest.TestCase):
    """The `tools` entry (tablex-mcp): built from a binary, folded per target, kept by updates."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.binary = self.dir / "tablex-mcp"
        self.binary.write_bytes(b"mcp-server-bytes")

    def tool(self, **overrides):
        kwargs = dict(version="0.1.0", url="https://example.com/tablex-mcp-macos-aarch64", target="macos-aarch64")
        kwargs.update(overrides)
        return dm.build_tool_entry(self.binary, **kwargs)

    def test_size_and_hash_come_from_the_built_binary(self):
        entry = self.tool()
        self.assertEqual((entry["id"], entry["kind"], entry["version"]), ("tablex-mcp", "mcpServer", "0.1.0"))
        artifact = entry["artifacts"][0]
        self.assertEqual(artifact["sha256"], hashlib.sha256(b"mcp-server-bytes").hexdigest())
        self.assertEqual(artifact["sizeBytes"], len(b"mcp-server-bytes"))
        self.assertEqual((artifact["target"], artifact["arch"]), ("macos-aarch64", "arm64"))
        self.assertEqual(entry["minAppVersion"], dm.DEFAULT_MIN_APP_VERSION)
        self.assertTrue(entry["displayName"] and "\n" not in entry["description"])

    def test_non_mac_targets_keep_the_target_in_arch_so_an_old_mac_app_never_matches(self):
        for target in ("linux-x86_64", "windows-x86_64"):
            self.assertEqual(self.tool(target=target)["artifacts"][0]["arch"], target)

    def test_fields_match_the_host_tool_entry(self):
        source = (ROOT / "rust-engine/crates/engine-driver-host/src/manifest.rs").read_text()
        body = re.search(r"pub struct ToolManifestEntry \{(.*?)\n\}", source, re.S).group(1)
        camel = {re.sub(r"_(\w)", lambda m: m.group(1).upper(), name) for name in re.findall(r"pub (\w+):", body)}
        # A free-standing tool has no `requires`; an add-on has every field the host reads.
        self.assertEqual(set(self.tool()), camel - {"requires"})
        self.assertEqual(set(self.shell_tool()), camel)
        for tool_id, (kind, _, _, _) in dm.TOOL_META.items():
            self.assertIn(f'"{kind}"', source)
            self.assertRegex(source, rf'"{tool_id}"')
        self.assertIn('McpServer', source)
        self.assertIn('MongoShell', source)
        self.assertEqual(dm.TOOL_META["tablex-mcp"][0], "mcpServer")
        self.assertEqual(dm.TOOL_META["tablex-mongo-shell"][0], "mongoShell")
        self.assertIn('MCP_TOOL_ID: &str = "tablex-mcp"', source)
        self.assertIn('MONGO_SHELL_TOOL_ID: &str = "tablex-mongo-shell"', source)

    def shell_tool(self, **overrides):
        kwargs = dict(
            version="0.1.0", url="https://example.com/tablex-mongo-shell-macos-aarch64", target="macos-aarch64",
            tool_id="tablex-mongo-shell", requires=[("mongodb", "0.2.0")],
        )
        kwargs.update(overrides)
        return dm.build_tool_entry(self.binary, **kwargs)

    def test_the_mongo_shell_plugin_is_an_add_on_of_the_mongodb_driver(self):
        entry = self.shell_tool()
        self.assertEqual((entry["id"], entry["kind"]), ("tablex-mongo-shell", "mongoShell"))
        self.assertEqual(entry["requires"], [{"driver": "mongodb", "minVersion": "0.2.0"}])
        # An add-on that names no driver is refused: the host would install it on any app.
        with self.assertRaisesRegex(dm.ManifestError, "add-on of the mongodb driver"):
            self.shell_tool(requires=[])
        with self.assertRaisesRegex(dm.ManifestError, "dotted numeric"):
            self.shell_tool(requires=[("mongodb", "soon")])
        with self.assertRaisesRegex(dm.ManifestError, "driver id"):
            self.shell_tool(requires=[("mongodb", "0.2.0"), ("../x", "1.0")])
        # The MCP server stays free-standing: no `requires` is written for it.
        self.assertNotIn("requires", self.tool())

    def test_the_sql_formatter_is_free_standing_and_names_what_it_is_built_on(self):
        entry = self.tool(tool_id="tablex-sqruff", url="https://example.com/tablex-sqruff-macos-aarch64")
        self.assertEqual((entry["id"], entry["kind"]), ("tablex-sqruff", "sqlFormatter"))
        self.assertNotIn("requires", entry)
        self.assertEqual((entry["license"], entry["sourceRepo"]), ("Apache-2.0", "https://github.com/quarylabs/sqruff"))
        # The wire names the host reads (ToolKind::SqlFormatter, SQRUFF_TOOL_ID).
        source = (ROOT / "rust-engine/crates/engine-driver-host/src/manifest.rs").read_text()
        self.assertIn('Self::SqlFormatter => "sqlFormatter"', source)
        self.assertIn('SQRUFF_TOOL_ID: &str = "tablex-sqruff"', source)
        self.assertEqual(dm.TOOL_META["tablex-sqruff"][0], "sqlFormatter")
        # It passes the release checks like the other tools, and needs no driver.
        driver = self.release_driver()
        dm.validate_release({"schemaVersion": 2, "generatedAt": "2026-10-14T12:00:00Z", "drivers": [driver], "tools": [entry]})
        # Every tool says its licence and source, which the Driver Manager shows.
        for tool_id in dm.TOOL_META:
            license, repo = dm.TOOL_SOURCES[tool_id]
            self.assertTrue(license and repo.startswith("https://"), tool_id)

    def test_release_validation_knows_the_plugin_and_its_requirement(self):
        driver = self.release_driver()
        def manifest(tool):
            return {"schemaVersion": 2, "generatedAt": "2026-10-14T12:00:00Z", "drivers": [driver], "tools": [tool]}
        dm.validate_release(manifest(self.shell_tool()))
        broken = self.shell_tool()
        del broken["requires"]
        with self.assertRaisesRegex(dm.ManifestError, "must require the mongodb driver"):
            dm.validate_release(manifest(broken))
        wrong_kind = self.shell_tool()
        wrong_kind["kind"] = "mcpServer"
        # (a tool of the other kind is not even a tool entry of its id)
        with self.assertRaisesRegex(dm.ManifestError, "kind must be"):
            dm.validate_release(manifest(wrong_kind))
        bad_minimum = self.shell_tool()
        bad_minimum["requires"][0]["minVersion"] = "x"
        with self.assertRaisesRegex(dm.ManifestError, "minVersion"):
            dm.validate_release(manifest(bad_minimum))

    def test_cli_writes_the_plugin_entry_and_merge_keeps_both_tools(self):
        plugin = self.dir / "plugin-entry.json"
        mcp = self.dir / "mcp-entry.json"
        code = dm.main([
            "tool-entry", "--binary", str(self.binary), "--version", "0.1.0", "--target", "macos-aarch64",
            "--url", "https://example.com/p", "--id", "tablex-mongo-shell", "--requires-driver", "mongodb:0.2.0",
            "--out", str(plugin),
        ])
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(plugin.read_text())["requires"][0]["minVersion"], "0.2.0")
        # Missing --requires-driver is an error code, not a half-valid entry.
        self.assertEqual(dm.main([
            "tool-entry", "--binary", str(self.binary), "--version", "0.1.0", "--url", "https://x",
            "--id", "tablex-mongo-shell", "--out", str(self.dir / "no.json"),
        ]), 1)
        self.assertEqual(dm.main(["tool-entry", "--binary", str(self.binary), "--version", "0.1.0", "--target", "macos-aarch64", "--url", "https://example.com/m", "--out", str(mcp)]), 0)
        driver_path = self.dir / "redis-manifest-entry.json"
        driver_path.write_text(json.dumps(self.release_driver()))
        out = self.dir / "drivers-manifest.json"
        self.assertEqual(dm.main(["merge", "--release", "--out", str(out), str(driver_path), str(mcp), str(plugin)]), 0)
        tools = json.loads(out.read_text())["tools"]
        self.assertEqual([tool["id"] for tool in tools], ["tablex-mcp", "tablex-mongo-shell"])
        self.assertEqual(tools[1]["kind"], "mongoShell")

    def test_rejects_what_the_host_would_reject_or_ignore(self):
        with self.assertRaisesRegex(dm.ManifestError, "unknown tool"):
            self.tool(tool_id="future-tool")
        with self.assertRaisesRegex(dm.ManifestError, "version"):
            self.tool(version="../1")
        with self.assertRaisesRegex(dm.ManifestError, "https"):
            self.tool(url="http://example.com/x")
        with self.assertRaisesRegex(dm.ManifestError, "unknown target"):
            self.tool(target="plan9-mips")
        with self.assertRaisesRegex(dm.ManifestError, "minAppVersion"):
            self.tool(min_app_version="soon")
        with self.assertRaisesRegex(dm.ManifestError, "not a file"):
            dm.build_tool_entry(self.dir / "missing", version="1", url="https://x", target="macos-aarch64")
        empty = self.dir / "empty"
        empty.write_bytes(b"")
        with self.assertRaisesRegex(dm.ManifestError, "bytes"):
            dm.build_tool_entry(empty, version="1", url="https://x", target="macos-aarch64")

    def test_merge_folds_per_target_entries_and_keeps_the_drivers(self):
        driver = dm.build_entry(
            inspect_json(), version="1.0.0", zip_path=self.dir / "tablex-mcp", url=HTTPS, arch="arm64",
            team_id="ABCDE12345", license="MIT",
        )
        mac = self.tool()
        linux = self.tool(target="linux-x86_64", url="https://example.com/tablex-mcp-linux-x86_64")
        manifest = dm.merge_entries([driver, mac, linux], now="2026-10-14T12:00:00Z")
        self.assertEqual([d["id"] for d in manifest["drivers"]], ["redis"])
        self.assertEqual(len(manifest["tools"]), 1)
        self.assertEqual([a["target"] for a in manifest["tools"][0]["artifacts"]], ["macos-aarch64", "linux-x86_64"])
        dm.validate_release(manifest)
        with self.assertRaisesRegex(dm.ManifestError, "two artifacts"):
            dm.merge_entries([mac, mac])
        with self.assertRaisesRegex(dm.ManifestError, "disagree on version"):
            dm.merge_entries([mac, self.tool(version="0.2.0", target="linux-x86_64")])

    def test_a_manifest_without_tools_gets_an_empty_list_and_old_ones_are_unchanged(self):
        manifest = dm.merge_entries([], {"drivers": []}, now="2026-10-14T12:00:00Z")
        self.assertEqual(manifest["tools"], [])

    def test_update_replaces_a_tool_of_the_same_id_and_keeps_everything_else(self):
        future = {"id": "future-tool", "kind": "cliTool", "version": "9", "artifacts": []}
        existing = {
            "schemaVersion": 2, "generatedAt": "2026-10-14T12:00:00Z",
            "drivers": [{"id": "sqlite", "version": "1.0.0"}],
            "tools": [self.tool(version="0.1.0"), future],
        }
        manifest = dm.merge_entries([self.tool(version="0.2.0")], existing, now="2026-10-14T12:00:00Z")
        self.assertEqual([t["id"] for t in manifest["tools"]], ["future-tool", "tablex-mcp"])
        self.assertEqual(manifest["tools"][0], future, "an entry of a kind this script does not know survives")
        self.assertEqual(manifest["tools"][1]["version"], "0.2.0")
        self.assertEqual(manifest["drivers"], existing["drivers"])
        self.assertGreater(manifest["generatedAt"], existing["generatedAt"])

    def test_release_validation_requires_https_a_valid_hash_and_a_known_tool(self):
        def manifest_with(edit):
            tool = self.tool()
            edit(tool)
            return {"schemaVersion": 2, "generatedAt": "2026-10-14T12:00:00Z", "drivers": [self.release_driver()], "tools": [tool]}

        dm.validate_release(manifest_with(lambda t: None))
        for edit, pattern in [
            (lambda t: t["artifacts"][0].update(url="file:///x"), "https"),
            (lambda t: t["artifacts"][0].update(sha256="abc"), "sha256"),
            (lambda t: t["artifacts"][0].update(sizeBytes=0), "sizeBytes"),
            (lambda t: t.update(artifacts=[]), "no artifacts"),
            (lambda t: t.update(id="future-tool"), "not one this build knows"),
            (lambda t: t["artifacts"].append(dict(t["artifacts"][0])), "two artifacts"),
        ]:
            with self.assertRaisesRegex(dm.ManifestError, pattern):
                dm.validate_release(manifest_with(edit))

    def release_driver(self):
        return dm.build_entry(
            inspect_json(), version="1.0.0", zip_path=self.binary, url=HTTPS, arch="arm64",
            team_id="ABCDE12345", license="MIT",
        )

    def test_cli_tool_entry_and_merge_roundtrip(self):
        entry_path = self.dir / "tablex-mcp-macos-aarch64-tool-entry.json"
        code = dm.main([
            "tool-entry", "--binary", str(self.binary), "--version", "0.1.0", "--target", "macos-aarch64",
            "--url", "https://example.com/tablex-mcp-macos-aarch64", "--out", str(entry_path),
        ])
        self.assertEqual(code, 0)
        driver_path = self.dir / "redis-manifest-entry.json"
        driver_path.write_text(json.dumps(self.release_driver()))
        out = self.dir / "drivers-manifest.json"
        self.assertEqual(dm.main(["merge", "--release", "--out", str(out), str(driver_path), str(entry_path)]), 0)
        manifest = json.loads(out.read_text())
        self.assertEqual(manifest["tools"][0]["artifacts"][0]["sha256"], hashlib.sha256(b"mcp-server-bytes").hexdigest())
        # Bad input is an error code, not a traceback.
        self.assertEqual(dm.main(["tool-entry", "--binary", str(self.dir / "nope"), "--version", "1", "--url", "https://x", "--out", str(entry_path)]), 1)

    def test_update_script_adds_a_tool_from_a_binary_into_an_existing_manifest(self):
        update = importlib.util.spec_from_file_location("update_driver_manifest", ROOT / "scripts" / "update-driver-manifest.py")
        module = importlib.util.module_from_spec(update)
        update.loader.exec_module(module)
        manifest = self.dir / "m.json"
        manifest.write_text(json.dumps(dm.merge_entries([self.release_driver()], now="2026-10-14T12:00:00Z")))
        before = json.loads(manifest.read_text())
        args = [str(manifest), "--tool-binary", str(self.binary), "--version", "0.1.0", "--target", "macos-aarch64", "--url", "file:///dist/tablex-mcp"]
        self.assertEqual(module.main(args), 0)
        after = json.loads(manifest.read_text())
        self.assertEqual(after["drivers"], before["drivers"])
        self.assertEqual(after["tools"][0]["artifacts"][0]["sizeBytes"], len(b"mcp-server-bytes"))
        self.assertGreater(after["generatedAt"], before["generatedAt"])
        # Running it again replaces rather than duplicates.
        self.binary.write_bytes(b"newer")
        self.assertEqual(module.main(args[:-1] + ["file:///dist/tablex-mcp"]), 0)
        again = json.loads(manifest.read_text())
        self.assertEqual(len(again["tools"]), 1)
        self.assertEqual(again["tools"][0]["artifacts"][0]["sizeBytes"], 5)
        # A driver entry file still works.
        entry_file = self.dir / "driver.json"
        entry_file.write_text(json.dumps(self.release_driver()))
        self.assertEqual(module.main([str(manifest), str(entry_file)]), 0)
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as usage:
            module.main([str(manifest)])
        self.assertEqual(usage.exception.code, 2)
        self.assertEqual(len(json.loads(manifest.read_text())["tools"]), 1, "a driver update keeps the tool")


@unittest.skipUnless(shutil.which("cargo"), "needs cargo to build the sign-manifest example")
class SigningDryRunTests(unittest.TestCase):
    """Signs with a throwaway key generated into a temp dir (never a real key)."""

    cargo = [
        "cargo", "run", "-q", "--release", "--manifest-path", str(ROOT / "rust-engine" / "Cargo.toml"),
        "-p", "engine-driver-host", "--example", "sign-manifest", "--",
    ]

    def setUp(self):
        import subprocess
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.key = Path(self.tmp.name) / "throwaway.key"
        keygen = subprocess.run(self.cargo + ["keygen", str(self.key)], capture_output=True, text=True)
        self.assertEqual(keygen.returncode, 0, keygen.stderr)
        self.manifest = Path(self.tmp.name) / "drivers-manifest.json"
        zip_path = Path(self.tmp.name) / "d.zip"
        zip_path.write_bytes(b"zip")
        entry = dm.build_entry(
            inspect_json(driver="clickhouse", features=["navigator", "query.sql"], schemes=["clickhouse"]),
            version="1.0.0", zip_path=zip_path, url=HTTPS, arch="arm64", team_id="ABCDE12345", license="Apache-2.0",
        )
        manifest = dm.merge_entries([entry], now="2026-10-08T12:00:00Z")
        self.manifest.write_text(json.dumps(manifest, indent=2) + "\n")

    def test_sign_writes_a_detached_ed25519_signature_over_the_final_bytes(self):
        self.assertEqual(dm.main(["sign", "--manifest", str(self.manifest), "--key", str(self.key)]), 0)
        sig = Path(str(self.manifest) + ".sig")
        first = base64.b64decode(sig.read_text().strip())
        self.assertEqual(len(first), 64)
        # Deterministic over identical bytes ...
        dm.main(["sign", "--manifest", str(self.manifest), "--key", str(self.key)])
        self.assertEqual(base64.b64decode(sig.read_text().strip()), first)
        # ... and any later rewrite needs a new signature.
        self.manifest.write_text(self.manifest.read_text().replace("2026-10-08T12:00:00Z", "2026-10-08T12:00:01Z"))
        dm.main(["sign", "--manifest", str(self.manifest), "--key", str(self.key)])
        self.assertNotEqual(base64.b64decode(sig.read_text().strip()), first)

    def test_the_key_can_come_from_a_ci_secret_variable(self):
        import os
        os.environ["TEST_MANIFEST_KEY"] = self.key.read_text()
        self.addCleanup(os.environ.pop, "TEST_MANIFEST_KEY", None)
        self.assertEqual(dm.main(["sign", "--manifest", str(self.manifest), "--key-env", "TEST_MANIFEST_KEY"]), 0)
        self.assertTrue(Path(str(self.manifest) + ".sig").exists())
        self.assertEqual(dm.main(["sign", "--manifest", str(self.manifest), "--key-env", "UNSET_KEY_VAR_XYZ"]), 1)

    def test_a_ci_secret_reaches_the_signer_on_stdin_only_and_the_build_is_locked(self):
        import os
        from unittest import mock
        secret = self.key.read_text()
        os.environ["TEST_MANIFEST_KEY"] = secret
        self.addCleanup(os.environ.pop, "TEST_MANIFEST_KEY", None)
        seen = {}

        def fake_run(command, **kwargs):
            seen.update(command=command, kwargs=kwargs)
            Path(str(self.manifest) + ".sig").write_text("sig\n")
            return mock.Mock(returncode=0, stdout="", stderr="")

        with mock.patch.object(dm.subprocess, "run", fake_run):
            self.assertEqual(dm.main(["sign", "--manifest", str(self.manifest), "--key-env", "TEST_MANIFEST_KEY"]), 0)
        command = seen["command"]
        self.assertIn("--locked", command)
        self.assertEqual(command[command.index("sign") + 1], "-", "the key is read from stdin")
        self.assertEqual(seen["kwargs"]["input"], secret.strip() + "\n")
        self.assertFalse(any(secret.strip() in part for part in command), "no secret on a command line")
        self.assertFalse(hasattr(dm, "tempfile"), "no temp key file is made any more")

    def test_a_dev_manifest_is_never_signed(self):
        manifest = json.loads(self.manifest.read_text())
        manifest["drivers"][0]["artifacts"][0]["url"] = "file:///d.zip"
        self.manifest.write_text(json.dumps(manifest))
        self.assertEqual(dm.main(["sign", "--manifest", str(self.manifest), "--key", str(self.key)]), 1)
        self.assertFalse(Path(str(self.manifest) + ".sig").exists())


class AppManifestTests(unittest.TestCase):
    """`app-manifest`: the update manifest of an app release, signed like the driver catalog."""

    BASE = "https://github.com/tablex-org/tablex/releases/download/v0.2.0"

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.arm = self.dir / "TableX-0.2.0-macos-apple-silicon.app.zip"
        self.intel = self.dir / "TableX-0.2.0-macos-intel.app.zip"
        self.arm.write_bytes(b"arm zip")
        self.intel.write_bytes(b"intel zip bytes")
        self.zips = [("x86_64-apple-darwin", self.intel), ("aarch64-apple-darwin", self.arm)]

    def test_the_manifest_lists_one_zip_per_target_by_size_and_sha256(self):
        manifest = dm.build_app_manifest("0.2.0", self.BASE + "/", self.zips, now="2026-11-11T00:00:00Z")
        self.assertEqual(manifest, {
            "kind": "tablex-app-update",
            "schema": 1,
            "version": "0.2.0",
            "generatedAt": "2026-11-11T00:00:00Z",
            "artifacts": [
                {"target": "aarch64-apple-darwin", "url": f"{self.BASE}/{self.arm.name}", "size": 7,
                 "sha256": hashlib.sha256(b"arm zip").hexdigest()},
                {"target": "x86_64-apple-darwin", "url": f"{self.BASE}/{self.intel.name}", "size": 15,
                 "sha256": hashlib.sha256(b"intel zip bytes").hexdigest()},
            ],
        })

    def test_an_incomplete_or_odd_release_is_refused(self):
        build = dm.build_app_manifest
        with self.assertRaises(dm.ManifestError):
            build("0.2.0", self.BASE, self.zips[:1])
        with self.assertRaises(dm.ManifestError):
            build("0.2.0", self.BASE, self.zips + self.zips[:1])
        with self.assertRaises(dm.ManifestError):
            build("0.2.0", self.BASE, [("aarch64-apple-darwin", self.arm), ("x86_64-unknown-linux-gnu", self.intel)])
        with self.assertRaises(dm.ManifestError):
            build("v0.2", self.BASE, self.zips)
        with self.assertRaises(dm.ManifestError):
            build("0.2.0", "http://example.com/v0.2.0", self.zips)
        dmg = self.dir / "TableX.dmg"
        dmg.write_bytes(b"x")
        with self.assertRaises(dm.ManifestError):
            build("0.2.0", self.BASE, [("aarch64-apple-darwin", dmg), ("x86_64-apple-darwin", self.intel)])
        self.arm.write_bytes(b"")
        with self.assertRaises(dm.ManifestError):
            build("0.2.0", self.BASE, self.zips)

    def args(self, *extra):
        return [
            "app-manifest", "--version", "0.2.0", "--base-url", self.BASE, "--out", str(self.dir / "app-update.json"),
            "--zip", f"aarch64-apple-darwin={self.arm}", "--zip", f"x86_64-apple-darwin={self.intel}", *extra,
        ]

    def test_the_secret_reaches_the_signer_on_stdin_only_and_the_build_is_locked(self):
        secret = "c2VjcmV0LXNlZWQtZm9yLXRlc3RzLW9ubHktMzItYnl0ZXM="
        seen = {}

        def fake_run(command, **kwargs):
            seen.update(command=command, kwargs=kwargs)
            Path(command[-1] + ".sig").write_text("sig\n")
            return mock.Mock(returncode=0, stdout="", stderr="")

        with mock.patch.dict(os.environ, {"TEST_APP_KEY": secret}), mock.patch.object(dm.subprocess, "run", fake_run), \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(dm.main(self.args("--key-env", "TEST_APP_KEY")), 0)
        command = seen["command"]
        self.assertIn("--locked", command)
        self.assertEqual(command[command.index("sign") + 1], "-", "the key is read from stdin")
        self.assertEqual(command[-1], str(self.dir / "app-update.json"), "the exact file that is published is signed")
        self.assertEqual(seen["kwargs"]["input"], secret + "\n")
        self.assertFalse(any(secret in part for part in command), "no secret on a command line")
        written = json.loads((self.dir / "app-update.json").read_text())
        self.assertEqual(written["version"], "0.2.0")
        self.assertEqual([a["target"] for a in written["artifacts"]], ["aarch64-apple-darwin", "x86_64-apple-darwin"])

    def test_the_signer_never_has_the_key_in_its_environment_but_on_stdin(self):
        secret = "c2VjcmV0LXNlZWQtZm9yLXRlc3RzLW9ubHktMzItYnl0ZXM="
        calls = []

        def fake_run(command, **kwargs):
            calls.append((command, kwargs))
            Path(command[-1] + ".sig").write_text("sig\n")
            return mock.Mock(returncode=0, stdout="", stderr="")

        signer = self.dir / "sign-manifest"
        signer.write_bytes(b"#!/bin/sh\n")
        environment = {"TEST_APP_KEY": secret, "OTHER_SECRET": "x", "PATH": "/usr/bin"}
        with mock.patch.dict(os.environ, environment, clear=True), mock.patch.object(dm.subprocess, "run", fake_run), \
                contextlib.redirect_stdout(io.StringIO()):
            # Built and run by cargo: the key variable is removed from what cargo (and so every build
            # script) inherits.
            self.assertEqual(dm.main(self.args("--key-env", "TEST_APP_KEY")), 0)
            # A prebuilt signer is run directly, with nothing but PATH.
            self.assertEqual(dm.main(self.args("--key-env", "TEST_APP_KEY", "--signer", str(signer))), 0)
            # The driver catalog's `sign` takes the same path.
            manifest = self.dir / "drivers-manifest.json"
            entry = dm.build_entry(
                inspect_json(driver="clickhouse", features=["navigator", "query.sql"], schemes=["clickhouse"]),
                version="1.0.0", zip_path=self.arm, url=HTTPS, arch="arm64", team_id="ABCDE12345", license="Apache-2.0",
            )
            manifest.write_text(json.dumps(dm.merge_entries([entry], now="2026-10-08T12:00:00Z")))
            self.assertEqual(dm.main(["sign", "--manifest", str(manifest), "--key-env", "TEST_APP_KEY", "--signer", str(signer)]), 0)
        (cargo_command, cargo), (direct, prebuilt), (catalog, catalog_kwargs) = calls
        self.assertEqual(cargo_command[0], "cargo")
        self.assertNotIn("TEST_APP_KEY", cargo["env"])
        self.assertEqual(cargo["input"], secret + "\n")
        self.assertEqual(direct[0], str(signer))
        self.assertEqual(direct[1:3], ["sign", "-"])
        self.assertEqual(prebuilt["env"], {"PATH": "/usr/bin"})
        self.assertEqual(prebuilt["input"], secret + "\n")
        self.assertEqual(catalog[0], str(signer))
        self.assertEqual(catalog_kwargs["env"], {"PATH": "/usr/bin"})

    def test_a_missing_signer_is_an_error_not_a_fallback_to_cargo(self):
        with contextlib.redirect_stderr(io.StringIO()), mock.patch.dict(os.environ, {"TEST_APP_KEY": "k"}):
            self.assertEqual(dm.main(self.args("--key-env", "TEST_APP_KEY", "--signer", str(self.dir / "nope"))), 1)

    def test_bad_input_is_an_error_code_not_a_traceback(self):
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(dm.main(self.args("--key-env", "UNSET_KEY_VAR_XYZ")), 1)
            self.assertEqual(dm.main(["app-manifest", "--version", "0.2.0", "--base-url", self.BASE, "--out", str(self.dir / "m.json"),
                                      "--zip", "nonsense", "--key-env", "X"]), 1)
        self.assertFalse((self.dir / "app-update.json.sig").exists())

    @unittest.skipUnless(shutil.which("cargo"), "needs cargo to build the sign-manifest example")
    def test_it_signs_with_a_throwaway_key_in_the_format_the_driver_catalog_uses(self):
        import subprocess
        key = self.dir / "throwaway.key"
        keygen = subprocess.run(SigningDryRunTests.cargo + ["keygen", str(key)], capture_output=True, text=True)
        self.assertEqual(keygen.returncode, 0, keygen.stderr)
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(dm.main(self.args("--key", str(key))), 0)
        manifest = self.dir / "app-update.json"
        sig = self.dir / "app-update.json.sig"
        self.assertEqual(len(base64.b64decode(sig.read_text().strip())), 64)
        # `sign-manifest sign` verifies the signature it wrote; a second signing of the same bytes is identical.
        before = sig.read_text()
        self.assertEqual(dm.sign_manifest(manifest, key).read_text(), before)
        # The prebuilt example (what CI runs, with the key in a variable) gives the same signature.
        built = ROOT / "rust-engine" / "target" / "release" / "examples" / "sign-manifest"
        if built.is_file():
            sig.unlink()
            with mock.patch.dict(os.environ, {"TEST_APP_KEY": key.read_text()}), contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(dm.main(self.args("--key-env", "TEST_APP_KEY", "--signer", str(built))), 0)
            # (the manifest was rewritten: signed over its new bytes, the same as the cargo-run signer does)
            self.assertEqual(dm.sign_manifest(manifest, key).read_text(), sig.read_text())


if __name__ == "__main__":
    unittest.main()
