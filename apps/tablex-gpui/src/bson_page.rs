//! BSON documents as the page the grid reads: one `PageLease` of BSON slices and the columns found
//! in the documents. The MongoDB shell's result has no driver page, so the app builds this one
//! itself (on the worker thread that ran the script, never on the UI's).

use bson::{Bson, Document};
use engine_driver_api::{CPageLease, CPageSlice, PageColumn};
use engine_driver_host::{PageError, PageLease};
use std::ffi::c_void;
use std::fmt;

#[derive(Debug)]
pub enum BsonPageError {
    Encode(bson::ser::Error),
    TooLarge,
    Lease(PageError),
}

impl fmt::Display for BsonPageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encode(error) => write!(f, "a document cannot be encoded as BSON: {error}"),
            Self::TooLarge => f.write_str("the result is too large to show"),
            Self::Lease(error) => write!(f, "the result page is invalid: {error}"),
        }
    }
}

impl std::error::Error for BsonPageError {}

/// The BSON type name a column header shows.
pub fn native_type(value: &Bson) -> &'static str {
    match value {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Document(_) => "document",
        Bson::Boolean(_) => "boolean",
        Bson::Array(_) => "array",
        Bson::ObjectId(_) => "object_id",
        Bson::DateTime(_) => "date_time",
        Bson::Decimal128(_) => "decimal128",
        Bson::Null => "null",
        Bson::Int32(_) => "int32",
        Bson::Int64(_) => "int64",
        _ => "other",
    }
}

/// The columns of `documents` (first-seen order, typed by the first non-null value) and a page of
/// their BSON bytes. `generation` labels the page (any number unique to the result).
pub fn documents_page(
    documents: &[Document],
    generation: u64,
) -> Result<(Vec<PageColumn>, PageLease), BsonPageError> {
    #[cfg(all(test, feature = "ui"))]
    crate::mongo_shell::THREADS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(("documents_page", std::thread::current().id()));
    // One pass: a column per distinct field in first-seen order, typed by its first non-null value.
    let mut index: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut columns: Vec<PageColumn> = Vec::new();
    for document in documents {
        for (name, value) in document {
            let at = *index.entry(name.as_str()).or_insert_with(|| {
                columns.push(PageColumn {
                    id: name.as_str().into(),
                    name: name.as_str().into(),
                    data_type: "null".into(),
                    nullable: name != "_id",
                });
                columns.len() - 1
            });
            if !matches!(value, Bson::Null) && columns[at].data_type == "null" {
                columns[at].data_type = native_type(value).into();
            }
        }
    }
    let mut data = Vec::new();
    let mut slices = Vec::with_capacity(documents.len());
    for document in documents {
        let bytes = bson::to_vec(document).map_err(BsonPageError::Encode)?;
        slices.push(CPageSlice {
            offset: u64::try_from(data.len()).map_err(|_| BsonPageError::TooLarge)?,
            length: u64::try_from(bytes.len()).map_err(|_| BsonPageError::TooLarge)?,
        });
        data.extend_from_slice(&bytes);
    }
    let backing = Box::new(Backing { data, slices });
    let data_ptr = if backing.data.is_empty() { std::ptr::null() } else { backing.data.as_ptr() };
    let slices_ptr = if backing.slices.is_empty() { std::ptr::null() } else { backing.slices.as_ptr() };
    let (data_len, slice_count) = (backing.data.len() as u64, backing.slices.len() as u64);
    // The page owns the allocation: `release` frees it once, when the last slice is dropped.
    let context = Box::into_raw(backing).cast::<c_void>();
    let lease = PageLease::from_raw(CPageLease {
        data: data_ptr,
        data_len,
        generation,
        release: Some(release_backing),
        release_context: context,
        slices: slices_ptr,
        slice_count,
    })
    .map_err(BsonPageError::Lease)?;
    Ok((columns, lease))
}

struct Backing {
    data: Vec<u8>,
    slices: Vec<CPageSlice>,
}

unsafe extern "C" fn release_backing(context: *mut c_void) {
    if !context.is_null() {
        // SAFETY: the context is the `Box<Backing>` leaked above, handed to `PageLease`, which calls
        // this once, from its final owner (also when `from_raw` rejects the page).
        unsafe { drop(Box::from_raw(context.cast::<Backing>())) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    #[test]
    fn columns_follow_the_documents_and_the_page_holds_their_bytes() {
        let documents = vec![
            doc! { "_id": 1, "name": "a", "gone": Bson::Null },
            doc! { "_id": 2_i64, "extra": { "x": true }, "gone": 3.5 },
        ];
        let (columns, lease) = documents_page(&documents, 7).unwrap();
        let named: Vec<(&str, &str)> = columns.iter().map(|c| (c.name.as_str(), c.data_type.as_str())).collect();
        assert_eq!(
            named,
            [("_id", "int32"), ("name", "string"), ("gone", "double"), ("extra", "document")],
            "typed by the first non-null value"
        );
        assert_eq!(lease.generation(), 7);
        assert_eq!(lease.slice_count(), 2);
        let second = lease.indexed_slice(1).unwrap();
        let back: Document = bson::from_slice(second.as_bytes()).unwrap();
        assert_eq!(back, documents[1]);
    }

    #[test]
    fn no_documents_is_an_empty_page() {
        let (columns, lease) = documents_page(&[], 1).unwrap();
        assert!(columns.is_empty() && lease.slice_count() == 0 && lease.is_empty());
    }
}
