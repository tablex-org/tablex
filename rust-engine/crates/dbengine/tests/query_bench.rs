use std::time::Instant;

use bson::{doc, oid::ObjectId, DateTime, Document};
use dbengine::{dbengine_free_arrow_cdata, documents_to_arrow_cdata};

/// The columns of `documents`, in the order a reader discovers them.
const COLUMNS: [&str; 10] = [
    "_id", "name", "age", "balance", "active", "created", "tags", "address", "visits", "note",
];

/// `count` documents shaped like a real collection: every BSON kind the grid shows, a nested document,
/// an array, and two sparse fields (`visits` on even rows only, `note` on every tenth). Built in the test,
/// so the benchmark needs no fixture file.
fn documents(count: usize) -> Vec<Document> {
    (0..count)
        .map(|i| {
            let n = i as i64;
            let mut document = doc! {
                "_id": ObjectId::from_bytes([(i % 256) as u8; 12]),
                "name": format!("customer-{i:05}"),
                "age": (18 + i % 60) as i32,
                "balance": n as f64 * 12.5,
                "active": i % 3 != 0,
                "created": DateTime::from_millis(1_700_000_000_000 + n * 60_000),
                "tags": ["retail", if i % 2 == 0 { "even" } else { "odd" }],
                "address": { "city": format!("city-{}", i % 40), "zip": format!("{:05}", i % 99_999) },
            };
            if i % 2 == 0 {
                document.insert("visits", n * 3);
            }
            if i % 10 == 0 {
                document.insert("note", bson::Bson::Null);
            }
            document
        })
        .collect()
}

#[test]
fn test_benchmark_arrow_conversion() {
    let doc_count = 1000;
    let docs = documents(doc_count);

    let bench_start = Instant::now();
    let data = documents_to_arrow_cdata(&docs).expect("arrow conversion");
    let elapsed = bench_start.elapsed();
    println!(
        "arrow_convert: {:.2}ms ({} docs, {} cols)",
        elapsed.as_secs_f64() * 1000.0,
        data.row_count,
        data.col_count
    );

    assert_eq!(data.row_count, doc_count);
    assert_eq!(data.col_count, COLUMNS.len(), "the sparse fields are columns too");
    assert_eq!(data.column_names, COLUMNS);
    assert!(data.cursor.is_null(), "a plain conversion carries no paging cursor");
    // SAFETY: the pointers come from `documents_to_arrow_cdata` and are freed once.
    unsafe { dbengine_free_arrow_cdata(data.schema_ptr, data.array_ptr) };
}
