use engine_driver_api::{
    CancelRequest, Capability, DriverCapabilities, DriverError, DriverErrorEnvelope, ExportRequest,
    ExportResult, MutationRequest, MutationResult, NavigatorNode, NavigatorPage, NavigatorRequest,
    OperationId, PageColumn, PageFlags, QueryPage, QueryRequest, RowField, RowFields,
    SqlDialectInfo, SuccessEnvelope, ABI_VERSION,
};

#[test]
fn abi_version_is_two() {
    assert_eq!(ABI_VERSION, 2);
}

#[test]
fn capabilities_use_the_v2_wire_shape() {
    let mut capabilities = DriverCapabilities::new(
        "mongodb",
        "MongoDB",
        vec!["navigator".into(), "query.find".into()],
    );
    capabilities.uri_schemes = vec!["mongodb".into(), "mongodb+srv".into()];
    capabilities.editors = vec!["mongo_find".into()];
    capabilities.navigator_kinds = vec!["database".into(), "collection".into()];

    let value = serde_json::to_value(capabilities).expect("capabilities serialize");

    assert_eq!(value["abi_version"], 2);
    assert_eq!(value["driver"], "mongodb");
    assert_eq!(value["uri_schemes"][1], "mongodb+srv");
    assert_eq!(value["single_connection_pool"], true);
    assert_eq!(
        value["features"],
        serde_json::json!(["navigator", "query.find"])
    );
    // The optional dialect and the internal set never appear on the wire.
    let object = value.as_object().expect("object");
    assert!(!object.contains_key("sql_dialect"));
    assert!(!object.contains_key("set"));
    assert_eq!(object.len(), 8);
}

const OLD_CAPABILITIES_JSON: &str = r#"{"abi_version":2,"driver":"sqlite","display_name":"SQLite",
    "uri_schemes":["sqlite"],"features":["navigator","query.sql","future.thing","export.json"],
    "editors":["sql"],"navigator_kinds":["database","table"],"single_connection_pool":true}"#;

#[test]
fn capabilities_match_exact_feature_tokens() {
    let capabilities: DriverCapabilities =
        serde_json::from_str(OLD_CAPABILITIES_JSON).expect("old capabilities parse");

    assert!(capabilities.has_token("query.sql"));
    assert!(!capabilities.has_token("query"));
    assert!(capabilities.has(Capability::QuerySql));
    assert!(!capabilities.has(Capability::OperationCancel));
    assert!(capabilities.has_any_export());
    assert!(!capabilities.has_any_mutation());
    assert!(!capabilities.supports_mutation_kind("update"));
}

#[test]
fn unknown_feature_tokens_are_preserved_through_serde() {
    let capabilities: DriverCapabilities =
        serde_json::from_str(OLD_CAPABILITIES_JSON).expect("capabilities parse");
    assert!(capabilities.has_token("future.thing"));
    assert!(!capabilities.has_token("future.other"));

    let encoded = serde_json::to_string(&capabilities).expect("capabilities serialize");
    let decoded: DriverCapabilities = serde_json::from_str(&encoded).expect("round trip parse");
    assert_eq!(decoded, capabilities);
    assert_eq!(
        decoded.features(),
        ["navigator", "query.sql", "future.thing", "export.json"]
    );
    assert!(decoded.has(Capability::Navigator));
    assert!(decoded.has_token("future.thing"));
}

#[test]
fn sql_dialect_is_optional_on_the_wire() {
    let old: DriverCapabilities =
        serde_json::from_str(OLD_CAPABILITIES_JSON).expect("old capabilities parse");
    assert_eq!(old.sql_dialect, None);

    let with_dialect = OLD_CAPABILITIES_JSON.replace(
        r#""single_connection_pool":true"#,
        r#""single_connection_pool":true,"sql_dialect":{"identifier_quote":"\"","string_literal_quote":"'","supports_schemas":false,"database_qualified_names":false}"#,
    );
    let parsed: DriverCapabilities = serde_json::from_str(&with_dialect).expect("dialect parses");
    assert_eq!(
        parsed.sql_dialect,
        Some(SqlDialectInfo {
            identifier_quote: "\"".into(),
            string_literal_quote: "'".into(),
            supports_schemas: false,
            database_qualified_names: false,
            // The four-field (pre `like_requires_text_cast`) JSON still parses.
            like_requires_text_cast: false,
            string_backslash_escapes: false,
            double_quote_identifiers: false,
            bracket_identifiers: false,
        })
    );
    let value = serde_json::to_value(&parsed).expect("serialize");
    assert_eq!(value["sql_dialect"]["identifier_quote"], "\"");
    assert_eq!(value["sql_dialect"]["like_requires_text_cast"], false);
    let again: DriverCapabilities = serde_json::from_value(value).expect("re-parse");
    assert_eq!(again, parsed);
}

#[test]
fn sql_dialect_like_text_cast_is_serde_defaulted_and_round_trips() {
    let with_cast = OLD_CAPABILITIES_JSON.replace(
        r#""single_connection_pool":true"#,
        r#""single_connection_pool":true,"sql_dialect":{"identifier_quote":"\"","string_literal_quote":"'","supports_schemas":true,"database_qualified_names":false,"like_requires_text_cast":true}"#,
    );
    let parsed: DriverCapabilities = serde_json::from_str(&with_cast).expect("dialect parses");
    let dialect = parsed.sql_dialect.clone().expect("dialect");
    assert!(dialect.like_requires_text_cast);
    assert!(dialect.supports_schemas);

    let encoded = serde_json::to_string(&parsed).expect("serialize");
    let decoded: DriverCapabilities = serde_json::from_str(&encoded).expect("round trip");
    assert_eq!(decoded, parsed);

    // A dialect object written by an older driver has no such key: false.
    let old: SqlDialectInfo = serde_json::from_str(
        r#"{"identifier_quote":"`","string_literal_quote":"'","supports_schemas":false,"database_qualified_names":true}"#,
    )
    .expect("old dialect parses");
    assert!(!old.like_requires_text_cast);
    assert!(!old.string_backslash_escapes);
    assert!(!old.double_quote_identifiers);
    assert!(!old.bracket_identifiers);
    assert!(old.database_qualified_names);

    let mysql: SqlDialectInfo = serde_json::from_str(
        r#"{"identifier_quote":"`","string_literal_quote":"'","supports_schemas":false,"database_qualified_names":true,"string_backslash_escapes":true}"#,
    )
    .expect("dialect with the escape flag parses");
    assert!(mysql.string_backslash_escapes);
    assert!(!mysql.double_quote_identifiers && !mysql.bracket_identifiers);
    let encoded = serde_json::to_string(&mysql).expect("serialize");
    assert_eq!(
        serde_json::from_str::<SqlDialectInfo>(&encoded).unwrap(),
        mysql
    );

    let ansi: SqlDialectInfo = serde_json::from_str(
        r#"{"identifier_quote":"`","string_literal_quote":"'","supports_schemas":false,"database_qualified_names":true,"double_quote_identifiers":true,"bracket_identifiers":true}"#,
    )
    .expect("dialect with the identifier flags parses");
    assert!(ansi.double_quote_identifiers && ansi.bracket_identifiers);
    let encoded = serde_json::to_string(&ansi).expect("serialize");
    assert_eq!(
        serde_json::from_str::<SqlDialectInfo>(&encoded).unwrap(),
        ansi
    );
}

#[test]
fn mutation_kind_support_uses_typed_and_unknown_tokens() {
    let capabilities = DriverCapabilities::new(
        "x",
        "X",
        vec![
            "mutation.update".into(),
            "mutation.custom_op".into(),
            "database.create".into(),
        ],
    );
    assert!(capabilities.supports_mutation_kind("update"));
    assert!(!capabilities.supports_mutation_kind("delete"));
    assert!(capabilities.supports_mutation_kind("custom_op"));
    assert!(capabilities.supports_mutation_kind("create_database"));
    assert!(capabilities.has_any_mutation());
}

#[test]
fn navigator_page_preserves_cursor_and_opaque_payload() {
    let page = NavigatorPage {
        nodes: vec![NavigatorNode {
            id: "database:app".into(),
            kind: "database".into(),
            label: "app".into(),
            subtitle: None,
            icon: Some("database".into()),
            has_children: true,
            payload: serde_json::json!({"database": "app"}),
            actions: vec!["refresh".into()],
        }],
        next_cursor: Some("next-page".into()),
    };

    let encoded = serde_json::to_string(&page).expect("navigator page serializes");
    let decoded: NavigatorPage = serde_json::from_str(&encoded).expect("navigator page decodes");

    assert_eq!(decoded.next_cursor.as_deref(), Some("next-page"));
    assert_eq!(decoded.nodes[0].payload["database"], "app");
}

#[test]
fn query_page_keeps_operation_and_count_metadata() {
    let page = QueryPage {
        operation_id: OperationId(812),
        page_id: 42,
        columns: vec![PageColumn {
            id: "_id".into(),
            name: "_id".into(),
            data_type: "object_id".into(),
            nullable: false,
        }],
        row_count: 500,
        total_count: Some(13_240),
        total_is_exact: true,
        flags: PageFlags {
            truncated: false,
            cancelled: false,
            read_only: false,
        },
    };

    let value = serde_json::to_value(page).expect("query page serializes");

    assert_eq!(value["operation_id"], 812);
    assert_eq!(value["columns"][0]["type"], "object_id");
    assert_eq!(value["total_count"], 13_240);
}

#[test]
fn errors_have_stable_nested_envelope() {
    let envelope = DriverErrorEnvelope {
        ok: false,
        error: DriverError {
            code: "query.invalid".into(),
            message: "Invalid filter".into(),
            retryable: false,
        },
    };

    let value = serde_json::to_value(envelope).expect("error serializes");

    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "query.invalid");
    assert_eq!(value["error"]["retryable"], false);
}

#[test]
fn success_envelopes_are_generic() {
    let envelope = SuccessEnvelope {
        ok: true,
        value: NavigatorPage {
            nodes: Vec::new(),
            next_cursor: None,
        },
    };

    let value = serde_json::to_value(envelope).expect("success serializes");

    assert_eq!(value["ok"], true);
    assert_eq!(value["value"]["nodes"], serde_json::json!([]));
}

#[test]
fn requests_keep_driver_specific_payloads_opaque() {
    let navigator = NavigatorRequest {
        parent: Some("database:app".into()),
        database: Some("app".into()),
        payload: Some(serde_json::json!({"group": "collections"})),
        cursor: Some("page-2".into()),
        limit: 500,
    };
    let query = QueryRequest {
        kind: "mongo_find".into(),
        operation_id: OperationId(900),
        database: Some("app".into()),
        object: Some("invoices".into()),
        offset: 0,
        limit: 500,
        payload: serde_json::json!({
            "filter": {},
            "sort": {"createdAt": -1},
            "project": {"name": 1}
        }),
    };

    let navigator_value = serde_json::to_value(navigator).expect("navigator request serializes");
    let query_value = serde_json::to_value(query).expect("query request serializes");

    assert_eq!(navigator_value["payload"]["group"], "collections");
    assert_eq!(query_value["operation_id"], 900);
    assert_eq!(query_value["payload"]["sort"]["createdAt"], -1);
    assert_eq!(query_value["payload"]["project"]["name"], 1);
}

#[test]
fn row_fields_preserve_native_values_and_mutation_identity() {
    let row = RowFields {
        page_id: 42,
        row_index: 3,
        fields: vec![RowField {
            name: "_id".into(),
            native_type: "object_id".into(),
            display_text: "66d4f84d71c18f22d433b003".into(),
            value: serde_json::json!({"$oid": "66d4f84d71c18f22d433b003"}),
            editable: false,
        }],
        mutation_identity: serde_json::json!({"_id": {"$oid": "66d4f84d71c18f22d433b003"}}),
    };

    let value = serde_json::to_value(&row).expect("row fields serialize");
    let decoded: RowFields = serde_json::from_value(value.clone()).expect("row fields decode");

    assert_eq!(value["fields"][0]["native_type"], "object_id");
    assert_eq!(
        value["fields"][0]["value"]["$oid"],
        "66d4f84d71c18f22d433b003"
    );
    assert_eq!(decoded, row);
}

#[test]
fn mutation_contract_keeps_database_payload_opaque() {
    let request = MutationRequest {
        operation_id: OperationId(901),
        kind: "update".into(),
        database: Some("app".into()),
        object: Some("invoices".into()),
        identity: serde_json::json!({"_id": {"$oid": "66d4f84d71c18f22d433b003"}}),
        payload: serde_json::json!({"$set": {"status": "paid"}}),
    };
    let result = MutationResult {
        affected_rows: 1,
        invalidated_page_ids: vec![42, 43],
    };

    let request_value = serde_json::to_value(request).expect("mutation request serializes");
    let result_value = serde_json::to_value(result).expect("mutation result serializes");

    assert_eq!(request_value["operation_id"], 901);
    assert_eq!(request_value["payload"]["$set"]["status"], "paid");
    assert_eq!(
        result_value["invalidated_page_ids"],
        serde_json::json!([42, 43])
    );
}

#[test]
fn cancellation_is_operation_scoped() {
    let request = CancelRequest {
        operation_id: OperationId(77),
    };

    assert_eq!(
        serde_json::to_value(request).expect("cancel request serializes"),
        serde_json::json!({"operation_id": 77})
    );
}

#[test]
fn export_contract_supports_page_or_query_context() {
    let request = ExportRequest {
        operation_id: OperationId(902),
        format: "json".into(),
        destination: "/tmp/invoices.json".into(),
        page_id: Some(42),
        query: None,
        options: serde_json::json!({"extended_json": true}),
    };
    let result = ExportResult {
        rows_written: 500,
        bytes_written: 32_768,
    };

    let request_value = serde_json::to_value(request).expect("export request serializes");
    let result_value = serde_json::to_value(result).expect("export result serializes");

    assert_eq!(request_value["page_id"], 42);
    assert_eq!(request_value["options"]["extended_json"], true);
    assert_eq!(result_value["bytes_written"], 32_768);
}
