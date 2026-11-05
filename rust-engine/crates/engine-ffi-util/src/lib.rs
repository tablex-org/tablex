use std::ffi::CString;
use std::os::raw::c_char;

pub fn string_to_c_ptr(value: impl Into<String>) -> *mut c_char {
    match CString::new(value.into()) {
        Ok(cstr) => cstr.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

pub fn error_json(msg: &str) -> *mut c_char {
    string_to_c_ptr(serde_json::json!({ "error": msg }).to_string())
}

pub fn success_json<T: serde::Serialize>(data: &T) -> *mut c_char {
    match serde_json::to_string(data) {
        Ok(json) => string_to_c_ptr(json),
        Err(_) => error_json("Failed to serialize response"),
    }
}

/// Free a C string returned by an engine FFI function.
///
/// # Safety
/// `ptr` must be a pointer returned by `CString::into_raw` from the same dylib
/// allocator, or null. It must not be freed more than once.
pub unsafe fn free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        unsafe {
            let _ = CString::from_raw(ptr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;
    use std::ffi::CStr;

    #[derive(Serialize)]
    struct Response<'a> {
        value: &'a str,
    }

    #[test]
    fn string_to_c_ptr_allocates_and_free_string_releases() {
        let ptr = string_to_c_ptr("ok");
        assert!(!ptr.is_null());
        let value = unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() };
        assert_eq!(value, "ok");
        unsafe { free_string(ptr) };
    }

    #[test]
    fn success_json_serializes_response() {
        let response = Response { value: "ok" };
        let ptr = success_json(&response);
        assert!(!ptr.is_null());
        let json = unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() };
        assert_eq!(json, r#"{"value":"ok"}"#);
        unsafe { free_string(ptr) };
    }

    #[test]
    fn error_json_escapes_message() {
        let ptr = error_json(r#"bad "value" \ path"#);
        assert!(!ptr.is_null());
        let json = unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() };
        assert_eq!(json, r#"{"error":"bad \"value\" \\ path"}"#);
        unsafe { free_string(ptr) };
    }
}
