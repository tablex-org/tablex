// The database object and the shell's global surface.

class Database {
  constructor(name) {
    if (typeof name !== 'string' || name.length === 0) throw new TypeError('a database name must be a non-empty string');
    // Neither the slot nor the state object can be redefined (a script can
    // find the symbol with Object.getOwnPropertySymbols): the shell reads the
    // name while the engine is not interruptible.
    defineProperty(this, STATE, {
      value: freeze({ name, collections: new Map() }),
      writable: false,
      configurable: false,
      enumerable: false,
    });
  }
  get [NOSER]() { return 'a Database'; }

  _run(command) { return execute(this[STATE].name, command); }
  getName() { return this[STATE].name; }
  toString() { return this[STATE].name; }
  getSiblingDB(name) { return databaseProxy(new Database(String(name))); }
  getMongo() { throw unsupported('getMongo()'); }
  getCollection(name) {
    const st = this[STATE];
    name = String(name);
    let collection = st.collections.get(name);
    if (!collection) {
      collection = collectionProxy(new Collection(this._proxy || this, name));
      st.collections.set(name, collection);
    }
    return collection;
  }
  runCommand(command, options) {
    const body = typeof command === 'string' ? { [command]: 1 } : oneKey(command, 'the command');
    if (Object.keys(body).length === 0) throw new TypeError('the command document is empty');
    return revive(this._run(body));
  }
  adminCommand(command) {
    const body = typeof command === 'string' ? { [command]: 1 } : oneKey(command, 'the command');
    return revive(execute('admin', body));
  }
  getCollectionInfos(filter, options) {
    return this.listCollections(filter, options).toArray();
  }
  listCollections(filter, options) {
    return new CommandCursor(this, (opts, batch) =>
      compact({
        listCollections: 1,
        filter: filter === undefined ? {} : filter,
        nameOnly: options && options.nameOnly ? true : undefined,
        authorizedCollections: options && options.authorizedCollections ? true : undefined,
        cursor: batch ? { batchSize: batch } : {},
      })
    );
  }
  getCollectionNames() {
    return this.listCollections({}, { nameOnly: true }).toArray().map((info) => info.name).sort();
  }
  createCollection(name, options) {
    this._run({ create: String(name), ...(options || {}) });
    return { ok: 1 };
  }
  createView(name, source, pipeline, options) {
    this._run({ create: String(name), viewOn: String(source), pipeline, ...(options || {}) });
    return { ok: 1 };
  }
  dropDatabase() {
    this._run({ dropDatabase: 1 });
    return { ok: 1, dropped: this[STATE].name };
  }
  stats(scale) {
    return revive(this._run(compact({ dbStats: 1, scale: typeof scale === 'object' && scale ? scale.scale : scale })));
  }
  version() { return execute('admin', { buildInfo: 1 }).version; }
  serverBuildInfo() { return revive(execute('admin', { buildInfo: 1 })); }
  serverStatus(options) { return revive(execute('admin', { serverStatus: 1, ...(options || {}) })); }
  hostInfo() { return revive(execute('admin', { hostInfo: 1 })); }
  hello() { return revive(execute('admin', { hello: 1 })); }
  isMaster() { return revive(execute('admin', { isMaster: 1 })); }
  currentOp(options) { return revive(execute('admin', { currentOp: 1, ...(options || {}) })); }
  killOp(opid) { return revive(execute('admin', { killOp: 1, op: opid })); }
  getProfilingStatus() { return revive(this._run({ profile: -1 })); }
  getUsers(options) { return revive(this._run({ usersInfo: 1, ...(options || {}) })).users; }
  getRoles(options) { return revive(this._run({ rolesInfo: 1, ...(options || {}) })).roles; }
  aggregate(pipeline, options) {
    if (!Array.isArray(pipeline)) throw new TypeError('aggregate() needs an array of stages');
    return aggregateOn(this, 1, pipeline, options);
  }
  watch() { throw unsupported('watch()', 'change streams need a long-lived connection'); }
}

function databaseProxy(database) {
  const proxy = new ProxyClass(database, {
    get(target, prop, receiver) {
      if (typeof prop === 'symbol' || prop in target) return reflectGet(target, prop, receiver);
      return target.getCollection(prop);
    },
  });
  rapply(weakAdd, knownDatabases, [proxy]);
  database._proxy = proxy;
  defineProperty(database, '_proxy', { enumerable: false });
  return proxy;
}

// ---- shell state --------------------------------------------------------------

let currentDb = null;

function setDatabase(name) {
  currentDb = databaseProxy(new Database(name));
  defineGlobal('db', currentDb);
  return currentDb;
}

// ---- printing -------------------------------------------------------------------

function textOf(value, pretty) {
  if (typeof value === 'string') return value;
  if (value === undefined) return 'undefined';
  if (value === null) return 'null';
  if (typeof value === 'number') return Object.is(value, -0) ? '-0' : String(value);
  if (typeof value === 'function') return `[Function${value.name ? ': ' + value.name : ''}]`;
  if (typeof value === 'symbol') return value.toString();
  if (typeof value === 'object' && value[NOSER]) return `[${value[NOSER].replace(/^an? /, '')}]`;
  return host.shellText(serialize(value), !!pretty);
}

function printLine(text) { host.print(text); }

function shellPrint() {
  printLine(Array.prototype.map.call(arguments, (v) => textOf(v, false)).join(' '));
}

function printjson() {
  printLine(Array.prototype.map.call(arguments, (v) => textOf(v, true)).join(' '));
}

function tojson(value, indent, nolint) {
  return textOf(value, !nolint);
}

function sleep(ms) {
  const n = Number(ms);
  if (!Number.isFinite(n) || n < 0) throw new RangeError('sleep needs a non-negative number of milliseconds');
  if (!host.sleep(n)) throw new ShellError('cancelled', 'operation cancelled');
}

const shellConsole = {
  log: shellPrint, info: shellPrint, debug: shellPrint, warn: shellPrint, error: shellPrint,
  table: shellPrint, dir: shellPrint,
};

// ---- line commands (`use`, `show`, `it`) -------------------------------------------

function tableText(rows) {
  const width = rows.reduce((w, r) => Math.max(w, r[0].length), 0);
  return rows.map((r) => r[0].padEnd(width) + '  ' + r[1]).join('\n');
}

function formatBytes(n) {
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let v = n;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) { v /= 1024; u++; }
  return u === 0 ? `${v} B` : `${v.toFixed(2)} ${units[u]}`;
}

const shellCommands = {
  use(name) {
    setDatabase(name);
    printLine(`switched to db ${name}`);
  },
  show(what, isLast) {
    const kind = String(what).toLowerCase();
    let rows;
    let lines;
    switch (kind) {
      case 'dbs':
      case 'databases': {
        const raw = revive(execute('admin', { listDatabases: 1 }));
        const dbs = raw.databases.slice().sort((a, b) => (a.name < b.name ? -1 : 1));
        rows = dbs.map((d) => ({ name: d.name, sizeOnDisk: d.sizeOnDisk, empty: d.empty }));
        lines = tableText(dbs.map((d) => [d.name, formatBytes(Number(d.sizeOnDisk))]));
        break;
      }
      case 'collections':
      case 'tables': {
        const infos = currentDb.getCollectionInfos().sort((a, b) => (a.name < b.name ? -1 : 1));
        rows = infos.map((i) => ({ name: i.name, type: i.type }));
        lines = infos.map((i) => i.name).join('\n');
        break;
      }
      case 'users':
        rows = currentDb.getUsers();
        lines = rows.map((u) => textOf(u, true)).join('\n');
        break;
      case 'roles':
        rows = currentDb.getRoles({ showBuiltinRoles: false });
        lines = rows.map((r) => textOf(r, true)).join('\n');
        break;
      case 'profile':
        rows = currentDb.getCollection('system.profile').find().sort({ ts: -1 }).limit(5).toArray();
        lines = rows.map((r) => textOf(r, true)).join('\n');
        break;
      default:
        throw new ShellError('script', `Unknown shell command: show ${what}`);
    }
    if (isLast) return new ShowResult(rows);
    if (lines.length > 0) printLine(lines);
    return undefined;
  },
  it(isLast, request) {
    const ref = request.cursor;
    if (!ref) {
      printLine('no cursor');
      return undefined;
    }
    const db = databaseProxy(new Database(ref.database));
    const cursor = new ResumedCursor(db, ref.collection, BigInt(ref.id), ref.batch_size);
    if (isLast) return cursor;
    cursor.forEach((doc) => printjson(doc));
    return undefined;
  },
};

class ShowResult {
  constructor(rows) { this.rows = rows; }
}

// ---- unsupported surface -----------------------------------------------------------

function installUnsupported() {
  const refuse = (name, hint) => defineGlobal(name, function () { throw unsupported(`${name}()`, hint); });
  refuse('load', 'scripts cannot read files');
  refuse('require', 'modules cannot be loaded');
  refuse('Mongo', 'the shell only talks to the connection it was opened on');
  refuse('connect', 'the shell only talks to the connection it was opened on');
  for (const name of ['exit', 'quit']) {
    // `exit` alone (no call) ends a mongosh session too, so a bare mention must fail.
    Object.defineProperty(globalThis, name, {
      configurable: true,
      get() { throw unsupported(`${name}`, 'a script ends when its last statement does'); },
    });
  }
  for (const name of ['cat', 'ls', 'pwd', 'cd', 'run', 'runProgram', 'mkdir', 'removeFile', 'listFiles', 'hostname', 'getHostName', 'copyDbpath', 'resetDbpath', 'fetch', 'setTimeout', 'setInterval', 'XMLHttpRequest', 'WebSocket']) {
    refuse(name, 'file, process and network access are blocked');
  }
  for (const name of ['process', 'fs', 'os', 'child_process', 'net', 'http', 'https', 'path', 'Deno', 'Bun']) {
    defineGlobal(name, new Proxy({}, {
      get(_t, prop) { throw unsupported(`${name}${typeof prop === 'string' ? '.' + prop : ''}`, 'file, process and network access are blocked'); },
      set() { throw unsupported(name, 'file, process and network access are blocked'); },
    }));
  }
}

// ---- finishing a run ------------------------------------------------------------------

function isDocumentLike(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value) && !value[RESULT] &&
    !(value instanceof Date) && !(value instanceof RegExp) && value._bsontype === undefined &&
    !(value instanceof ShowResult) && !value[NOSER] && !ArrayBuffer.isView(value);
}

function documentsDescriptor(docs) {
  return `{"kind":"documents","data":{"documents":[${docs.map(serialize).join(',')}]}}`;
}

/** The run's last value as the descriptor JSON the engine reads. */
function finish(value, pageSize) {
  if (value instanceof Cursor) {
    const page = value._page(pageSize);
    return `{"kind":"cursor","data":{"documents":[${page.documents.join(',')}],"cursor":${JSON.stringify(page.cursor)}}}`;
  }
  if (value === undefined) return '{"kind":"none"}';
  if (value instanceof ShowResult) return documentsDescriptor(value.rows);
  if (isDocumentLike(value)) return documentsDescriptor([value]);
  if (Array.isArray(value) && value.every(isDocumentLike)) return documentsDescriptor(value);
  const text = typeof value === 'string' ? host.shellText(serialize(value), true) : textOf(value, true);
  return JSON.stringify({ kind: 'value', data: { text } });
}

/** Ends cursors the script left open (the one handed to the host stays). */
function releaseCursors(keep) {
  for (const cursor of Array.from(openCursors)) {
    if (cursor === keep) continue;
    try { cursor.close(); } catch (_) { /* best effort */ }
  }
}

function setup(request) {
  const globals = {
    ObjectId: callable(ObjectId), Long: callable(Long), NumberLong: callable(Long),
    NumberInt: callable(Int32), Int32: callable(Int32), Double: callable(Double),
    NumberDecimal: callable(Decimal128), Decimal128: callable(Decimal128),
    UUID: callable(UUID), Timestamp: callable(Timestamp), MinKey: callable(MinKey), MaxKey: callable(MaxKey),
    BSONRegExp: callable(BSONRegExp), DBRef: callable(DBRef), Code: callable(Code),
    BinData: (subType, base64) => new Binary(String(base64), Number(subType)),
    HexData: (subType, hex) => new Binary(bytesToBase64(hexToBytes(String(hex))), Number(subType)),
    Binary: callable(Binary),
    ISODate: isoDate,
    EJSON, print: shellPrint, printjson, tojson, sleep, console: shellConsole,
    __shell: { use: shellCommands.use, show: shellCommands.show, it: (isLast) => shellCommands.it(isLast, request) },
  };
  for (const name of Object.keys(globals)) defineGlobal(name, globals[name]);
  installUnsupported();
  setDatabase(request.database);
  return {
    finish,
    releaseCursors,
    // Runs while the engine is not interruptible, so it never calls anything a
    // script can replace: a plain data-property read, an identity lookup in a
    // private set, and a read of private state.
    currentDatabase() {
      const descriptor = rapply(getOwnDescriptor, Object, [globalThis, 'db']);
      const value = descriptor !== undefined && 'value' in descriptor ? descriptor.value : undefined;
      if (value !== undefined && rapply(weakHas, knownDatabases, [value])) return value[STATE].name;
      return currentDb[STATE].name;
    },
  };
}

return setup;
