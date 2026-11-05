use engine_driver_api::{
    validate_arrow_function_table, validate_page_function_table, ArrowFunctionTableError,
    CArrowFunctionTable, CArrowPageLease, CCellText, CCellTextBatch, CPageFunctionTable,
    CPageLease, PageFunctionTableError, ARROW_FUNCTION_TABLE_HEADER_SIZE, ARROW_PAGE_ABI_VERSION,
    PAGE_ABI_VERSION, PAGE_FUNCTION_TABLE_HEADER_SIZE,
};
use std::mem::{align_of, size_of};

#[test]
fn cell_text_layout_is_stable_on_64_bit_macos() {
    assert_eq!(size_of::<CCellText>(), 40);
    assert_eq!(align_of::<CCellText>(), 8);
}

#[test]
fn cell_text_batch_layout_is_stable_on_64_bit_macos() {
    assert_eq!(size_of::<CCellTextBatch>(), 32);
    assert_eq!(align_of::<CCellTextBatch>(), 8);
}

#[test]
fn page_lease_layout_is_stable_on_64_bit_macos() {
    assert_eq!(size_of::<CPageLease>(), 56);
    assert_eq!(align_of::<CPageLease>(), 8);
}

#[test]
fn page_function_table_accepts_header_only_fallback() {
    let table = CPageFunctionTable {
        struct_size: PAGE_FUNCTION_TABLE_HEADER_SIZE as u32,
        abi_version: PAGE_ABI_VERSION,
        acquire_page: None,
    };
    validate_page_function_table(&table).expect("header-only table uses v2 fallback");
}

#[test]
fn page_function_table_rejects_newer_abi() {
    let table = CPageFunctionTable {
        struct_size: std::mem::size_of::<CPageFunctionTable>() as u32,
        abi_version: PAGE_ABI_VERSION + 1,
        acquire_page: None,
    };
    assert!(matches!(
        validate_page_function_table(&table),
        Err(PageFunctionTableError::UnsupportedVersion { .. })
    ));
}

#[test]
fn arrow_page_lease_layout_is_stable_on_64_bit_macos() {
    assert_eq!(std::mem::size_of::<CArrowPageLease>(), 56);
    assert_eq!(std::mem::align_of::<CArrowPageLease>(), 8);
}

#[test]
fn arrow_function_table_accepts_header_only_fallback() {
    let table = CArrowFunctionTable {
        struct_size: ARROW_FUNCTION_TABLE_HEADER_SIZE as u32,
        abi_version: ARROW_PAGE_ABI_VERSION,
        acquire_page: None,
    };
    validate_arrow_function_table(&table).expect("header-only table uses cell fallback");
}

#[test]
fn arrow_function_table_rejects_newer_abi() {
    let table = CArrowFunctionTable {
        struct_size: std::mem::size_of::<CArrowFunctionTable>() as u32,
        abi_version: ARROW_PAGE_ABI_VERSION + 1,
        acquire_page: None,
    };
    assert!(matches!(
        validate_arrow_function_table(&table),
        Err(ArrowFunctionTableError::UnsupportedVersion { .. })
    ));
}
