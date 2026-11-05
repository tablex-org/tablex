// Cursors: lazy batches over find / aggregate / listCollections / listIndexes.

const DEFAULT_BATCH = 0;

class Cursor {
  constructor(db, open) {
    this[STATE] = {
      db,           // Database the cursor runs in
      open,         // () => cursor reply (raw)
      started: false,
      closed: false,
      batch: [],
      pos: 0,
      id: 0n,
      collection: '',
      batchSize: undefined,
      transforms: [],
      emptyBatches: 0,
    };
  }
  get [NOSER]() { return 'a Cursor'; }

  // -- iteration --
  _start() {
    const st = this[STATE];
    if (st.started) return;
    st.started = true;
    this._load(st.open.call(this), 'firstBatch');
  }
  _load(raw, key) {
    const st = this[STATE];
    const c = raw.cursor;
    st.batch = c[key] || [];
    st.text = raw[TEXT];
    st.textKey = key;
    st.pos = 0;
    st.id = cursorId(c.id);
    const ns = String(c.ns || '');
    const dot = ns.indexOf('.');
    if (dot >= 0) st.collection = ns.slice(dot + 1);
    if (st.id !== 0n) openCursors.add(this);
    else openCursors.delete(this);
  }
  _more() {
    const st = this[STATE];
    const command = { getMore: st.id, collection: st.collection };
    if (st.batchSize) command.batchSize = st.batchSize;
    this._load(execute(st.db[STATE].name, command), 'nextBatch');
  }
  hasNext() {
    const st = this[STATE];
    if (st.closed) return false;
    this._start();
    while (st.pos >= st.batch.length) {
      if (st.id === 0n) return false;
      this._more();
      if (st.pos >= st.batch.length && ++st.emptyBatches > 1000) return false;
    }
    st.emptyBatches = 0;
    return true;
  }
  _take() {
    const st = this[STATE];
    return st.batch[st.pos++];
  }
  _shape(raw) {
    let doc = revive(raw);
    for (const fn of this[STATE].transforms) doc = fn(doc);
    return doc;
  }
  next() {
    if (!this.hasNext()) throw new ShellError('script', 'Cursor is exhausted: there is no next document');
    return this._shape(this._take());
  }
  tryNext() {
    return this.hasNext() ? this._shape(this._take()) : null;
  }
  forEach(fn) {
    if (typeof fn !== 'function') throw new TypeError('forEach needs a function');
    while (this.hasNext()) fn(this.next());
  }
  map(fn) {
    if (typeof fn !== 'function') throw new TypeError('map needs a function');
    this[STATE].transforms.push(fn);
    return this;
  }
  toArray() {
    const out = [];
    let bytes = 0;
    while (this.hasNext()) {
      const raw = this._take();
      bytes += JSON.stringify(raw).length;
      if (out.length >= host.limits.maxDocuments || bytes > host.limits.maxBytes) {
        throw new ShellError(
          'limit',
          `toArray() would hold more than ${host.limits.maxDocuments} documents or ${host.limits.maxBytes >> 20} MiB; ` +
            'use forEach(), limit() or a $match / $limit stage'
        );
      }
      out.push(this._shape(raw));
    }
    return out;
  }
  itcount() {
    let n = 0;
    while (this.hasNext()) { this._take(); n++; }
    return n;
  }
  objsLeftInBatch() {
    const st = this[STATE];
    return st.started ? st.batch.length - st.pos : 0;
  }
  isClosed() { return this[STATE].closed || (this[STATE].started && this[STATE].id === 0n && this.objsLeftInBatch() === 0); }
  isExhausted() { return this.isClosed(); }
  close() {
    const st = this[STATE];
    if (st.id !== 0n) {
      try {
        execute(st.db[STATE].name, { killCursors: st.collection, cursors: [st.id] });
      } catch (error) {
        if (error && (error.kind === 'cancelled' || error.kind === 'timeout')) throw error;
      }
    }
    st.id = 0n;
    st.closed = true;
    openCursors.delete(this);
  }
  pretty() { return this; }
  batchSize(n) { return this._set('batchSize', n); }
  _set(key, value) {
    const st = this[STATE];
    if (st.started) throw new ShellError('script', 'Cursor is already started: set options before iterating');
    st[key] = value;
    return this;
  }
  [Symbol.iterator]() {
    const cursor = this;
    return {
      next() { return cursor.hasNext() ? { value: cursor.next(), done: false } : { value: undefined, done: true }; },
      return() { cursor.close(); return { value: undefined, done: true }; },
      [Symbol.iterator]() { return this; },
    };
  }

  /** The first page for the host: raw documents + where to continue. */
  _page(size) {
    const st = this[STATE];
    if (!st.started && !st.batchSize) st.batchSize = size;
    this._start();
    if (st.pos >= st.batch.length && st.id !== 0n) this._more();
    const start = st.pos;
    const raw = st.batch.slice(start);
    st.pos = st.batch.length;
    // The server's own text for each document keeps its key order (a JS object
    // would move integer-like keys first); only mapped documents are rebuilt.
    const documents = st.transforms.length > 0
      ? raw.map((doc) => serializeText(this._shape(doc)))
      : st.text !== undefined
        ? host.batchDocuments(st.text, st.textKey).slice(start)
        : raw.map((doc) => JSON.stringify(doc));
    return {
      documents,
      cursor: st.id === 0n ? null : { id: st.id.toString(), database: st.db[STATE].name, collection: st.collection, batch_size: size },
    };
  }
}

function serializeText(value) { return serialize(value); }

/** `find` cursor with the mongosh modifiers. */
class FindCursor extends Cursor {
  constructor(collection, filter, projection, options) {
    super(collection[STATE].db, null);
    const find = {
      filter: filter === undefined || filter === null ? {} : filter,
      projection: projection === undefined || projection === null ? undefined : projection,
    };
    if (options && typeof options === 'object') {
      if (options.noCursorTimeout) throw unsupported('the noCursorTimeout option', 'a script run keeps no long-lived cursors');
      for (const key of Object.keys(options)) find[key === 'projection' ? 'projection' : key] = options[key];
    }
    this[STATE].find = find;
    this[STATE].collectionName = collection[STATE].name;
    this[STATE].open = this._openFind;
  }
  _command() {
    const st = this[STATE];
    const f = st.find;
    const command = { find: st.collectionName, filter: f.filter };
    const pairs = {
      sort: f.sort, projection: f.projection, hint: f.hint, skip: f.skip, collation: f.collation,
      comment: f.comment, maxTimeMS: f.maxTimeMS, min: f.min, max: f.max, returnKey: f.returnKey,
      showRecordId: f.showRecordId, allowDiskUse: f.allowDiskUse, noCursorTimeout: f.noCursorTimeout,
      let: f.let, readConcern: f.readConcern,
    };
    Object.assign(command, compact(pairs));
    if (f.limit !== undefined && f.limit !== 0) {
      command.limit = Math.abs(f.limit);
      if (f.limit < 0) command.singleBatch = true;
    }
    if (st.batchSize) command.batchSize = st.batchSize;
    return command;
  }
  _openFind() {
    return execute(this[STATE].db[STATE].name, this._command());
  }
  _find(key, value) {
    if (this[STATE].started) throw new ShellError('script', 'Cursor is already started: set options before iterating');
    this[STATE].find[key] = value;
    return this;
  }
  sort(spec, direction) {
    if (typeof spec === 'string') spec = { [spec]: direction === undefined ? 1 : direction };
    else if (Array.isArray(spec)) spec = Object.fromEntries(spec.map((p) => (Array.isArray(p) ? p : [p, 1])));
    return this._find('sort', spec);
  }
  limit(n) { return this._find('limit', Number(n)); }
  skip(n) { return this._find('skip', Number(n)); }
  project(spec) { return this._find('projection', spec); }
  projection(spec) { return this._find('projection', spec); }
  hint(spec) { return this._find('hint', spec); }
  collation(spec) { return this._find('collation', spec); }
  comment(text) { return this._find('comment', text); }
  maxTimeMS(ms) { return this._find('maxTimeMS', Number(ms)); }
  min(spec) { return this._find('min', spec); }
  max(spec) { return this._find('max', spec); }
  returnKey(flag) { return this._find('returnKey', flag === undefined ? true : !!flag); }
  showRecordId() { return this._find('showRecordId', true); }
  allowDiskUse(flag) { return this._find('allowDiskUse', flag === undefined ? true : !!flag); }
  noCursorTimeout() { throw unsupported('noCursorTimeout()', 'a script run keeps no long-lived cursors'); }
  readPref() { return this; }
  readConcern(level) { return this._find('readConcern', { level }); }
  addOption() { return this; }
  _countCommand(applySkipLimit) {
    const st = this[STATE];
    const f = st.find;
    const command = { count: st.collectionName, query: f.filter };
    if (applySkipLimit) {
      if (f.skip) command.skip = f.skip;
      if (f.limit) command.limit = Math.abs(f.limit);
    }
    Object.assign(command, compact({ hint: f.hint, collation: f.collation, maxTimeMS: f.maxTimeMS }));
    return command;
  }
  count(applySkipLimit) {
    return numOf(execute(this[STATE].db[STATE].name, this._countCommand(!!applySkipLimit)).n);
  }
  size() {
    return numOf(execute(this[STATE].db[STATE].name, this._countCommand(true)).n);
  }
  explain(verbosity) {
    const command = { explain: this._command(), verbosity: verbosity === undefined ? 'queryPlanner' : String(verbosity) };
    return revive(execute(this[STATE].db[STATE].name, command));
  }
}

/** A cursor over a command that answers with a cursor reply. */
class CommandCursor extends Cursor {
  constructor(db, build) {
    super(db, null);
    this[STATE].build = build;
    this[STATE].options = {};
    this[STATE].open = this._openCommand;
  }
  _openCommand() {
    const st = this[STATE];
    const command = st.build(st.options, st.batchSize);
    return execute(st.db[STATE].name, command);
  }
  _opt(key, value) {
    if (this[STATE].started) throw new ShellError('script', 'Cursor is already started: set options before iterating');
    this[STATE].options[key] = value;
    return this;
  }
  maxTimeMS(ms) { return this._opt('maxTimeMS', Number(ms)); }
  hint(spec) { return this._opt('hint', spec); }
  collation(spec) { return this._opt('collation', spec); }
  comment(text) { return this._opt('comment', text); }
  allowDiskUse(flag) { return this._opt('allowDiskUse', flag === undefined ? true : !!flag); }
  explain(verbosity) {
    const st = this[STATE];
    const command = st.build(st.options, undefined);
    command.explain = true;
    if (verbosity !== undefined) command.explain = true;
    return revive(execute(st.db[STATE].name, command));
  }
}

/** A cursor that continues one the previous run left open (`it`). */
class ResumedCursor extends Cursor {
  constructor(db, collection, id, batchSize) {
    super(db, null);
    const st = this[STATE];
    st.id = id;
    st.collection = collection;
    st.batchSize = batchSize;
    st.started = true;
    st.batch = [];
    st.pos = 0;
    openCursors.add(this);
  }
}
