import contextlib
import io
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import package_managers as pm

VERSION = "0.1.11"


def digest(name: str) -> str:
    """A distinct, valid SHA-256 per file name."""
    import hashlib

    return hashlib.sha256(name.encode()).hexdigest()


def sums_for(version=VERSION):
    names = list(pm.asset_names(version).values())
    # What a real SHA256SUMS also holds: the other assets of the release.
    names += [f"tablex-{version}-macos-intel.app.zip", "app-update.json", "app-update.json.sig"]
    return {name: digest(name) for name in names}


def sums_text(sums):
    return "".join(f"{value}  {name}\n" for name, value in sums.items())


def run_cli(*argv):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = pm.main(list(argv))
    return code, out.getvalue(), err.getvalue()


class ParseTest(unittest.TestCase):
    def test_sha256sums_reads_text_and_binary_mode_lines(self):
        a, b = "a" * 64, "B" * 64
        parsed = pm.parse_sha256sums(f"{a}  one.dmg\n\n{b} *two file.zip\r\n")
        self.assertEqual(parsed, {"one.dmg": a, "two file.zip": b.lower()})

    def test_sha256sums_rejects_a_malformed_line(self):
        with self.assertRaises(pm.InputError):
            pm.parse_sha256sums("not-a-hash  tablex.dmg\n")

    def test_release_json_gives_digests_tag_and_date(self):
        release = {
            "tagName": "v0.1.11",
            "publishedAt": "2026-11-25T09:38:36Z",
            "assets": [
                {"name": "a.dmg", "digest": "sha256:" + "c" * 64},
                {"name": "no-digest.bin", "digest": None},
                {"name": "other.bin", "digest": "sha512:" + "d" * 128},
            ],
        }
        sums, tag, date = pm.parse_release_json(json.dumps(release))
        self.assertEqual(sums, {"a.dmg": "c" * 64})
        self.assertEqual((tag, date), ("v0.1.11", "2026-11-25"))

    def test_release_json_without_assets_is_an_input_error(self):
        with self.assertRaises(pm.InputError):
            pm.parse_release_json("{}")


class ValidationTest(unittest.TestCase):
    def test_a_missing_asset_names_the_file(self):
        sums = sums_for()
        del sums[f"tablex-{VERSION}-windows-x86_64-setup.exe"]
        with self.assertRaisesRegex(pm.InputError, "windows-x86_64-setup.exe"):
            pm.render_all(VERSION, sums)

    def test_a_bad_version_repo_date_or_digest_is_refused(self):
        for bad_version in ("0.1", "v0.1.11", "0.1.11\"; rm -rf /", "0.1.11-rc1", "0.1.11\n"):
            with self.assertRaises(pm.InputError, msg=bad_version):
                pm.render_all(bad_version, sums_for())
        with self.assertRaises(pm.InputError):
            pm.render_all(VERSION, sums_for(), repo="not a repo")
        with self.assertRaises(pm.InputError):
            pm.render_all(VERSION, sums_for(), release_date="yesterday")
        sums = sums_for()
        sums[f"tablex-{VERSION}-macos-intel.dmg"] = "z" * 64
        with self.assertRaises(pm.InputError):
            pm.render_all(VERSION, sums)


class RenderTest(unittest.TestCase):
    def setUp(self):
        self.sums = sums_for()
        self.files = pm.render_all(VERSION, self.sums, release_date="2026-11-25")
        names = pm.asset_names(VERSION)
        self.sha = {role: self.sums[name] for role, name in names.items()}

    def test_the_file_set(self):
        winget = f"winget-pkgs/manifests/t/TablexOrg/tablex/{VERSION}/TablexOrg.tablex"
        self.assertEqual(
            sorted(self.files),
            sorted(
                [
                    "homebrew-tap/Casks/tablex.rb",
                    f"{winget}.yaml",
                    f"{winget}.installer.yaml",
                    f"{winget}.locale.en-US.yaml",
                    "aur/tablex-bin/PKGBUILD",
                    "aur/tablex-bin/.SRCINFO",
                ]
            ),
        )

    def test_cask_has_one_sha_per_architecture_and_the_real_asset_names(self):
        cask = self.files["homebrew-tap/Casks/tablex.rb"]
        arm = cask.split("on_arm do")[1].split("end\n")[0]
        intel = cask.split("on_intel do")[1].split("end\n")[0]
        self.assertIn(self.sha["mac-arm"], arm)
        self.assertIn("macos-apple-silicon.dmg", arm)
        self.assertIn(self.sha["mac-intel"], intel)
        self.assertIn("macos-intel.dmg", intel)
        self.assertIn(f'version "{VERSION}"', cask)
        self.assertIn('app "tablex.app"', cask)
        self.assertIn("strategy :github_latest", cask)
        self.assertIn("~/Library/Application Support/tablex", cask)
        self.assertIn("org.tablex.TableX", cask)
        self.assertNotIn("@@", cask)

    def test_winget_installer_is_inno_user_x64_with_the_setup_hash(self):
        installer = self.files[f"winget-pkgs/manifests/t/TablexOrg/tablex/{VERSION}/TablexOrg.tablex.installer.yaml"]
        for line in (
            "InstallerType: inno",
            "Scope: user",
            "Architecture: x64",
            f"InstallerSha256: {self.sha['windows'].upper()}",
            f"/download/v{VERSION}/tablex-{VERSION}-windows-x86_64-setup.exe",
            "Silent: /SP- /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CURRENTUSER",
            "ReleaseDate: 2026-11-25",
            "ManifestVersion: 1.12.0",
        ):
            self.assertIn(line, installer)

    def test_winget_release_date_is_left_out_when_unknown(self):
        files = pm.render_all(VERSION, self.sums)
        installer = [text for path, text in files.items() if path.endswith("installer.yaml")][0]
        self.assertNotIn("ReleaseDate", installer)

    def test_winget_product_code_is_the_inno_app_id_of_the_script(self):
        iss = (Path(__file__).resolve().parents[1] / "windows" / "tablex.iss").read_text(encoding="utf-8")
        # `AppId={{GUID}`: Inno writes the doubled brace as one.
        self.assertIn("AppId={" + pm.WINDOWS_PRODUCT_CODE.removesuffix("_is1"), iss)

    def test_pkgbuild_and_srcinfo_agree_with_the_release(self):
        pkgbuild = self.files["aur/tablex-bin/PKGBUILD"]
        srcinfo = self.files["aur/tablex-bin/.SRCINFO"]
        for arch in ("x86_64", "aarch64"):
            digest_ = self.sha[f"linux-{arch}"]
            self.assertIn(f"sha256sums_{arch}=('{digest_}')", pkgbuild)
            self.assertIn(f"\tsha256sums_{arch} = {digest_}", srcinfo)
            self.assertIn(f"tablex-{VERSION}-linux-{arch}.tar.gz", srcinfo)
        self.assertIn(f"pkgver={VERSION}", pkgbuild)
        for dep in ("glibc", "vulkan-icd-loader", "libxkbcommon", "wayland", "libx11", "fontconfig", "libsecret", "dbus"):
            self.assertIn(f"\tdepends = {dep}\n", srcinfo)
        self.assertTrue(srcinfo.endswith("\npkgname = tablex-bin\n"))

    @unittest.skipUnless(shutil.which("bash"), "bash is needed")
    def test_srcinfo_lists_what_the_pkgbuild_declares(self):
        """Source the generated PKGBUILD in bash (it only assigns variables) and compare with .SRCINFO."""
        with tempfile.TemporaryDirectory() as tmp:
            pkgbuild = Path(tmp) / "PKGBUILD"
            pkgbuild.write_text(self.files["aur/tablex-bin/PKGBUILD"], encoding="utf-8")
            script = (
                'source "$1"; '
                'for var in depends optdepends provides conflicts options source_x86_64 sha256sums_x86_64 '
                'source_aarch64 sha256sums_aarch64; do '
                # eval, not `declare -n`: macOS ships bash 3.2.
                '  eval "items=(\\"\\${$var[@]}\\")"; for item in "${items[@]}"; do printf "%s = %s\\n" "$var" "$item"; done; '
                "done"
            )
            result = subprocess.run(["bash", "-c", script, "bash", str(pkgbuild)], capture_output=True, text=True, check=True)
        declared = sorted(result.stdout.splitlines())
        listed = sorted(
            line.strip()
            for line in self.files["aur/tablex-bin/.SRCINFO"].splitlines()
            if line.startswith("\t")
            and line.strip().split(" = ")[0]
            in {"depends", "optdepends", "provides", "conflicts", "options", "source_x86_64", "sha256sums_x86_64",
                "source_aarch64", "sha256sums_aarch64"}
        )
        self.assertEqual(declared, listed)

    @unittest.skipUnless(shutil.which("bash"), "bash is needed")
    def test_pkgbuild_is_valid_bash(self):
        subprocess.run(["bash", "-n"], input=self.files["aur/tablex-bin/PKGBUILD"], text=True, check=True)

    @unittest.skipUnless(shutil.which("ruby"), "ruby is needed")
    def test_cask_is_valid_ruby_and_winget_files_are_valid_yaml(self):
        for path, text in self.files.items():
            if path.endswith(".rb"):
                subprocess.run(["ruby", "-c"], input=text, text=True, check=True, capture_output=True)
            elif path.endswith(".yaml"):
                result = subprocess.run(
                    ["ruby", "-ryaml", "-rdate", "-e", "d = YAML.safe_load($stdin.read, permitted_classes: [Date]); puts d['ManifestType']"],
                    input=text,
                    text=True,
                    capture_output=True,
                )
                self.assertEqual(result.returncode, 0, f"{path}: {result.stderr}")
                self.assertIn(result.stdout.strip(), {"version", "installer", "defaultLocale"})

    def test_a_quote_in_a_description_cannot_break_the_yaml(self):
        self.assertEqual(pm.yaml_single_quoted("it's"), "it''s")


class CliTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def test_sha256sums_input_writes_the_three_sets(self):
        sums_file = self.root / "SHA256SUMS"
        sums_file.write_text(sums_text(sums_for()), encoding="utf-8")
        out = self.root / "out"
        code, stdout, stderr = run_cli("--version", VERSION, "--sha256sums", str(sums_file), "--out", str(out))
        self.assertEqual((code, stderr), (0, ""))
        self.assertEqual(len(stdout.splitlines()), 6)
        self.assertTrue((out / "homebrew-tap/Casks/tablex.rb").is_file())
        self.assertTrue((out / "aur/tablex-bin/.SRCINFO").is_file())
        self.assertTrue((out / f"winget-pkgs/manifests/t/TablexOrg/tablex/{VERSION}/TablexOrg.tablex.yaml").is_file())

    def test_release_json_input_takes_the_date_and_checks_the_tag(self):
        assets = [{"name": name, "digest": "sha256:" + value} for name, value in sums_for().items()]
        release = self.root / "release.json"
        release.write_text(json.dumps({"tagName": f"v{VERSION}", "publishedAt": "2026-11-25T09:38:36Z", "assets": assets}))
        out = self.root / "out"
        code, _, stderr = run_cli("--version", VERSION, "--release-json", str(release), "--out", str(out))
        self.assertEqual((code, stderr), (0, ""))
        installer = next(out.rglob("TablexOrg.tablex.installer.yaml")).read_text(encoding="utf-8")
        self.assertIn("ReleaseDate: 2026-11-25", installer)

        code, _, stderr = run_cli("--version", "0.1.12", "--release-json", str(release), "--out", str(out))
        self.assertEqual(code, 1)
        self.assertIn("v0.1.11", stderr)

    def test_an_incomplete_release_writes_nothing_and_exits_1(self):
        sums_file = self.root / "SHA256SUMS"
        sums_file.write_text("a" * 64 + "  tablex-0.1.11-macos-intel.dmg\n", encoding="utf-8")
        out = self.root / "out"
        code, _, stderr = run_cli("--version", VERSION, "--sha256sums", str(sums_file), "--out", str(out))
        self.assertEqual(code, 1)
        self.assertIn("no SHA-256 for", stderr)
        self.assertFalse(out.exists())


if __name__ == "__main__":
    unittest.main()
