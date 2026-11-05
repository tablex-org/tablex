// BSON value classes (the shell's helpers) and Extended JSON.

const PROCESS_BYTES = host.random(5);
let objectIdCounter = parseInt(host.random(3), 16);

function newObjectIdHex() {
  const seconds = Math.floor(Date.now() / 1000) >>> 0;
  objectIdCounter = (objectIdCounter + 1) & 0xffffff;
  return (
    ('00000000' + seconds.toString(16)).slice(-8) +
    PROCESS_BYTES +
    ('000000' + objectIdCounter.toString(16)).slice(-6)
  );
}

class ObjectId {
  constructor(id) {
    let hex;
    if (id === undefined || id === null) hex = newObjectIdHex();
    else if (id instanceof ObjectId) hex = id[STATE];
    else if (typeof id === 'string' && /^[0-9a-fA-F]{24}$/.test(id)) hex = id.toLowerCase();
    else if (typeof id === 'number' && Number.isInteger(id)) {
      hex = ('00000000' + (id >>> 0).toString(16)).slice(-8) + '0000000000000000';
    } else {
      throw new TypeError('Argument passed in must be a string of 24 hex characters');
    }
    this[STATE] = hex;
  }
  get _bsontype() { return 'ObjectId'; }
  toString() { return this[STATE]; }
  toHexString() { return this[STATE]; }
  toJSON() { return this[STATE]; }
  getTimestamp() { return new Date(parseInt(this[STATE].slice(0, 8), 16) * 1000); }
  equals(other) {
    return other instanceof ObjectId
      ? other[STATE] === this[STATE]
      : typeof other === 'string' && other.toLowerCase() === this[STATE];
  }
  static createFromHexString(hex) { return new ObjectId(String(hex)); }
  static createFromTime(seconds) { return new ObjectId(Number(seconds)); }
  static isValid(id) { try { new ObjectId(id); return true; } catch (_) { return false; } }
  [EJ]() { return `{"$oid":"${this[STATE]}"}`; }
}

const INT64_MIN = -(2n ** 63n);
const INT64_MAX = 2n ** 63n - 1n;

function toInt64(value) {
  let big;
  if (typeof value === 'bigint') big = value;
  else if (value instanceof Long) return value[STATE];
  else if (typeof value === 'number') {
    if (!Number.isFinite(value) || !Number.isInteger(value)) {
      throw new TypeError(`${value} is not an integer`);
    }
    big = BigInt(value);
  } else if (typeof value === 'string' && /^\s*[+-]?\d+\s*$/.test(value)) big = BigInt(value.trim());
  else throw new TypeError('Argument must be a number, a numeric string, a bigint or a Long');
  if (big < INT64_MIN || big > INT64_MAX) throw new RangeError(`${big} does not fit in a 64-bit integer`);
  return big;
}

class Long {
  constructor(low, high) {
    if (high === undefined) this[STATE] = toInt64(low);
    else {
      this[STATE] = BigInt.asIntN(64, (BigInt(high | 0) << 32n) | BigInt(low >>> 0));
    }
  }
  get _bsontype() { return 'Long'; }
  get low() { return Number(BigInt.asIntN(32, this[STATE])); }
  get high() { return Number(BigInt.asIntN(32, this[STATE] >> 32n)); }
  toString(radix) { return this[STATE].toString(radix); }
  toNumber() { return Number(this[STATE]); }
  toBigInt() { return this[STATE]; }
  valueOf() { return Number(this[STATE]); }
  toJSON() { return this[STATE].toString(); }
  equals(other) { return toInt64(other) === this[STATE]; }
  compare(other) { const o = toInt64(other); return this[STATE] < o ? -1 : this[STATE] > o ? 1 : 0; }
  isZero() { return this[STATE] === 0n; }
  isNegative() { return this[STATE] < 0n; }
  add(o) { return new Long(BigInt.asIntN(64, this[STATE] + toInt64(o))); }
  subtract(o) { return new Long(BigInt.asIntN(64, this[STATE] - toInt64(o))); }
  multiply(o) { return new Long(BigInt.asIntN(64, this[STATE] * toInt64(o))); }
  divide(o) {
    const d = toInt64(o);
    if (d === 0n) throw new RangeError('division by zero');
    return new Long(BigInt.asIntN(64, this[STATE] / d));
  }
  static fromNumber(n) { return new Long(toInt64(n)); }
  static fromString(s) { return new Long(toInt64(String(s))); }
  static fromBigInt(b) { return new Long(toInt64(b)); }
  [EJ]() { return `{"$numberLong":"${this[STATE]}"}`; }
}

class Int32 {
  constructor(value) {
    const n = typeof value === 'string' ? Number(value) : Number(value);
    if (!Number.isInteger(n) || n < -2147483648 || n > 2147483647) {
      throw new RangeError(`${value} is not a 32-bit integer`);
    }
    this[STATE] = n;
  }
  get _bsontype() { return 'Int32'; }
  valueOf() { return this[STATE]; }
  toString() { return String(this[STATE]); }
  toJSON() { return this[STATE]; }
  [EJ]() { return `{"$numberInt":"${this[STATE]}"}`; }
}

class Double {
  constructor(value) {
    this[STATE] = Number(value);
  }
  get _bsontype() { return 'Double'; }
  valueOf() { return this[STATE]; }
  toString() { return String(this[STATE]); }
  toJSON() { return this[STATE]; }
  [EJ]() { return `{"$numberDouble":"${doubleText(this[STATE])}"}`; }
}

function doubleText(n) {
  if (Number.isNaN(n)) return 'NaN';
  if (n === Infinity) return 'Infinity';
  if (n === -Infinity) return '-Infinity';
  if (n === 0 && 1 / n < 0) return '-0.0';
  return String(n);
}

const DECIMAL_SHAPE = /^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$|^[+-]?(Infinity|Inf|NaN)$/;

class Decimal128 {
  constructor(value) {
    const text = String(value).trim();
    if (!DECIMAL_SHAPE.test(text)) throw new TypeError(`"${value}" is not a valid Decimal128 string`);
    this[STATE] = text;
  }
  get _bsontype() { return 'Decimal128'; }
  toString() { return this[STATE]; }
  toJSON() { return { $numberDecimal: this[STATE] }; }
  static fromString(text) { return new Decimal128(text); }
  [EJ]() { return `{"$numberDecimal":${JSON.stringify(this[STATE])}}`; }
}

class Binary {
  constructor(base64, subType) {
    this.sub_type = subType === undefined ? 0 : subType;
    if (!Number.isInteger(this.sub_type) || this.sub_type < 0 || this.sub_type > 255) {
      throw new RangeError('binary subtype must be an integer from 0 to 255');
    }
    if (typeof base64 !== 'string') {
      const bytes = base64 instanceof ArrayBuffer ? new Uint8Array(base64) : base64;
      base64 = bytesToBase64(bytes);
    }
    base64ToBytes(base64);
    Object.defineProperty(this, STATE, { value: base64, enumerable: false });
  }
  get _bsontype() { return 'Binary'; }
  base64() { return this[STATE]; }
  hex() { return bytesToHex(base64ToBytes(this[STATE])); }
  length() { return base64ToBytes(this[STATE]).length; }
  buffer() { return base64ToBytes(this[STATE]); }
  toString() { return `BinData(${this.sub_type},"${this[STATE]}")`; }
  toJSON() { return this[STATE]; }
  [EJ]() {
    const sub = ('0' + this.sub_type.toString(16)).slice(-2);
    return `{"$binary":{"base64":"${this[STATE]}","subType":"${sub}"}}`;
  }
}

function uuidHex(text) {
  const hex = String(text).replace(/[{}-]/g, '').toLowerCase();
  if (!/^[0-9a-f]{32}$/.test(hex)) throw new TypeError('Invalid UUID string: ' + text);
  return hex;
}

class UUID extends Binary {
  constructor(text) {
    let hex;
    if (text === undefined) {
      const bytes = hexToBytes(host.random(16));
      bytes[6] = (bytes[6] & 0x0f) | 0x40;
      bytes[8] = (bytes[8] & 0x3f) | 0x80;
      hex = bytesToHex(bytes);
    } else if (text instanceof Binary) hex = text.hex();
    else hex = uuidHex(text);
    super(bytesToBase64(hexToBytes(hex)), 4);
  }
  get _bsontype() { return 'UUID'; }
  toString(format) {
    const hex = this.hex();
    if (format === 'hex') return hex;
    return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
  }
  toJSON() { return this.toString(); }
}

class Timestamp {
  constructor(t, i) {
    if (t !== null && typeof t === 'object') { i = t.i; t = t.t; }
    if (i === undefined) { i = 0; }
    t = Number(t); i = Number(i);
    if (!Number.isInteger(t) || !Number.isInteger(i) || t < 0 || i < 0 || t > 0xffffffff || i > 0xffffffff) {
      throw new RangeError('Timestamp parts must be unsigned 32-bit integers');
    }
    this.t = t;
    this.i = i;
  }
  get _bsontype() { return 'Timestamp'; }
  toString() { return `Timestamp({ t: ${this.t}, i: ${this.i} })`; }
  toJSON() { return { $timestamp: { t: this.t, i: this.i } }; }
  [EJ]() { return `{"$timestamp":{"t":${this.t},"i":${this.i}}}`; }
}

class MinKey {
  get _bsontype() { return 'MinKey'; }
  toString() { return 'MinKey()'; }
  [EJ]() { return '{"$minKey":1}'; }
}

class MaxKey {
  get _bsontype() { return 'MaxKey'; }
  toString() { return 'MaxKey()'; }
  [EJ]() { return '{"$maxKey":1}'; }
}

class BSONRegExp {
  constructor(pattern, options) {
    this.pattern = String(pattern);
    this.options = String(options === undefined ? '' : options).split('').sort().join('');
  }
  get _bsontype() { return 'BSONRegExp'; }
  toString() { return `/${this.pattern}/${this.options}`; }
  [EJ]() {
    return `{"$regularExpression":{"pattern":${JSON.stringify(this.pattern)},"options":${JSON.stringify(this.options)}}}`;
  }
}

class DBRef {
  constructor(collection, oid, db, fields) {
    this.collection = collection;
    this.oid = oid;
    if (db !== undefined) this.db = db;
    if (fields) Object.assign(this, fields);
  }
  get _bsontype() { return 'DBRef'; }
  toString() { return `DBRef(${JSON.stringify(this.collection)}, ${String(this.oid)})`; }
  [EJ](depth) {
    let text = `{"$ref":${JSON.stringify(this.collection)},"$id":${serialize(this.oid, depth + 1)}`;
    if (this.db !== undefined) text += `,"$db":${JSON.stringify(this.db)}`;
    for (const key of Object.keys(this)) {
      if (key === 'collection' || key === 'oid' || key === 'db') continue;
      if (this[key] !== undefined) text += `,${JSON.stringify(key)}:${serialize(this[key], depth + 1)}`;
    }
    return text + '}';
  }
}

class Code {
  constructor(code, scope) {
    this.code = typeof code === 'function' ? code.toString() : String(code);
    this.scope = scope;
  }
  get _bsontype() { return 'Code'; }
  toString() { return this.code; }
  [EJ](depth) {
    let text = `{"$code":${JSON.stringify(this.code)}`;
    if (this.scope !== undefined && this.scope !== null) text += `,"$scope":${serialize(this.scope, depth + 1)}`;
    return text + '}';
  }
}

// ---- Date helpers -----------------------------------------------------------

const ISO_SHAPE =
  /^(\d{4})-(\d{2})-(\d{2})(?:[T ](\d{2}):(\d{2})(?::(\d{2})(?:\.(\d{1,3})\d*)?)?)?\s*(Z|[+-]\d{2}:?\d{2})?$/;

function isoDate(text) {
  if (text === undefined) return new Date();
  if (typeof text !== 'string') return new Date(text);
  const m = ISO_SHAPE.exec(text.trim());
  if (!m) throw new ShellError('script', `invalid ISODate "${text}": expected YYYY-MM-DD[THH:mm:ss[.sss]][Z|±hh:mm]`);
  let ms = Date.UTC(+m[1], +m[2] - 1, +m[3], +(m[4] || 0), +(m[5] || 0), +(m[6] || 0), m[7] ? +(m[7] + '00').slice(0, 3) : 0);
  if (m[8] && m[8] !== 'Z') {
    const sign = m[8][0] === '-' ? -1 : 1;
    const digits = m[8].slice(1).replace(':', '');
    ms -= sign * (+digits.slice(0, 2) * 60 + +digits.slice(2)) * 60000;
  }
  const date = new Date(ms);
  if (Number.isNaN(date.getTime())) throw new ShellError('script', `invalid ISODate "${text}"`);
  return date;
}

// ---- Serialization (JS value -> canonical Extended JSON text) ----------------

function serializeNumber(n) {
  // Every number is tagged: a bare JSON integer is read differently by the
  // parsers on the way (a negative one becomes an Int64 in some of them).
  if (Number.isInteger(n) && n >= -2147483648 && n <= 2147483647 && !(n === 0 && 1 / n < 0)) {
    return `{"$numberInt":"${n}"}`;
  }
  return `{"$numberDouble":"${doubleText(n)}"}`;
}

function serialize(value, depth) {
  depth = depth || 0;
  if (depth > 100) throw new ShellError('script', 'cannot convert the value to BSON: nested too deeply or circular');
  switch (typeof value) {
    case 'string': return JSON.stringify(value);
    case 'boolean': return value ? 'true' : 'false';
    case 'number': return serializeNumber(value);
    case 'bigint': return `{"$numberLong":"${toInt64(value)}"}`;
    case 'undefined': return 'null';
    case 'symbol': return 'null';
    case 'function': return `{"$code":${JSON.stringify(value.toString())}}`;
  }
  if (value === null) return 'null';
  if (typeof value[EJ] === 'function') return value[EJ](depth);
  if (value[NOSER]) {
    throw new ShellError('script', `cannot use ${value[NOSER]} as a value${value[NOSER] === 'a Cursor' ? '; call toArray() first' : ''}`);
  }
  if (value instanceof Date) {
    const ms = value.getTime();
    if (Number.isNaN(ms)) throw new ShellError('script', 'cannot convert an Invalid Date to BSON');
    return `{"$date":{"$numberLong":"${ms}"}}`;
  }
  if (value instanceof RegExp) {
    const options = value.flags.replace(/[^imsu]/g, '');
    return `{"$regularExpression":{"pattern":${JSON.stringify(value.source)},"options":"${options}"}}`;
  }
  if (Array.isArray(value)) {
    let out = '[';
    for (let i = 0; i < value.length; i++) out += (i ? ',' : '') + serialize(value[i], depth + 1);
    return out + ']';
  }
  if (ArrayBuffer.isView(value) && value instanceof Uint8Array) {
    return `{"$binary":{"base64":"${bytesToBase64(value)}","subType":"00"}}`;
  }
  let out = '{';
  let first = true;
  for (const key of Object.keys(value)) {
    const item = value[key];
    if (item === undefined || typeof item === 'symbol') continue;
    out += (first ? '' : ',') + JSON.stringify(key) + ':' + serialize(item, depth + 1);
    first = false;
  }
  return out + '}';
}

// ---- Revival (parsed canonical Extended JSON -> shell values) ----------------

function setOwn(target, key, value) {
  if (key === '__proto__') Object.defineProperty(target, key, { value, enumerable: true, writable: true, configurable: true });
  else target[key] = value;
}

function revive(value) {
  if (value === null || typeof value !== 'object') return value;
  if (Array.isArray(value)) {
    for (let i = 0; i < value.length; i++) value[i] = revive(value[i]);
    return value;
  }
  const keys = Object.keys(value);
  if (keys.length > 0 && keys[0].charCodeAt(0) === 36) {
    const wrapped = reviveWrapper(keys[0], value, keys);
    if (wrapped !== NO_WRAPPER) return wrapped;
  }
  for (let i = 0; i < keys.length; i++) setOwn(value, keys[i], revive(value[keys[i]]));
  return value;
}

const NO_WRAPPER = Symbol('no-wrapper');

function reviveDate(inner) {
  let ms;
  if (inner !== null && typeof inner === 'object') ms = Number(BigInt(inner.$numberLong));
  else if (typeof inner === 'string') ms = isoDate(inner).getTime();
  else ms = Number(inner);
  if (!Number.isFinite(ms) || Math.abs(ms) > 8.64e15) {
    throw new ShellError('script', 'a date in the result is outside the range JavaScript can represent');
  }
  return new Date(ms);
}

function reviveWrapper(first, value, keys) {
  const n = keys.length;
  switch (first) {
    case '$oid': return n === 1 ? new ObjectId(value.$oid) : NO_WRAPPER;
    case '$numberInt': return n === 1 ? Number(value.$numberInt) : NO_WRAPPER;
    case '$numberDouble': return n === 1 ? Number(value.$numberDouble) : NO_WRAPPER;
    case '$numberLong': return n === 1 ? new Long(BigInt(value.$numberLong)) : NO_WRAPPER;
    case '$numberDecimal': return n === 1 ? new Decimal128(value.$numberDecimal) : NO_WRAPPER;
    case '$date': return n === 1 ? reviveDate(value.$date) : NO_WRAPPER;
    case '$binary': {
      const inner = value.$binary;
      if (n !== 1 || inner === null || typeof inner !== 'object') return NO_WRAPPER;
      const sub = parseInt(inner.subType, 16);
      if (sub === 4 && base64ToBytes(inner.base64).length === 16) {
        return new UUID(bytesToHex(base64ToBytes(inner.base64)));
      }
      return new Binary(inner.base64, sub);
    }
    case '$uuid': return n === 1 ? new UUID(value.$uuid) : NO_WRAPPER;
    case '$timestamp': return n === 1 ? new Timestamp(value.$timestamp.t, value.$timestamp.i) : NO_WRAPPER;
    case '$regularExpression':
      return n === 1 ? new BSONRegExp(value.$regularExpression.pattern, value.$regularExpression.options) : NO_WRAPPER;
    case '$code':
      if (n === 1 || (n === 2 && keys[1] === '$scope')) {
        return new Code(value.$code, n === 2 ? revive(value.$scope) : undefined);
      }
      return NO_WRAPPER;
    case '$minKey': return n === 1 ? new MinKey() : NO_WRAPPER;
    case '$maxKey': return n === 1 ? new MaxKey() : NO_WRAPPER;
    case '$symbol': return n === 1 ? value.$symbol : NO_WRAPPER;
    case '$undefined': return n === 1 ? undefined : NO_WRAPPER;
    case '$ref':
      if (keys.length >= 2 && keys[1] === '$id') {
        const extra = {};
        for (let i = 2; i < keys.length; i++) if (keys[i] !== '$db') setOwn(extra, keys[i], revive(value[keys[i]]));
        return new DBRef(value.$ref, revive(value.$id), value.$db, extra);
      }
      return NO_WRAPPER;
    default: return NO_WRAPPER;
  }
}

// ---- Relaxed Extended JSON (EJSON.stringify default) -------------------------

function relax(value) {
  if (value === null || typeof value !== 'object') return value;
  if (Array.isArray(value)) return value.map(relax);
  const keys = Object.keys(value);
  if (keys.length === 1) {
    const k = keys[0];
    if (k === '$numberInt') return Number(value.$numberInt);
    if (k === '$numberDouble') {
      const n = Number(value.$numberDouble);
      return Number.isFinite(n) && !(n === 0 && /^-/.test(value.$numberDouble)) ? n : value;
    }
    if (k === '$numberLong') {
      const big = BigInt(value.$numberLong);
      return big >= -(2n ** 53n) && big <= 2n ** 53n ? Number(big) : value;
    }
    if (k === '$date') {
      const ms = Number(BigInt(value.$date.$numberLong));
      return ms >= 0 && ms <= 253402300799999 ? { $date: new Date(ms).toISOString() } : value;
    }
  }
  const out = {};
  for (const key of keys) setOwn(out, key, relax(value[key]));
  return out;
}

/** Plain JSON integers become `$numberInt` (what canonical Extended JSON says). */
function canonicalize(value) {
  if (typeof value === 'number') return { $numberInt: String(value) };
  if (value === null || typeof value !== 'object') return value;
  if (Array.isArray(value)) return value.map(canonicalize);
  const out = {};
  for (const key of Object.keys(value)) {
    const item = value[key];
    // Wrapper payloads (a `$numberLong` string, a `$timestamp` pair, ...) stay as they are.
    setOwn(out, key, key.charCodeAt(0) === 36 && key !== '$ref' && key !== '$id' && key !== '$db' && key !== '$code' && key !== '$scope' ? item : canonicalize(item));
  }
  return out;
}

const EJSON = {
  parse(text, options) {
    let parsed;
    try { parsed = JSON.parse(String(text)); } catch (error) {
      throw new ShellError('script', 'EJSON.parse: ' + error.message);
    }
    return revive(parsed);
  },
  stringify(value, replacer, space, options) {
    if (replacer !== null && typeof replacer === 'object' && !Array.isArray(replacer) && space === undefined) {
      options = replacer; replacer = undefined; space = undefined;
    }
    if (typeof space === 'object' && space !== null) { options = space; space = undefined; }
    const canonical = JSON.parse(serialize(value));
    const out = options && options.relaxed === false ? canonicalize(canonical) : relax(canonical);
    return JSON.stringify(out, typeof replacer === 'function' ? replacer : null, space);
  },
  serialize(value) { return canonicalize(JSON.parse(serialize(value))); },
  deserialize(value) { return revive(JSON.parse(JSON.stringify(value))); },
};
