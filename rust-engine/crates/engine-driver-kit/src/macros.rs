/// Emits every `#[no_mangle] extern "C"` symbol of an ABI v2 driver, backed by
/// the [`DriverBackend`](crate::DriverBackend) implementation `$backend`.
///
/// ```ignore
/// #[cfg(feature = "abi-v2")]
/// engine_driver_kit::export_driver!(MyBackend; cancel_operation);
/// ```
///
/// Always emitted: `dbengine_abi_version`, `dbengine_capabilities`,
/// `dbengine_free_string`, `dbengine_init`, `dbengine_shutdown`,
/// `dbengine_connect`, `dbengine_disconnect`, `dbengine_list_nav_nodes`,
/// `dbengine_open_query`, `dbengine_render_cell_batch`,
/// `dbengine_free_cell_text_batch`, `dbengine_inspect_row`,
/// `dbengine_release_page`, `dbengine_arrow_function_table` and
/// `dbengine_acquire_arrow_page`.
///
/// Optional (list the ones the driver's capabilities need, after a `;`):
/// `cancel_operation` (`operation.cancel`), `mutate` (`mutation.*`),
/// `export` (`export.*`), `describe_object` (`metadata.table_properties`).
/// `dbengine_init` rejects a list that disagrees with the capabilities.
///
/// Invoke it once per crate, at the crate root, in the 2021 edition. It does not
/// emit `dbengine_page_function_table` (byte-page leases are Mongo-specific).
///
/// Every entry point catches panics and converts them to an error envelope
/// (`<op>.internal`), an empty Arrow lease, or a null/0 result where the ABI has
/// no error channel (see [`Driver`](crate::Driver) for each case).
#[macro_export]
macro_rules! export_driver {
    ($backend:ty $(; $($symbol:ident),+ $(,)?)?) => {
        static __ENGINE_DRIVER_KIT: $crate::Driver<$backend> = $crate::Driver::new(
            $crate::ExportedSymbols::NONE
                $($(.union($crate::__exported_symbol_flag!($symbol)))+)?
        );

        static __ENGINE_DRIVER_KIT_ARROW: $crate::api::CArrowFunctionTable =
            $crate::api::CArrowFunctionTable {
                struct_size: ::std::mem::size_of::<$crate::api::CArrowFunctionTable>() as u32,
                abi_version: $crate::api::ARROW_PAGE_ABI_VERSION,
                acquire_page: Some(dbengine_acquire_arrow_page),
            };

        #[no_mangle]
        pub extern "C" fn dbengine_abi_version() -> u32 {
            __ENGINE_DRIVER_KIT.abi_version()
        }

        #[no_mangle]
        pub extern "C" fn dbengine_capabilities() -> *mut ::std::ffi::c_char {
            __ENGINE_DRIVER_KIT.capabilities()
        }

        #[no_mangle]
        pub extern "C" fn dbengine_arrow_function_table() -> *const $crate::api::CArrowFunctionTable {
            &__ENGINE_DRIVER_KIT_ARROW
        }

        #[no_mangle]
        pub extern "C" fn dbengine_acquire_arrow_page(
            connection_id: u64,
            page_id: u64,
            row_start: u64,
            row_count: u64,
        ) -> $crate::api::CArrowPageLease {
            __ENGINE_DRIVER_KIT.acquire_arrow_page(connection_id, page_id, row_start, row_count)
        }

        #[no_mangle]
        pub extern "C" fn dbengine_init() -> *mut ::std::ffi::c_char {
            __ENGINE_DRIVER_KIT.init()
        }

        #[no_mangle]
        pub extern "C" fn dbengine_shutdown() -> *mut ::std::ffi::c_char {
            __ENGINE_DRIVER_KIT.shutdown()
        }

        /// # Safety
        /// `request_json` must be null or point to a valid NUL-terminated string.
        #[no_mangle]
        #[allow(clippy::missing_safety_doc)]
        pub unsafe extern "C" fn dbengine_connect(
            request_json: *const ::std::ffi::c_char,
        ) -> *mut ::std::ffi::c_char {
            // SAFETY: the caller upholds this function's own contract.
            unsafe { __ENGINE_DRIVER_KIT.connect(request_json) }
        }

        #[no_mangle]
        pub extern "C" fn dbengine_disconnect(connection_id: u64) -> *mut ::std::ffi::c_char {
            __ENGINE_DRIVER_KIT.disconnect(connection_id)
        }

        /// # Safety
        /// `request_json` must be null or point to a valid NUL-terminated string.
        #[no_mangle]
        #[allow(clippy::missing_safety_doc)]
        pub unsafe extern "C" fn dbengine_list_nav_nodes(
            connection_id: u64,
            request_json: *const ::std::ffi::c_char,
        ) -> *mut ::std::ffi::c_char {
            // SAFETY: the caller upholds this function's own contract.
            unsafe { __ENGINE_DRIVER_KIT.list_nav_nodes(connection_id, request_json) }
        }

        /// # Safety
        /// `request_json` must be null or point to a valid NUL-terminated string.
        #[no_mangle]
        #[allow(clippy::missing_safety_doc)]
        pub unsafe extern "C" fn dbengine_open_query(
            connection_id: u64,
            request_json: *const ::std::ffi::c_char,
        ) -> *mut ::std::ffi::c_char {
            // SAFETY: the caller upholds this function's own contract.
            unsafe { __ENGINE_DRIVER_KIT.open_query(connection_id, request_json) }
        }

        /// # Safety
        /// `out_batch` must be null or writable for one pointer; `out_error`
        /// must be null or writable for one pointer.
        #[no_mangle]
        #[allow(clippy::missing_safety_doc)]
        pub unsafe extern "C" fn dbengine_render_cell_batch(
            page_id: u64,
            row_start: u64,
            row_count: u64,
            column_start: u64,
            column_count: u64,
            out_batch: *mut *mut $crate::api::CCellTextBatch,
            out_error: *mut *mut ::std::ffi::c_char,
        ) -> i32 {
            // SAFETY: the caller upholds this function's own contract.
            unsafe {
                __ENGINE_DRIVER_KIT.render_cell_batch(
                    page_id,
                    row_start,
                    row_count,
                    column_start,
                    column_count,
                    out_batch,
                    out_error,
                )
            }
        }

        /// # Safety
        /// `batch` must be null or a batch returned by `dbengine_render_cell_batch`
        /// that has not been freed.
        #[no_mangle]
        #[allow(clippy::missing_safety_doc)]
        pub unsafe extern "C" fn dbengine_free_cell_text_batch(
            batch: *mut $crate::api::CCellTextBatch,
        ) {
            // SAFETY: the caller upholds this function's own contract.
            unsafe { __ENGINE_DRIVER_KIT.free_cell_text_batch(batch) }
        }

        #[no_mangle]
        pub extern "C" fn dbengine_inspect_row(
            page_id: u64,
            row_index: u64,
        ) -> *mut ::std::ffi::c_char {
            __ENGINE_DRIVER_KIT.inspect_row(page_id, row_index)
        }

        #[no_mangle]
        pub extern "C" fn dbengine_release_page(page_id: u64) -> *mut ::std::ffi::c_char {
            __ENGINE_DRIVER_KIT.release_page(page_id)
        }

        /// # Safety
        /// `value` must be null or a string returned by this driver that has not
        /// been freed.
        #[no_mangle]
        #[allow(clippy::missing_safety_doc)]
        pub unsafe extern "C" fn dbengine_free_string(value: *mut ::std::ffi::c_char) {
            // SAFETY: the caller upholds this function's own contract.
            unsafe { __ENGINE_DRIVER_KIT.free_string(value) }
        }

        $($($crate::__export_optional_symbol!($symbol);)+)?
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __exported_symbol_flag {
    (cancel_operation) => {
        $crate::ExportedSymbols::CANCEL_OPERATION
    };
    (mutate) => {
        $crate::ExportedSymbols::MUTATE
    };
    (export) => {
        $crate::ExportedSymbols::EXPORT
    };
    (describe_object) => {
        $crate::ExportedSymbols::DESCRIBE_OBJECT
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __export_optional_symbol {
    (cancel_operation) => {
        /// # Safety
        /// `request_json` must be null or point to a valid NUL-terminated string.
        #[no_mangle]
        #[allow(clippy::missing_safety_doc)]
        pub unsafe extern "C" fn dbengine_cancel_operation(
            connection_id: u64,
            request_json: *const ::std::ffi::c_char,
        ) -> *mut ::std::ffi::c_char {
            // SAFETY: the caller upholds this function's own contract.
            unsafe { __ENGINE_DRIVER_KIT.cancel_operation(connection_id, request_json) }
        }
    };
    (mutate) => {
        /// # Safety
        /// `request_json` must be null or point to a valid NUL-terminated string.
        #[no_mangle]
        #[allow(clippy::missing_safety_doc)]
        pub unsafe extern "C" fn dbengine_mutate(
            connection_id: u64,
            request_json: *const ::std::ffi::c_char,
        ) -> *mut ::std::ffi::c_char {
            // SAFETY: the caller upholds this function's own contract.
            unsafe { __ENGINE_DRIVER_KIT.mutate(connection_id, request_json) }
        }
    };
    (export) => {
        /// # Safety
        /// `request_json` must be null or point to a valid NUL-terminated string.
        #[no_mangle]
        #[allow(clippy::missing_safety_doc)]
        pub unsafe extern "C" fn dbengine_export(
            connection_id: u64,
            request_json: *const ::std::ffi::c_char,
        ) -> *mut ::std::ffi::c_char {
            // SAFETY: the caller upholds this function's own contract.
            unsafe { __ENGINE_DRIVER_KIT.export(connection_id, request_json) }
        }
    };
    (describe_object) => {
        /// # Safety
        /// `request_json` must be null or point to a valid NUL-terminated string.
        #[no_mangle]
        #[allow(clippy::missing_safety_doc)]
        pub unsafe extern "C" fn dbengine_describe_object(
            connection_id: u64,
            request_json: *const ::std::ffi::c_char,
        ) -> *mut ::std::ffi::c_char {
            // SAFETY: the caller upholds this function's own contract.
            unsafe { __ENGINE_DRIVER_KIT.describe_object(connection_id, request_json) }
        }
    };
}
