//! Files only the current user may read: created that way, so a secret is never exposed, not
//! even for a moment.
//!
//! - Unix: mode `0600` from the start.
//! - Windows, which has no mode bits: a file normally inherits its directory's ACL, which is
//!   private under the user's profile and not elsewhere (`C:\tools` grants every user read). So
//!   the file is created (`CreateFileW`) with its own *protected* DACL, [`private_sddl`], that
//!   grants the user, `SYSTEM` and `Administrators` and inherits nothing. A rename keeps it.
//!   [`exposed`] reads a file's ACL back ([`crate::acl`]) to tell whether it is still private.
//!
//! The SDDL is pure and tested on every host; only the two calls are Windows code.

use std::fs::File;
use std::io;
use std::path::Path;

/// The security descriptor (SDDL) of a private file: a protected DACL (`P`: nothing inherited)
/// with full access (`FA`) for the user `me` (a SID, `S-1-5-21-...`), `SYSTEM` (`SY`) and
/// `Administrators` (`BA`), which is what a file under the user's profile gets anyway.
pub fn private_sddl(me: &str) -> String {
    format!("D:P(A;;FA;;;{me})(A;;FA;;;SY)(A;;FA;;;BA)")
}

/// Creates `path`, which must not exist yet, for writing, readable and writable by the current
/// user only (see the module docs). Fails with `AlreadyExists` when something is there.
#[cfg(unix)]
pub fn create(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Creates `path`, which must not exist yet, for writing, readable and writable by the current
/// user only (see the module docs). Fails with `AlreadyExists` when something is there.
#[cfg(windows)]
pub fn create(path: &Path) -> io::Result<File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{
        GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::{CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL};

    let me = crate::acl::current_user_sid()
        .ok_or_else(|| io::Error::other("cannot find out the current user's SID"))?;
    let sddl: Vec<u16> = private_sddl(&me).encode_utf16().chain(Some(0)).collect();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: `sddl` is NUL-terminated; `sd` receives a LocalAlloc'd descriptor, freed below.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            null_mut(),
        )
    };
    if ok == 0 || sd.is_null() {
        return Err(io::Error::last_os_error());
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is NUL-terminated and `sa` points at a valid descriptor for the call.
    // Not shared (0) while open; CREATE_NEW never opens something already there.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            &sa,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    let err = io::Error::last_os_error();
    // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW, freed once;
    // CreateFileW has copied what it needs.
    unsafe { LocalFree(sd) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(err);
    }
    // SAFETY: a valid handle we own, handed to `File`, which closes it.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// Creates `path` for writing without special access rights (elsewhere than on Unix and
/// Windows).
#[cfg(not(any(unix, windows)))]
pub fn create(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Windows: the principals besides the user, `SYSTEM` and `Administrators` that `path`'s ACL
/// lets read or change it (empty: private), by the same rule that guards the secrets file.
#[cfg(windows)]
pub fn exposed(path: &Path) -> Result<Vec<String>, String> {
    let acl = crate::acl::read(path)?;
    Ok(crate::acl::exposed_to(&acl))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_grants_only_the_user_system_and_administrators() {
        let sddl = private_sddl("S-1-5-21-1111-2222-3333-1001");
        assert_eq!(
            sddl,
            "D:P(A;;FA;;;S-1-5-21-1111-2222-3333-1001)(A;;FA;;;SY)(A;;FA;;;BA)"
        );
        assert!(sddl.starts_with("D:P("), "protected: nothing inherited");
        // Exactly three entries, all allow (`A`) with full access (`FA`).
        assert_eq!(sddl.matches("(A;;FA;;;").count(), 3);
        assert_eq!(sddl.matches('(').count(), 3);
    }

    #[test]
    fn created_private_and_never_over_something_there() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("secret");
        {
            use std::io::Write;
            let mut f = create(&p).unwrap();
            f.write_all(b"s").unwrap();
        }
        assert_eq!(std::fs::read(&p).unwrap(), b"s");
        assert_eq!(create(&p).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        #[cfg(windows)]
        assert_eq!(exposed(&p).unwrap(), Vec::<String>::new());
    }
}
