//! Windows: the `PATH` an installer leaves in the registry. winget and scoop add directories to
//! the machine or user `Path` (winget's portable-app `Links`, a package's own `bin`), which only
//! new processes see. A run that installs a tool and then calls it re-reads the registry, so the
//! later steps of the same run find it (see `RunEnvCell::add_system_path`).
//!
//! `path: { dir: { persist: user } }` also writes `dot`'s own entries to the user `Path`
//! (`HKCU\Environment`), so programs started outside a `dot env` shell see them:
//! [`add_user_path`], [`remove_user_path`], [`user_path_has`].

use std::io;
use std::path::{Path, PathBuf};

/// The directories of the machine `Path` then the user `Path` (with `%VAR%`s expanded), as a
/// process started now from Explorer would get them. Empty off Windows, and on any error.
pub fn registry_path() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let machine = imp::read(
            imp::Hive::Machine,
            r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
        )
        .ok()
        .flatten();
        let user = imp::read(imp::Hive::User, "Environment").ok().flatten();
        split(&[machine, user])
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// The directories of `values` (`;`-separated `Path` values), in order, without empties or
/// repeats (compared case-insensitively, as Windows does).
pub fn split(values: &[Option<String>]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for v in values.iter().flatten() {
        for d in v.split(';').map(str::trim).filter(|d| !d.is_empty()) {
            let d = d.trim_end_matches(['\\', '/']);
            if !out
                .iter()
                .any(|o| o.to_string_lossy().eq_ignore_ascii_case(d))
            {
                out.push(PathBuf::from(d));
            }
        }
    }
    out
}

/// Whether the user `Path` in the registry lists `dir` (as written or once `%VAR%`s are
/// expanded). Always `false` off Windows.
pub fn user_path_has(dir: &Path) -> bool {
    #[cfg(windows)]
    {
        let want = dir.to_string_lossy();
        [imp::read_user(true), imp::read_user(false)]
            .into_iter()
            .filter_map(|r| r.ok().flatten())
            .any(|(v, _)| position(&v, &want).is_some())
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        false
    }
}

/// Adds `dir` to the user `Path` in the registry (at the end when `append`, else at the front)
/// unless it is there already, then tells running programs the environment changed. `Ok(true)`:
/// the registry was changed. Only a missing `Path` counts as empty: any other failure to read it
/// is an error, so the value is never rewritten from nothing. An error off Windows.
pub fn add_user_path(dir: &Path, append: bool) -> io::Result<bool> {
    #[cfg(windows)]
    {
        let want = dir.to_string_lossy();
        let current = imp::read_user(false)?;
        // As written or once `%VAR%`s are expanded (`%USERPROFILE%\.local\bin`).
        if let Some((expanded, _)) = imp::read_user(true)?
            && position(&expanded, &want).is_some()
        {
            return Ok(false);
        }
        let (raw, ty) = current.unwrap_or_default();
        let Some(new) = with_dir(&raw, &want, append) else {
            return Ok(false);
        };
        imp::write_user(&new, ty)?;
        imp::broadcast();
        Ok(true)
    }
    #[cfg(not(windows))]
    {
        let _ = (dir, append);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the user Path in the registry exists only on Windows",
        ))
    }
}

/// Takes `dir` out of the user `Path` in the registry (every entry naming it as written), then
/// tells running programs the environment changed. `Ok(false)`: it was not there. Does nothing
/// off Windows.
pub fn remove_user_path(dir: &Path) -> io::Result<bool> {
    #[cfg(windows)]
    {
        let Some((raw, ty)) = imp::read_user(false)? else {
            return Ok(false);
        };
        let Some(new) = without_dir(&raw, &dir.to_string_lossy()) else {
            return Ok(false);
        };
        imp::write_user(&new, ty)?;
        imp::broadcast();
        Ok(true)
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        Ok(false)
    }
}

/// `Path` entries compare as Windows does: case-insensitively, without trailing separators.
fn same_entry(a: &str, b: &str) -> bool {
    let t = |s: &str| s.trim().trim_end_matches(['\\', '/']).to_string();
    t(a).eq_ignore_ascii_case(&t(b))
}

/// Where `dir` is in the `;`-separated `value`.
fn position(value: &str, dir: &str) -> Option<usize> {
    value.split(';').position(|d| same_entry(d, dir))
}

/// `value` with `dir` added (at the end when `append`); `None` when it already lists `dir`.
pub fn with_dir(value: &str, dir: &str, append: bool) -> Option<String> {
    if position(value, dir).is_some() {
        return None;
    }
    let rest = value.trim_matches(';');
    Some(match (rest.is_empty(), append) {
        (true, _) => dir.to_string(),
        (false, true) => format!("{rest};{dir}"),
        (false, false) => format!("{dir};{rest}"),
    })
}

/// `value` without the entries naming `dir`; `None` when it has none.
pub fn without_dir(value: &str, dir: &str) -> Option<String> {
    position(value, dir)?;
    Some(
        value
            .split(';')
            .filter(|d| !d.is_empty() && !same_entry(d, dir))
            .collect::<Vec<_>>()
            .join(";"),
    )
}

#[cfg(windows)]
mod imp {
    use std::io;

    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA};
    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_EXPAND_SZ, REG_SZ, RRF_NOEXPAND,
        RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW, RegSetKeyValueW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
    };

    pub enum Hive {
        Machine,
        User,
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// The `Path` value of `key` in `hive`, expanded; `Ok(None)` when there is none.
    pub fn read(hive: Hive, key: &str) -> io::Result<Option<String>> {
        let hkey = match hive {
            Hive::Machine => HKEY_LOCAL_MACHINE,
            Hive::User => HKEY_CURRENT_USER,
        };
        Ok(read_value(hkey, key, "Path", true)?.map(|(v, _)| v))
    }

    /// The user `Path` with its type: as stored (`%VAR%`s kept) unless `expanded`. `Ok(None)`:
    /// there is no user `Path`.
    pub fn read_user(expanded: bool) -> io::Result<Option<(String, u32)>> {
        read_value(HKEY_CURRENT_USER, "Environment", "Path", expanded)
    }

    /// The string value `name` of `key` with its type, `%VAR%`s expanded when `expanded` and it
    /// is a `REG_EXPAND_SZ`. `Ok(None)` only when the key or value does not exist; a value of
    /// another type, or any other failure, is an error.
    ///
    /// Read with `RRF_NOEXPAND` and expanded here: `RegGetValueW` refuses
    /// `RRF_RT_REG_EXPAND_SZ` without `RRF_NOEXPAND` (`ERROR_INVALID_PARAMETER`).
    pub fn read_value(
        hkey: HKEY,
        key: &str,
        name: &str,
        expanded: bool,
    ) -> io::Result<Option<(String, u32)>> {
        let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
        let key = wide(key);
        let name = wide(name);
        // The value can grow between the size query and the read (another installer writing
        // it): asked again a few times.
        for _ in 0..3 {
            let mut size: u32 = 0;
            // SAFETY: valid NUL-terminated strings; a null buffer asks for the size.
            let rc = unsafe {
                RegGetValueW(
                    hkey,
                    key.as_ptr(),
                    name.as_ptr(),
                    flags,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut size,
                )
            };
            match rc {
                0 => {}
                ERROR_FILE_NOT_FOUND => return Ok(None),
                e => return Err(io::Error::from_raw_os_error(e as i32)),
            }
            let mut buf: Vec<u16> = vec![0; (size as usize).div_ceil(2) + 1];
            let mut bytes = (buf.len() * 2) as u32;
            let mut ty: u32 = 0;
            // SAFETY: `buf` holds `bytes` bytes.
            let rc = unsafe {
                RegGetValueW(
                    hkey,
                    key.as_ptr(),
                    name.as_ptr(),
                    flags,
                    &mut ty,
                    buf.as_mut_ptr().cast(),
                    &mut bytes,
                )
            };
            match rc {
                0 => {}
                ERROR_MORE_DATA => continue,
                ERROR_FILE_NOT_FOUND => return Ok(None),
                e => return Err(io::Error::from_raw_os_error(e as i32)),
            }
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            let raw = &buf[..len];
            let value = if expanded && ty == REG_EXPAND_SZ {
                expand(raw)?
            } else {
                String::from_utf16_lossy(raw)
            };
            return Ok(Some((value, ty)));
        }
        Err(io::Error::from_raw_os_error(ERROR_MORE_DATA as i32))
    }

    /// `raw` (UTF-16, no NUL) with its `%VAR%`s expanded from this process's environment.
    fn expand(raw: &[u16]) -> io::Result<String> {
        let src: Vec<u16> = raw.iter().copied().chain(std::iter::once(0)).collect();
        let mut cap = src.len().max(256) as u32;
        loop {
            let mut out: Vec<u16> = vec![0; cap as usize];
            // SAFETY: `src` is NUL-terminated; `out` holds `cap` UTF-16 units.
            let n = unsafe { ExpandEnvironmentStringsW(src.as_ptr(), out.as_mut_ptr(), cap) };
            if n == 0 {
                return Err(io::Error::last_os_error());
            }
            if n <= cap {
                // `n` counts the terminating NUL.
                let len = out.iter().position(|&c| c == 0).unwrap_or(n as usize);
                return Ok(String::from_utf16_lossy(&out[..len]));
            }
            cap = n;
        }
    }

    /// Writes the user `Path`, keeping its type (`REG_EXPAND_SZ` for a new value).
    pub fn write_user(value: &str, ty: u32) -> std::io::Result<()> {
        let ty = if ty == REG_SZ { REG_SZ } else { REG_EXPAND_SZ };
        let key = wide("Environment");
        let name = wide("Path");
        let data = wide(value);
        // SAFETY: valid NUL-terminated strings; `data` holds `data.len() * 2` bytes, NUL
        // included, as REG_SZ / REG_EXPAND_SZ require.
        let rc = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                ty,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc as i32));
        }
        Ok(())
    }

    /// Tells running programs (Explorer, so the next terminal it starts) that the environment
    /// changed. Best effort: a program that does not answer within 5 s is skipped.
    pub fn broadcast() {
        let what = wide("Environment");
        let mut result: usize = 0;
        // SAFETY: `what` is a NUL-terminated string that outlives the call.
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                what.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                5000,
                &mut result,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_and_dedupes_like_windows() {
        let dirs = split(&[
            Some(r"C:\Windows\system32;C:\Windows;;".into()),
            None,
            Some(r"C:\WINDOWS\System32\;C:\Users\me\AppData\Local\Microsoft\WinGet\Links".into()),
        ]);
        assert_eq!(
            dirs,
            [
                PathBuf::from(r"C:\Windows\system32"),
                PathBuf::from(r"C:\Windows"),
                PathBuf::from(r"C:\Users\me\AppData\Local\Microsoft\WinGet\Links"),
            ]
        );
    }

    #[test]
    fn adds_a_dir_once_at_either_end() {
        assert_eq!(with_dir("", r"C:\x", false).as_deref(), Some(r"C:\x"));
        assert_eq!(
            with_dir(r"C:\a;%USERPROFILE%\b;", r"C:\x", false).as_deref(),
            Some(r"C:\x;C:\a;%USERPROFILE%\b")
        );
        assert_eq!(
            with_dir(r"C:\a", r"C:\x", true).as_deref(),
            Some(r"C:\a;C:\x")
        );
        assert_eq!(with_dir(r"C:\a;c:\X\", r"C:\x", true), None);
    }

    /// A throwaway key under `HKCU\Software`, deleted when dropped.
    #[cfg(windows)]
    struct TestKey(String);

    #[cfg(windows)]
    impl TestKey {
        fn new() -> Self {
            TestKey(format!(r"Software\dot-winpath-test-{}", std::process::id()))
        }

        fn set(&self, name: &str, ty: u32, data: &[u16]) {
            use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RegSetKeyValueW};
            let w = |s: &str| {
                s.encode_utf16()
                    .chain(std::iter::once(0))
                    .collect::<Vec<u16>>()
            };
            let (key, name) = (w(&self.0), w(name));
            // SAFETY: NUL-terminated strings; `data` holds `data.len() * 2` bytes.
            let rc = unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    name.as_ptr(),
                    ty,
                    data.as_ptr().cast(),
                    (data.len() * 2) as u32,
                )
            };
            assert_eq!(rc, 0, "writing the test value");
        }
    }

    #[cfg(windows)]
    impl Drop for TestKey {
        fn drop(&mut self) {
            use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteTreeW};
            let key: Vec<u16> = self.0.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: a NUL-terminated string.
            unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key.as_ptr()) };
        }
    }

    #[cfg(windows)]
    #[test]
    fn reads_a_reg_expand_sz_path_raw_and_expanded() {
        use windows_sys::Win32::System::Registry::{
            HKEY_CURRENT_USER, REG_EXPAND_SZ, REG_MULTI_SZ,
        };
        let key = TestKey::new();
        let wide = |s: &str| {
            s.encode_utf16()
                .chain(std::iter::once(0))
                .collect::<Vec<u16>>()
        };
        key.set(
            "Path",
            REG_EXPAND_SZ,
            &wide(r"%USERPROFILE%\.local\bin;C:\y"),
        );
        let raw = imp::read_value(HKEY_CURRENT_USER, &key.0, "Path", false).unwrap();
        assert_eq!(
            raw,
            Some((r"%USERPROFILE%\.local\bin;C:\y".to_string(), REG_EXPAND_SZ))
        );
        let (expanded, ty) = imp::read_value(HKEY_CURRENT_USER, &key.0, "Path", true)
            .unwrap()
            .expect("an expanded value");
        assert_eq!(ty, REG_EXPAND_SZ);
        let profile = std::env::var("USERPROFILE").unwrap();
        assert_eq!(expanded, format!(r"{profile}\.local\bin;C:\y"));
        let dir = format!(r"{profile}\.local\bin");
        assert!(position(&expanded, &dir).is_some());
        // Missing: none. Another type: an error, never an empty `Path`.
        assert_eq!(
            imp::read_value(HKEY_CURRENT_USER, &key.0, "Nope", false).unwrap(),
            None
        );
        let multi: Vec<u16> = "a\0b\0\0".encode_utf16().collect();
        key.set("Multi", REG_MULTI_SZ, &multi);
        assert!(imp::read_value(HKEY_CURRENT_USER, &key.0, "Multi", false).is_err());
        assert!(imp::read_value(HKEY_CURRENT_USER, &key.0, "Multi", true).is_err());
    }

    #[test]
    fn removes_every_entry_naming_the_dir() {
        assert_eq!(
            without_dir(r"C:\x;C:\a;c:\X\;;C:\b", r"C:\x").as_deref(),
            Some(r"C:\a;C:\b")
        );
        assert_eq!(without_dir(r"C:\a", r"C:\x"), None);
        #[cfg(not(windows))]
        {
            assert!(!user_path_has(Path::new("/x")));
            assert!(add_user_path(Path::new("/x"), false).is_err());
            assert!(!remove_user_path(Path::new("/x")).unwrap());
        }
    }
}
