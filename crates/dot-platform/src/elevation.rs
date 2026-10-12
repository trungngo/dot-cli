//! Whether this process runs elevated (Windows: an administrator token, "Run as
//! administrator"). Windows has no sudo: actions that need administrator rights (system
//! services, machine-wide winget installs) say [`dot_core::Action::needs_sudo`], and the prepare
//! stage fails before anything runs when the terminal is not elevated ([`crate::sudo`]).

/// Whether the process token is elevated. Always false off Windows (see
/// [`crate::exec::Runner::is_root`] for Unix).
pub fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        windows_token_elevated()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(windows)]
fn windows_token_elevated() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing; `token` is a
    // valid out pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len = 0u32;
    // SAFETY: `token` is an open token handle with TOKEN_QUERY; the buffer is a TOKEN_ELEVATION
    // of the size passed.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    };
    // SAFETY: `token` was opened above and is closed once.
    unsafe { CloseHandle(token) };
    ok != 0 && elevation.TokenIsElevated != 0
}

/// The error for a run whose `steps` need administrator rights while the terminal is not
/// elevated.
pub fn not_elevated_error(steps: &[String]) -> String {
    format!(
        "administrator rights are needed by {}; run dot from an elevated terminal (Run as \
         administrator). Nothing was changed.",
        steps.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_error_lists_every_step() {
        let e = not_elevated_error(&["services: sshd".into(), "apps: winget".into()]);
        assert!(e.contains("needed by services: sshd; apps: winget"), "{e}");
        assert!(e.contains("elevated terminal"), "{e}");
    }

    #[cfg(not(windows))]
    #[test]
    fn never_elevated_off_windows() {
        assert!(!is_elevated());
    }
}
