import contextlib
import hashlib
import importlib.util
import io
import json
import shutil
import stat
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).parents[2]
spec = importlib.util.spec_from_file_location("build_npm_packages", ROOT / "scripts" / "build_npm_packages.py")
bnp = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bnp)

VERSION = "9.8.7"


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class Release:
    """A folder of release assets (`tablex-mcp-<target>`) with distinct bytes per target."""

    def __init__(self, root: Path, targets):
        self.dir = root / "assets"
        self.dir.mkdir()
        self.bytes = {}
        for target in targets:
            platform = next(p for p in bnp.load_platforms() if p["target"] == target)
            data = f"signed bytes of {target}\n".encode() + bytes(range(256)) * 4
            (self.dir / bnp.asset_name(platform)).write_bytes(data)
            self.bytes[target] = data

    def manifest(self, root: Path, version=VERSION, tamper=None, drop=None) -> Path:
        artifacts = [
            {"target": target, "sha256": sha(data), "sizeBytes": len(data), "url": "https://example.invalid/x"}
            for target, data in self.bytes.items()
            if target != drop
        ]
        if tamper:
            for artifact in artifacts:
                if artifact["target"] == tamper:
                    artifact["sha256"] = "0" * 64
        path = root / "drivers-manifest.json"
        path.write_text(json.dumps({"tools": [{"id": "tablex-mcp", "version": version, "artifacts": artifacts}]}))
        return path


class BuildNpmPackagesTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)

    def build(self, targets, **kwargs):
        release = Release(self.tmp, targets)
        out = self.tmp / "out"
        folders = bnp.build(release.dir, out, VERSION, **kwargs)
        return release, out, folders

    def test_every_binary_becomes_a_package_with_the_same_bytes_and_the_launcher_comes_last(self):
        targets = ["macos-aarch64", "linux-x86_64", "windows-x86_64"]
        release, out, folders = self.build(targets)
        self.assertEqual([f.name for f in folders], ["mcp-darwin-arm64", "mcp-linux-x64-gnu", "mcp-win32-x64", "mcp-server"])
        for target, name, binary in [
            ("macos-aarch64", "mcp-darwin-arm64", "tablex-mcp"),
            ("linux-x86_64", "mcp-linux-x64-gnu", "tablex-mcp"),
            ("windows-x86_64", "mcp-win32-x64", "tablex-mcp.exe"),
        ]:
            packed = out / name / "bin" / binary
            self.assertEqual(packed.read_bytes(), release.bytes[target], "published bytes are packed unchanged")
            self.assertTrue(packed.stat().st_mode & stat.S_IXUSR)
            self.assertTrue((out / name / "LICENSE").is_file())
            package = json.loads((out / name / "package.json").read_text())
            self.assertEqual(package["version"], VERSION)
            self.assertEqual(package["tablex"], {"target": target, "sha256": sha(release.bytes[target])})
            self.assertEqual(sorted(p.name for p in (out / name / "bin").iterdir()), [binary])

    def test_os_cpu_and_libc_fields_select_the_machine(self):
        _, out, _ = self.build(["macos-aarch64", "macos-x86_64", "linux-x86_64", "linux-aarch64", "windows-x86_64", "windows-aarch64"])
        fields = {
            name: json.loads((out / name / "package.json").read_text())
            for name in ["mcp-darwin-arm64", "mcp-darwin-x64", "mcp-linux-x64-gnu", "mcp-linux-arm64-gnu", "mcp-win32-x64", "mcp-win32-arm64"]
        }
        expect = {
            "mcp-darwin-arm64": (["darwin"], ["arm64"], None),
            "mcp-darwin-x64": (["darwin"], ["x64"], None),
            "mcp-linux-x64-gnu": (["linux"], ["x64"], ["glibc"]),
            "mcp-linux-arm64-gnu": (["linux"], ["arm64"], ["glibc"]),
            "mcp-win32-x64": (["win32"], ["x64"], None),
            "mcp-win32-arm64": (["win32"], ["arm64"], None),
        }
        for name, (os_, cpu, libc) in expect.items():
            self.assertEqual(fields[name]["os"], os_, name)
            self.assertEqual(fields[name]["cpu"], cpu, name)
            self.assertEqual(fields[name].get("libc"), libc, name)
            self.assertEqual(fields[name]["name"], "@tablex/" + name)

    def test_the_launcher_package_names_what_was_built_and_carries_the_checksums(self):
        release, out, _ = self.build(["macos-aarch64", "linux-x86_64"])
        launcher = out / "mcp-server"
        package = json.loads((launcher / "package.json").read_text())
        self.assertEqual(package["name"], "@tablex/mcp-server")
        self.assertEqual(package["version"], VERSION)
        self.assertEqual(
            package["optionalDependencies"],
            {"@tablex/mcp-darwin-arm64": VERSION, "@tablex/mcp-linux-x64-gnu": VERSION},
        )
        self.assertNotIn("dependencies", package)
        self.assertEqual(
            json.loads((launcher / "checksums.json").read_text()),
            {
                "@tablex/mcp-darwin-arm64": sha(release.bytes["macos-aarch64"]),
                "@tablex/mcp-linux-x64-gnu": sha(release.bytes["linux-x86_64"]),
            },
        )
        for name in ["bin/tablex-mcp.js", "lib/launcher.js", "platforms.json", "LICENSE", "server.json"]:
            self.assertTrue((launcher / name).is_file(), name)
        self.assertFalse((launcher / "tests").exists())
        registry = json.loads((launcher / "server.json").read_text())
        self.assertEqual(registry["version"], VERSION)
        self.assertEqual([p["version"] for p in registry["packages"]], [VERSION])
        self.assertEqual(registry["packages"][0]["identifier"], package["name"])
        self.assertEqual(registry["name"], package["mcpName"])

    def test_the_manifest_must_name_exactly_the_bytes_that_are_packed(self):
        targets = ["macos-aarch64", "linux-x86_64"]
        release = Release(self.tmp, targets)
        out = self.tmp / "out"
        bnp.build(release.dir, out, VERSION, manifest=release.manifest(self.tmp))
        for kwargs, message in [
            ({"tamper": "linux-x86_64"}, "not the manifest's"),
            ({"drop": "linux-x86_64"}, "no tablex-mcp artifact for linux-x86_64"),
            ({"version": "1.0.0"}, "has tablex-mcp 1.0.0"),
        ]:
            with self.assertRaises(bnp.BuildError, msg=str(kwargs)) as caught:
                bnp.build(release.dir, out, VERSION, manifest=release.manifest(self.tmp, **kwargs))
            self.assertIn(message, str(caught.exception))

    def test_a_required_target_without_a_binary_and_an_empty_folder_are_errors(self):
        release = Release(self.tmp, ["linux-x86_64"])
        with self.assertRaises(bnp.BuildError) as caught:
            bnp.build(release.dir, self.tmp / "out", VERSION, require=("macos-aarch64",))
        self.assertIn("macos-aarch64 is required", str(caught.exception))
        with self.assertRaises(bnp.BuildError):
            bnp.build(release.dir, self.tmp / "out", VERSION, require=("no-such-target",))
        empty = self.tmp / "empty"
        empty.mkdir()
        with self.assertRaises(bnp.BuildError):
            bnp.build(empty, self.tmp / "out", VERSION)

    def test_the_source_package_agrees_with_the_crate_and_its_own_platform_table(self):
        source = json.loads((bnp.SOURCE / "package.json").read_text())
        version = bnp.crate_version()
        self.assertEqual(source["version"], version, "bump packages/mcp-server with apps/tablex-mcp")
        registry = json.loads((bnp.SOURCE / "server.json").read_text())
        self.assertEqual(registry["version"], version)
        self.assertEqual([p["version"] for p in registry["packages"]], [version])
        self.assertEqual(registry["name"], source["mcpName"])
        names = sorted(p["package"] for p in bnp.load_platforms())
        self.assertEqual(sorted(source["optionalDependencies"]), names)
        self.assertEqual(set(source["optionalDependencies"].values()), {version})
        targets = [p["target"] for p in bnp.load_platforms()]
        self.assertEqual(len(set(targets)), len(targets))
        # The platform table is the same set of target names the release workflow builds.
        workflow = (ROOT / ".github" / "workflows" / "release-drivers.yml").read_text()
        for target in targets:
            if target != "windows-aarch64":  # being added to the workflow
                self.assertIn(f"target: {target}", workflow)

    def test_server_json_keeps_to_the_registry_schema_rules_that_reject_a_publish(self):
        # The 2025-12-11 server.schema.json: `name`, `description` and `version` are required, the
        # name is `namespace/name` in reverse DNS, the description is at most 100 characters, the
        # title at most 100, and a package needs registryType, identifier and a transport.
        registry = json.loads((bnp.SOURCE / "server.json").read_text())
        self.assertEqual(registry["$schema"], "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json")
        self.assertRegex(registry["name"], r"^[a-zA-Z0-9.-]+/[a-zA-Z0-9._-]+$")
        self.assertTrue(3 <= len(registry["name"]) <= 200)
        self.assertTrue(0 < len(registry["description"]) <= 100, len(registry["description"]))
        self.assertTrue(0 < len(registry["title"]) <= 100)
        self.assertEqual(registry["repository"]["source"], "github")
        for package in registry["packages"]:
            self.assertEqual(package["registryType"], "npm")
            self.assertEqual(package["registryBaseUrl"], "https://registry.npmjs.org")
            self.assertEqual(package["transport"], {"type": "stdio"})

    def test_main_reports_errors_on_stderr_with_exit_one(self):
        release = Release(self.tmp, ["linux-x86_64"])
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            code = bnp.main(["--binaries", str(release.dir), "--out", str(self.tmp / "o"), "--version", VERSION, "--require", "macos-aarch64"])
        self.assertEqual(code, 1)
        self.assertIn("build_npm_packages:", stderr.getvalue())
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(
                bnp.main(["--binaries", str(release.dir), "--out", str(self.tmp / "o2"), "--version", VERSION]), 0
            )

    @unittest.skipUnless(shutil.which("npm"), "npm is not installed")
    def test_npm_pack_makes_a_tarball_per_package_with_only_the_declared_files(self):
        release = Release(self.tmp, ["macos-aarch64", "windows-x86_64"])
        out = self.tmp / "out"
        folders = bnp.build(release.dir, out, VERSION)
        tarballs = bnp.pack(folders, out / "tarballs")
        self.assertEqual(
            [t.name for t in tarballs],
            [f"tablex-mcp-darwin-arm64-{VERSION}.tgz", f"tablex-mcp-win32-x64-{VERSION}.tgz", f"tablex-mcp-server-{VERSION}.tgz"],
        )
        import tarfile

        with tarfile.open(tarballs[0]) as archive:
            members = {m.name: m for m in archive.getmembers()}
            self.assertEqual(sorted(members), ["package/LICENSE", "package/bin/tablex-mcp", "package/package.json"])
            self.assertEqual(archive.extractfile("package/bin/tablex-mcp").read(), release.bytes["macos-aarch64"])
            self.assertTrue(members["package/bin/tablex-mcp"].mode & stat.S_IXUSR, "the executable bit survives the tarball")
        with tarfile.open(tarballs[2]) as archive:
            names = sorted(archive.getnames())
        for expected in ["package/bin/tablex-mcp.js", "package/lib/launcher.js", "package/checksums.json", "package/platforms.json", "package/server.json"]:
            self.assertIn(expected, names)
        self.assertFalse([n for n in names if "tests" in n])


if __name__ == "__main__":
    unittest.main()
