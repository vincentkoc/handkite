use std::ffi::{CStr, OsStr};
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// Resolve the effective account, without consulting caller-controlled environment paths.
pub(crate) fn account_home() -> Option<PathBuf> {
    let uid = unsafe { libc::geteuid() };
    let mut buffer = vec![0_u8; 1024];
    loop {
        let mut record = MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // The OS record points into this buffer, which stays alive until the path is copied.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                record.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && buffer.len() < 1024 * 1024 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if status != 0 || result.is_null() {
            return None;
        }
        let directory = unsafe { (*result).pw_dir };
        if directory.is_null() {
            return None;
        }
        let home = PathBuf::from(OsStr::from_bytes(unsafe {
            CStr::from_ptr(directory).to_bytes()
        }));
        return home.is_absolute().then_some(home);
    }
}
