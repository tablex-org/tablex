use std::ffi::c_void;
use std::fmt::{Display, Formatter};

pub const ARROW_PAGE_ABI_VERSION: u32 = 1;

pub type ArrowReleaseFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void);
pub type AcquireArrowPageFn = unsafe extern "C" fn(u64, u64, u64, u64) -> CArrowPageLease;
pub type ArrowFunctionTableFn = unsafe extern "C" fn() -> *const CArrowFunctionTable;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CArrowPageLease {
    pub schema: *mut c_void,
    pub array: *mut c_void,
    pub row_count: u64,
    pub column_count: u64,
    pub generation: u64,
    pub release: Option<ArrowReleaseFn>,
    pub release_context: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CArrowFunctionTable {
    pub struct_size: u32,
    pub abi_version: u32,
    pub acquire_page: Option<AcquireArrowPageFn>,
}

pub const ARROW_FUNCTION_TABLE_HEADER_SIZE: usize =
    std::mem::size_of::<u32>() + std::mem::size_of::<u32>();

#[derive(Debug, PartialEq, Eq)]
pub enum ArrowFunctionTableError {
    UnsupportedVersion { found: u32, supported: u32 },
    TruncatedHeader,
    MissingAcquirePage,
}

impl Display for ArrowFunctionTableError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion { found, supported } => write!(
                formatter,
                "unsupported Arrow page ABI version {found}; host supports {supported}"
            ),
            Self::TruncatedHeader => {
                formatter.write_str("Arrow function table is smaller than its header")
            }
            Self::MissingAcquirePage => {
                formatter.write_str("Arrow function table contains no acquire_page function")
            }
        }
    }
}

impl std::error::Error for ArrowFunctionTableError {}

pub fn validate_arrow_function_table(
    table: &CArrowFunctionTable,
) -> Result<(), ArrowFunctionTableError> {
    if table.abi_version != ARROW_PAGE_ABI_VERSION {
        return Err(ArrowFunctionTableError::UnsupportedVersion {
            found: table.abi_version,
            supported: ARROW_PAGE_ABI_VERSION,
        });
    }
    if table.struct_size < ARROW_FUNCTION_TABLE_HEADER_SIZE as u32 {
        return Err(ArrowFunctionTableError::TruncatedHeader);
    }
    if table.struct_size as usize >= std::mem::size_of::<CArrowFunctionTable>()
        && table.acquire_page.is_none()
    {
        return Err(ArrowFunctionTableError::MissingAcquirePage);
    }
    Ok(())
}
