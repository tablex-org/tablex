use std::ffi::c_void;

pub const PAGE_ABI_VERSION: u32 = 1;
pub const PAGE_FUNCTION_TABLE_HEADER_SIZE: usize = 8;

pub type PageReleaseFn = unsafe extern "C" fn(*mut c_void);
pub type AcquirePageFn = unsafe extern "C" fn(u64, u64, u64, u64) -> CPageLease;
pub type PageFunctionTableFn = unsafe extern "C" fn() -> *const CPageFunctionTable;

/// Immutable bytes owned by a driver generation until `release` is called.
/// Ownership of `release_context` transfers to the host when this value is
/// returned, including when the host rejects malformed metadata.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CPageLease {
    pub data: *const u8,
    pub data_len: u64,
    pub generation: u64,
    pub release: Option<PageReleaseFn>,
    pub release_context: *mut c_void,
    pub slices: *const CPageSlice,
    pub slice_count: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CPageSlice {
    pub offset: u64,
    pub length: u64,
}

/// Optional page API table. `struct_size` lets a newer driver append fields
/// without making an older host read past the table it received.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CPageFunctionTable {
    pub struct_size: u32,
    pub abi_version: u32,
    pub acquire_page: Option<AcquirePageFn>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageFunctionTableError {
    UnsupportedVersion { found: u32, expected: u32 },
    StructTooSmall { found: usize, minimum: usize },
    MissingAcquireFunction,
}

pub fn validate_page_function_table(
    table: &CPageFunctionTable,
) -> Result<(), PageFunctionTableError> {
    if table.abi_version != PAGE_ABI_VERSION {
        return Err(PageFunctionTableError::UnsupportedVersion {
            found: table.abi_version,
            expected: PAGE_ABI_VERSION,
        });
    }

    let struct_size = table.struct_size as usize;
    if struct_size < PAGE_FUNCTION_TABLE_HEADER_SIZE {
        return Err(PageFunctionTableError::StructTooSmall {
            found: struct_size,
            minimum: PAGE_FUNCTION_TABLE_HEADER_SIZE,
        });
    }

    if struct_size >= std::mem::size_of::<CPageFunctionTable>() && table.acquire_page.is_none() {
        return Err(PageFunctionTableError::MissingAcquireFunction);
    }

    Ok(())
}
