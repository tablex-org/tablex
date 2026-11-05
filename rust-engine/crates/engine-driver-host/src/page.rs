use engine_driver_api::{CPageLease, CPageSlice};
use std::sync::Arc;
use thiserror::Error;

use crate::loader::{DriverHandle, LoaderError};

#[derive(Debug, Error)]
pub enum PageError {
    #[error("invalid page metadata: {0}")]
    InvalidMetadata(String),
    #[error("page slice {offset}..{end} exceeds page length {length}")]
    OutOfBounds { offset: u64, end: u64, length: u64 },
    #[error(transparent)]
    Driver(#[from] LoaderError),
}

/// A reference-counted immutable page backed by one driver-owned allocation.
/// Clones share the backing and invoke the foreign release callback exactly
/// once, after the final page or slice is dropped.
pub struct PageLease {
    backing: Arc<PageBacking>,
}

struct PageBacking {
    raw: CPageLease,
    generation: Option<Arc<DriverHandle>>,
}

unsafe impl Send for PageBacking {}
unsafe impl Sync for PageBacking {}

impl PageLease {
    /// Adopt ownership of a foreign page. A rejected page is released before
    /// the error is returned, so callers must not release it again.
    pub fn from_raw(raw: CPageLease) -> Result<Self, PageError> {
        validate_raw(raw)?;
        Ok(Self {
            backing: Arc::new(PageBacking {
                raw,
                generation: None,
            }),
        })
    }

    pub(crate) fn from_raw_with_generation(
        raw: CPageLease,
        generation: Arc<DriverHandle>,
    ) -> Result<Self, PageError> {
        validate_raw(raw)?;
        generation.retain_page_lease();
        Ok(Self {
            backing: Arc::new(PageBacking {
                raw,
                generation: Some(generation),
            }),
        })
    }

    pub fn generation(&self) -> u64 {
        self.backing.raw.generation
    }

    pub fn len(&self) -> u64 {
        self.backing.raw.data_len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn slice_count(&self) -> u64 {
        self.backing.raw.slice_count
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.backing.as_bytes()
    }

    pub fn slice(&self, offset: u64, length: u64) -> Result<PageSlice, PageError> {
        let end = offset.checked_add(length).ok_or(PageError::OutOfBounds {
            offset,
            end: u64::MAX,
            length: self.len(),
        })?;
        if end > self.len() {
            return Err(PageError::OutOfBounds {
                offset,
                end,
                length: self.len(),
            });
        }
        Ok(PageSlice {
            backing: Arc::clone(&self.backing),
            offset,
            length,
        })
    }

    pub fn indexed_slice(&self, index: u64) -> Result<PageSlice, PageError> {
        if index >= self.slice_count() {
            return Err(PageError::OutOfBounds {
                offset: index,
                end: index.saturating_add(1),
                length: self.slice_count(),
            });
        }
        // SAFETY: the driver page contract keeps the index array alive with
        // the backing and validation bounds the index before this read.
        let range = unsafe { *self.backing.raw.slices.add(index as usize) };
        self.slice(range.offset, range.length)
    }
}

impl Clone for PageLease {
    fn clone(&self) -> Self {
        Self {
            backing: Arc::clone(&self.backing),
        }
    }
}

#[derive(Clone)]
pub struct PageSlice {
    backing: Arc<PageBacking>,
    offset: u64,
    length: u64,
}

impl PageSlice {
    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn len(&self) -> u64 {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: PageLease validation checked the base pointer and length;
        // this slice was checked against that immutable backing range.
        if self.length == 0 {
            return &[];
        }
        unsafe {
            std::slice::from_raw_parts(
                self.backing.raw.data.add(self.offset as usize),
                self.length as usize,
            )
        }
    }
}

impl PageBacking {
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: PageLease validation checked the base pointer and length;
        // the backing remains owned by this Arc until the returned borrow ends.
        if self.raw.data_len == 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(self.raw.data, self.raw.data_len as usize) }
    }
}

impl Drop for PageBacking {
    fn drop(&mut self) {
        if let Some(release) = self.raw.release {
            // SAFETY: ownership of release_context transferred with this raw
            // page and this is the sole final Arc drop.
            unsafe { release(self.raw.release_context) };
        }
        if let Some(generation) = &self.generation {
            generation.release_page_lease();
        }
    }
}

fn validate_raw(raw: CPageLease) -> Result<(), PageError> {
    if raw.release.is_none() {
        return reject_raw(
            raw,
            PageError::InvalidMetadata("missing release callback".into()),
        );
    }
    if raw.data_len > 0 && raw.data.is_null() {
        return reject_raw(
            raw,
            PageError::InvalidMetadata("non-empty page has a null data pointer".into()),
        );
    }
    if raw.data_len > isize::MAX as u64 {
        return reject_raw(
            raw,
            PageError::InvalidMetadata("page is too large for a Rust slice".into()),
        );
    }
    if raw.slice_count > 0 && raw.slices.is_null() {
        return reject_raw(
            raw,
            PageError::InvalidMetadata("non-empty slice index has a null pointer".into()),
        );
    }
    if raw.slice_count > 0
        && !(raw.slices as usize).is_multiple_of(std::mem::align_of::<CPageSlice>())
    {
        return reject_raw(
            raw,
            PageError::InvalidMetadata("slice index pointer is misaligned".into()),
        );
    }
    let max_slice_count = (isize::MAX as usize / std::mem::size_of::<CPageSlice>()) as u64;
    if raw.slice_count > max_slice_count {
        return reject_raw(
            raw,
            PageError::InvalidMetadata("slice index allocation is too large".into()),
        );
    }
    Ok(())
}

fn reject_raw(raw: CPageLease, error: PageError) -> Result<(), PageError> {
    if let Some(release) = raw.release {
        // SAFETY: the page contract transfers release ownership at entry;
        // rejecting metadata still requires exactly one cleanup callback.
        unsafe { release(raw.release_context) };
    }
    Err(error)
}
