use arrow_array::ffi::{from_ffi, FFI_ArrowArray, FFI_ArrowSchema};
use arrow_array::{make_array, RecordBatch, StructArray};
use arrow_buffer::{BooleanBuffer, Buffer, NullBuffer};
use arrow_data::ArrayData;
use engine_driver_api::CArrowPageLease;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr::NonNull;
use std::sync::Arc;
use thiserror::Error;

use crate::loader::{DriverHandle, LoaderError};

#[derive(Debug, Error)]
pub enum ArrowPageError {
    #[error("invalid Arrow page metadata: {0}")]
    InvalidMetadata(String),
    #[error(transparent)]
    Driver(#[from] LoaderError),
}

pub struct ArrowPageLease {
    backing: Arc<ArrowPageBacking>,
}

struct ArrowPageBacking {
    raw: CArrowPageLease,
    generation: Option<Arc<ArrowLeaseOwner>>,
}

unsafe impl Send for ArrowPageBacking {}
unsafe impl Sync for ArrowPageBacking {}

pub struct ImportedArrowBatch {
    pub batch: RecordBatch,
    _generation: Option<Arc<ArrowLeaseOwner>>,
}

struct ArrowLeaseOwner {
    generation: Arc<DriverHandle>,
}

struct ArrowBufferOwner {
    _buffer: Buffer,
    _lease: Arc<ArrowLeaseOwner>,
}

impl std::fmt::Debug for ImportedArrowBatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImportedArrowBatch")
            .field("batch", &self.batch)
            .finish_non_exhaustive()
    }
}

impl ArrowPageLease {
    pub fn from_raw(raw: CArrowPageLease) -> Result<Self, ArrowPageError> {
        validate_raw(raw)?;
        Ok(Self {
            backing: Arc::new(ArrowPageBacking {
                raw,
                generation: None,
            }),
        })
    }

    pub(crate) fn from_raw_with_generation(
        raw: CArrowPageLease,
        generation: Arc<DriverHandle>,
    ) -> Result<Self, ArrowPageError> {
        validate_raw(raw)?;
        generation.retain_page_lease();
        Ok(Self {
            backing: Arc::new(ArrowPageBacking {
                raw,
                generation: Some(Arc::new(ArrowLeaseOwner { generation })),
            }),
        })
    }

    pub fn schema(&self) -> *mut c_void {
        self.backing.raw.schema
    }

    pub fn array(&self) -> *mut c_void {
        self.backing.raw.array
    }

    pub fn row_count(&self) -> u64 {
        self.backing.raw.row_count
    }

    pub fn column_count(&self) -> u64 {
        self.backing.raw.column_count
    }

    pub fn generation(&self) -> u64 {
        self.backing.raw.generation
    }

    /// Move the driver's C Data exports into Arrow without copying buffers.
    /// The lease must be uniquely owned because the raw C structs are moved
    /// out of the lease exactly once.
    ///
    /// # Safety
    /// The driver must have returned initialized, properly aligned Arrow C Data
    /// Interface schema and array pointers whose ownership is transferred by
    /// the lease release callback.
    pub unsafe fn into_record_batch(self) -> Result<ImportedArrowBatch, ArrowPageError> {
        let Self { backing } = self;
        let backing = Arc::try_unwrap(backing)
            .map_err(|_| ArrowPageError::InvalidMetadata("Arrow page is still shared".into()))?;
        let backing = ManuallyDrop::new(backing);
        let raw = backing.raw;
        // SAFETY: `backing` is never dropped after its fields are moved into
        // the imported Arrow buffers and release guard.
        let generation = unsafe { std::ptr::read(&backing.generation) };
        let _release = ArrowExportGuard { raw };
        // SAFETY: the driver contract exposes these pointers as owned,
        // initialized FFI_ArrowSchema/FFI_ArrowArray allocations. These APIs
        // replace the values with empty structs and transfer their contents.
        let schema = unsafe { FFI_ArrowSchema::from_raw(raw.schema.cast()) };
        let array = unsafe { FFI_ArrowArray::from_raw(raw.array.cast()) };
        // SAFETY: the driver produced a valid Arrow C Data Interface pair.
        let data = from_ffi(array, &schema).map_err(|error| {
            ArrowPageError::InvalidMetadata(format!("Arrow import failed: {error}"))
        })?;
        let data = if let Some(owner) = generation.as_ref() {
            retain_array_data(data, owner)
        } else {
            data
        };
        let array = make_array(data);
        let struct_array = array
            .as_any()
            .downcast_ref::<StructArray>()
            .ok_or_else(|| {
                ArrowPageError::InvalidMetadata("Arrow page root is not a struct".into())
            })?
            .clone();
        Ok(ImportedArrowBatch {
            batch: RecordBatch::from(struct_array),
            _generation: generation,
        })
    }
}

impl Clone for ArrowPageLease {
    fn clone(&self) -> Self {
        Self {
            backing: Arc::clone(&self.backing),
        }
    }
}

impl Drop for ArrowPageBacking {
    fn drop(&mut self) {
        release_raw(self.raw);
    }
}

impl Drop for ArrowLeaseOwner {
    fn drop(&mut self) {
        self.generation.release_page_lease();
    }
}

impl Drop for ArrowExportGuard {
    fn drop(&mut self) {
        release_raw(self.raw);
    }
}

struct ArrowExportGuard {
    raw: CArrowPageLease,
}

fn retain_array_data(data: ArrayData, owner: &Arc<ArrowLeaseOwner>) -> ArrayData {
    let buffers = data
        .buffers()
        .iter()
        .map(|buffer| retain_buffer(buffer, owner))
        .collect();
    let nulls = data.nulls().map(|nulls| {
        let buffer = retain_buffer(nulls.buffer(), owner);
        // SAFETY: the buffer and null count are copied unchanged from a valid
        // ArrayData produced by Arrow's C Data importer.
        unsafe {
            NullBuffer::new_unchecked(
                BooleanBuffer::new(buffer, nulls.offset(), nulls.len()),
                nulls.null_count(),
            )
        }
    });
    let child_data = data
        .child_data()
        .iter()
        .cloned()
        .map(|child| retain_array_data(child, owner))
        .collect();

    // SAFETY: all fields are copied from the valid input ArrayData; only the
    // buffer owners are replaced with wrappers that retain the same bytes.
    unsafe {
        ArrayData::builder(data.data_type().clone())
            .len(data.len())
            .offset(data.offset())
            .nulls(nulls)
            .buffers(buffers)
            .child_data(child_data)
            .build_unchecked()
    }
}

fn retain_buffer(buffer: &Buffer, lease: &Arc<ArrowLeaseOwner>) -> Buffer {
    let pointer =
        NonNull::new(buffer.as_ptr() as *mut u8).expect("Arrow buffer must have a data pointer");
    let owner = Arc::new(ArrowBufferOwner {
        _buffer: buffer.clone(),
        _lease: Arc::clone(lease),
    });
    // SAFETY: the cloned buffer keeps the allocation alive for the returned
    // view, and the pointer/length pair is copied without changing the bytes.
    unsafe { Buffer::from_custom_allocation(pointer, buffer.len(), owner) }
}

fn release_raw(raw: CArrowPageLease) {
    if let Some(release) = raw.release {
        // SAFETY: ownership was transferred with this lease and this is the
        // single cleanup path for the exported C data containers.
        unsafe { release(raw.schema, raw.array, raw.release_context) };
    }
}

fn validate_raw(raw: CArrowPageLease) -> Result<(), ArrowPageError> {
    if raw.release.is_none() {
        return Err(ArrowPageError::InvalidMetadata(
            "missing release callback".into(),
        ));
    }
    if raw.schema.is_null() {
        return reject_raw(
            raw,
            ArrowPageError::InvalidMetadata("null Arrow schema pointer".into()),
        );
    }
    if !(raw.schema as usize).is_multiple_of(std::mem::align_of::<FFI_ArrowSchema>()) {
        return reject_raw(
            raw,
            ArrowPageError::InvalidMetadata("Arrow schema pointer is misaligned".into()),
        );
    }
    if raw.array.is_null() {
        return reject_raw(
            raw,
            ArrowPageError::InvalidMetadata("null Arrow array pointer".into()),
        );
    }
    if !(raw.array as usize).is_multiple_of(std::mem::align_of::<FFI_ArrowArray>()) {
        return reject_raw(
            raw,
            ArrowPageError::InvalidMetadata("Arrow array pointer is misaligned".into()),
        );
    }
    Ok(())
}

fn reject_raw(raw: CArrowPageLease, error: ArrowPageError) -> Result<(), ArrowPageError> {
    release_raw(raw);
    Err(error)
}
