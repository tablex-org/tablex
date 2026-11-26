'use strict';

// Finds the native `tablex-mcp` binary for this machine and runs it with the launcher's own
// standard streams. No dependencies: the binary does everything, this only points to it.

const { spawn } = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const { platforms: PLATFORMS } = require('../platforms.json');

/** Names a local binary to run instead of the installed package (a development build). */
const BIN_ENV = 'TABLEX_MCP_BIN';
/** Set to `1` to skip the check of the binary against the checksum baked in at pack time. */
const NO_VERIFY_ENV = 'TABLEX_MCP_NO_VERIFY';

class LauncherError extends Error {
  constructor(code, message) {
    super(message);
    this.name = 'LauncherError';
    this.code = code;
  }
}

/** glibc or musl on Linux, null elsewhere. A Node built for glibc reports its version. */
function detectLibc(platform = process.platform) {
  if (platform !== 'linux') return null;
  try {
    const header = process.report.getReport().header;
    return header.glibcVersionRuntime ? 'glibc' : 'musl';
  } catch {
    return 'glibc';
  }
}

/** The entry of platforms.json for this machine, or null when no build exists for it. */
function findPlatform({ platform, arch, libc }) {
  return (
    PLATFORMS.find(
      (entry) =>
        entry.os === platform && entry.cpu === arch && (entry.libc === undefined || entry.libc === libc),
    ) || null
  );
}

function describeMachine({ platform, arch, libc }) {
  return libc ? `${platform}-${arch} (${libc})` : `${platform}-${arch}`;
}

function sha256File(file) {
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
}

/** The folder of an installed package (null when it is not installed). */
function defaultResolvePackage(name) {
  try {
    return path.dirname(require.resolve(`${name}/package.json`));
  } catch {
    return null;
  }
}

/** The checksums packed with a release; null when the file is missing or is not a table. */
function defaultChecksums() {
  try {
    const table = JSON.parse(fs.readFileSync(path.join(__dirname, '..', 'checksums.json'), 'utf8'));
    return table && typeof table === 'object' && !Array.isArray(table) ? table : null;
  } catch {
    return null;
  }
}

/** A release is `major.minor.patch`: no prerelease or build tag (`0.1.9-dev`, `0.1.9+local`). */
function isRelease(version) {
  return /^\d+\.\d+\.\d+$/.test(String(version));
}

function ownVersion() {
  return JSON.parse(fs.readFileSync(path.join(__dirname, '..', 'package.json'), 'utf8')).version;
}

/**
 * The binary to run: `{ binary, source }`. Throws a LauncherError that says what to do.
 *
 * `TABLEX_MCP_BIN` wins and is not checked against any checksum (it is the user's own build).
 * Otherwise the platform package of this machine is looked up; when this package carries
 * checksums (it does once packed for release), the binary must match the one baked in. A release
 * version without checksums is refused; only a development version (a source checkout, with a
 * prerelease tag) runs unverified.
 */
function locateBinary({
  env = process.env,
  platform = process.platform,
  arch = process.arch,
  libc = detectLibc(platform),
  resolvePackage = defaultResolvePackage,
  checksums = defaultChecksums(),
  version = ownVersion(),
} = {}) {
  const override = env[BIN_ENV];
  if (override) {
    if (!fs.existsSync(override)) {
      throw new LauncherError('BIN_MISSING', `${BIN_ENV} names ${override}, which does not exist.`);
    }
    return { binary: override, source: 'env' };
  }
  const machine = { platform, arch, libc };
  const entry = findPlatform(machine);
  if (!entry) {
    const supported = PLATFORMS.map((p) => `${p.os}-${p.cpu}${p.libc ? ` (${p.libc})` : ''}`).join(', ');
    throw new LauncherError(
      'UNSUPPORTED',
      `tablex-mcp has no build for ${describeMachine(machine)}. Supported: ${supported}. ` +
        `Build tablex-mcp from source and point ${BIN_ENV} at it.`,
    );
  }
  const dir = resolvePackage(entry.package);
  if (!dir) {
    throw new LauncherError(
      'NOT_INSTALLED',
      `The package ${entry.package} (the tablex-mcp binary for ${describeMachine(machine)}) is not installed. ` +
        'It is an optional dependency: reinstall @tablex/mcp-server without --no-optional / --omit=optional, ' +
        `or install ${entry.package} yourself. To use a binary you built, set ${BIN_ENV}.`,
    );
  }
  const binary = path.join(dir, 'bin', entry.binary);
  if (!fs.existsSync(binary)) {
    throw new LauncherError('BIN_MISSING', `The tablex-mcp binary is missing from ${entry.package} (looked for ${binary}).`);
  }
  let installed;
  try {
    installed = JSON.parse(fs.readFileSync(path.join(dir, 'package.json'), 'utf8')).version;
  } catch {
    installed = undefined;
  }
  if (installed !== version) {
    throw new LauncherError(
      'VERSION_MISMATCH',
      `${entry.package} is version ${installed} but @tablex/mcp-server is ${version}; reinstall both so they match.`,
    );
  }
  if (!checksums && isRelease(version) && env[NO_VERIFY_ENV] !== '1') {
    // A release is packed with its checksums: without them nothing says the binary is the one
    // that was published, so a missing or damaged file must not turn the check off.
    throw new LauncherError(
      'CHECKSUM',
      `The checksums this release of @tablex/mcp-server was published with (checksums.json) are missing or unreadable, ` +
        `so the tablex-mcp binary cannot be verified: reinstall @tablex/mcp-server. To run it unverified anyway, set ${NO_VERIFY_ENV}=1.`,
    );
  }
  if (checksums && env[NO_VERIFY_ENV] !== '1') {
    const expected = checksums[entry.package];
    if (!expected) {
      throw new LauncherError('CHECKSUM', `No checksum is recorded for ${entry.package}; reinstall @tablex/mcp-server.`);
    }
    if (sha256File(binary) !== expected) {
      throw new LauncherError(
        'CHECKSUM',
        `The tablex-mcp binary in ${entry.package} does not match the checksum this package was published with ` +
          `(${binary}). It was changed after install, or the install is damaged: reinstall it.`,
      );
    }
  }
  return { binary, source: 'package' };
}

/** A unix binary that lost its executable bit (some installers drop it) is given it back. */
function ensureExecutable(binary) {
  if (process.platform === 'win32') return;
  try {
    fs.accessSync(binary, fs.constants.X_OK);
  } catch {
    try {
      fs.chmodSync(binary, 0o755);
    } catch {
      // The spawn below reports the real problem.
    }
  }
}

const FORWARDED = ['SIGINT', 'SIGTERM', 'SIGHUP'];

/**
 * Runs `binary` with `args`, the standard streams inherited (the MCP client talks to the binary
 * itself), forwards termination signals to it, and ends this process the way it ended: the same
 * exit code, or the same signal.
 */
function run(binary, args, { env = process.env } = {}) {
  ensureExecutable(binary);
  const failed = (error) => {
    process.stderr.write(`tablex-mcp: cannot start ${binary}: ${error.message}\n`);
    process.exit(1);
  };
  let child;
  try {
    child = spawn(binary, args, { stdio: 'inherit', env, windowsHide: true });
  } catch (error) {
    // Some failures (a file that is not a program) are thrown, others are an 'error' event.
    return failed(error);
  }
  const forwarders = FORWARDED.map((signal) => {
    const forward = () => child.kill(signal);
    process.on(signal, forward);
    return [signal, forward];
  });
  child.on('error', failed);
  child.on('exit', (code, signal) => {
    for (const [name, forward] of forwarders) process.removeListener(name, forward);
    if (signal) {
      process.kill(process.pid, signal);
      // Where a signal cannot be re-raised (Windows) the conventional code says the same.
      setTimeout(() => process.exit(128 + (os.constants.signals[signal] || 1)), 100);
      return;
    }
    process.exit(code === null ? 1 : code);
  });
  return child;
}

function main(argv = process.argv.slice(2)) {
  let located;
  try {
    located = locateBinary();
  } catch (error) {
    if (!(error instanceof LauncherError)) throw error;
    process.stderr.write(`tablex-mcp: ${error.message}\n`);
    process.exit(1);
  }
  run(located.binary, argv);
}

module.exports = { BIN_ENV, NO_VERIFY_ENV, LauncherError, PLATFORMS, detectLibc, findPlatform, isRelease, locateBinary, run, main };
