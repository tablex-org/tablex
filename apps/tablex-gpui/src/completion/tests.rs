//! Behaviour of the completion core. Cursors are written as `|` in the text.

use std::time::Instant;

use super::*;

struct Out {
    replace: Range<usize>,
    text: String,
    items: Vec<(String, String, Kind, String)>,
}

impl Out {
    fn labels(&self) -> Vec<&str> {
        self.items.iter().map(|item| item.0.as_str()).collect()
    }

    fn has(&self, label: &str) -> bool {
        self.items.iter().any(|item| item.0 == label)
    }

    fn position(&self, label: &str) -> usize {
        self.labels()
            .iter()
            .position(|l| *l == label)
            .unwrap_or_else(|| panic!("{label} missing in {:?}", self.labels()))
    }

    fn insert_of(&self, label: &str) -> &str {
        &self.items[self.position(label)].1
    }

    fn kind_of(&self, label: &str) -> Kind {
        self.items[self.position(label)].2
    }

    fn detail_of(&self, label: &str) -> &str {
        &self.items[self.position(label)].3
    }

    /// Text after accepting the first item.
    fn accept_first(&self) -> String {
        let mut text = self.text.clone();
        text.replace_range(self.replace.clone(), &self.items[0].1);
        text
    }
}

fn run(editor: &Editor<'_>, marked: &str, catalog: &dyn Catalog) -> Out {
    let cursor = marked.find('|').expect("cursor marker");
    let text = marked.replacen('|', "", 1);
    let completions = complete(editor, &text, cursor, catalog);
    Out {
        replace: completions.replace.clone(),
        items: completions
            .items
            .iter()
            .map(|item| {
                (
                    item.label.to_string(),
                    item.insert.to_string(),
                    item.kind,
                    item.detail.to_string(),
                )
            })
            .collect(),
        text: text.clone(),
    }
}

fn sql_editor(flavor: SqlFlavor) -> Editor<'static> {
    let identifier_quote = if matches!(
        flavor,
        SqlFlavor::Mysql | SqlFlavor::Mariadb | SqlFlavor::Clickhouse
    ) {
        '`'
    } else {
        '"'
    };
    Editor::Sql(SqlEditor {
        flavor,
        identifier_quote,
        fragment_of: None,
    })
}

fn pg(marked: &str, catalog: &dyn Catalog) -> Out {
    run(&sql_editor(SqlFlavor::Postgres), marked, catalog)
}

fn shop() -> StaticCatalog {
    StaticCatalog::default()
        .typed_table(
            "users",
            &[("id", "integer"), ("name", "text"), ("email", "text")],
        )
        .table("orders", &["id", "user_id", "total", "status"])
        .table("Order Items", &["order_id", "sku"])
        .view("active_users")
}

fn mongo(role: MongoRole<'static>, json: bool) -> Editor<'static> {
    Editor::Mongo(MongoEditor {
        role,
        collection: Some("orders"),
        json,
    })
}

fn script(marked: &str, catalog: &dyn Catalog) -> Out {
    run(&mongo(MongoRole::Script, false), marked, catalog)
}

fn mongo_catalog() -> StaticCatalog {
    StaticCatalog::default()
        .collections(&["orders", "users", "my-coll"])
        .fields(&["status", "total", "customer.name"])
}

// ---------------------------------------------------------------- SQL

#[test]
fn statement_start_offers_snippet_first_then_keywords_and_functions() {
    let out = pg("s|", &EmptyCatalog);
    assert_eq!(out.labels()[0], "select *");
    assert_eq!(out.kind_of("select *"), Kind::Snippet);
    assert_eq!(
        out.insert_of("select *"),
        "SELECT *\nFROM table\nLIMIT 100;"
    );
    assert_eq!(out.detail_of("select *"), "SELECT * FROM table LIMIT 100;");
    for function in [
        "SUM",
        "SIGN",
        "SQRT",
        "STDDEV",
        "STRPOS",
        "SUBSTR",
        "SUBSTRING",
        "SPLIT_PART",
        "STRING_AGG",
    ] {
        assert_eq!(out.kind_of(function), Kind::Function, "{function}");
        assert!(
            !out.detail_of(function).is_empty(),
            "{function} has a description"
        );
    }
    assert!(out.position("select *") < out.position("SELECT"));
    assert!(out.position("SELECT") < out.position("SUM"));
    assert!(out.position("SUM") < out.position("SIGN"));
}

#[test]
fn snippets_match_their_shorthand() {
    assert_eq!(pg("ct|", &EmptyCatalog).labels()[0], "create table");
    assert_eq!(pg("ins|", &EmptyCatalog).labels()[0], "insert into");
}

#[test]
fn no_popup_for_an_empty_statement_or_inside_strings_and_comments() {
    assert!(pg("|", &EmptyCatalog).items.is_empty());
    assert!(pg("select 'ab|c'", &EmptyCatalog).items.is_empty());
    assert!(pg("select 'ab|", &EmptyCatalog).items.is_empty());
    assert!(pg("-- sel|", &EmptyCatalog).items.is_empty());
    assert!(pg("/* sel| */ select 1", &EmptyCatalog).items.is_empty());
    assert!(pg("select 12|", &EmptyCatalog).items.is_empty());
}

#[test]
fn after_from_and_join_only_tables_views_are_offered() {
    let out = pg("select * from |", &shop());
    assert_eq!(
        out.labels(),
        ["users", "orders", "Order Items", "active_users"]
    );
    assert_eq!(out.kind_of("users"), Kind::Table);
    assert_eq!(out.kind_of("active_users"), Kind::View);
    let joined = pg("select * from users u join o|", &shop());
    assert_eq!(joined.labels()[0], "orders");
    assert!(!joined.has("SELECT"));
}

#[test]
fn table_names_are_quoted_when_the_dialect_requires_it() {
    let out = pg("select * from ord|", &shop());
    assert_eq!(out.insert_of("orders"), "orders");
    assert_eq!(out.insert_of("Order Items"), "\"Order Items\"");
    let mysql = run(&sql_editor(SqlFlavor::Mysql), "select * from ord|", &shop());
    assert_eq!(mysql.insert_of("Order Items"), "`Order Items`");
    // Reserved words are quoted too.
    let reserved = StaticCatalog::default()
        .table("user", &[])
        .table("order", &[]);
    assert_eq!(
        pg("select * from us|", &reserved).insert_of("user"),
        "\"user\""
    );
    // Already inside the quote: no second quote.
    let quoted = pg("select * from \"Ord|", &shop());
    assert_eq!(quoted.insert_of("Order Items"), "Order Items");
    assert_eq!(quoted.replace, 15..18);
}

#[test]
fn qualifier_resolves_alias_to_columns() {
    let out = pg("select u.| from users u", &shop());
    assert_eq!(out.labels(), ["id", "name", "email"]);
    assert!(out.items.iter().all(|item| item.2 == Kind::Column));
    assert_eq!(out.detail_of("name"), "text");
    let filtered = pg("select u.em| from users u", &shop());
    assert_eq!(filtered.labels(), ["email"]);
    let by_table = pg("select users.n| from users", &shop());
    assert_eq!(by_table.labels(), ["name"]);
}

#[test]
fn qualifier_picks_the_aliased_table_among_joins() {
    let out = pg("select * from users u join orders o on u.id = o.|", &shop());
    assert_eq!(out.labels(), ["id", "user_id", "total", "status"]);
}

#[test]
fn select_list_offers_columns_before_functions_and_keywords() {
    let out = pg("select na| from users", &shop());
    assert_eq!(out.labels()[0], "name");
    assert_eq!(out.kind_of("name"), Kind::Column);
    let wide = pg("select s| from orders", &shop());
    assert!(wide.position("status") < wide.position("SUM"));
    assert!(wide.has("SIGN"));
}

#[test]
fn columns_of_every_table_in_scope_with_ambiguous_names_qualified() {
    let out = pg("select i| from users u join orders o on 1 = 1", &shop());
    assert!(out.has("u.id") && out.has("o.id"), "{:?}", out.labels());
    assert_eq!(out.insert_of("u.id"), "u.id");
    assert!(!out.has("id"));
    assert!(out.has("user_id") || out.has("o.user_id"));
}

#[test]
fn where_clause_lists_columns_without_a_prefix() {
    let out = pg("select * from users where |", &shop());
    assert_eq!(out.labels()[..3], ["id", "name", "email"]);
    assert!(pg("select * from users |", &shop()).items.is_empty());
}

#[test]
fn update_and_insert_targets() {
    let out = pg("update users set na| = 1", &shop());
    assert_eq!(out.labels()[0], "name");
    assert_eq!(pg("update |", &shop()).labels()[0], "users");
    let insert = pg("insert into users (id, |)", &shop());
    assert_eq!(insert.labels(), ["id", "name", "email"]);
    assert_eq!(pg("insert into |", &shop()).labels()[0], "users");
}

#[test]
fn subqueries_see_their_own_and_the_outer_tables() {
    let sql = "select * from users where id in (select user_id from orders where |)";
    let out = pg(sql, &shop());
    assert!(out.has("total") && out.has("name"), "{:?}", out.labels());
    // After the closing paren only the outer block is in scope.
    let outer = pg(
        "select * from users where id in (select user_id from orders) and na|",
        &shop(),
    );
    assert_eq!(outer.labels()[0], "name");
    assert!(!outer.has("total"));
}

#[test]
fn only_the_statement_under_the_cursor_counts() {
    let out = pg("select * from users; select na| from orders;", &shop());
    assert!(
        !out.has("name"),
        "users is another statement: {:?}",
        out.labels()
    );
    let second = pg("select 1; sel|", &EmptyCatalog);
    assert_eq!(second.labels()[0], "select *");
}

#[test]
fn schema_qualifier_lists_the_tables_of_that_schema() {
    let catalog = shop()
        .in_schema("sales", "invoices", &["id", "amount"])
        .in_schema("sales", "refunds", &["id"]);
    let out = pg("select * from sales.|", &catalog);
    assert_eq!(out.labels(), ["invoices", "refunds"]);
    let columns = pg(
        "select * from sales.invoices i where sales.invoices.|",
        &catalog,
    );
    assert_eq!(columns.labels(), ["id", "amount"]);
    // Other schemas appear qualified next to the default one.
    let from = pg("select * from inv|", &catalog);
    assert_eq!(from.labels()[0], "sales.invoices");
    assert_eq!(from.insert_of("sales.invoices"), "sales.invoices");
    assert!(pg("select * from |", &catalog).has("sales"));
}

#[test]
fn ctes_are_offered_as_tables() {
    let out = pg(
        "with recent as (select * from orders) select * from rec|",
        &shop(),
    );
    assert_eq!(out.labels()[0], "recent");
    assert_eq!(out.detail_of("recent"), "cte");
}

#[test]
fn derived_table_aliases_are_in_scope_without_columns() {
    let out = pg("select d.| from (select 1 as x) d", &shop());
    assert!(out.items.is_empty());
}

#[test]
fn functions_follow_the_dialect() {
    let postgres = run(
        &sql_editor(SqlFlavor::Postgres),
        "select str|",
        &EmptyCatalog,
    );
    assert!(postgres.has("STRING_AGG") && postgres.has("STRPOS"));
    let mysql = run(&sql_editor(SqlFlavor::Mysql), "select gro|", &EmptyCatalog);
    assert!(mysql.has("GROUP_CONCAT") && mysql.has("GROUP BY"));
    assert!(!run(
        &sql_editor(SqlFlavor::Mysql),
        "select string_a|",
        &EmptyCatalog
    )
    .has("STRING_AGG"));
    assert!(run(
        &sql_editor(SqlFlavor::Mariadb),
        "select date_f|",
        &EmptyCatalog
    )
    .has("DATE_FORMAT"));
    assert!(!run(
        &sql_editor(SqlFlavor::Postgres),
        "select date_f|",
        &EmptyCatalog
    )
    .has("DATE_FORMAT"));
    assert!(run(
        &sql_editor(SqlFlavor::Postgres),
        "select date_t|",
        &EmptyCatalog
    )
    .has("DATE_TRUNC"));
    assert!(run(
        &sql_editor(SqlFlavor::Sqlite),
        "select strf|",
        &EmptyCatalog
    )
    .has("STRFTIME"));
    assert!(
        !run(&sql_editor(SqlFlavor::Sqlite), "select ifn|", &EmptyCatalog)
            .items
            .is_empty()
    );
    assert!(run(
        &sql_editor(SqlFlavor::Postgres),
        "select ifn|",
        &EmptyCatalog
    )
    .items
    .is_empty());
}

#[test]
fn short_queries_skip_fuzzy_subsequence_noise() {
    let su = run(
        &sql_editor(SqlFlavor::Postgres),
        "select su|",
        &EmptyCatalog,
    );
    for noise in [
        "LAST_VALUE",
        "FIRST_VALUE",
        "JSON_BUILD_OBJECT",
        "JSONB_BUILD_OBJECT",
    ] {
        assert!(!su.has(noise), "{noise} in {:?}", su.labels());
    }
    assert!(su.has("SUM") && su.has("SUBSTR") && su.has("SUBSTRING"));
    // Three characters enable subsequence matching.
    assert!(run(
        &sql_editor(SqlFlavor::Postgres),
        "select sbs|",
        &EmptyCatalog
    )
    .has("SUBSTRING"));
    // Word-start matches still work with two characters.
    assert!(run(
        &sql_editor(SqlFlavor::Postgres),
        "select ag|",
        &EmptyCatalog
    )
    .has("STRING_AGG"));
    // Mongo follows the same rule.
    let catalog = mongo_catalog();
    assert!(!script("db.orders.aggregate([{ $mt|", &catalog).has("$match"));
    assert!(script("db.orders.aggregate([{ $mtc|", &catalog).has("$match"));
    assert!(script("db.orders.aggregate([{ $m|", &catalog).has("$match"));
}

#[test]
fn keywords_follow_the_dialect() {
    assert!(run(
        &sql_editor(SqlFlavor::Postgres),
        "select * from a where x il|",
        &EmptyCatalog
    )
    .has("ILIKE"));
    assert!(!run(
        &sql_editor(SqlFlavor::Mysql),
        "select * from a where x il|",
        &EmptyCatalog
    )
    .has("ILIKE"));
    assert!(run(&sql_editor(SqlFlavor::Sqlite), "prag|", &EmptyCatalog).has("PRAGMA"));
    assert!(!run(&sql_editor(SqlFlavor::Postgres), "prag|", &EmptyCatalog).has("PRAGMA"));
    assert!(run(&sql_editor(SqlFlavor::Mysql), "us|", &EmptyCatalog).has("USE"));
}

#[test]
fn clause_keywords_continue_a_query_after_a_table() {
    let out = pg("select * from users wh|", &shop());
    assert_eq!(out.labels()[0], "WHERE");
    assert!(pg("select * from users u le|", &shop()).has("LEFT JOIN"));
}

#[test]
fn inserted_words_follow_the_typed_case() {
    assert_eq!(
        pg("select * from a wh|", &EmptyCatalog).insert_of("WHERE"),
        "where"
    );
    assert_eq!(
        pg("select * from a WH|", &EmptyCatalog).insert_of("WHERE"),
        "WHERE"
    );
    assert_eq!(pg("select su|", &EmptyCatalog).insert_of("SUM"), "sum(");
    assert_eq!(pg("select NO|", &EmptyCatalog).insert_of("NOW"), "NOW()");
}

#[test]
fn replace_range_and_highlight_cover_the_typed_prefix() {
    let out = pg("select * from us|ers", &shop());
    assert_eq!(out.replace, 14..16);
    let catalog = shop();
    let completions = complete(
        &sql_editor(SqlFlavor::Postgres),
        "select * from us",
        16,
        &catalog,
    );
    assert_eq!(completions.items[0].matched, 2);
    let word = complete(
        &sql_editor(SqlFlavor::Postgres),
        "select * from rs",
        16,
        &catalog,
    );
    // `rs` would only match "users" by subsequence, which needs 3 characters.
    assert!(word.items.is_empty());
    let fuzzy = complete(
        &sql_editor(SqlFlavor::Postgres),
        "select * from urs",
        17,
        &catalog,
    );
    assert!(fuzzy.items.iter().all(|item| item.matched == 0));
}

#[test]
fn table_fragment_editor_completes_columns_of_its_table() {
    let editor = Editor::Sql(SqlEditor {
        flavor: SqlFlavor::Postgres,
        identifier_quote: '"',
        fragment_of: Some(Relation {
            namespace: None,
            name: "users",
            view: false,
        }),
    });
    let catalog = shop();
    let out = run(&editor, "na|", &catalog);
    assert_eq!(out.labels()[0], "name");
    assert_eq!(
        run(&editor, "name = 'x' and |", &catalog).labels()[..3],
        ["id", "name", "email"]
    );
    assert!(run(&editor, "name = 'a|", &catalog).items.is_empty());
}

#[test]
fn results_are_capped_and_ranked_prefix_before_word_before_subsequence() {
    let catalog = StaticCatalog::default()
        .table("customer_ids", &[])
        .table("id", &[])
        .table("idx", &[])
        .table("c_i_d", &[]);
    let out = pg("select * from id|", &catalog);
    assert_eq!(out.labels(), ["id", "idx", "customer_ids"]);
    // Three characters allow fuzzy matches, shorter names first.
    let catalog = catalog.table("cust_i_d", &[]);
    assert_eq!(
        pg("select * from cid|", &catalog).labels(),
        ["c_i_d", "cust_i_d", "customer_ids"]
    );
    let many: Vec<String> = (0..500).map(|i| format!("table_{i}")).collect();
    let mut big = StaticCatalog::default();
    for name in &many {
        big = big.table(name, &[]);
    }
    assert_eq!(pg("select * from t|", &big).items.len(), MAX_ITEMS);
}

#[test]
fn accepting_the_first_item_produces_valid_text() {
    assert_eq!(
        pg("select * from us|", &shop()).accept_first(),
        "select * from users"
    );
    assert_eq!(
        pg("select u.na| from users u", &shop()).accept_first(),
        "select u.name from users u"
    );
}

// -------------------------------------------------------------- Mongo

#[test]
fn script_root_offers_templates_then_shell_methods() {
    let out = script("d|", &mongo_catalog());
    for snippet in [
        "db.collection.find",
        "db.collection.aggregate",
        "db.getCollection",
        "db.version",
        "db.stats",
        "db.serverStatus",
    ] {
        assert_eq!(out.kind_of(snippet), Kind::Snippet, "{snippet}");
    }
    for method in ["distinct", "deleteOne", "deleteMany", "dataSize"] {
        assert_eq!(out.kind_of(method), Kind::Method, "{method}");
        assert!(!out.detail_of(method).is_empty());
    }
    assert_eq!(out.detail_of("db.collection.find"), "Find documents");
    assert!(out.position("db.collection.find") < out.position("distinct"));
    // The tab's collection is substituted into the template.
    assert_eq!(out.insert_of("db.collection.find"), "db.orders.find({})");
    assert!(script("|", &mongo_catalog()).items.is_empty());
}

#[test]
fn db_dot_lists_collections_and_database_methods() {
    let out = script("db.|", &mongo_catalog());
    assert_eq!(out.labels()[..3], ["orders", "users", "my-coll"]);
    assert_eq!(out.kind_of("orders"), Kind::Collection);
    for method in ["getCollection", "version", "stats", "serverStatus"] {
        assert_eq!(out.kind_of(method), Kind::Method, "{method}");
    }
    assert_eq!(out.insert_of("my-coll"), "getCollection(\"my-coll\")");
    let filtered = script("db.us|", &mongo_catalog());
    assert_eq!(filtered.labels()[0], "users");
    assert_eq!(filtered.replace, 3..5);
}

#[test]
fn collection_dot_lists_collection_methods() {
    let out = script("db.orders.|", &mongo_catalog());
    assert_eq!(out.labels()[..3], ["find", "findOne", "aggregate"]);
    assert_eq!(out.insert_of("find"), "find({})");
    let filtered = script("db.orders.fin|", &mongo_catalog());
    assert!(filtered.has("find") && filtered.has("findOneAndUpdate") && !filtered.has("distinct"));
    assert_eq!(filtered.accept_first(), "db.orders.find({})");
}

#[test]
fn chained_calls_offer_cursor_and_collection_methods() {
    let cursor = script("db.orders.find({}).|", &mongo_catalog());
    assert!(cursor.has("sort") && cursor.has("limit") && cursor.has("skip") && cursor.has("count"));
    let sorted = script("db.orders.find({}).sort({ a: 1 }).|", &mongo_catalog());
    assert!(sorted.has("limit") && !sorted.has("count"));
    let by_name = script("db.getCollection(\"x\").fi|", &mongo_catalog());
    assert!(by_name.has("find"));
}

#[test]
fn the_shell_surface_is_completed_db_collection_cursor_helpers_use_and_show() {
    let catalog = mongo_catalog();
    // db.<helper>
    let db = script("db.|", &catalog);
    for helper in ["getSiblingDB", "createCollection", "adminCommand", "currentOp", "killOp", "dropDatabase", "hello"] {
        assert_eq!(db.kind_of(helper), Kind::Method, "{helper}");
    }
    // db.<collection>.<method>
    let collection = script("db.orders.|", &catalog);
    for method in ["bulkWrite", "estimatedDocumentCount", "replaceOne", "createIndexes", "renameCollection", "explain"] {
        assert_eq!(collection.kind_of(method), Kind::Method, "{method}");
    }
    // cursor methods after find / aggregate / a modifier, and the call shape of the new ones
    for source in ["db.orders.find({}).|", "db.orders.aggregate([]).|", "db.orders.find({}).project({ a: 1 }).|", "db.orders.find({}).batchSize(10).|"] {
        let cursor = script(source, &catalog);
        for method in ["forEach", "map", "hasNext", "next", "itcount", "batchSize", "hint", "maxTimeMS", "close"] {
            assert!(cursor.has(method), "{source}: {method}");
        }
    }
    assert!(script("db.getSiblingDB(\"x\").|", &catalog).has("dropDatabase"));
    assert!(script("db.orders.find({}).project({ |", &catalog).has("status"), "project takes fields");
    assert!(script("db.orders.remove({ |", &catalog).has("status"), "legacy remove takes a filter");
    // statement start: show / use / it and the printing helpers
    let root = script("s|", &catalog);
    for text in ["show dbs", "show collections"] {
        assert_eq!(root.kind_of(text), Kind::Snippet, "{text}");
    }
    assert_eq!(script("p|", &catalog).kind_of("printjson"), Kind::Snippet);
    assert_eq!(script("us|", &catalog).kind_of("use"), Kind::Snippet);
    assert_eq!(script("i|", &catalog).kind_of("it"), Kind::Snippet);
    // BSON constructors in value position
    let value = script("db.orders.insertOne({ n: Num|", &catalog);
    assert!(value.has("NumberInt") && value.has("NumberDecimal") && value.has("NumberLong"));
}

#[test]
fn get_collection_argument_completes_collection_names() {
    let out = script("db.getCollection(\"or|", &mongo_catalog());
    assert_eq!(out.labels()[0], "orders");
    assert_eq!(out.replace, 18..20);
}

#[test]
fn filter_argument_offers_fields_and_logical_operators_then_comparisons() {
    let keys = script("db.orders.find({ |", &mongo_catalog());
    assert_eq!(keys.labels()[..2], ["status", "total"]);
    assert_eq!(keys.kind_of("status"), Kind::Field);
    assert_eq!(keys.insert_of("status"), "status: ");
    assert!(keys.has("$and"));
    assert!(!keys.has("$gt"));
    let logical = script("db.orders.find({ $|", &mongo_catalog());
    assert!(logical.has("$or") && logical.has("$expr") && !logical.has("$gt"));
    let comparison = script("db.orders.find({ total: { $|", &mongo_catalog());
    assert_eq!(comparison.kind_of("$gt"), Kind::Operator);
    assert_eq!(
        comparison.detail_of("$gt"),
        "Matches values greater than a value"
    );
    assert!(comparison.has("$in") && !comparison.has("$and"));
    let after_comma = script("db.orders.find({ status: 'A', |", &mongo_catalog());
    assert!(after_comma.has("total"));
    let nested = script("db.orders.find({ $or: [{ |", &mongo_catalog());
    assert!(nested.has("status"));
}

#[test]
fn update_documents_offer_update_operators_then_fields() {
    let out = script("db.orders.updateOne({}, { $|", &mongo_catalog());
    assert!(out.has("$set") && out.has("$inc") && !out.has("$gt"));
    let fields = script("db.orders.updateOne({}, { $set: { |", &mongo_catalog());
    assert!(fields.has("status") && !fields.has("$set"));
    let push = script(
        "db.orders.updateOne({}, { $push: { tags: { $|",
        &mongo_catalog(),
    );
    assert!(push.has("$each") && !push.has("$set"));
}

#[test]
fn pipeline_stages_accumulators_and_nested_filters() {
    let stages = script("db.orders.aggregate([{ $|", &mongo_catalog());
    assert_eq!(stages.labels()[0], "$match");
    assert!(stages.has("$lookup") && !stages.has("$gt"));
    let group = script(
        "db.orders.aggregate([{ $group: { _id: \"$status\", n: { $|",
        &mongo_catalog(),
    );
    assert!(group.has("$sum") && group.has("$avg") && !group.has("$match"));
    let matching = script("db.orders.aggregate([{ $match: { |", &mongo_catalog());
    assert!(matching.has("status"));
    let after_group_id = script("db.orders.aggregate([{ $group: { _id: |", &mongo_catalog());
    assert!(after_group_id.items.is_empty());
    let lookup = script("db.orders.aggregate([{ $lookup: { |", &mongo_catalog());
    assert_eq!(
        lookup.labels()[..4],
        ["from", "localField", "foreignField", "as"]
    );
    let from = script(
        "db.orders.aggregate([{ $lookup: { from: \"u|",
        &mongo_catalog(),
    );
    assert_eq!(from.labels()[0], "users");
    let reference = script(
        "db.orders.aggregate([{ $group: { _id: \"$st|",
        &mongo_catalog(),
    );
    assert_eq!(reference.insert_of("status"), "$status");
}

#[test]
fn json_filter_editor_quotes_keys_and_completes_inside_quotes() {
    let editor = mongo(MongoRole::Filter, true);
    let catalog = mongo_catalog();
    let bare = run(&editor, "{ st|", &catalog);
    assert_eq!(bare.insert_of("status"), "\"status\": ");
    assert_eq!(bare.accept_first(), "{ \"status\": ");
    let quoted = run(&editor, "{ \"to|", &catalog);
    assert_eq!(quoted.labels()[0], "total");
    assert_eq!(quoted.insert_of("total"), "total");
    assert_eq!(quoted.replace, 3..5);
    let operator = run(&editor, "{ \"total\": { \"$|", &catalog);
    assert!(operator.has("$gte"));
    assert_eq!(operator.insert_of("$gte"), "$gte");
    let value = run(&editor, "{ \"total\": nu|", &catalog);
    assert_eq!(value.labels(), ["null"]);
}

#[test]
fn sort_projection_pipeline_and_stage_editors_know_their_root() {
    let catalog = mongo_catalog();
    assert!(run(&mongo(MongoRole::Sort, true), "{ |", &catalog).has("status"));
    assert!(!run(&mongo(MongoRole::Sort, true), "{ |", &catalog).has("$and"));
    // Second argument of `find` is a projection document.
    assert!(script("db.orders.find({}, { |", &catalog).has("total"));
    let pipeline = run(&mongo(MongoRole::Pipeline, false), "[ { $m|", &catalog);
    assert!(pipeline.has("$match") && pipeline.has("$merge"));
    let stage = run(
        &mongo(MongoRole::Stage("$lookup"), false),
        "{ fr|",
        &catalog,
    );
    assert_eq!(stage.labels()[0], "from");
    let matching = run(&mongo(MongoRole::Stage("$match"), false), "{ |", &catalog);
    assert!(matching.has("status") && matching.has("$and"));
}

#[test]
fn nothing_is_offered_inside_comments_or_unknown_positions() {
    assert!(script("// db.|", &mongo_catalog()).items.is_empty());
    assert!(script("/* d| */", &mongo_catalog()).items.is_empty());
    assert!(script("db.orders.find({ status: |", &mongo_catalog())
        .items
        .is_empty());
    assert!(script("print(|", &mongo_catalog()).items.is_empty());
}

// -------------------------------------------------------- performance

fn big_catalog(tables: usize) -> StaticCatalog {
    let mut catalog = StaticCatalog::default();
    for index in 0..tables {
        catalog
            .relations
            .push((None, format!("table_{index:05}"), index % 20 == 0));
    }
    catalog.columns.push((
        None,
        "table_00042".to_owned(),
        (0..40)
            .map(|i| (format!("col_{i}"), Some("integer".to_owned())))
            .collect(),
    ));
    catalog
}

/// 10k-table cache: every keystroke context stays far below a frame.
#[test]
fn ten_thousand_tables_complete_within_a_millisecond() {
    let catalog = big_catalog(10_000);
    let editor = sql_editor(SqlFlavor::Postgres);
    let cases = [
        ("select * from |", "empty prefix after FROM"),
        ("select * from t|", "one-letter prefix"),
        ("select * from table_00042|", "full name"),
        ("select * from ta_042|", "subsequence"),
        (
            "select * from table_00042 t where t.c|",
            "columns of one table",
        ),
        ("s|", "statement start"),
    ];
    for (marked, what) in cases {
        let cursor = marked.find('|').unwrap();
        let text = marked.replacen('|', "", 1);
        for _ in 0..20 {
            std::hint::black_box(complete(&editor, &text, cursor, &catalog));
        }
        let runs = 200;
        let start = Instant::now();
        for _ in 0..runs {
            std::hint::black_box(complete(&editor, &text, cursor, &catalog));
        }
        let micros = start.elapsed().as_secs_f64() * 1e6 / f64::from(runs);
        println!("completion latency, 10k tables, {what:<28}: {micros:>8.1} us");
        // The 1 ms budget is for optimized builds; unoptimized runs under a
        // loaded test machine only guard against pathological slowdowns.
        let budget = if cfg!(debug_assertions) {
            250_000.0
        } else {
            1_000.0
        };
        assert!(micros < budget, "{what}: {micros} us");
    }
}


// ------------------------------------------------------------ ClickHouse

fn ch(marked: &str, catalog: &dyn Catalog) -> Out {
    run(&sql_editor(SqlFlavor::Clickhouse), marked, catalog)
}

#[test]
fn clickhouse_from_lists_tables_with_backtick_quoting() {
    let out = ch("select * from |", &shop());
    for table in ["users", "orders", "active_users"] {
        assert!(out.has(table), "{table} in {:?}", out.labels());
    }
    assert_eq!(out.insert_of("Order Items"), "`Order Items`");
    assert_eq!(out.kind_of("active_users"), Kind::View);
}

#[test]
fn clickhouse_function_names_keep_their_case_and_match_by_prefix() {
    let out = ch("select toSt|", &EmptyCatalog);
    for name in ["toStartOfInterval", "toStartOfDay", "toStartOfMonth"] {
        assert_eq!(out.kind_of(name), Kind::Function, "{name}");
    }
    assert_eq!(out.insert_of("toStartOfInterval"), "toStartOfInterval(");
    assert!(!out.detail_of("toStartOfInterval").is_empty());
    // A lower-case query lower-cases SQL words but never a case-sensitive name.
    let lower = ch("select tostart|", &EmptyCatalog);
    assert_eq!(lower.insert_of("toStartOfInterval"), "toStartOfInterval(");
    let words = ch("select * from users wh|", &shop());
    assert_eq!(words.insert_of("WHERE"), "where");
    for (marked, name) in [
        ("select uniq|", "uniqExact"),
        ("select quant|", "quantile"),
        ("select groupA|", "groupArray"),
        ("select arrayJ|", "arrayJoin"),
        ("select countI|", "countIf"),
        ("select sumI|", "sumIf"),
        ("select argM|", "argMax"),
        ("select formatReadableS|", "formatReadableSize"),
        ("select JSONExt|", "JSONExtract"),
        ("select toDateT|", "toDateTime"),
    ] {
        assert_eq!(ch(marked, &EmptyCatalog).kind_of(name), Kind::Function, "{name}");
    }
    assert_eq!(ch("select any|", &EmptyCatalog).kind_of("any"), Kind::Function);
    assert_eq!(ch("select no|", &EmptyCatalog).insert_of("NOW"), "now()");
    assert_eq!(ch("select tod|", &EmptyCatalog).insert_of("today"), "today()");
}

#[test]
fn clickhouse_engine_names_follow_engine_with_or_without_equals() {
    for marked in [
        "create table t (id UInt64) engine = |",
        "CREATE TABLE t (id UInt64) ENGINE=|",
        "create table t (id UInt64) engine |",
    ] {
        let out = ch(marked, &EmptyCatalog);
        for engine in [
            "MergeTree",
            "ReplacingMergeTree",
            "SummingMergeTree",
            "AggregatingMergeTree",
            "CollapsingMergeTree",
            "ReplicatedMergeTree",
            "Memory",
            "Log",
            "Distributed",
        ] {
            assert!(out.has(engine), "{engine} in {marked}");
        }
        assert!(!out.has("SELECT") && !out.has("users"), "{marked}");
    }
    let typed = ch("create table t (id UInt64) engine = Repl|", &EmptyCatalog);
    assert_eq!(typed.labels()[0], "ReplacingMergeTree");
    assert!(typed.has("ReplicatedMergeTree") && !typed.has("MergeTree"));
    // Case of the typed word does not rewrite the engine's spelling.
    let lower = ch("create table t (id UInt64) engine = merget|", &EmptyCatalog);
    assert_eq!(lower.insert_of("MergeTree"), "MergeTree");
    // Other dialects have no ClickHouse engines here.
    assert!(!run(
        &sql_editor(SqlFlavor::Postgres),
        "create table t (id int) engine = |",
        &EmptyCatalog
    )
    .has("ReplacingMergeTree"));
}

#[test]
fn clickhouse_table_modifiers_follow_a_table_and_are_not_aliases() {
    for (marked, keyword) in [
        ("select * from users fin|", "FINAL"),
        ("select * from users prew|", "PREWHERE"),
        ("select * from users sam|", "SAMPLE"),
        ("select * from users sett|", "SETTINGS"),
        ("select * from users for|", "FORMAT"),
        ("select * from users array|", "ARRAY JOIN"),
    ] {
        let out = ch(marked, &shop());
        assert_eq!(out.kind_of(keyword), Kind::Keyword, "{keyword}");
    }
    // `FINAL` / `SAMPLE` are modifiers, not the alias of `users`.
    for marked in [
        "select users.| from users final",
        "select users.| from users sample 0.1",
    ] {
        let out = ch(marked, &shop());
        assert!(out.has("email"), "{marked}: {:?}", out.labels());
    }
    // PREWHERE filters like WHERE: columns are offered after it.
    assert!(ch("select * from users prewhere |", &shop()).has("email"));
    assert!(ch("select * from users prewhere na|", &shop()).has("name"));
    // ARRAY JOIN takes columns, not tables.
    let array_join = ch("select * from users array join |", &shop());
    assert!(!array_join.has("orders"));
    // None of it leaks into the other dialects.
    assert!(!pg("select * from users fin|", &shop()).has("FINAL"));
    assert!(!pg("select toSt|", &EmptyCatalog).has("toStartOfInterval"));
}

#[test]
fn clickhouse_statements_honour_backslash_escapes_and_hash_comments() {
    // `\'` keeps the `;` inside the string, so this is one statement.
    let out = ch(r"select 'a\'; select', toSt|", &EmptyCatalog);
    assert!(out.has("toStartOfInterval"), "{:?}", out.labels());
    // `#` comments out the rest of the line.
    assert!(ch("select 1 # toSt|", &EmptyCatalog).items.is_empty());
    // Standard SQL dialects read the first example as two statements.
    assert!(run(
        &sql_editor(SqlFlavor::Postgres),
        r"select 'a\'; select', toSt|",
        &EmptyCatalog
    )
    .items
    .is_empty());
}
