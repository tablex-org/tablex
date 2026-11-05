# Driver build and packaging scripts

The GPUI host (`engine-driver-host`) loads **ABI v2** driver dylibs, verifies the
sha256 from the manifest, and installs them from `dist/drivers/`. These scripts
build, verify and package the seven drivers: `mongodb postgresql mysql mariadb redis sqlite clickhouse`, and the optional MongoDB Shell plugin (an add-on of the MongoDB driver: see `build-plugin.sh` / `package-plugin.sh` below).

## One command (local testing)

```sh
rust-engine/scripts/dev-drivers.sh            # all seven
rust-engine/scripts/dev-drivers.sh redis      # just one (manifest keeps the others)
```

Run from anywhere in the repo. For each driver it does build, host-load verification,
unsigned local packaging, then writes and validates `dist/drivers/drivers-manifest.json`.
Debug app builds read that file automatically when launched from the repo root
(`apps/tablex-gpui/src/drivers.rs`); override with `TABLEX_DRIVER_MANIFEST_URL=file:///.../drivers-manifest.json`,
and use a throwaway `TABLEX_DRIVER_DATA_DIR` to keep test installs separate.
Artifact URLs are absolute `file://` URLs (the installer's `file` scheme), so the manifest is
machine-local and untracked.

Dev versions are `<crate version>-dev.<8 hex of dylib sha256>`: every rebuild with changed code shows up
as an update in the Driver Manager. `TABLEX_DEV_VERSION=1.2.3` pins a version instead.

## Pieces

| Script | Purpose |
| --- | --- |
| `driver-registry.sh` | Sourced table: id to crate, cargo features, cdylib name (`library_file_name`: `.dylib` / `.so` / `.dll`; `host_target`). `abi-v2` for every driver except `dbengine-sqlite` and `dbengine-clickhouse` (v2-only). Without `abi-v2` the crates export the ABI 1 symbols for the Swift app and the host rejects them. |
| `build-driver.sh <id>` | Builds one driver (ABI v2) and applies the strip policy below. Prints the dylib path (default `rust-engine/target/driver-dylibs/<arch>/`). |
| `verify-driver.sh <dylib> [--json out]` | Loads the dylib through the real host (`DriverLoader`: dlopen, ABI, capabilities, symbol resolution, init) via `cargo run --release -p engine-driver-host --example inspect-driver -- <dylib>`. Non-zero exit on any failure. |
| `package-driver.sh [--target <id>] [--inspect FILE] <id> <version> <dylib>` | Verifies, then packages for `--target` (default: this host): macOS = codesign + zip (`ditto`), Linux / Windows = the bare `libdbengine_<id>-<target>.so` / `dbengine_<id>-<target>.dll`. Writes `dist/drivers/<id>-<target>-manifest-entry.json` (the target is forwarded to `driver_manifest.py entry --target`). A target other than this host cannot be loaded here, so it needs `--inspect FILE` (what `verify-driver.sh --json` wrote on a runner of that target). `PACKAGE_OUT_DIR` overrides `dist/drivers`. id, displayName, uriSchemes, appAbiVersion and **features come from the dylib's capabilities**, never hand-listed. Only licence and a minimum-feature guard live in the script. Signing: `SIGNING_IDENTITY` / `TEAM_ID`, or `ALLOW_UNSIGNED_LOCAL=1`. `ARTIFACT_BASE_URL` for release URLs. |
| `make-local-driver-manifest.sh` | Merges `dist/drivers/*-manifest-entry.json` into `drivers-manifest.json`, then validates it with the host: `inspect-driver --manifest` parses it with `DriverManifest`, installs every driver through `DriverInstaller` (size, sha256, arch, ABI probe) into a temp store, loads it with `load_verified`, and requires manifest id/schemes/features to equal the dylib's capabilities. |
| `test-sshd.sh start\|stop\|status\|env` | Throwaway OpenSSH bastion container `tablex-test-sshd` (127.0.0.1:52222) for the `engine-net` live tests: generates a password and ed25519 / ecdsa / rsa (OpenSSH + PEM) / passphrase-protected keys into `$TMPDIR/tablex-test-sshd` (never in the repo); can reach the other `tablex-test-*` databases via `host.docker.internal`. Idempotent. `source "$(rust-engine/scripts/test-sshd.sh env)"` exports the `TABLEX_TEST_SSHD_*` variables. |
| `test-tls-servers.sh start\|stop [pg\|mysql\|mariadb\|mysql-notls\|mariadb-notls\|all]\|status\|env` | Throwaway TLS database servers for the driver TLS live tests: `tablex-test-pg-tls` (:55433, `hostssl` only, client-cert auth for `tablex_cert`), `tablex-test-mysql-tls` (:53316) and `tablex-test-mariadb-tls` (:53317) (`require_secure_transport=ON`, `tablex_x509` REQUIRE X509), plus `tablex-test-mysql-notls` / `tablex-test-mariadb-notls` (:53326 / :53327, TLS off, for the `PREFERRED` fallback). A throwaway CA, server certificate (SAN `localhost`, `db.tablex.test`), client certificates in PKCS#8 / PKCS#1 / SEC1 encodings and passwords are generated into `$TMPDIR/tablex-test-tls` (0700, never in the repo). Idempotent. `source "$(rust-engine/scripts/test-tls-servers.sh env)"` exports `TABLEX_TEST_TLS_*`, `TABLEX_TEST_{PG,MYSQL,MARIADB}_TLS_*`. |
| `build-plugin.sh [--out-dir DIR]` | Builds the MongoDB Shell plugin (`tablex-mongo-shell`, one executable the app spawns per script; QuickJS inside) with the drivers' strip policy. Prints the path (default `rust-engine/target/plugins/<arch>/`). |
| `package-plugin.sh [--target <id>] <version> <executable>` | Signs it (macOS: `SIGNING_IDENTITY`, or `ALLOW_UNSIGNED_LOCAL=1` ad hoc), says-hello smoke on this host's own build, copies it to `dist/tools/tablex-mongo-shell-<target>[.exe]` and writes `dist/drivers/tablex-mongo-shell-<target>-tool-entry.json`: a `mongoShell` tool entry with `requires: [{driver: "mongodb", minVersion}]` (`MIN_DRIVER_VERSION`, default the `dbengine-mongo` crate version of this checkout). The app installs it only next to a recent enough MongoDB driver. |
| `dev-drivers.sh` | The one-command flow above (it also builds and packages the plugin unless you name drivers; `dev-drivers.sh mongo-shell` for just the plugin). |
| `../../scripts/driver_manifest.py` | `entry` (capabilities JSON + zip to manifest entry) and `merge`. Tests: `python3 -m unittest discover -s scripts/tests -p 'test_*.py'`. |
| `sign-driver.sh`, `build-mongo-driver.sh` | Unchanged. `build-mongo-driver.sh` (SDK 26.5 workaround) is used by the host loader tests. |

Inspect a dylib by hand:

```sh
cargo run -q --release --manifest-path rust-engine/Cargo.toml -p engine-driver-host \
  --example inspect-driver -- path/to/libdbengine_redis.dylib
```

## Strip policy (why not cargo's default)

Xcode 27 `ld` (27037) plus cargo's default release `strip = "debuginfo"` produced a Redis cdylib
that dyld rejects (`mis-aligned LINKEDIT string pool`). `build-driver.sh` therefore:

1. builds with `CARGO_PROFILE_RELEASE_STRIP=none` (cargo never post-processes the image),
2. runs Apple `strip -x` (drops local symbols; every exported `dbengine_*` symbol stays),
3. re-signs ad hoc (`codesign --force --sign -`; strip invalidates the linker signature).

`TABLEX_DRIVER_STRIP=none` skips step 2 and 3 (larger, loads too). Because packaging always
dlopens the final file through the host, a toolchain regression fails the build instead of shipping
an unloadable driver. Release signing in `package-driver.sh` re-signs the stripped file.

## Architectures

- arm64: built, verified and packaged.
- x86_64 macOS: `TABLEX_DRIVER_TARGET=x86_64-apple-darwin build-driver.sh <id>` produces the dylib
  (sqlite checked), but it cannot be dlopen-verified on an arm64 host without Rosetta. Package it with
  `package-driver.sh --target macos-x86_64 --inspect <json from an Intel runner>`. No universal binary yet.
- Linux / Windows: `build-driver.sh` skips the macOS-only `strip -x` / `codesign` and emits `.so` / `.dll`;
  `package-driver.sh --target linux-x86_64 ...` publishes the bare library (no zip, no Team ID).
  Not exercised locally: no Linux or Windows toolchain is installed on the development machine.

## Release CI

`.github/workflows/release-drivers.yml` builds exactly what dev packaging builds, with these scripts
(no bare `cargo build`):

1. **`build` job (matrix, no secrets).** `build-driver.sh <id>` (abi-v2 features,
   `CARGO_PROFILE_RELEASE_STRIP=none` + `strip -x` + ad hoc re-sign) and `verify-driver.sh` (real host
   dlopen) for all seven drivers, uploads the dylibs as `dylibs-<target>` artifacts. Targets:
   `macos-aarch64` (releases), `macos-x86_64`, `linux-x86_64`, `linux-aarch64`, `windows-x86_64`
   (all but the first are `continue-on-error`: the app's Linux / Windows loading is unfinished, and the
   first Intel run should confirm the x86_64 zip before it blocks a release). Each job also uploads the
   `<id>.inspect.json` that `verify-driver.sh --json` wrote on its own runner.
2. **`release` job (secrets, `release` environment, `macos-14`).** Imports the Developer ID certificate
   into a temporary keychain, runs `package-driver.sh --target <matrix target>` per target and driver (the
   host target is dlopen-verified; the others are packaged from their runner's `--inspect` JSON; `SIGNING_IDENTITY` / `TEAM_ID`,
   `ARTIFACT_BASE_URL=https://github.com/<repo>/releases/download/<tag>`, version = the driver crate's
   version), `driver_manifest.py merge --release`, then `driver_manifest.py sign ... --key-env
   DRIVER_MANIFEST_SIGNING_KEY` **last**, and uploads the zips / bare libraries, `drivers-manifest.json` and
   `drivers-manifest.json.sig` to the GitHub Release of the tag. The temporary keychain and `.p12` are
   removed in an `always()` step. The manifest key never touches disk.

Open TODOs (not faked): notarization (no script does it); Windows Authenticode signing.

### The app's trusted manifest keys

The app trusts the ed25519 **public** keys committed as `OFFICIAL_MANIFEST_PUBLIC_KEYS` in
`apps/tablex-gpui/src/drivers.rs` (comma-separated base64, several for rotation). Public keys are not
secrets, so every build from source verifies the official catalog; the private half is only the
`DRIVER_MANIFEST_SIGNING_KEY` secret. Build the app with `cargo build -p tablex-gpui --release` and never
with `--features dev-keys`. A fork that signs its own catalog sets `TABLEX_MANIFEST_PUBLIC_KEYS` at compile
time, which replaces the official keys. Debug builds also trust the development key. See "Signing keys &
open source" in `docs/plans/2026-10-08-driver-manager.md`.

### Trigger and hardening (the repository is public)

- Triggers are only `push` of a `drivers-v*` tag and `workflow_dispatch` (run it with the tag selected
  as the ref; the jobs refuse branches). Never add `pull_request` / `pull_request_target`; no secret is
  reachable from a PR.
- Top-level `permissions: contents: read`; only the `release` job has `contents: write`.
- Every job needs `github.repository_owner == vars.RELEASE_OWNER`: set the repository **variable**
  `RELEASE_OWNER` to the owning user / org, so forks skip the workflow.
- Third-party actions are pinned to full commit SHAs (version in the trailing comment). Update them
  deliberately, never to a tag or branch.
- Secrets are scoped to the individual steps that need them via `env:`; nothing echoes them. The
  checkout uses `persist-credentials: false`. The tag name reaches shell only through `env`.
- Repository setup: create the GitHub **Environment** `release`, add **required reviewers**, and
  restrict **Deployment branches and tags** to protected tags (add a tag protection / ruleset for
  `drivers-v*`, restricted to maintainers). Put the secrets below in that environment, not at repo level.

### Required secrets (environment `release`) and variables

| Name | Kind | Purpose |
| --- | --- | --- |
| `APPLE_CERT_P12_BASE64` | secret | base64 of the Developer ID Application certificate + key (`.p12`) |
| `APPLE_CERT_PASSWORD` | secret | password of that `.p12` |
| `APPLE_TEAM_ID` | secret | Team ID; the identity "Developer ID Application: ... (TEAMID)" is looked up in the keychain and passed as `SIGNING_IDENTITY`, the Team ID as `TEAM_ID` |
| `DRIVER_MANIFEST_SIGNING_KEY` | secret | base64 manifest signing key (`--key-env`); make a throwaway one for dry runs with `cargo run -p engine-driver-host --example sign-manifest -- keygen <file>` |
| `RELEASE_OWNER` | repository variable | owner allowed to run the workflow |

## CI (`.github/workflows/ci.yml`)

Runs on `pull_request` and pushes to `main`. No secrets, `permissions: contents: read`,
no `pull_request_target` (safe for fork PRs), actions pinned to full commit SHAs,
`persist-credentials: false`, superseded runs cancelled. `Swatinem/rust-cache` keeps one
cache per OS covering both cargo workspaces (`.` and `rust-engine`). This is separate from
the tag-only `release-drivers.yml` above.

| Job | Runners | Command (reproduce locally from the repo root) |
| --- | --- | --- |
| `rust` check | macos-14, ubuntu-latest, windows-latest | `cargo check --workspace --all-targets` |
| `rust` app tests | same | `cargo test -p tablex-gpui` |
| `rust` engine tests | same | `cargo test --manifest-path rust-engine/Cargo.toml --workspace` |
| `scripts` | ubuntu-latest, macos-14 | `python3 -m unittest discover -s scripts/tests` |

- Live-database tests are `#[ignore]` and stay skipped. No `xvfb`: the app tests use
  GPUI's headless `test-support` platform. The Linux job installs the apt packages needed to
  compile the Wayland/X11, fontconfig, vulkan, openssl, sqlite and libsecret/dbus (keyring)
  dependencies; install the same on your machine to reproduce it.
- Linux and Windows are `experimental: true` in the matrix: their `cargo check` fails the
  job, but the two test steps use `continue-on-error`, because driver loading and
  Authenticode verification are typed stubs there and the app crate had never been compiled
  on those OSes. Set `experimental: false` once a run is green. macOS is always strict.
- Windows enables `core.longpaths` before checkout; no extra toolchain is installed.
- Dependencies are not built with `--locked` yet (the root `Cargo.lock` is new); add it once
  both lockfiles are committed.

## Manifest schema v2 and signing

`scripts/driver_manifest.py` writes schema v2: top-level `schemaVersion: 2` and
`generatedAt` (RFC 3339 UTC, always present, strictly greater than the manifest
it replaces), and per driver `category` (`sql` | `nosql` | `analytics` |
`key_value`), `description`, `minAppVersion`, `releaseNotesUrl`, `publishedAt`.
The category and description of each driver live in `DRIVER_META`.

- **Dev** (`dev-drivers.sh`): `file://` artifact URLs, `teamId: ""` for the
  ad-hoc signed dylibs, no signature (the host's Developer policy accepts it).
- **Release**: https artifact URLs and a Team ID (`package-driver.sh` with
  `SIGNING_IDENTITY`/`TEAM_ID`, `ARTIFACT_BASE_URL=https://...`), then
  `python3 scripts/driver_manifest.py merge --release --out drivers-manifest.json <entries>`
  (refuses `file://` and empty Team IDs) and, **last**, over the final bytes:
  `python3 scripts/driver_manifest.py sign --manifest drivers-manifest.json --key-env DRIVER_MANIFEST_SIGNING_KEY`
  (or `--key <file>`), which runs the `sign-manifest` example of
  `engine-driver-host` and writes `drivers-manifest.json.sig`. Any later rewrite
  of the manifest invalidates the signature. Keys come from a CI secret or a key
  file outside the repository; never commit one (`cargo run -p engine-driver-host
  --example sign-manifest -- keygen <file>` makes a throwaway key for dry runs).

## macOS app bundle

`scripts/bundle-macos.sh` (repo root) builds `tablex-gpui` in release mode and assembles
`target/release/bundle/TableX.app` (`Info.plist` with name "TableX", id `org.tablex.TableX`, minimum
macOS 12, Retina-capable), then ad-hoc codesigns it. Run from anywhere; flags: `--no-build` (reuse the
existing release binary) and `--binary PATH`; `TABLEX_BUNDLE_ID` overrides the identifier.
Unbundled runs (`cargo run`) show the executable name in the menu bar; the bundle gives "TableX".

Open items: the bundle identifier is a placeholder (TODO: the real reverse-DNS id), and no icon
artwork exists yet. Drop a square PNG (>= 1024 px) at `apps/tablex-gpui/assets/AppIcon.png` and the
script generates `AppIcon.icns` with `sips` + `iconutil`. Ad-hoc signing is local-only; distributing
needs a Developer ID signature and notarization.
