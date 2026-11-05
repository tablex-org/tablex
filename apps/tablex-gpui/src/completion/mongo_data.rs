//! Static MongoDB shell / operator vocabulary, grouped by where each entry may
//! legally appear so the engine never filters one flat list by context.

pub(super) struct Spec {
    pub label: &'static str,
    pub desc: &'static str,
    /// Everyday entry, floated above the long tail.
    pub hot: bool,
}

const fn op(label: &'static str, desc: &'static str) -> Spec {
    Spec {
        label,
        desc,
        hot: false,
    }
}

const fn hot(label: &'static str, desc: &'static str) -> Spec {
    Spec {
        label,
        desc,
        hot: true,
    }
}

/// A shell function offered with the text it inserts.
pub(super) struct Method {
    pub label: &'static str,
    pub desc: &'static str,
    pub insert: &'static str,
}

const fn method(label: &'static str, desc: &'static str, insert: &'static str) -> Method {
    Method {
        label,
        desc,
        insert,
    }
}

/// Top-level keys of a filter document, next to field names.
pub(super) static FILTER_KEYS: &[Spec] = &[
    hot("$and", "Joins clauses with a logical AND"),
    hot("$or", "Joins clauses with a logical OR"),
    op("$nor", "Joins clauses with a logical NOR"),
    hot("$expr", "Uses aggregation expressions in a query"),
    op("$text", "Performs a text search"),
    op("$where", "Matches with a JavaScript predicate"),
    op("$jsonSchema", "Matches documents against a JSON schema"),
];

/// Operators inside a field condition: `{ age: { $gt: 1 } }`.
pub(super) static QUERY_OPERATORS: &[Spec] = &[
    hot("$eq", "Matches values equal to a value"),
    hot("$ne", "Matches values not equal to a value"),
    hot("$gt", "Matches values greater than a value"),
    hot("$gte", "Matches values greater than or equal to a value"),
    hot("$lt", "Matches values less than a value"),
    hot("$lte", "Matches values less than or equal to a value"),
    hot("$in", "Matches any value in an array"),
    hot("$nin", "Matches no value in an array"),
    hot("$not", "Inverts the effect of a query expression"),
    hot("$exists", "Matches documents that have the field"),
    op("$type", "Matches documents by BSON type"),
    hot("$regex", "Matches a regular expression"),
    op("$mod", "Matches values by modulo division"),
    op("$all", "Matches arrays containing all the values"),
    hot(
        "$elemMatch",
        "Matches arrays with an element matching all criteria",
    ),
    op("$size", "Matches arrays of a given length"),
    op("$bitsAllSet", "Matches numbers with all the given bits set"),
    op(
        "$bitsAnySet",
        "Matches numbers with any of the given bits set",
    ),
    op(
        "$bitsAllClear",
        "Matches numbers with all the given bits clear",
    ),
    op(
        "$bitsAnyClear",
        "Matches numbers with any of the given bits clear",
    ),
    op("$geoWithin", "Matches geometry contained within a shape"),
    op("$geoIntersects", "Matches geometry intersecting a shape"),
    op("$near", "Matches points near a point, nearest first"),
    op("$nearSphere", "Matches points near a point on a sphere"),
];

pub(super) static UPDATE_OPERATORS: &[Spec] = &[
    hot("$set", "Sets field values"),
    hot("$unset", "Removes fields"),
    op(
        "$setOnInsert",
        "Sets field values only when an upsert inserts",
    ),
    hot("$inc", "Increments field values"),
    op("$mul", "Multiplies field values"),
    op("$min", "Updates only when the new value is lower"),
    op("$max", "Updates only when the new value is higher"),
    op("$rename", "Renames fields"),
    op("$currentDate", "Sets fields to the current date"),
    hot("$push", "Appends values to an array"),
    hot("$addToSet", "Appends values to an array without duplicates"),
    op("$pop", "Removes the first or last element of an array"),
    hot("$pull", "Removes array elements matching a condition"),
    op("$pullAll", "Removes all the listed values from an array"),
    op("$bit", "Performs a bitwise update"),
];

/// Valid inside the object value of `$push` / `$addToSet`.
pub(super) static PUSH_MODIFIERS: &[Spec] = &[
    hot("$each", "Appends multiple values"),
    op("$slice", "Limits the array length after the push"),
    op("$sort", "Sorts the array elements after the push"),
    op("$position", "Insert position for $each"),
];

pub(super) static PIPELINE_STAGES: &[Spec] = &[
    hot("$match", "Filters documents"),
    hot("$group", "Groups documents by a key"),
    hot("$project", "Reshapes documents"),
    hot("$sort", "Sorts documents"),
    hot("$limit", "Limits the number of documents"),
    hot("$skip", "Skips documents"),
    hot("$lookup", "Joins another collection"),
    hot(
        "$unwind",
        "Deconstructs an array field into one document per element",
    ),
    hot("$count", "Counts documents into a single field"),
    hot("$addFields", "Adds new fields to documents"),
    op("$set", "Adds new fields to documents (alias of $addFields)"),
    op("$unset", "Removes fields from documents"),
    op("$replaceRoot", "Promotes a subdocument to the top level"),
    op("$replaceWith", "Promotes a subdocument to the top level"),
    op("$facet", "Runs multiple pipelines over the same input"),
    op(
        "$sortByCount",
        "Groups by a value and counts, highest first",
    ),
    op("$bucket", "Groups documents into user-defined buckets"),
    op(
        "$bucketAuto",
        "Groups documents into evenly distributed buckets",
    ),
    op("$sample", "Randomly selects documents"),
    op("$unionWith", "Combines results with another collection"),
    op(
        "$graphLookup",
        "Performs a recursive search on a collection",
    ),
    op("$geoNear", "Orders documents by proximity to a point"),
    op(
        "$setWindowFields",
        "Adds fields computed over a window of documents",
    ),
    op("$densify", "Fills gaps in a sequence of field values"),
    op("$fill", "Fills null or missing field values"),
    op("$documents", "Returns literal documents"),
    op("$redact", "Restricts document content based on a condition"),
    op("$out", "Writes the results to a collection"),
    op("$merge", "Merges the results into a collection"),
    op("$collStats", "Returns statistics about the collection"),
    op("$indexStats", "Returns index usage statistics"),
];

pub(super) static ACCUMULATORS: &[Spec] = &[
    hot("$sum", "Sums values"),
    hot("$avg", "Averages values"),
    hot("$min", "Returns the lowest value"),
    hot("$max", "Returns the highest value"),
    op("$count", "Counts the documents in the group"),
    hot("$first", "Returns the first value in the group"),
    hot("$last", "Returns the last value in the group"),
    op("$push", "Collects values into an array"),
    op("$addToSet", "Collects unique values into an array"),
    op("$mergeObjects", "Merges documents into one"),
    op("$stdDevPop", "Population standard deviation"),
    op("$stdDevSamp", "Sample standard deviation"),
    op("$top", "Returns the top element by a sort order"),
    op("$topN", "Returns the top N elements by a sort order"),
    op("$bottom", "Returns the bottom element by a sort order"),
    op("$bottomN", "Returns the bottom N elements by a sort order"),
    op("$firstN", "Returns the first N values in the group"),
    op("$lastN", "Returns the last N values in the group"),
    op("$maxN", "Returns the N highest values"),
    op("$minN", "Returns the N lowest values"),
    op("$median", "Approximates the median"),
    op("$percentile", "Approximates the requested percentiles"),
];

pub(super) static EXPRESSION_OPERATORS: &[Spec] = &[
    hot("$cond", "Returns one of two values from a condition"),
    hot("$ifNull", "Returns the first non-null value"),
    op("$switch", "Evaluates branches in order"),
    op("$eq", "Returns true when the values are equal"),
    op("$ne", "Returns true when the values differ"),
    op("$gt", "Returns true when the first value is greater"),
    op(
        "$gte",
        "Returns true when the first value is greater or equal",
    ),
    op("$lt", "Returns true when the first value is less"),
    op("$lte", "Returns true when the first value is less or equal"),
    op("$cmp", "Compares two values and returns -1, 0 or 1"),
    op("$and", "Logical AND"),
    op("$or", "Logical OR"),
    op("$not", "Logical NOT"),
    hot("$add", "Adds numbers or a number to a date"),
    hot("$subtract", "Subtracts two numbers or dates"),
    hot("$multiply", "Multiplies numbers"),
    op("$divide", "Divides two numbers"),
    op("$mod", "Returns the remainder of a division"),
    op("$abs", "Absolute value"),
    op("$ceil", "Rounds up to the next integer"),
    op("$floor", "Rounds down to the previous integer"),
    op("$round", "Rounds to a number of decimal places"),
    op("$pow", "Raises a number to an exponent"),
    op("$sqrt", "Square root"),
    hot("$concat", "Concatenates strings"),
    op("$split", "Splits a string on a delimiter"),
    op("$substr", "Returns a substring"),
    op("$strLenCP", "Number of code points in a string"),
    op("$toLower", "Converts a string to lower case"),
    op("$toUpper", "Converts a string to upper case"),
    op("$trim", "Removes whitespace from both ends"),
    op("$replaceOne", "Replaces the first match in a string"),
    op("$replaceAll", "Replaces every match in a string"),
    op("$regexMatch", "Returns true when a regex matches"),
    hot("$size", "Number of elements in an array"),
    hot("$arrayElemAt", "Element at an array index"),
    op("$first", "First element of an array"),
    op("$last", "Last element of an array"),
    op("$slice", "Returns a subset of an array"),
    op("$in", "Returns true when a value is in an array"),
    op("$concatArrays", "Concatenates arrays"),
    hot("$filter", "Selects array elements matching a condition"),
    hot("$map", "Applies an expression to each array element"),
    op("$reduce", "Folds an array into a single value"),
    op("$range", "Generates a sequence of numbers"),
    op("$isArray", "Returns true when the value is an array"),
    op("$arrayToObject", "Converts an array of pairs to a document"),
    op("$objectToArray", "Converts a document to an array of pairs"),
    op("$setUnion", "Union of arrays as sets"),
    hot("$dateToString", "Formats a date as a string"),
    op("$dateFromString", "Parses a string into a date"),
    op("$dateToParts", "Splits a date into its parts"),
    op("$dateAdd", "Adds a time unit to a date"),
    op("$dateDiff", "Difference between two dates"),
    op("$dateTrunc", "Truncates a date to a time unit"),
    op("$year", "Year of a date"),
    op("$month", "Month of a date"),
    op("$dayOfMonth", "Day of the month of a date"),
    op("$dayOfWeek", "Day of the week of a date"),
    op("$hour", "Hour of a date"),
    op("$type", "Returns the BSON type of a value"),
    op("$convert", "Converts a value to a given type"),
    hot("$toString", "Converts a value to a string"),
    op("$toInt", "Converts a value to an integer"),
    op("$toDouble", "Converts a value to a double"),
    op("$toBool", "Converts a value to a boolean"),
    op("$toDate", "Converts a value to a date"),
    op("$toObjectId", "Converts a value to an ObjectId"),
    op("$literal", "Returns a value without parsing it"),
    op("$mergeObjects", "Merges documents into one"),
    op(
        "$getField",
        "Reads a field, including names starting with $",
    ),
    op("$let", "Binds variables for use in an expression"),
];

static LOOKUP_OPTIONS: &[Spec] = &[
    hot("from", "Collection to join"),
    hot("localField", "Field on the input documents"),
    hot("foreignField", "Field on the joined collection"),
    hot("as", "Output array field"),
    op("let", "Variables for the joined pipeline"),
    op("pipeline", "Pipeline to run on the joined collection"),
];

static GRAPHLOOKUP_OPTIONS: &[Spec] = &[
    hot("from", "Collection to search"),
    hot("startWith", "Expression to start the search from"),
    hot("connectFromField", "Field to recurse from"),
    hot("connectToField", "Field to match against"),
    hot("as", "Output array field"),
    op("maxDepth", "Maximum recursion depth"),
    op("depthField", "Field holding the recursion depth"),
    op(
        "restrictSearchWithMatch",
        "Extra filter on the searched documents",
    ),
];

static UNWIND_OPTIONS: &[Spec] = &[
    hot("path", "Array field to unwind"),
    op("includeArrayIndex", "Field holding the array index"),
    op(
        "preserveNullAndEmptyArrays",
        "Keep documents with a missing or empty array",
    ),
];

static MERGE_OPTIONS: &[Spec] = &[
    hot("into", "Target collection"),
    op("on", "Fields identifying a document"),
    op("let", "Variables for whenMatched"),
    op("whenMatched", "Action when a document matches"),
    op("whenNotMatched", "Action when no document matches"),
];

static BUCKET_OPTIONS: &[Spec] = &[
    hot("groupBy", "Expression to group by"),
    hot("boundaries", "Bucket boundaries"),
    op("default", "Bucket for values outside the boundaries"),
    op("output", "Accumulators for each bucket"),
];

static BUCKETAUTO_OPTIONS: &[Spec] = &[
    hot("groupBy", "Expression to group by"),
    hot("buckets", "Number of buckets"),
    op("output", "Accumulators for each bucket"),
    op("granularity", "Preferred number series"),
];

static SAMPLE_OPTIONS: &[Spec] = &[hot("size", "Number of documents to return")];

static UNIONWITH_OPTIONS: &[Spec] = &[
    hot("coll", "Collection to union with"),
    op("pipeline", "Pipeline to run on that collection"),
];

static SETWINDOWFIELDS_OPTIONS: &[Spec] = &[
    hot("partitionBy", "Expression to partition by"),
    op("sortBy", "Sort order within a partition"),
    hot("output", "Window accumulators"),
];

static GEONEAR_OPTIONS: &[Spec] = &[
    hot("near", "Point to measure from"),
    hot("distanceField", "Field holding the computed distance"),
    op("maxDistance", "Maximum distance in meters"),
    op("minDistance", "Minimum distance in meters"),
    op("query", "Extra filter on the documents"),
    op("spherical", "Use spherical geometry"),
];

static REPLACEROOT_OPTIONS: &[Spec] =
    &[hot("newRoot", "Expression producing the new root document")];

/// Option keys of stages whose body is a fixed set of names.
pub(super) fn stage_options(stage: &str) -> &'static [Spec] {
    match stage {
        "$lookup" => LOOKUP_OPTIONS,
        "$graphLookup" => GRAPHLOOKUP_OPTIONS,
        "$unwind" => UNWIND_OPTIONS,
        "$merge" => MERGE_OPTIONS,
        "$bucket" => BUCKET_OPTIONS,
        "$bucketAuto" => BUCKETAUTO_OPTIONS,
        "$sample" => SAMPLE_OPTIONS,
        "$unionWith" => UNIONWITH_OPTIONS,
        "$setWindowFields" => SETWINDOWFIELDS_OPTIONS,
        "$geoNear" => GEONEAR_OPTIONS,
        "$replaceRoot" => REPLACEROOT_OPTIONS,
        _ => &[],
    }
}

/// Stages whose body is an option document (see [`stage_options`]).
pub(super) fn is_option_stage(stage: &str) -> bool {
    !stage_options(stage).is_empty()
}

/// Methods of a collection, most used first.
pub(super) static COLLECTION_METHODS: &[Method] = &[
    method("find", "Query matching documents", "find({})"),
    method("findOne", "Query one matching document", "findOne({})"),
    method("aggregate", "Run an aggregation pipeline", "aggregate([])"),
    method(
        "countDocuments",
        "Count matching documents",
        "countDocuments({})",
    ),
    method(
        "distinct",
        "List the distinct values of a field",
        "distinct(\"\")",
    ),
    method("insertOne", "Insert one document", "insertOne({})"),
    method(
        "insertMany",
        "Insert multiple documents",
        "insertMany([{}])",
    ),
    method(
        "updateOne",
        "Update one matching document",
        "updateOne({}, { $set: {} })",
    ),
    method(
        "updateMany",
        "Update all matching documents",
        "updateMany({}, { $set: {} })",
    ),
    method("deleteOne", "Delete one matching document", "deleteOne({})"),
    method(
        "deleteMany",
        "Delete all matching documents",
        "deleteMany({})",
    ),
    method(
        "findOneAndUpdate",
        "Atomically update and return a document",
        "findOneAndUpdate({}, { $set: {} })",
    ),
    method(
        "findOneAndReplace",
        "Atomically replace and return a document",
        "findOneAndReplace({}, {})",
    ),
    method(
        "findOneAndDelete",
        "Atomically delete and return a document",
        "findOneAndDelete({})",
    ),
    method("getIndexes", "List collection indexes", "getIndexes()"),
    method("stats", "Show collection statistics", "stats()"),
    method("createIndex", "Create an index", "createIndex({})"),
    method(
        "count",
        "Count matching documents (legacy helper)",
        "count({})",
    ),
    method("dataSize", "Total size of documents in bytes", "dataSize()"),
    method(
        "storageSize",
        "Allocated storage size in bytes",
        "storageSize()",
    ),
    method(
        "totalIndexSize",
        "Total size of all indexes in bytes",
        "totalIndexSize()",
    ),
    method("dropIndex", "Drop one index", "dropIndex(\"\")"),
    method("dropIndexes", "Drop collection indexes", "dropIndexes()"),
    method("drop", "Drop the collection", "drop()"),
    method(
        "estimatedDocumentCount",
        "Count the documents from collection metadata",
        "estimatedDocumentCount()",
    ),
    method(
        "replaceOne",
        "Replace one matching document",
        "replaceOne({}, {})",
    ),
    method(
        "bulkWrite",
        "Run several write operations in one call",
        "bulkWrite([])",
    ),
    method("createIndexes", "Create several indexes", "createIndexes([])"),
    method(
        "renameCollection",
        "Rename the collection",
        "renameCollection(\"\")",
    ),
    method(
        "totalSize",
        "Documents plus indexes size in bytes",
        "totalSize()",
    ),
    method("explain", "Explain a find, aggregate or count", "explain()"),
    method("getName", "Name of the collection", "getName()"),
    method("getFullName", "Database and collection name", "getFullName()"),
    method("insert", "Insert documents (legacy helper)", "insert({})"),
    method("update", "Update documents (legacy helper)", "update({}, { $set: {} })"),
    method("remove", "Delete documents (legacy helper)", "remove({})"),
];

/// Helpers of `db` itself, offered next to the collection names after `db.`.
pub(super) static DATABASE_METHODS: &[Method] = &[
    method(
        "getCollection",
        "Reference a collection by name",
        "getCollection(\"\")",
    ),
    method(
        "getCollectionNames",
        "List the collection names",
        "getCollectionNames()",
    ),
    method(
        "listCollections",
        "List collections with their options",
        "listCollections()",
    ),
    method("version", "Show the MongoDB server version", "version()"),
    method("stats", "Show database statistics", "stats()"),
    method(
        "serverStatus",
        "Show server status counters",
        "serverStatus()",
    ),
    method("getName", "Name of the current database", "getName()"),
    method("runCommand", "Run a database command", "runCommand({})"),
    method(
        "getSiblingDB",
        "Use another database for this expression",
        "getSiblingDB(\"\")",
    ),
    method(
        "createCollection",
        "Create a collection",
        "createCollection(\"\")",
    ),
    method("createView", "Create a view", "createView(\"\", \"\", [])"),
    method(
        "adminCommand",
        "Run a command on the admin database",
        "adminCommand({})",
    ),
    method("aggregate", "Run a database-level aggregation", "aggregate([])"),
    method("currentOp", "List the operations in progress", "currentOp()"),
    method("killOp", "Kill an operation by id", "killOp(0)"),
    method("dropDatabase", "Drop the current database", "dropDatabase()"),
    method("hello", "Server role and topology", "hello()"),
    method("hostInfo", "Host system information", "hostInfo()"),
    method(
        "getProfilingStatus",
        "Show the profiler level",
        "getProfilingStatus()",
    ),
    method("getUsers", "List the database users", "getUsers()"),
    method("getRoles", "List the database roles", "getRoles()"),
    method(
        "serverBuildInfo",
        "Show the server build information",
        "serverBuildInfo()",
    ),
];

/// Methods chained after `find(...)`.
pub(super) static CURSOR_METHODS: &[Method] = &[
    method("limit", "Limit cursor results", "limit(100)"),
    method("sort", "Sort cursor results", "sort({})"),
    method("skip", "Skip cursor results", "skip(0)"),
    method("count", "Count the documents matched by find()", "count()"),
    method("toArray", "Collect the results into an array", "toArray()"),
    method("explain", "Show the query plan", "explain()"),
    method("project", "Choose the returned fields", "project({})"),
    method("projection", "Choose the returned fields", "projection({})"),
    method("batchSize", "Documents per server batch", "batchSize(100)"),
    method("hint", "Force an index", "hint({})"),
    method("collation", "Set the collation", "collation({})"),
    method("comment", "Tag the query", "comment(\"\")"),
    method("maxTimeMS", "Server time limit in milliseconds", "maxTimeMS(1000)"),
    method("forEach", "Call a function for each document", "forEach(doc => {})"),
    method("map", "Transform each document", "map(doc => doc)"),
    method("hasNext", "Whether another document is waiting", "hasNext()"),
    method("next", "The next document", "next()"),
    method("tryNext", "The next document or null", "tryNext()"),
    method("itcount", "Count by iterating the cursor", "itcount()"),
    method("size", "Count with skip and limit applied", "size()"),
    method("pretty", "Format the output (no-op in the shell)", "pretty()"),
    method("close", "Close the cursor", "close()"),
    method("min", "Lower index bound", "min({})"),
    method("max", "Upper index bound", "max({})"),
    method("allowDiskUse", "Let the server spill to disk", "allowDiskUse()"),
    method("noCursorTimeout", "Keep the cursor open while idle", "noCursorTimeout()"),
    method("returnKey", "Return only the index keys", "returnKey()"),
    method("showRecordId", "Add the record id to results", "showRecordId()"),
];

/// Statement templates at the start of a script. `{collection}` is replaced
/// with the tab's collection (or a placeholder).
pub(super) static ROOT_SNIPPETS: &[Method] = &[
    method(
        "db.collection.find",
        "Find documents",
        "db.{collection}.find({})",
    ),
    method(
        "db.collection.aggregate",
        "Aggregation pipeline",
        "db.{collection}.aggregate([\n  { $match: {} }\n])",
    ),
    method(
        "db.getCollection",
        "Reference a collection by name",
        "db.getCollection(\"{collection}\")",
    ),
    method("use", "Switch the active database", "use database"),
    method(
        "db.version",
        "Show the MongoDB server version",
        "db.version()",
    ),
    method("db.stats", "Show database statistics", "db.stats()"),
    method(
        "db.serverStatus",
        "Show server status counters",
        "db.serverStatus()",
    ),
    method(
        "db.getCollectionNames",
        "List the collection names",
        "db.getCollectionNames()",
    ),
    method("show dbs", "List the databases", "show dbs"),
    method("show collections", "List the collections", "show collections"),
    method("show users", "List the database users", "show users"),
    method("show roles", "List the database roles", "show roles"),
    method("show profile", "Show recent profiler entries", "show profile"),
    method("it", "Show the next page of the last cursor", "it"),
    method("print", "Write a line to the Output pane", "print()"),
    method("printjson", "Write a value as JSON to the Output", "printjson()"),
    method("sleep", "Wait for a number of milliseconds", "sleep(1000)"),
    method("tojson", "Format a value as shell text", "tojson()"),
    method(
        "db.getSiblingDB",
        "Use another database for this expression",
        "db.getSiblingDB(\"\")",
    ),
    method(
        "for",
        "Loop over a range",
        "for (let i = 0; i < 10; i++) {\n  \n}",
    ),
    method("EJSON.stringify", "Extended JSON text of a value", "EJSON.stringify()"),
    method("EJSON.parse", "Value of Extended JSON text", "EJSON.parse(\"\")"),
];

/// Literal values worth completing in value position.
pub(super) static VALUE_SNIPPETS: &[Method] = &[
    method("ObjectId", "MongoDB ObjectId value", "ObjectId(\"\")"),
    method("ISODate", "MongoDB ISODate value", "ISODate(\"\")"),
    method("new Date", "JavaScript date value", "new Date(\"\")"),
    method("NumberLong", "64-bit integer value", "NumberLong(\"\")"),
    method("NumberInt", "32-bit integer value", "NumberInt(0)"),
    method("NumberDecimal", "128-bit decimal value", "NumberDecimal(\"\")"),
    method("UUID", "UUID value", "UUID(\"\")"),
    method("Timestamp", "BSON timestamp value", "Timestamp(0, 0)"),
    method("BinData", "Binary value", "BinData(0, \"\")"),
    method("MinKey", "Lowest BSON value", "MinKey()"),
    method("MaxKey", "Highest BSON value", "MaxKey()"),
    method("null", "Null value", "null"),
    method("true", "Boolean true", "true"),
    method("false", "Boolean false", "false"),
];

/// Keywords for the JSON editors, which cannot hold shell constructors.
pub(super) static JSON_VALUES: &[Method] = &[
    method("null", "Null value", "null"),
    method("true", "Boolean true", "true"),
    method("false", "Boolean false", "false"),
];
