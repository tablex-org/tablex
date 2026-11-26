#!/usr/bin/env python3
"""Write the package-manager files of one tablex release: a Homebrew cask, winget manifests, an AUR package.

  python3 scripts/package_managers.py --version 0.1.11 --sha256sums SHA256SUMS --out packaging
  python3 scripts/package_managers.py --version 0.1.11 --release-json release.json --out packaging
      (release.json: `gh release view v0.1.11 --json assets,tagName,publishedAt`)

Output, laid out as the repository each set is copied into:

  <out>/homebrew-tap/Casks/tablex.rb                                        the tablex-org/homebrew-tap tap
  <out>/winget-pkgs/manifests/t/TablexOrg/tablex/<version>/*.yaml           microsoft/winget-pkgs
  <out>/aur/tablex-bin/PKGBUILD, .SRCINFO                                   ssh://aur@aur.archlinux.org/tablex-bin.git

Every checksum comes from the release's own SHA256SUMS (or the digests GitHub shows for its assets); nothing is
downloaded here. Version, checksums, repository and date are validated before they reach a Ruby, YAML or shell
file, so a malformed input fails the run instead of producing a file that does something else.
Stdlib only. scripts/tests/test_package_managers.py covers it.
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

DEFAULT_REPO = "tablex-org/tablex"
HOMEPAGE = "https://tablex-org.github.io/"
DESCRIPTION = "Performance-first native database client written in Rust"
LONG_DESCRIPTION = (
    "A performance-first native database client written in Rust. Database support comes from driver plugins "
    "that the app downloads, verifies and loads at run time: PostgreSQL, MySQL, MariaDB, SQLite, MongoDB, "
    "Redis and ClickHouse."
)
# winget manifest schema in use by the community repository (the client reads 1.6 and later).
WINGET_SCHEMA = "1.12.0"
WINGET_ID = "TablexOrg.tablex"
# `AppId` of scripts/windows/tablex.iss: Inno Setup names the uninstall key `<AppId>_is1`.
WINDOWS_PRODUCT_CODE = "{D4E7242A-991A-46AC-819A-6FF566E5906C}_is1"
LICENSE_SPDX = "AGPL-3.0-only"

VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+\Z")
SHA256_RE = re.compile(r"^[0-9a-f]{64}\Z")
REPO_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\Z")
DATE_RE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}\Z")


class InputError(ValueError):
    """The version, the checksums or the options cannot produce correct files."""


def asset_names(version: str) -> dict[str, str]:
    """The release assets the package files point at, by role."""
    return {
        "mac-arm": f"tablex-{version}-macos-apple-silicon.dmg",
        "mac-intel": f"tablex-{version}-macos-intel.dmg",
        "linux-x86_64": f"tablex-{version}-linux-x86_64.tar.gz",
        "linux-aarch64": f"tablex-{version}-linux-aarch64.tar.gz",
        "windows": f"tablex-{version}-windows-x86_64-setup.exe",
    }


def parse_sha256sums(text: str) -> dict[str, str]:
    """`<hex>  <name>` lines (`shasum -a 256`, `sha256sum`, binary mode `*` too) to {name: hex}."""
    sums: dict[str, str] = {}
    for number, line in enumerate(text.splitlines(), 1):
        if not line.strip():
            continue
        match = re.fullmatch(r"([0-9a-fA-F]{64}) [ *](.+)", line.rstrip("\r"))
        if not match:
            raise InputError(f"SHA256SUMS line {number} is not '<sha256>  <file>'")
        sums[match.group(2)] = match.group(1).lower()
    return sums


def parse_release_json(text: str) -> tuple[dict[str, str], str | None, str | None]:
    """`gh release view --json assets,tagName,publishedAt` to ({name: hex}, tag, published date)."""
    try:
        release = json.loads(text)
        assets = release["assets"]
    except (ValueError, KeyError, TypeError) as error:
        raise InputError(f"release JSON has no assets list: {error}") from error
    sums: dict[str, str] = {}
    for asset in assets:
        digest = asset.get("digest") or ""
        if digest.startswith("sha256:"):
            sums[asset["name"]] = digest[len("sha256:") :].lower()
    published = release.get("publishedAt")
    return sums, release.get("tagName"), published[:10] if isinstance(published, str) else None


def check_inputs(version: str, sums: dict[str, str], repo: str, release_date: str | None) -> None:
    if not VERSION_RE.match(version):
        raise InputError(f"version {version!r} is not major.minor.patch")
    if not REPO_RE.match(repo):
        raise InputError(f"repository {repo!r} is not owner/name")
    if release_date is not None and not DATE_RE.match(release_date):
        raise InputError(f"release date {release_date!r} is not YYYY-MM-DD")
    for role, name in asset_names(version).items():
        digest = sums.get(name)
        if digest is None:
            raise InputError(f"no SHA-256 for {name} ({role}); is the release complete?")
        if not SHA256_RE.match(digest):
            raise InputError(f"the SHA-256 of {name} is not 64 lowercase hex digits")


def fill(template: str, **values: str) -> str:
    """Replace @@KEY@@ markers (Ruby, YAML and shell all use braces and dollars, so no format strings)."""
    out = template
    for key, value in values.items():
        out = out.replace(f"@@{key}@@", value)
    leftover = re.search(r"@@[A-Z0-9_]+@@", out)
    if leftover:
        raise AssertionError(f"unfilled marker {leftover.group(0)}")
    return out


# --- Homebrew cask -------------------------------------------------------------------------------------------

CASK = '''\
# Generated by scripts/package_managers.py in tablex-org/tablex. Do not edit by hand.
cask "tablex" do
  version "@@VERSION@@"

  on_arm do
    sha256 "@@MAC_ARM_SHA@@"

    url "https://github.com/@@REPO@@/releases/download/v#{version}/tablex-#{version}-macos-apple-silicon.dmg",
        verified: "github.com/@@REPO@@/"
  end
  on_intel do
    sha256 "@@MAC_INTEL_SHA@@"

    url "https://github.com/@@REPO@@/releases/download/v#{version}/tablex-#{version}-macos-intel.dmg",
        verified: "github.com/@@REPO@@/"
  end

  name "tablex"
  desc "@@DESCRIPTION@@"
  homepage "@@HOMEPAGE@@"

  livecheck do
    url :url
    strategy :github_latest
  end

  # The app updates itself on macOS ("Install & Restart").
  auto_updates true
  depends_on macos: ">= :monterey"

  app "tablex.app"

  uninstall quit: "org.tablex.TableX"

  # What tablex writes (rust-engine/crates/engine-connect/src/platform.rs, apps/tablex-gpui/src/platform.rs):
  # saved connections, settings, logs and the driver store in Application Support/tablex, the grid copy default
  # and Snowflake state in Application Support/TableX (a different folder only on a case-sensitive volume).
  # Not removed: saved passwords (Keychain items whose service starts with "tablex.") and exports in
  # ~/Downloads/TableX, which are the user's own files.
  zap trash: [
    "~/Library/Application Support/tablex",
    "~/Library/Application Support/TableX",
    "~/Library/Caches/org.tablex.TableX",
    "~/Library/Preferences/org.tablex.TableX.plist",
    "~/Library/Saved Application State/org.tablex.TableX.savedState",
  ]

  caveats <<~EOS
    tablex is not signed with an Apple Developer ID and is not notarized, so macOS blocks the first launch.
    Either open System Settings > Privacy & Security and click "Open Anyway" (the button is there for about an
    hour after the blocked launch), or remove the quarantine flag once:

      xattr -dr com.apple.quarantine "#{appdir}/tablex.app"
  EOS
end
'''


def render_cask(version: str, sums: dict[str, str], repo: str) -> str:
    names = asset_names(version)
    return fill(
        CASK,
        VERSION=version,
        REPO=repo,
        MAC_ARM_SHA=sums[names["mac-arm"]],
        MAC_INTEL_SHA=sums[names["mac-intel"]],
        DESCRIPTION=DESCRIPTION,
        HOMEPAGE=HOMEPAGE,
    )


# --- winget --------------------------------------------------------------------------------------------------

WINGET_VERSION = """\
# Generated by scripts/package_managers.py in tablex-org/tablex.
# yaml-language-server: $schema=https://aka.ms/winget-manifest.version.@@SCHEMA@@.schema.json

PackageIdentifier: @@ID@@
PackageVersion: @@VERSION@@
DefaultLocale: en-US
ManifestType: version
ManifestVersion: @@SCHEMA@@
"""

# The default Inno switches of winget are `/SP- /VERYSILENT /SUPPRESSMSGBOXES /NORESTART` (silent) and
# `/SP- /SILENT ...` (with progress); /CURRENTUSER is added to both. tablex.iss has PrivilegesRequired=lowest,
# so it installs per user either way, and the release workflow's installer smoke test passes the same switches.
WINGET_INSTALLER = """\
# Generated by scripts/package_managers.py in tablex-org/tablex.
# yaml-language-server: $schema=https://aka.ms/winget-manifest.installer.@@SCHEMA@@.schema.json

PackageIdentifier: @@ID@@
PackageVersion: @@VERSION@@
InstallerLocale: en-US
MinimumOSVersion: 10.0.0.0
InstallerType: inno
Scope: user
InstallModes:
- interactive
- silent
- silentWithProgress
InstallerSwitches:
  Silent: /SP- /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CURRENTUSER
  SilentWithProgress: /SP- /SILENT /SUPPRESSMSGBOXES /NORESTART /CURRENTUSER
UpgradeBehavior: install
ProductCode: '@@PRODUCT_CODE@@'
@@RELEASE_DATE@@Installers:
- Architecture: x64
  InstallerUrl: https://github.com/@@REPO@@/releases/download/v@@VERSION@@/@@SETUP@@
  InstallerSha256: @@SHA@@
ManifestType: installer
ManifestVersion: @@SCHEMA@@
"""

WINGET_LOCALE = """\
# Generated by scripts/package_managers.py in tablex-org/tablex.
# yaml-language-server: $schema=https://aka.ms/winget-manifest.defaultLocale.@@SCHEMA@@.schema.json

PackageIdentifier: @@ID@@
PackageVersion: @@VERSION@@
PackageLocale: en-US
Publisher: tablex
PublisherUrl: https://github.com/@@OWNER@@
PublisherSupportUrl: https://github.com/@@REPO@@/issues
PackageName: tablex
PackageUrl: @@HOMEPAGE@@
License: @@LICENSE@@
LicenseUrl: https://github.com/@@REPO@@/blob/v@@VERSION@@/LICENSE
ShortDescription: '@@DESCRIPTION@@'
Description: '@@LONG_DESCRIPTION@@'
Moniker: tablex
Tags:
- database
- database-client
- sql
- postgresql
- mysql
- sqlite
- mongodb
- redis
ReleaseNotesUrl: https://github.com/@@REPO@@/releases/tag/v@@VERSION@@
ManifestType: defaultLocale
ManifestVersion: @@SCHEMA@@
"""


def yaml_single_quoted(text: str) -> str:
    """The inside of a '...' YAML scalar: the only character to escape is the quote itself."""
    return text.replace("'", "''")


def render_winget(version: str, sums: dict[str, str], repo: str, release_date: str | None) -> dict[str, str]:
    """{file name: text} of the three files of manifests/t/TablexOrg/tablex/<version>/."""
    setup = asset_names(version)["windows"]
    common = {"SCHEMA": WINGET_SCHEMA, "ID": WINGET_ID, "VERSION": version}
    return {
        f"{WINGET_ID}.yaml": fill(WINGET_VERSION, **common),
        f"{WINGET_ID}.installer.yaml": fill(
            WINGET_INSTALLER,
            **common,
            PRODUCT_CODE=WINDOWS_PRODUCT_CODE,
            RELEASE_DATE=f"ReleaseDate: {release_date}\n" if release_date else "",
            REPO=repo,
            SETUP=setup,
            SHA=sums[setup].upper(),
        ),
        f"{WINGET_ID}.locale.en-US.yaml": fill(
            WINGET_LOCALE,
            **common,
            OWNER=repo.split("/")[0],
            REPO=repo,
            HOMEPAGE=HOMEPAGE,
            LICENSE=LICENSE_SPDX,
            DESCRIPTION=yaml_single_quoted(DESCRIPTION),
            LONG_DESCRIPTION=yaml_single_quoted(LONG_DESCRIPTION),
        ),
    }


# --- AUR -----------------------------------------------------------------------------------------------------

AUR_ARCHES = ("x86_64", "aarch64")
# Linked by the binary (readelf -d: libxcb, libxkbcommon, libxkbcommon-x11, libgcc_s, glibc) and loaded at run
# time (wayland-client / wayland-egl, X11, fontconfig, Vulkan and EGL through the loader, the Secret Service
# over D-Bus). namcap sees only the first group, so it may call the rest unneeded.
AUR_DEPENDS = (
    "glibc",
    "gcc-libs",
    "libxcb",
    "libxkbcommon",
    "libxkbcommon-x11",
    "libx11",
    "wayland",
    "fontconfig",
    "vulkan-icd-loader",
    "libsecret",
    "dbus",
)
AUR_OPTDEPENDS = (
    "mesa: Vulkan and OpenGL drivers for Intel, AMD and software rendering",
    "vulkan-driver: a Vulkan driver for your GPU (vulkan-radeon, vulkan-intel, nvidia-utils)",
    "gnome-keyring: Secret Service that stores saved connection passwords (or kwallet, keepassxc)",
)

PKGBUILD = """\
# Maintainer: <your name> <you@example.org>
# Generated by scripts/package_managers.py in tablex-org/tablex: pkgver and the checksums come from the
# release's SHA256SUMS. pkgrel goes back to 1 with each pkgver.
#
# namcap notes:
#  - The program is installed to /opt/tablex with symlinks in /usr/bin; it is a prebuilt binary, so namcap may
#    mention /opt and the ELF hardening flags. Neither is fixable from here.
#  - wayland, libx11, fontconfig, vulkan-icd-loader, libsecret and dbus are loaded at run time (dlopen, D-Bus),
#    not linked, so namcap cannot see why they are needed and may report them as unused. They are needed.
#  - options=('!strip' '!debug'): the release binary is already stripped, and there is nothing to split off.
#  - license: AGPL-3.0-only is in the `licenses` package; the LICENSE and the third-party texts are shipped
#    anyway, because the binary carries code that is under the Apache-2.0 licence.
pkgname=tablex-bin
pkgver=@@VERSION@@
pkgrel=1
pkgdesc='@@DESCRIPTION@@ (prebuilt)'
arch=(@@ARCHES@@)
url='https://github.com/@@REPO@@'
license=('@@LICENSE@@')
depends=(@@DEPENDS@@)
optdepends=(@@OPTDEPENDS@@)
provides=('tablex')
conflicts=('tablex')
options=('!strip' '!debug')
@@SOURCES@@

package() {
  cd "tablex-${pkgver}-linux-${CARCH}"

  install -Dm755 bin/tablex bin/tablex-mcp -t "${pkgdir}/opt/tablex/bin"
  install -d "${pkgdir}/usr/bin"
  ln -s /opt/tablex/bin/tablex "${pkgdir}/usr/bin/tablex"
  ln -s /opt/tablex/bin/tablex-mcp "${pkgdir}/usr/bin/tablex-mcp"

  # Exec=tablex, StartupWMClass=tablex: the symlink in /usr/bin is what the entry runs.
  install -Dm644 share/applications/tablex.desktop -t "${pkgdir}/usr/share/applications"
  for size in 16 32 48 64 128 256 512; do
    install -Dm644 "share/icons/hicolor/${size}x${size}/apps/tablex.png" \\
      -t "${pkgdir}/usr/share/icons/hicolor/${size}x${size}/apps"
  done

  install -Dm644 LICENSE -t "${pkgdir}/usr/share/licenses/${pkgname}"
  install -d "${pkgdir}/usr/share/licenses/${pkgname}/third-party"
  cp -r licenses/. "${pkgdir}/usr/share/licenses/${pkgname}/third-party"
}
"""


def aur_source(version: str, arch: str, repo: str) -> str:
    asset = asset_names(version)[f"linux-{arch}"]
    return f"tablex-bin-{version}-{arch}.tar.gz::https://github.com/{repo}/releases/download/v{version}/{asset}"


def render_pkgbuild(version: str, sums: dict[str, str], repo: str) -> str:
    sources = []
    for arch in AUR_ARCHES:
        digest = sums[asset_names(version)[f"linux-{arch}"]]
        sources.append(f"source_{arch}=('{aur_source(version, arch, repo)}')")
        sources.append(f"sha256sums_{arch}=('{digest}')")
    return fill(
        PKGBUILD,
        VERSION=version,
        DESCRIPTION=DESCRIPTION,
        ARCHES=" ".join(f"'{arch}'" for arch in AUR_ARCHES),
        REPO=repo,
        LICENSE=LICENSE_SPDX,
        DEPENDS=" ".join(f"'{dep}'" for dep in AUR_DEPENDS),
        OPTDEPENDS="\n            ".join(f"'{dep}'" for dep in AUR_OPTDEPENDS),
        SOURCES="\n".join(sources),
    )


def render_srcinfo(version: str, sums: dict[str, str], repo: str) -> str:
    """What `makepkg --printsrcinfo` prints for the PKGBUILD above (tab indented, keys in makepkg's order)."""
    lines = [
        "pkgbase = tablex-bin",
        f"\tpkgdesc = {DESCRIPTION} (prebuilt)",
        f"\tpkgver = {version}",
        "\tpkgrel = 1",
        f"\turl = https://github.com/{repo}",
        *(f"\tarch = {arch}" for arch in AUR_ARCHES),
        f"\tlicense = {LICENSE_SPDX}",
        *(f"\tdepends = {dep}" for dep in AUR_DEPENDS),
        *(f"\toptdepends = {dep}" for dep in AUR_OPTDEPENDS),
        "\tprovides = tablex",
        "\tconflicts = tablex",
        "\toptions = !strip",
        "\toptions = !debug",
    ]
    for arch in AUR_ARCHES:
        lines.append(f"\tsource_{arch} = {aur_source(version, arch, repo)}")
        lines.append(f"\tsha256sums_{arch} = {sums[asset_names(version)[f'linux-{arch}']]}")
    lines += ["", "pkgname = tablex-bin", ""]
    return "\n".join(lines)


# --- driver --------------------------------------------------------------------------------------------------


def render_all(version: str, sums: dict[str, str], repo: str = DEFAULT_REPO, release_date: str | None = None) -> dict[str, str]:
    """{path relative to the output directory: file text}."""
    check_inputs(version, sums, repo, release_date)
    files = {"homebrew-tap/Casks/tablex.rb": render_cask(version, sums, repo)}
    for name, text in render_winget(version, sums, repo, release_date).items():
        files[f"winget-pkgs/manifests/t/TablexOrg/tablex/{version}/{name}"] = text
    files["aur/tablex-bin/PKGBUILD"] = render_pkgbuild(version, sums, repo)
    files["aur/tablex-bin/.SRCINFO"] = render_srcinfo(version, sums, repo)
    return files


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--version", required=True, help="app version, major.minor.patch (the tag without v)")
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--sha256sums", type=Path, help="the release's SHA256SUMS")
    source.add_argument("--release-json", type=Path, help="gh release view --json assets,tagName,publishedAt")
    parser.add_argument("--out", type=Path, required=True, help="directory the three sets are written under")
    parser.add_argument("--repo", default=DEFAULT_REPO, help=f"GitHub owner/name (default {DEFAULT_REPO})")
    parser.add_argument("--release-date", help="YYYY-MM-DD for winget (default: the release JSON's publishedAt)")
    args = parser.parse_args(argv)

    try:
        release_date = args.release_date
        if args.sha256sums:
            sums = parse_sha256sums(args.sha256sums.read_text(encoding="utf-8"))
        else:
            sums, tag, published = parse_release_json(args.release_json.read_text(encoding="utf-8"))
            if tag is not None and tag != f"v{args.version}":
                raise InputError(f"release JSON is for {tag}, not v{args.version}")
            release_date = release_date or published
        files = render_all(args.version, sums, args.repo, release_date)
    except (InputError, OSError) as error:
        print(f"package_managers: {error}", file=sys.stderr)
        return 1

    for relative, text in files.items():
        path = args.out / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8", newline="\n")
        print(path)
    return 0


if __name__ == "__main__":
    sys.exit(main())
