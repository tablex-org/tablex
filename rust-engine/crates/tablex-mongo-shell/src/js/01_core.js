// Shell core: errors, hidden-state symbols, small encoders.
// All prelude files are concatenated into one strict-mode function body that
// receives the native `host` object and returns the API the engine drives.
'use strict';

const NOSER = Symbol('noser');
const EJ = Symbol('ejson');
const STATE = Symbol('state');
const TEXT = Symbol('text');

// Builtins the shell's own bookkeeping relies on, captured before any script
// runs: a script that replaces `Object.getOwnPropertyDescriptor` or
// `Reflect.get` must not be able to run its code inside the shell's
// uninterruptible steps.
const rapply = Reflect.apply;
const reflectGet = Reflect.get;
const getOwnDescriptor = Object.getOwnPropertyDescriptor;
const defineProperty = Object.defineProperty;
const freeze = Object.freeze;
const ProxyClass = Proxy;
const weakHas = WeakSet.prototype.has;
const weakAdd = WeakSet.prototype.add;
const knownDatabases = new WeakSet();

/** A failure the shell reports with a kind the engine maps to a typed error. */
class ShellError extends Error {
  constructor(kind, message, extra) {
    super(message);
    this.name = 'ShellError';
    this.kind = kind;
    if (extra) Object.assign(this, extra);
  }
}

class MongoServerError extends ShellError {
  constructor(message, code, codeName, extra) {
    super('server', message, extra);
    this.name = 'MongoServerError';
    this.code = code;
    this.codeName = codeName;
  }
}

function unsupported(what, hint) {
  return new ShellError(
    'unsupported',
    `${what} is not available in the TableX MongoDB shell` + (hint ? `; ${hint}` : '')
  );
}

const HEX = '0123456789abcdef';

function bytesToHex(bytes) {
  let out = '';
  for (let i = 0; i < bytes.length; i++) out += HEX[bytes[i] >> 4] + HEX[bytes[i] & 15];
  return out;
}

function hexToBytes(hex) {
  if (typeof hex !== 'string' || hex.length % 2 !== 0 || !/^[0-9a-fA-F]*$/.test(hex)) {
    throw new TypeError('invalid hex string');
  }
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < bytes.length; i++) bytes[i] = parseInt(hex.substr(i * 2, 2), 16);
  return bytes;
}

const B64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';

function bytesToBase64(bytes) {
  let out = '';
  for (let i = 0; i < bytes.length; i += 3) {
    const a = bytes[i];
    const b = i + 1 < bytes.length ? bytes[i + 1] : 0;
    const c = i + 2 < bytes.length ? bytes[i + 2] : 0;
    out += B64[a >> 2] + B64[((a & 3) << 4) | (b >> 4)];
    out += i + 1 < bytes.length ? B64[((b & 15) << 2) | (c >> 6)] : '=';
    out += i + 2 < bytes.length ? B64[c & 63] : '=';
  }
  return out;
}

function base64ToBytes(text) {
  const clean = String(text).replace(/\s+/g, '');
  if (clean.length % 4 !== 0 || !/^[A-Za-z0-9+/]*={0,2}$/.test(clean)) {
    throw new TypeError('invalid base64 string');
  }
  const pad = clean.endsWith('==') ? 2 : clean.endsWith('=') ? 1 : 0;
  const bytes = new Uint8Array((clean.length / 4) * 3 - pad);
  let o = 0;
  for (let i = 0; i < clean.length; i += 4) {
    const n =
      (B64.indexOf(clean[i]) << 18) |
      (B64.indexOf(clean[i + 1]) << 12) |
      ((clean[i + 2] === '=' ? 0 : B64.indexOf(clean[i + 2])) << 6) |
      (clean[i + 3] === '=' ? 0 : B64.indexOf(clean[i + 3]));
    if (o < bytes.length) bytes[o++] = (n >> 16) & 255;
    if (o < bytes.length) bytes[o++] = (n >> 8) & 255;
    if (o < bytes.length) bytes[o++] = n & 255;
  }
  return bytes;
}

/** A class that is also callable without `new`, as the shell's helpers are. */
function callable(cls) {
  return new Proxy(cls, {
    apply(target, _this, args) {
      return new target(...args);
    },
  });
}

function defineGlobal(name, value) {
  Object.defineProperty(globalThis, name, {
    value,
    writable: true,
    configurable: true,
    enumerable: false,
  });
}

function isPlainObject(value) {
  if (value === null || typeof value !== 'object') return false;
  const proto = Object.getPrototypeOf(value);
  return proto === Object.prototype || proto === null;
}
