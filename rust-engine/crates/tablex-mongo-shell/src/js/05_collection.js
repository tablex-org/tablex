// Collection methods.

const MAX_INSERT_BATCH_DOCS = 1000;
const MAX_INSERT_BATCH_BYTES = 8 * 1024 * 1024;

function hasOperators(update) {
  if (Array.isArray(update)) return true;
  const keys = Object.keys(oneKey(update, 'the update'));
  return keys.length > 0 && keys[0].startsWith('$');
}

function requireOperators(update, method) {
  if (!hasOperators(update)) {
    throw new ShellError('script', `${method}: the update document must contain atomic operators ($set, $inc, ...) or be an aggregation pipeline; use replaceOne() to replace a document`);
  }
}

function requirePlain(replacement, method) {
  const keys = Object.keys(oneKey(replacement, 'the replacement'));
  if (Array.isArray(replacement) || (keys.length > 0 && keys[0].startsWith('$'))) {
    throw new ShellError('script', `${method}: the replacement document must not contain atomic operators`);
  }
}

function indexName(keys) {
  return Object.keys(keys).map((k) => `${k}_${String(keys[k])}`).join('_');
}

class Collection {
  constructor(db, name) {
    this[STATE] = { db, name };
  }
  get [NOSER]() { return 'a Collection'; }

  _run(command) { return execute(this[STATE].db[STATE].name, command); }
  _name() { return this[STATE].name; }

  getName() { return this[STATE].name; }
  getFullName() { return `${this[STATE].db[STATE].name}.${this[STATE].name}`; }
  getDB() { return this[STATE].db; }
  getMongo() { throw unsupported('getMongo()'); }
  getCollection(name) { return this[STATE].db.getCollection(`${this[STATE].name}.${name}`); }
  toString() { return this.getFullName(); }

  // ---- reads ----
  find(filter, projection, options) { return new FindCursor(this, filter, projection, options); }
  findOne(filter, projection, options) {
    const cursor = new FindCursor(this, filter, projection, options);
    cursor.limit(-1);
    return cursor.tryNext();
  }
  aggregate(pipeline, options) {
    let stages = pipeline;
    if (!Array.isArray(pipeline)) {
      stages = Array.prototype.slice.call(arguments);
      options = undefined;
      if (stages.length > 0 && stages.every((s) => s !== null && typeof s === 'object') === false) {
        throw new TypeError('aggregate() needs an array of stages');
      }
    }
    return aggregateOn(this[STATE].db, this[STATE].name, stages, options);
  }
  countDocuments(filter, options) {
    const pipeline = [{ $match: filter === undefined ? {} : filter }];
    if (options && options.skip) pipeline.push({ $skip: options.skip });
    if (options && options.limit) pipeline.push({ $limit: options.limit });
    pipeline.push({ $group: { _id: 1, n: { $sum: 1 } } });
    const extra = options ? compact({ hint: options.hint, collation: options.collation, maxTimeMS: options.maxTimeMS, comment: options.comment }) : {};
    const raw = this._run({ aggregate: this[STATE].name, pipeline, cursor: {}, ...extra });
    const batch = raw.cursor.firstBatch;
    return batch.length === 0 ? 0 : numOf(batch[0].n);
  }
  estimatedDocumentCount(options) {
    return numOf(this._run({ count: this[STATE].name, ...(options ? compact({ maxTimeMS: options.maxTimeMS, comment: options.comment }) : {}) }).n);
  }
  count(query, options) {
    const command = { count: this[STATE].name, query: query === undefined ? {} : query };
    if (options) Object.assign(command, compact({ limit: options.limit, skip: options.skip, hint: options.hint, collation: options.collation, maxTimeMS: options.maxTimeMS }));
    return numOf(this._run(command).n);
  }
  distinct(field, query, options) {
    const command = { distinct: this[STATE].name, key: String(field), query: query === undefined ? {} : query };
    if (options) Object.assign(command, compact({ collation: options.collation, maxTimeMS: options.maxTimeMS, hint: options.hint }));
    return revive(this._run(command).values);
  }
  getIndexes() {
    return new CommandCursor(this[STATE].db, (opts, batch) => ({
      listIndexes: this[STATE].name,
      cursor: batch ? { batchSize: batch } : {},
    })).toArray();
  }
  stats(options) {
    const scale = typeof options === 'number' ? options : options && options.scale;
    return revive(this._run(compact({ collStats: this[STATE].name, scale })));
  }
  dataSize() { return numOf(this._run({ collStats: this[STATE].name }).size); }
  storageSize() { return numOf(this._run({ collStats: this[STATE].name }).storageSize); }
  totalIndexSize() { return numOf(this._run({ collStats: this[STATE].name }).totalIndexSize); }
  totalSize() {
    const raw = this._run({ collStats: this[STATE].name });
    return numOf(raw.storageSize) + numOf(raw.totalIndexSize);
  }
  explain(verbosity) { return new Explainable(this, verbosity === undefined ? 'queryPlanner' : String(verbosity)); }

  // ---- writes ----
  insertOne(document, options) {
    oneKey(document, 'the document');
    const doc = ensureId(document);
    const raw = this._run(compact({ insert: this[STATE].name, documents: [doc], ordered: true, bypassDocumentValidation: options && options.bypassDocumentValidation, comment: options && options.comment }));
    return result({ acknowledged: true, insertedId: doc._id });
  }
  insertMany(documents, options) {
    if (!Array.isArray(documents)) throw new TypeError('insertMany needs an array of documents');
    const ordered = !options || options.ordered !== false;
    const docs = documents.map((d) => ensureId(oneKey(d, 'a document')));
    const insertedIds = {};
    let index = 0;
    for (const batch of splitInserts(docs)) {
      this._run({ insert: this[STATE].name, documents: batch, ordered });
      for (const d of batch) insertedIds[index++] = d._id;
    }
    return result({ acknowledged: true, insertedIds });
  }
  insert(documents, options) {
    return Array.isArray(documents) ? this.insertMany(documents, options) : this.insertOne(documents, options);
  }

  _update(filter, update, options, multi, method) {
    const spec = compact({
      q: filter === undefined ? {} : filter,
      u: update,
      multi: multi || undefined,
      upsert: options && options.upsert ? true : undefined,
      arrayFilters: options && options.arrayFilters,
      collation: options && options.collation,
      hint: options && options.hint,
    });
    spec.multi = !!multi;
    const raw = this._run(compact({ update: this[STATE].name, updates: [spec], ordered: true, comment: options && options.comment, let: options && options.let }));
    const upserted = raw.upserted && raw.upserted[0];
    const out = {
      acknowledged: true,
      insertedId: upserted ? revive(upserted._id) : null,
      matchedCount: numOf(raw.n) - (upserted ? 1 : 0),
      modifiedCount: numOf(raw.nModified),
      upsertedCount: upserted ? 1 : 0,
    };
    return result(out);
  }
  updateOne(filter, update, options) { requireOperators(update, 'updateOne'); return this._update(filter, update, options, false); }
  updateMany(filter, update, options) { requireOperators(update, 'updateMany'); return this._update(filter, update, options, true); }
  replaceOne(filter, replacement, options) { requirePlain(replacement, 'replaceOne'); return this._update(filter, replacement, options, false); }
  update(filter, update, options) {
    const multi = !!(options && options.multi);
    if (multi) requireOperators(update, 'update');
    return this._update(filter, update, options, multi);
  }

  _delete(filter, options, many) {
    const spec = compact({ q: filter === undefined ? {} : filter, limit: many ? 0 : 1, collation: options && options.collation, hint: options && options.hint });
    const raw = this._run(compact({ delete: this[STATE].name, deletes: [spec], ordered: true, comment: options && options.comment, let: options && options.let }));
    return result({ acknowledged: true, deletedCount: numOf(raw.n) });
  }
  deleteOne(filter, options) { return this._delete(filter, options, false); }
  deleteMany(filter, options) { return this._delete(filter, options, true); }
  remove(filter, options) {
    const justOne = typeof options === 'boolean' ? options : !!(options && options.justOne);
    return this._delete(filter, typeof options === 'object' ? options : undefined, !justOne);
  }

  findOneAndUpdate(filter, update, options) {
    requireOperators(update, 'findOneAndUpdate');
    return this._modify(filter, { update }, options);
  }
  findOneAndReplace(filter, replacement, options) {
    requirePlain(replacement, 'findOneAndReplace');
    return this._modify(filter, { update: replacement }, options);
  }
  findOneAndDelete(filter, options) { return this._modify(filter, { remove: true }, options); }
  _modify(filter, extra, options) {
    const wantsNew = options && (options.returnDocument === 'after' || options.returnNewDocument === true);
    if (options && options.returnDocument !== undefined && options.returnDocument !== 'before' && options.returnDocument !== 'after') {
      throw new TypeError("returnDocument must be 'before' or 'after'");
    }
    const command = compact({
      findAndModify: this[STATE].name,
      query: filter === undefined ? {} : filter,
      sort: options && options.sort,
      fields: options && (options.projection || options.fields),
      ...extra,
      new: wantsNew ? true : undefined,
      upsert: options && options.upsert ? true : undefined,
      arrayFilters: options && options.arrayFilters,
      collation: options && options.collation,
      hint: options && options.hint,
      maxTimeMS: options && options.maxTimeMS,
      comment: options && options.comment,
      let: options && options.let,
    });
    const raw = this._run(command);
    return raw.value === null || raw.value === undefined ? null : revive(raw.value);
  }

  bulkWrite(operations, options) {
    if (!Array.isArray(operations) || operations.length === 0) {
      throw new TypeError('bulkWrite needs a non-empty array of operations');
    }
    const ordered = !options || options.ordered !== false;
    const name = this[STATE].name;
    const totals = { insertedCount: 0, matchedCount: 0, modifiedCount: 0, deletedCount: 0, upsertedCount: 0 };
    const insertedIds = {};
    const upsertedIds = {};
    let i = 0;
    const commandOf = (op) => {
      const name = Object.keys(op)[0];
      return name === 'insertOne' ? 'insert' : name.startsWith('delete') ? 'delete' : 'update';
    };
    while (i < operations.length) {
      const command = commandOf(operations[i]);
      let j = i;
      while (j < operations.length && commandOf(operations[j]) === command) j++;
      const group = operations.slice(i, j);
      if (command === 'insert') {
        const docs = group.map((o) => ensureId(oneKey(o.insertOne.document, 'insertOne.document')));
        let offset = i;
        for (const batch of splitInserts(docs)) {
          this._run({ insert: name, documents: batch, ordered });
          for (const d of batch) insertedIds[offset++] = d._id;
          totals.insertedCount += batch.length;
        }
      } else if (command === 'update') {
        const updates = group.map((op) => {
          const kind = Object.keys(op)[0];
          const o = op[kind];
          if (kind === 'replaceOne') requirePlain(o.replacement, 'bulkWrite replaceOne');
          else if (kind === 'updateOne' || kind === 'updateMany') requireOperators(o.update, `bulkWrite ${kind}`);
          else throw new ShellError('script', `bulkWrite: unknown operation "${kind}"`);
          return compact({
            q: o.filter === undefined ? {} : o.filter,
            u: kind === 'replaceOne' ? o.replacement : o.update,
            multi: kind === 'updateMany',
            upsert: o.upsert ? true : undefined,
            arrayFilters: o.arrayFilters, collation: o.collation, hint: o.hint,
          });
        });
        const raw = this._run({ update: name, updates, ordered });
        const upserted = raw.upserted || [];
        for (const u of upserted) upsertedIds[i + numOf(u.index)] = revive(u._id);
        totals.upsertedCount += upserted.length;
        totals.matchedCount += numOf(raw.n) - upserted.length;
        totals.modifiedCount += numOf(raw.nModified);
      } else {
        const deletes = group.map((op) => {
          const kind = Object.keys(op)[0];
          if (kind !== 'deleteOne' && kind !== 'deleteMany') throw new ShellError('script', `bulkWrite: unknown operation "${kind}"`);
          const o = op[kind];
          return compact({ q: o.filter === undefined ? {} : o.filter, limit: kind === 'deleteOne' ? 1 : 0, collation: o.collation, hint: o.hint });
        });
        totals.deletedCount += numOf(this._run({ delete: name, deletes, ordered }).n);
      }
      i = j;
    }
    return result({ acknowledged: true, ...totals, insertedIds, upsertedIds });
  }

  // ---- indexes and collection admin ----
  createIndex(keys, options) {
    const names = this.createIndexes([keys], options);
    return names[0];
  }
  createIndexes(specs, options) {
    if (!Array.isArray(specs)) throw new TypeError('createIndexes needs an array of key patterns');
    const indexes = specs.map((keys) => {
      const spec = { key: oneKey(keys, 'an index key pattern') };
      if (options && typeof options === 'object') Object.assign(spec, options);
      if (spec.name === undefined) spec.name = indexName(keys);
      return spec;
    });
    this._run({ createIndexes: this[STATE].name, indexes });
    return indexes.map((spec) => spec.name);
  }
  dropIndex(index) {
    if (typeof index !== 'string') oneKey(index, 'the index');
    return revive(this._run({ dropIndexes: this[STATE].name, index }));
  }
  dropIndexes(indexes) {
    return revive(this._run({ dropIndexes: this[STATE].name, index: indexes === undefined ? '*' : indexes }));
  }
  drop() {
    try {
      this._run({ drop: this[STATE].name });
      return true;
    } catch (error) {
      if (error && error.code === 26) return false;
      throw error;
    }
  }
  renameCollection(target, dropTarget) {
    const dbName = this[STATE].db[STATE].name;
    execute('admin', compact({ renameCollection: `${dbName}.${this[STATE].name}`, to: `${dbName}.${target}`, dropTarget: dropTarget ? true : undefined }));
    return { ok: 1 };
  }
  watch() { throw unsupported('watch()', 'change streams need a long-lived connection'); }
  mapReduce() { throw unsupported('mapReduce()', 'use an aggregation pipeline'); }
}

function ensureId(document) {
  if (Array.isArray(document)) throw new TypeError('a document must be an object, not an array');
  if (document._id !== undefined) return document;
  const out = { _id: new ObjectId() };
  for (const key of Object.keys(document)) out[key] = document[key];
  return out;
}

function* splitInserts(docs) {
  let batch = [];
  let bytes = 0;
  for (const doc of docs) {
    const size = serialize(doc).length;
    if (batch.length > 0 && (batch.length >= MAX_INSERT_BATCH_DOCS || bytes + size > MAX_INSERT_BATCH_BYTES)) {
      yield batch;
      batch = [];
      bytes = 0;
    }
    batch.push(doc);
    bytes += size;
  }
  if (batch.length > 0) yield batch;
}

function aggregateOn(db, target, stages, options) {
  const opts = options || {};
  const command = (extra, batch) => {
    const out = { aggregate: target, pipeline: stages };
    out.cursor = batch ? { batchSize: batch } : opts.batchSize ? { batchSize: opts.batchSize } : {};
    return Object.assign(
      out,
      compact({
        allowDiskUse: opts.allowDiskUse, collation: opts.collation, hint: opts.hint, comment: opts.comment,
        maxTimeMS: opts.maxTimeMS, let: opts.let, readConcern: opts.readConcern,
        bypassDocumentValidation: opts.bypassDocumentValidation,
      }),
      compact(extra)
    );
  };
  if (opts.explain) {
    const explained = command({}, undefined);
    delete explained.cursor;
    explained.explain = true;
    return revive(execute(db[STATE].name, explained));
  }
  const cursor = new CommandCursor(db, (extra, batch) => command(extra, batch));
  if (opts.batchSize) cursor.batchSize(opts.batchSize);
  const last = stages.length > 0 ? stages[stages.length - 1] : undefined;
  if (last && typeof last === 'object' && ('$out' in last || '$merge' in last)) cursor.hasNext();
  return cursor;
}

/** `collection.explain(verbosity)`: the methods whose plan can be asked for. */
class Explainable {
  constructor(collection, verbosity) {
    this.collection = collection;
    this.verbosity = verbosity;
  }
  get [NOSER]() { return 'an Explainable'; }
  find(filter, projection, options) {
    const cursor = new FindCursor(this.collection, filter, projection, options);
    const verbosity = this.verbosity;
    cursor[STATE].open = function () {
      const plan = this.explain(verbosity);
      return { cursor: { firstBatch: [JSON.parse(serialize(plan))], id: 0, ns: this[STATE].db[STATE].name + '.' + this[STATE].collectionName } };
    };
    return cursor;
  }
  aggregate(pipeline, options) {
    return aggregateOn(this.collection[STATE].db, this.collection[STATE].name, pipeline, { ...(options || {}), explain: true });
  }
  count(query) {
    const name = this.collection[STATE].name;
    return revive(execute(this.collection[STATE].db[STATE].name, { explain: { count: name, query: query === undefined ? {} : query }, verbosity: this.verbosity }));
  }
  distinct(field, query) {
    const name = this.collection[STATE].name;
    return revive(execute(this.collection[STATE].db[STATE].name, { explain: { distinct: name, key: field, query: query === undefined ? {} : query }, verbosity: this.verbosity }));
  }
}

function collectionProxy(collection) {
  return new ProxyClass(collection, {
    get(target, prop, receiver) {
      if (typeof prop === 'symbol' || prop in target) return reflectGet(target, prop, receiver);
      return target[STATE].db.getCollection(`${target[STATE].name}.${prop}`);
    },
  });
}
