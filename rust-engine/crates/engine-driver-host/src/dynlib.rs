//! The one place that talks to the OS dynamic loader. Unix uses
//! `dlopen`/`dlsym`/`dlclose`, Windows `LoadLibraryExW`/`GetProcAddress`/
//! `FreeLibrary`; any other OS is a typed `Unsupported`.

use std::ffi::c_void;
use std::path::Path;
use std::ptr::NonNull;
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum NativeLibraryError {
    #[error("{0}")]
    Open(String),
    #[error("loading driver libraries is not implemented on {0} yet")]
    #[cfg_attr(any(unix, windows), allow(dead_code))]
    Unsupported(&'static str),
}

/// An open driver library; closed when dropped. Symbols resolved from it are
/// only valid until then.
pub(crate) struct NativeLibrary {
    #[cfg(any(unix, windows))]
    raw: NonNull<c_void>,
}

#[cfg(unix)]
impl NativeLibrary {
    pub(crate) fn open(path: &Path) -> Result<Self, NativeLibraryError> {
        use std::ffi::{CStr, CString};

        let c_path = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|error| NativeLibraryError::Open(error.to_string()))?;
        // SAFETY: c_path is a valid NUL-terminated path for this call.
        let raw = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        NonNull::new(raw).map(|raw| Self { raw }).ok_or_else(|| {
            // SAFETY: dlerror returns a thread-local string or null.
            let error = unsafe { libc::dlerror() };
            NativeLibraryError::Open(if error.is_null() {
                "unknown dlopen failure".into()
            } else {
                // SAFETY: error points to a NUL-terminated thread-local string.
                unsafe { CStr::from_ptr(error) }
                    .to_string_lossy()
                    .into_owned()
            })
        })
    }

    pub(crate) fn symbol(&self, name: &std::ffi::CStr) -> Option<NonNull<c_void>> {
        // SAFETY: name is NUL-terminated; self.raw is a live dlopen handle.
        NonNull::new(unsafe { libc::dlsym(self.raw.as_ptr(), name.as_ptr()) })
    }
}

#[cfg(unix)]
impl Drop for NativeLibrary {
    fn drop(&mut self) {
        // SAFETY: self.raw came from dlopen and is closed exactly once, here.
        unsafe { libc::dlclose(self.raw.as_ptr()) };
    }
}

#[cfg(windows)]
impl NativeLibrary {
    pub(crate) fn open(path: &Path) -> Result<Self, NativeLibraryError> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::System::LibraryLoader::{
            LoadLibraryExW, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
        };

        // The search flags need an absolute path; they limit the DLLs the
        // driver depends on to its own directory and the system ones, never
        // the current directory or PATH.
        let absolute = std::path::absolute(path)
            .map_err(|error| NativeLibraryError::Open(error.to_string()))?;
        let mut wide: Vec<u16> = absolute.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(NativeLibraryError::Open(
                "the path contains a NUL character".into(),
            ));
        }
        wide.push(0);
        // SAFETY: wide is a NUL-terminated UTF-16 path that outlives the call;
        // no file handle is passed (the parameter is reserved).
        let raw = unsafe {
            LoadLibraryExW(
                wide.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        };
        NonNull::new(raw)
            .map(|raw| Self { raw })
            .ok_or_else(|| NativeLibraryError::Open(std::io::Error::last_os_error().to_string()))
    }

    pub(crate) fn symbol(&self, name: &std::ffi::CStr) -> Option<NonNull<c_void>> {
        use windows_sys::Win32::System::LibraryLoader::GetProcAddress;

        // SAFETY: name is NUL-terminated; self.raw is a live module handle.
        let address = unsafe { GetProcAddress(self.raw.as_ptr(), name.as_ptr().cast()) }?;
        NonNull::new(address as *mut c_void)
    }
}

#[cfg(windows)]
impl Drop for NativeLibrary {
    fn drop(&mut self) {
        // SAFETY: self.raw came from LoadLibraryExW and is freed exactly once, here.
        unsafe { windows_sys::Win32::Foundation::FreeLibrary(self.raw.as_ptr()) };
    }
}

#[cfg(not(any(unix, windows)))]
impl NativeLibrary {
    pub(crate) fn open(_path: &Path) -> Result<Self, NativeLibraryError> {
        Err(NativeLibraryError::Unsupported(std::env::consts::OS))
    }

    pub(crate) fn symbol(&self, _name: &std::ffi::CStr) -> Option<NonNull<c_void>> {
        None
    }
}
