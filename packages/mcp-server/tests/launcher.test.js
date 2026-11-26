'use strict';

// Tests of the launcher with `node:test` only. A "binary" is the Node executable itself running a
// script given as arguments, so the same tests run on every platform without a build.

const assert = require('node:assert/strict');
const { spawn, spawnSync } = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { test } = require('node:test');

const launcher = require('../lib/launcher.js');
const pkg = require('../package.json');

const BIN = path.join(__dirname, '..', 'bin', 'tablex-mcp.js');
const NODE = process.execPath;

function tmp() {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'tablex-mcp-launcher-'));
}

/** An installed platform package for `machine`, in a fresh folder; returns what locateBinary needs. */
function fakeInstall(machine, { version = pkg.version, bytes = 'binary bytes' } = {}) {
  const entry = launcher.findPlatform(machine);
  const root = tmp();
  const dir = path.join(root, entry.package);
  fs.mkdirSync(path.join(dir, 'bin'), { recursive: true });
  fs.writeFileSync(path.join(dir, 'package.json'), JSON.stringify({ name: entry.package, version }));
  const binary = path.join(dir, 'bin', entry.binary);
  fs.writeFileSync(binary, bytes);
  const sha = crypto.createHash('sha256').update(bytes).digest('hex');
  return {
    entry,
    binary,
    sha,
    options: { ...machine, env: {}, resolvePackage: (name) => (name === entry.package ? dir : null), version: pkg.version },
  };
}

const MACHINES = [
  [{ platform: 'darwin', arch: 'arm64' }, '@tablex/mcp-darwin-arm64'],
  [{ platform: 'darwin', arch: 'x64' }, '@tablex/mcp-darwin-x64'],
  [{ platform: 'linux', arch: 'x64', libc: 'glibc' }, '@tablex/mcp-linux-x64-gnu'],
  [{ platform: 'linux', arch: 'arm64', libc: 'glibc' }, '@tablex/mcp-linux-arm64-gnu'],
  [{ platform: 'win32', arch: 'x64' }, '@tablex/mcp-win32-x64'],
  [{ platform: 'win32', arch: 'arm64' }, '@tablex/mcp-win32-arm64'],
];

test('each supported machine resolves to its own package, and the Windows ones to an .exe', () => {
  for (const [machine, name] of MACHINES) {
    const entry = launcher.findPlatform(machine);
    assert.equal(entry.package, name, JSON.stringify(machine));
    assert.equal(entry.binary, machine.platform === 'win32' ? 'tablex-mcp.exe' : 'tablex-mcp');
  }
  assert.equal(new Set(MACHINES.map(([, name]) => name)).size, MACHINES.length);
});

test('a machine without a build is an error that lists what is supported', () => {
  for (const machine of [
    { platform: 'linux', arch: 'x64', libc: 'musl' },
    { platform: 'linux', arch: 'ia32', libc: 'glibc' },
    { platform: 'freebsd', arch: 'x64' },
    { platform: 'win32', arch: 'ia32' },
  ]) {
    assert.equal(launcher.findPlatform(machine), null);
    assert.throws(
      () => launcher.locateBinary({ ...machine, env: {}, checksums: null }),
      (error) => error.code === 'UNSUPPORTED' && /Supported: .*darwin-arm64/.test(error.message) && error.message.includes('TABLEX_MCP_BIN'),
    );
  }
});

test('a missing platform package says how to fix it (the install left out optional packages)', () => {
  const machine = { platform: 'linux', arch: 'x64', libc: 'glibc' };
  assert.throws(
    () => launcher.locateBinary({ ...machine, env: {}, resolvePackage: () => null, checksums: null }),
    (error) =>
      error.code === 'NOT_INSTALLED' &&
      error.message.includes('@tablex/mcp-linux-x64-gnu') &&
      error.message.includes('--no-optional') &&
      error.message.includes('TABLEX_MCP_BIN'),
  );
});

test('a package without its binary, or of another version, is refused', () => {
  const machine = { platform: 'darwin', arch: 'arm64' };
  const gone = fakeInstall(machine);
  fs.rmSync(gone.binary);
  assert.throws(() => launcher.locateBinary({ ...gone.options, checksums: null }), { code: 'BIN_MISSING' });
  const stale = fakeInstall(machine, { version: '0.0.1' });
  assert.throws(() => launcher.locateBinary({ ...stale.options, checksums: null }), (error) => error.code === 'VERSION_MISMATCH' && /0\.0\.1/.test(error.message));
});

test('the installed binary is found when it matches the checksum baked in at pack time', () => {
  const install = fakeInstall({ platform: 'win32', arch: 'x64' });
  const found = launcher.locateBinary({ ...install.options, checksums: { [install.entry.package]: install.sha } });
  assert.deepEqual(found, { binary: install.binary, source: 'package' });
});

test('a development version without checksums (a source checkout) runs unverified', () => {
  const install = fakeInstall({ platform: 'win32', arch: 'x64' }, { version: '0.1.9-dev' });
  const options = { ...install.options, version: '0.1.9-dev', checksums: null };
  assert.equal(launcher.locateBinary(options).binary, install.binary);
  assert.equal(launcher.locateBinary({ ...options, version: '0.1.9-dev', checksums: null }).source, 'package');
});

test('a release without its checksums is refused: a missing or damaged file does not turn the check off', () => {
  const install = fakeInstall({ platform: 'linux', arch: 'x64', libc: 'glibc' });
  for (const checksums of [null]) {
    assert.throws(
      () => launcher.locateBinary({ ...install.options, checksums }),
      (error) => error.code === 'CHECKSUM' && /checksums\.json/.test(error.message) && error.message.includes('reinstall'),
    );
  }
  // The explicit opt-out still works.
  const found = launcher.locateBinary({ ...install.options, env: { TABLEX_MCP_NO_VERIFY: '1' }, checksums: null });
  assert.equal(found.source, 'package');
  assert.deepEqual(['0.1.8', '10.20.30'].map(launcher.isRelease), [true, true]);
  assert.deepEqual(['0.1.8-dev', '0.1.8-rc.1', '0.1.8+x', 'x', ''].map(launcher.isRelease), [false, false, false, false, false]);
});

test('a binary that does not match the checksum is never run', () => {
  const install = fakeInstall({ platform: 'darwin', arch: 'x64' });
  fs.writeFileSync(install.binary, 'tampered');
  assert.throws(
    () => launcher.locateBinary({ ...install.options, checksums: { [install.entry.package]: install.sha } }),
    (error) => error.code === 'CHECKSUM' && error.message.includes('does not match'),
  );
  // A table that does not know the package is as bad as a wrong value.
  assert.throws(() => launcher.locateBinary({ ...install.options, checksums: {} }), { code: 'CHECKSUM' });
  // The escape hatch is explicit.
  const found = launcher.locateBinary({
    ...install.options,
    env: { TABLEX_MCP_NO_VERIFY: '1' },
    checksums: { [install.entry.package]: install.sha },
  });
  assert.equal(found.source, 'package');
});

test('TABLEX_MCP_BIN wins over the package and is not checked against a checksum', () => {
  const install = fakeInstall({ platform: 'darwin', arch: 'arm64' });
  const found = launcher.locateBinary({
    ...install.options,
    env: { TABLEX_MCP_BIN: NODE },
    checksums: { [install.entry.package]: 'not-the-hash-of-anything' },
  });
  assert.deepEqual(found, { binary: NODE, source: 'env' });
  // It even works on a machine that has no build.
  assert.equal(
    launcher.locateBinary({ platform: 'freebsd', arch: 'x64', env: { TABLEX_MCP_BIN: NODE }, checksums: null }).binary,
    NODE,
  );
  assert.throws(
    () => launcher.locateBinary({ env: { TABLEX_MCP_BIN: path.join(os.tmpdir(), 'no-such-tablex-mcp-binary') } }),
    { code: 'BIN_MISSING' },
  );
});

/** The launcher as a process, with the Node executable standing in for the binary. */
function launch(args, { env = {}, input } = {}) {
  return spawnSync(NODE, [BIN, ...args], {
    encoding: 'utf8',
    input,
    env: { PATH: process.env.PATH, ...env, TABLEX_MCP_BIN: NODE },
    timeout: 30000,
  });
}

test('arguments reach the binary untouched, including ones that look like launcher flags', () => {
  // Node itself consumes the first `--`; everything after it is the launcher's argument list.
  const args = ['-e', 'console.log(JSON.stringify(process.argv.slice(1)))', '--', '--http', '127.0.0.1:0', '--version', 'with space', '-h', ''];
  const result = launch(args);
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(JSON.parse(result.stdout), ['--http', '127.0.0.1:0', '--version', 'with space', '-h', '']);
});

test('the exit code of the binary is the exit code of the launcher', () => {
  for (const code of [0, 1, 2, 7, 42, 255]) {
    const result = launch(['-e', `process.exit(${code})`]);
    assert.equal(result.status, code);
  }
});

test('standard input and output are the binary\'s own: what is sent in comes back, stderr stays stderr', () => {
  const script = "process.stdin.pipe(process.stdout); process.stderr.write('err-line')";
  const result = launch(['-e', script], { input: '{"jsonrpc":"2.0"}\n' });
  assert.equal(result.status, 0);
  assert.equal(result.stdout, '{"jsonrpc":"2.0"}\n');
  assert.equal(result.stderr, 'err-line');
});

test('a launcher that cannot find a binary says why on stderr and exits 1, with nothing on stdout', () => {
  const result = spawnSync(NODE, [BIN], {
    encoding: 'utf8',
    env: { PATH: process.env.PATH, TABLEX_MCP_BIN: path.join(os.tmpdir(), 'no-such-tablex-mcp-binary') },
  });
  assert.equal(result.status, 1);
  assert.equal(result.stdout, '');
  assert.match(result.stderr, /^tablex-mcp: TABLEX_MCP_BIN names .* does not exist/);
});

test('a binary that cannot be started is an error and exit 1', () => {
  const notExecutable = path.join(tmp(), process.platform === 'win32' ? 'x.exe' : 'x');
  fs.writeFileSync(notExecutable, 'not a program', { mode: 0o644 });
  const result = spawnSync(NODE, [BIN], {
    encoding: 'utf8',
    env: { PATH: process.env.PATH, TABLEX_MCP_BIN: notExecutable },
  });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /cannot start/);
});

const unix = process.platform === 'win32' ? { skip: 'signals are a Unix notion' } : {};

test('a binary killed by a signal ends the launcher with that signal', unix, () => {
  const result = launch(['-e', "process.kill(process.pid, 'SIGKILL')"]);
  assert.equal(result.signal, 'SIGKILL', `status ${result.status}`);
});

test('SIGTERM sent to the launcher reaches the binary, and its exit code comes back', unix, async () => {
  const script =
    "process.on('SIGTERM', () => { console.log('got-term'); process.exit(42); }); console.log('ready'); setInterval(() => {}, 1000);";
  const child = spawn(NODE, [BIN, '-e', script], {
    env: { PATH: process.env.PATH, TABLEX_MCP_BIN: NODE },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let out = '';
  const ready = new Promise((resolve) => {
    child.stdout.on('data', (chunk) => {
      out += chunk;
      if (out.includes('ready')) resolve();
    });
  });
  const closed = new Promise((resolve) => child.on('close', (code, signal) => resolve({ code, signal })));
  await ready;
  child.kill('SIGTERM');
  const { code, signal } = await closed;
  assert.equal(signal, null);
  assert.equal(code, 42);
  assert.match(out, /got-term/);
});

test('the package describes exactly the platforms the launcher knows', () => {
  const names = launcher.PLATFORMS.map((entry) => entry.package).sort();
  assert.deepEqual(Object.keys(pkg.optionalDependencies).sort(), names);
  for (const version of Object.values(pkg.optionalDependencies)) assert.equal(version, pkg.version);
  assert.equal(pkg.bin['tablex-mcp'], 'bin/tablex-mcp.js');
  assert.equal(pkg.dependencies, undefined, 'no runtime dependencies');
  for (const entry of launcher.PLATFORMS) {
    assert.ok(['darwin', 'linux', 'win32'].includes(entry.os));
    assert.ok(['arm64', 'x64'].includes(entry.cpu));
    assert.equal(entry.binary.endsWith('.exe'), entry.os === 'win32');
    assert.equal(entry.libc !== undefined, entry.os === 'linux');
  }
});

test('the launcher loads nothing but Node\'s own modules', () => {
  const source = fs.readFileSync(path.join(__dirname, '..', 'lib', 'launcher.js'), 'utf8');
  const required = [...source.matchAll(/require\('([^']+)'\)/g)].map((match) => match[1]);
  for (const name of required) assert.ok(name.startsWith('node:') || name.startsWith('../'), name);
});
