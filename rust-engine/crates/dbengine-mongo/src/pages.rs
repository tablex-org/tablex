use bson::Document;
use engine_driver_api::{
    CCellText, CPageLease, CPageSlice, OperationId, PageColumn, PageFlags, QueryPage, RowFields,
};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;

use crate::models::document_row_fields;
use crate::render::{render_cell_text_batch, DEFAULT_MAX_BYTES_PER_CELL};

pub(crate) struct RenderedCellBatch {
    pub cells: Vec<CCellText>,
    pub text: Vec<u8>,
}

pub(crate) struct BsonPageCache {
    max_bytes: usize,
    next_page_id: u64,
    pages: HashMap<u64, BsonPage>,
    order: VecDeque<u64>,
    bytes: usize,
    // A pending old/new pair counts once logically, while both allocations stay live;
    // physical bytes can therefore temporarily reach 2x the logical cap. The number
    // of pages is not capped: every open result tab holds one, only bytes are.
    logical_bytes: usize,
}

pub(crate) const MAX_PAGE_BYTES: usize = 64 * 1024 * 1024;

struct BsonPage {
    connection_id: u64,
    documents: Vec<Document>,
    columns: Vec<PageColumn>,
    raw_data: Vec<u8>,
    raw_slices: Vec<CPageSlice>,
    bytes: usize,
    replaces: Option<u64>,
    /// `(database, collection)` of a find / index page. `None` for pages that
    /// can read several collections (aggregates), which any write may stale.
    namespace: Option<(String, String)>,
}

struct RawPageAllocation {
    data: Vec<u8>,
    slices: Vec<CPageSlice>,
}

unsafe extern "C" fn release_raw_page(context: *mut c_void) {
    if !context.is_null() {
        // SAFETY: context is created by acquire_raw_page and released once by
        // the host lease contract.
        unsafe { drop(Box::from_raw(context.cast::<RawPageAllocation>())) };
    }
}

fn native_type_id(native_type: &str) -> String {
    native_type.to_string()
}

fn union_columns(documents: &[Document]) -> Vec<PageColumn> {
    let mut names: Vec<String> = Vec::new();
    for document in documents {
        for (name, _value) in document.iter() {
            if !names.iter().any(|existing| existing == name) {
                names.push(name.into());
            }
        }
    }
    names
        .into_iter()
        .map(|name| {
            let data_type = column_native_type(documents, &name);
            PageColumn {
                id: name.clone(),
                name,
                data_type,
                nullable: true,
            }
        })
        .collect()
}

/// The BSON type of every value of the column, `number` when they are Int32 /
/// Int64 / Double mixed, `mixed` for any other mix. Null and missing values do
/// not take part (a column of only nulls is `null`).
fn column_native_type(documents: &[Document], name: &str) -> String {
    let mut types = documents
        .iter()
        .filter_map(|document| document.get(name))
        .map(crate::models::bson_native_type)
        .filter(|native_type| *native_type != "null");
    let Some(first) = types.next() else {
        return native_type_id("null");
    };
    let mut mixed = false;
    let mut numbers_only = matches!(first, "int32" | "int64" | "double");
    for native_type in types {
        if native_type != first {
            mixed = true;
            numbers_only &= matches!(native_type, "int32" | "int64" | "double");
        }
    }
    match (mixed, numbers_only) {
        (false, _) => native_type_id(first),
        (true, true) => native_type_id("number"),
        (true, false) => native_type_id("mixed"),
    }
}

fn document_bytes(documents: &[Document]) -> usize {
    documents
        .iter()
        .map(|doc| bson::to_vec(doc).map(|bytes| bytes.len()).unwrap_or(0))
        .sum()
}

fn encode_raw_documents(documents: &[Document]) -> (Vec<u8>, Vec<CPageSlice>) {
    let mut data = Vec::new();
    let mut slices = Vec::with_capacity(documents.len());
    for document in documents {
        let bytes = bson::to_vec(document).expect("validated BSON document serializes");
        let offset = data.len() as u64;
        let length = bytes.len() as u64;
        data.extend_from_slice(&bytes);
        slices.push(CPageSlice { offset, length });
    }
    (data, slices)
}

impl BsonPageCache {
    pub(crate) fn default() -> Self {
        Self::new(MAX_PAGE_BYTES)
    }

    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            next_page_id: 1,
            pages: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            logical_bytes: 0,
        }
    }

    pub(crate) fn insert(
        &mut self,
        operation_id: OperationId,
        documents: Vec<Document>,
        total_count: Option<u64>,
        total_is_exact: bool,
    ) -> Option<QueryPage> {
        self.insert_for_connection(0, operation_id, documents, total_count, total_is_exact)
    }

    pub(crate) fn insert_for_connection(
        &mut self,
        connection_id: u64,
        operation_id: OperationId,
        documents: Vec<Document>,
        total_count: Option<u64>,
        total_is_exact: bool,
    ) -> Option<QueryPage> {
        let (columns, raw_data, raw_slices, bytes) = Self::prepare_page(&documents);
        if self.logical_bytes.saturating_add(bytes) > self.max_bytes {
            return None;
        }
        let page = self.insert_prepared(
            connection_id,
            operation_id,
            documents,
            total_count,
            total_is_exact,
            columns,
            raw_data,
            raw_slices,
            bytes,
            None,
        );
        self.logical_bytes += bytes;
        Some(page)
    }

    pub(crate) fn insert_replacement_for_connection(
        &mut self,
        connection_id: u64,
        replacement_page_id: u64,
        operation_id: OperationId,
        documents: Vec<Document>,
        total_count: Option<u64>,
        total_is_exact: bool,
    ) -> Option<QueryPage> {
        let Some(old_page) = self.pages.get(&replacement_page_id) else {
            return self.insert_for_connection(
                connection_id,
                operation_id,
                documents,
                total_count,
                total_is_exact,
            );
        };
        if old_page.connection_id != connection_id
            || old_page.replaces.is_some()
            || self.replacement_child(replacement_page_id).is_some()
        {
            return None;
        }

        let old_bytes = old_page.bytes;
        let (columns, raw_data, raw_slices, bytes) = Self::prepare_page(&documents);
        let logical_bytes = self
            .logical_bytes
            .saturating_sub(old_bytes)
            .saturating_add(old_bytes.max(bytes));
        if logical_bytes > self.max_bytes {
            return None;
        }

        let page = self.insert_prepared(
            connection_id,
            operation_id,
            documents,
            total_count,
            total_is_exact,
            columns,
            raw_data,
            raw_slices,
            bytes,
            Some(replacement_page_id),
        );
        self.logical_bytes = logical_bytes;
        Some(page)
    }

    fn prepare_page(documents: &[Document]) -> (Vec<PageColumn>, Vec<u8>, Vec<CPageSlice>, usize) {
        let columns = union_columns(documents);
        let document_bytes = document_bytes(documents);
        let (raw_data, raw_slices) = encode_raw_documents(documents);
        let bytes = document_bytes.saturating_add(raw_data.len());
        (columns, raw_data, raw_slices, bytes)
    }

    fn insert_prepared(
        &mut self,
        connection_id: u64,
        operation_id: OperationId,
        documents: Vec<Document>,
        total_count: Option<u64>,
        total_is_exact: bool,
        columns: Vec<PageColumn>,
        raw_data: Vec<u8>,
        raw_slices: Vec<CPageSlice>,
        bytes: usize,
        replaces: Option<u64>,
    ) -> QueryPage {
        let row_count = documents.len() as u64;
        let page_id = self.next_page_id;
        self.next_page_id += 1;

        self.pages.insert(
            page_id,
            BsonPage {
                connection_id,
                documents,
                columns: columns.clone(),
                raw_data,
                raw_slices,
                bytes,
                replaces,
                namespace: None,
            },
        );
        self.order.push_back(page_id);
        self.bytes += bytes;

        QueryPage {
            operation_id,
            page_id,
            columns,
            row_count,
            total_count,
            total_is_exact,
            flags: PageFlags {
                truncated: false,
                cancelled: false,
                read_only: true,
            },
        }
    }

    fn replacement_child(&self, page_id: u64) -> Option<u64> {
        self.order.iter().find_map(|candidate| {
            self.pages
                .get(candidate)
                .and_then(|page| (page.replaces == Some(page_id)).then_some(*candidate))
        })
    }

    pub(crate) fn contains(&self, page_id: u64) -> bool {
        self.pages.contains_key(&page_id)
    }

    pub(crate) fn len(&self) -> usize {
        self.pages.len()
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn release(&mut self, page_id: u64) -> bool {
        let Some(page) = self.pages.get(&page_id) else {
            return false;
        };
        let page_bytes = page.bytes;
        let replaces = page.replaces;
        let child = (replaces.is_none()).then(|| self.replacement_child(page_id));
        let old_bytes = replaces.and_then(|old_id| self.pages.get(&old_id).map(|page| page.bytes));

        self.pages.remove(&page_id);
        self.order.retain(|id| *id != page_id);
        self.bytes = self.bytes.saturating_sub(page_bytes);
        match (replaces, child.flatten(), old_bytes) {
            (None, Some(child_id), _) => {
                if let Some(child_page) = self.pages.get_mut(&child_id) {
                    child_page.replaces = None;
                    self.logical_bytes = self
                        .logical_bytes
                        .saturating_sub(page_bytes.max(child_page.bytes))
                        .saturating_add(child_page.bytes);
                }
            }
            (Some(_), _, Some(old_bytes)) => {
                self.logical_bytes = self
                    .logical_bytes
                    .saturating_sub(old_bytes.max(page_bytes))
                    .saturating_add(old_bytes);
            }
            (Some(_), _, None) => {
                self.logical_bytes = self.logical_bytes.saturating_sub(page_bytes);
            }
            (None, None, _) => {
                self.logical_bytes = self.logical_bytes.saturating_sub(page_bytes);
            }
        }
        true
    }

    pub(crate) fn acquire_raw_page(&mut self, page_id: u64) -> Option<CPageLease> {
        self.acquire_raw_page_for_connection(0, page_id)
    }

    pub(crate) fn acquire_raw_page_for_connection(
        &mut self,
        connection_id: u64,
        page_id: u64,
    ) -> Option<CPageLease> {
        let page = self.pages.get_mut(&page_id)?;
        if page.connection_id != connection_id {
            return None;
        }
        let allocation = Box::new(RawPageAllocation {
            data: std::mem::take(&mut page.raw_data),
            slices: std::mem::take(&mut page.raw_slices),
        });
        let data_len = allocation.data.len() as u64;
        let slice_count = allocation.slices.len() as u64;
        let data = if allocation.data.is_empty() {
            std::ptr::null()
        } else {
            allocation.data.as_ptr()
        };
        let slices = if allocation.slices.is_empty() {
            std::ptr::null()
        } else {
            allocation.slices.as_ptr()
        };
        let context = Box::into_raw(allocation).cast::<c_void>();
        Some(CPageLease {
            data,
            data_len,
            generation: page_id,
            release: Some(release_raw_page),
            release_context: context,
            slices,
            slice_count,
        })
    }

    /// Records the collection a find / index page was read from, so a write
    /// elsewhere leaves it alone ([`Self::clear_namespace`]).
    pub(crate) fn set_namespace(&mut self, page_id: u64, database: &str, collection: &str) {
        if let Some(page) = self.pages.get_mut(&page_id) {
            page.namespace = Some((database.to_owned(), collection.to_owned()));
        }
    }

    /// Drops the connection's pages a write to `database.collection` can make
    /// stale: the pages read from that collection and those with no single
    /// known collection. Returns their ids.
    pub(crate) fn clear_namespace(
        &mut self,
        connection_id: u64,
        database: &str,
        collection: &str,
    ) -> Vec<u64> {
        let page_ids = self
            .order
            .iter()
            .copied()
            .filter(|page_id| {
                self.pages.get(page_id).is_some_and(|page| {
                    page.connection_id == connection_id
                        && page.namespace.as_ref().is_none_or(|(db, coll)| {
                            db == database && coll == collection
                        })
                })
            })
            .collect::<Vec<_>>();
        for page_id in &page_ids {
            self.release(*page_id);
        }
        page_ids
    }

    pub(crate) fn clear_connection(&mut self, connection_id: u64) -> Vec<u64> {
        let page_ids = self
            .order
            .iter()
            .copied()
            .filter(|page_id| {
                self.pages
                    .get(page_id)
                    .is_some_and(|page| page.connection_id == connection_id)
            })
            .collect::<Vec<_>>();
        for page_id in &page_ids {
            self.release(*page_id);
        }
        page_ids
    }

    pub(crate) fn clear(&mut self) -> Vec<u64> {
        let page_ids = self.order.iter().copied().collect();
        self.pages.clear();
        self.order.clear();
        self.bytes = 0;
        self.logical_bytes = 0;
        page_ids
    }

    pub(crate) fn inspect_row(&self, page_id: u64, row_index: usize) -> Option<RowFields> {
        let page = self.pages.get(&page_id)?;
        let document = page.documents.get(row_index)?;
        // `Null` when the page has no usable `_id` for this row (a projection
        // that dropped it): the row cannot be written, and no other filter is
        // ever invented for it.
        let mutation_identity =
            crate::models::mutation_identity(document).unwrap_or(serde_json::Value::Null);
        Some(RowFields {
            page_id,
            row_index: row_index as u64,
            fields: document_row_fields(document),
            mutation_identity,
        })
    }

    pub(crate) fn render_cells(
        &self,
        page_id: u64,
        row_start: usize,
        row_count: usize,
        column_start: usize,
        column_count: usize,
    ) -> Option<RenderedCellBatch> {
        let page = self.pages.get(&page_id)?;
        let column_names: Vec<String> = page
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect();
        let batch = render_cell_text_batch(
            &page.documents,
            &column_names,
            row_start,
            row_start.saturating_add(row_count),
            column_start,
            column_start.saturating_add(column_count),
            DEFAULT_MAX_BYTES_PER_CELL,
        );
        Some(RenderedCellBatch {
            cells: batch
                .cells
                .iter()
                .map(|cell| CCellText {
                    row: cell.row as u64,
                    column: cell.column as u64,
                    offset: cell.offset as u64,
                    length: cell.length as u64,
                    flags: cell.flags,
                })
                .collect(),
            text: batch.text_buffer,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::{doc, oid::ObjectId};

    fn sample_documents() -> Vec<Document> {
        vec![
            doc! { "_id": ObjectId::parse_str("66d4f84d0000000000000003").expect("id"), "name": "Ada", "age": 37 },
            doc! { "_id": ObjectId::parse_str("66d4f84d0000000000000004").expect("id"), "name": "Lin", "active": true },
        ]
    }

    #[test]
    fn a_write_drops_only_the_pages_of_its_collection_and_unscoped_ones() {
        let mut cache = BsonPageCache::new(1_000_000);
        let mut page = |connection: u64, namespace: Option<(&str, &str)>| {
            let page_id = cache
                .insert_for_connection(connection, OperationId(1), sample_documents(), None, true)
                .expect("page fits cache")
                .page_id;
            if let Some((database, collection)) = namespace {
                cache.set_namespace(page_id, database, collection);
            }
            page_id
        };
        let same = page(1, Some(("app", "users")));
        let other_collection = page(1, Some(("app", "orders")));
        let other_database = page(1, Some(("reports", "users")));
        let aggregate = page(1, None);
        let other_connection = page(2, Some(("app", "users")));

        let dropped = cache.clear_namespace(1, "app", "users");

        assert_eq!(dropped, [same, aggregate]);
        assert!(!cache.contains(same) && !cache.contains(aggregate));
        assert!(cache.contains(other_collection));
        assert!(cache.contains(other_database));
        assert!(cache.contains(other_connection));
    }

    #[test]
    fn inserts_page_with_union_columns_and_count_metadata() {
        let mut cache = BsonPageCache::new(1_000_000);
        let page = cache
            .insert(OperationId(9), sample_documents(), Some(200), false)
            .expect("page fits cache");

        assert_eq!(page.operation_id, OperationId(9));
        assert_eq!(page.row_count, 2);
        assert_eq!(page.total_count, Some(200));
        assert!(!page.total_is_exact);
        assert!(page.flags.read_only);
        assert_eq!(
            page.columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["_id", "name", "age", "active"]
        );
    }

    #[test]
    fn renders_only_requested_viewport_into_one_text_buffer() {
        let mut cache = BsonPageCache::new(1_000_000);
        let page = cache
            .insert(OperationId(9), sample_documents(), None, true)
            .expect("page fits cache");
        let batch = cache
            .render_cells(page.page_id, 0, 2, 1, 1)
            .expect("page exists");

        assert_eq!(batch.cells.len(), 2);
        assert_eq!(batch.text, b"AdaLin");
        assert_eq!(batch.cells[0].offset, 0);
        assert_eq!(batch.cells[0].length, 3);
        assert_eq!(batch.cells[1].offset, 3);
        assert_eq!(batch.cells[1].length, 3);
    }

    #[test]
    fn inspects_native_fields_and_uses_bson_id_as_mutation_identity() {
        let mut cache = BsonPageCache::new(1_000_000);
        let page = cache
            .insert(OperationId(9), sample_documents(), None, true)
            .expect("page fits cache");
        let row = cache.inspect_row(page.page_id, 0).expect("row exists");

        assert_eq!(row.fields[0].native_type, "object_id");
        assert_eq!(
            row.mutation_identity["_id"]["$oid"],
            "66d4f84d0000000000000003"
        );
    }

    #[test]
    fn a_row_without_id_has_no_identity_and_no_editable_field() {
        let mut cache = BsonPageCache::new(1_000_000);
        let page = cache
            .insert(
                OperationId(9),
                vec![doc! { "name": "Ada" }, doc! { "_id": 5_i64, "name": "Lin" }],
                None,
                true,
            )
            .expect("page fits cache");
        let projected = cache.inspect_row(page.page_id, 0).expect("row exists");
        assert!(
            projected.mutation_identity.is_null(),
            "no invented rowIndex filter"
        );
        assert!(projected.fields.iter().all(|field| !field.editable));
        let whole = cache.inspect_row(page.page_id, 1).expect("row exists");
        assert_eq!(
            whole.mutation_identity,
            serde_json::json!({"_id": {"$numberLong": "5"}})
        );
    }

    #[test]
    fn column_types_report_mixed_columns_honestly() {
        let documents = vec![
            doc! { "a": 1, "b": 1, "c": "x", "d": bson::Bson::Null, "e": 1 },
            doc! { "a": 2_i64, "b": 1, "c": 2, "d": bson::Bson::Null },
            doc! { "a": 1.5, "b": bson::Bson::Null, "e": 2 },
        ];
        let types: Vec<_> = union_columns(&documents)
            .into_iter()
            .map(|column| (column.name, column.data_type))
            .collect();
        assert_eq!(
            types,
            [
                ("a".into(), "number".into()),
                ("b".into(), "int32".into()),
                ("c".into(), "mixed".into()),
                ("d".into(), "null".into()),
                ("e".into(), "int32".into()),
            ]
        );
    }

    #[test]
    fn refuses_a_page_past_the_byte_budget_and_keeps_the_others() {
        let document_bytes = bson::to_vec(&doc! { "value": "x".repeat(64) })
            .expect("serialize")
            .len();
        let mut cache = BsonPageCache::new(document_bytes * 4);
        let first = cache
            .insert(
                OperationId(1),
                vec![doc! { "value": "x".repeat(64) }],
                None,
                true,
            )
            .expect("first page fits cache");
        let second = cache
            .insert(
                OperationId(2),
                vec![doc! { "value": "x".repeat(64) }],
                None,
                true,
            )
            .expect("second page fits cache");
        let third = cache.insert(
            OperationId(3),
            vec![doc! { "value": "x".repeat(64) }],
            None,
            true,
        );

        assert!(third.is_none());
        assert!(cache.contains(first.page_id));
        assert!(cache.contains(second.page_id));
        assert_eq!(cache.len(), 2);
        assert!(cache.bytes() <= document_bytes * 4);
    }

    #[test]
    fn the_number_of_live_pages_is_not_capped() {
        let mut cache = BsonPageCache::new(1_000_000);
        let pages: Vec<_> = (0..100)
            .map(|n| {
                cache
                    .insert(OperationId(n), vec![doc! { "n": n as i64 }], None, true)
                    .expect("a small page fits whatever the number of open pages")
            })
            .collect();
        assert_eq!(cache.len(), 100);
        assert!(pages.iter().all(|page| cache.contains(page.page_id)));
    }

    #[test]
    fn release_removes_page_and_storage_budget() {
        let mut cache = BsonPageCache::new(1_000_000);
        let page = cache
            .insert(OperationId(1), sample_documents(), None, true)
            .expect("page fits cache");

        assert!(cache.release(page.page_id));
        assert!(!cache.contains(page.page_id));
        assert_eq!(cache.bytes(), 0);
        assert!(!cache.release(page.page_id));
    }

    #[test]
    fn raw_page_exports_one_backing_buffer_and_document_slices() {
        let mut cache = BsonPageCache::new(1_000_000);
        let page = cache
            .insert(OperationId(1), sample_documents(), None, true)
            .expect("page fits cache");
        let raw = cache.acquire_raw_page(page.page_id).expect("raw page");

        let bytes = unsafe { std::slice::from_raw_parts(raw.data, raw.data_len as usize) };
        let slices = unsafe { std::slice::from_raw_parts(raw.slices, raw.slice_count as usize) };
        assert_eq!(slices.len(), 2);
        for slice in slices {
            let start = slice.offset as usize;
            let end = start + slice.length as usize;
            assert_eq!(
                bson::Document::from_reader(&bytes[start..end]).is_ok(),
                true
            );
        }
        assert_eq!(
            bytes.len(),
            slices
                .iter()
                .map(|slice| slice.length as usize)
                .sum::<usize>()
        );

        let release = raw.release.expect("release callback");
        unsafe { release(raw.release_context) };
    }

    #[test]
    fn oversized_page_is_reported_without_indexing_an_evicted_page() {
        let mut cache = BsonPageCache::new(1);
        let page = cache.insert(OperationId(1), vec![doc! { "value": "x" }], Some(1), true);

        assert!(page.is_none());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.bytes(), 0);
    }

    #[test]
    fn connection_clear_removes_only_owned_pages() {
        let mut cache = BsonPageCache::new(1_000_000);
        let first = cache
            .insert_for_connection(7, OperationId(1), sample_documents(), None, true)
            .expect("first page fits cache");
        let second = cache
            .insert_for_connection(8, OperationId(2), sample_documents(), None, true)
            .expect("second page fits cache");

        let cleared = cache.clear_connection(7);

        assert_eq!(cleared, vec![first.page_id]);
        assert!(!cache.contains(first.page_id));
        assert!(cache.contains(second.page_id));
    }

    #[test]
    fn replacement_fits_when_count_and_bytes_are_full() {
        let documents = vec![doc! { "value": "x".repeat(64) }];
        let page_bytes = document_bytes(&documents) + encode_raw_documents(&documents).0.len();
        let mut cache = BsonPageCache::new(page_bytes * 8);
        let mut pages = Vec::new();
        for operation_id in 0..8 {
            pages.push(
                cache
                    .insert_for_connection(
                        7,
                        OperationId(operation_id),
                        documents.clone(),
                        None,
                        true,
                    )
                    .expect("page fits cache"),
            );
        }

        let replacement = cache.insert_replacement_for_connection(
            7,
            pages[0].page_id,
            OperationId(9),
            documents,
            None,
            true,
        );

        assert!(replacement.is_some());
        assert_eq!(cache.len(), 9);
        assert!(cache.bytes() <= page_bytes * 16);
        assert!(cache.contains(pages[0].page_id));
    }

    #[test]
    fn rejected_replacement_keeps_old_page_inspectable() {
        let old_documents = sample_documents();
        let page_bytes =
            document_bytes(&old_documents) + encode_raw_documents(&old_documents).0.len();
        let mut cache = BsonPageCache::new(page_bytes);
        let old = cache
            .insert_for_connection(7, OperationId(1), old_documents, None, true)
            .expect("old page fits");

        assert!(cache
            .insert_replacement_for_connection(
                7,
                old.page_id,
                OperationId(2),
                vec![doc! { "value": "x".repeat(1024) }],
                None,
                true,
            )
            .is_none());
        assert!(cache.contains(old.page_id));
        assert!(cache.inspect_row(old.page_id, 0).is_some());
    }

    #[test]
    fn releasing_aborted_replacement_keeps_old_page_inspectable() {
        let mut cache = BsonPageCache::new(1_000_000);
        let old = cache
            .insert_for_connection(7, OperationId(1), sample_documents(), None, true)
            .expect("old page fits");
        let new = cache
            .insert_replacement_for_connection(
                7,
                old.page_id,
                OperationId(2),
                sample_documents(),
                None,
                true,
            )
            .expect("replacement fits");

        assert!(cache.release(new.page_id));
        assert!(cache.contains(old.page_id));
        assert!(cache.inspect_row(old.page_id, 0).is_some());
    }

    #[test]
    fn releasing_committed_old_page_keeps_new_page_for_next_replacement() {
        let mut cache = BsonPageCache::new(1_000_000);
        let old = cache
            .insert_for_connection(7, OperationId(1), sample_documents(), None, true)
            .expect("old page fits");
        let new = cache
            .insert_replacement_for_connection(
                7,
                old.page_id,
                OperationId(2),
                sample_documents(),
                None,
                true,
            )
            .expect("replacement fits");

        assert!(cache.release(old.page_id));
        assert!(cache.contains(new.page_id));
        let next = cache.insert_replacement_for_connection(
            7,
            new.page_id,
            OperationId(3),
            sample_documents(),
            None,
            true,
        );
        assert!(next.is_some());
        assert!(cache.contains(new.page_id));
    }

    #[test]
    fn replacement_rejects_chains_and_siblings() {
        let mut cache = BsonPageCache::new(1_000_000);
        let old = cache
            .insert_for_connection(7, OperationId(1), sample_documents(), None, true)
            .expect("old page fits");
        let new = cache
            .insert_replacement_for_connection(
                7,
                old.page_id,
                OperationId(2),
                sample_documents(),
                None,
                true,
            )
            .expect("replacement fits");

        assert!(cache
            .insert_replacement_for_connection(
                7,
                old.page_id,
                OperationId(3),
                sample_documents(),
                None,
                true,
            )
            .is_none());
        assert!(cache
            .insert_replacement_for_connection(
                7,
                new.page_id,
                OperationId(4),
                sample_documents(),
                None,
                true,
            )
            .is_none());
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn a_replacement_cannot_replace_another_connections_page() {
        let mut cache = BsonPageCache::new(1_000_000);
        let page = cache
            .insert_for_connection(7, OperationId(1), sample_documents(), None, true)
            .expect("page fits");

        assert!(cache
            .insert_replacement_for_connection(
                8,
                page.page_id,
                OperationId(2),
                sample_documents(),
                None,
                true,
            )
            .is_none());
        assert!(cache.contains(page.page_id));
        assert_eq!(cache.len(), 1);
    }
}
