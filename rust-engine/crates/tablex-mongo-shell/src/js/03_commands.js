// Running commands: the one place a script reaches the server.

function numOf(x) {
  if (typeof x === 'number') return x;
  if (x !== null && typeof x === 'object') {
    const v = x.$numberInt !== undefined ? x.$numberInt
      : x.$numberLong !== undefined ? x.$numberLong
      : x.$numberDouble !== undefined ? x.$numberDouble
      : x.$numberDecimal;
    return Number(v);
  }
  return Number(x);
}

function hostFailure(failure) {
  switch (failure.kind) {
    case 'cancelled': return new ShellError('cancelled', 'operation cancelled');
    case 'timeout': return new ShellError('timeout', failure.message);
    case 'read_only': return new ShellError('read_only', failure.message);
    default: return new ShellError('server', failure.message);
  }
}

function checkReply(raw) {
  if (!(numOf(raw.ok) >= 1)) {
    throw new MongoServerError(
      raw.errmsg !== undefined ? String(raw.errmsg) : 'the command failed',
      raw.code !== undefined ? numOf(raw.code) : undefined,
      raw.codeName,
      raw.errorLabels ? { errorLabels: raw.errorLabels } : undefined
    );
  }
  const writeErrors = raw.writeErrors;
  if (Array.isArray(writeErrors) && writeErrors.length > 0) {
    const first = writeErrors[0];
    throw new MongoServerError(String(first.errmsg), numOf(first.code), first.codeName, {
      writeErrors: revive(writeErrors),
    });
  }
  if (raw.writeConcernError) {
    const e = raw.writeConcernError;
    throw new MongoServerError(String(e.errmsg), numOf(e.code), e.codeName, { writeConcernError: revive(e) });
  }
}

/** Runs one command and returns the parsed reply with its wrappers intact. */
function execute(database, command) {
  const result = host.run(database, serialize(command));
  if (typeof result !== 'string') throw hostFailure(result);
  const raw = JSON.parse(result);
  checkReply(raw);
  raw[TEXT] = result;
  return raw;
}

function compact(object) {
  const out = {};
  for (const key of Object.keys(object)) if (object[key] !== undefined) out[key] = object[key];
  return out;
}

function oneKey(value, what) {
  if (value === null || typeof value !== 'object') {
    throw new TypeError(`${what} must be an object`);
  }
  if (value[NOSER]) {
    throw new ShellError('script', `cannot use ${value[NOSER]} as ${what}${value[NOSER] === 'a Cursor' ? '; call toArray() first' : ''}`);
  }
  return value;
}

/** Hands a cursor reply's id (`{$numberLong}`) back as a BigInt. */
function cursorId(raw) {
  return raw === undefined ? 0n : BigInt(raw.$numberLong !== undefined ? raw.$numberLong : raw);
}

/** A write's result object: printed as text, not shown as a document. */
class WriteResult {
  constructor(fields) {
    Object.assign(this, fields);
  }
}
const RESULT = Symbol('result');
WriteResult.prototype[RESULT] = true;

function result(fields) {
  return new WriteResult(fields);
}

const openCursors = new Set();
