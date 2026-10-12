//! Windows: who besides the current user can get at a secrets file, from its access control
//! list (DACL). Windows files have no mode bits; a file inherits its directory's ACL, which under
//! the user's profile normally grants only the user, `SYSTEM` and `Administrators`.
//!
//! The rule ([`exposed_to`]) is pure and tested with fixture ACLs on every host; only reading
//! the ACL ([`read`], `GetNamedSecurityInfoW`) is Windows code. It is deliberately conservative:
//! an *allow* entry that grants any read, write or permission-changing right to a principal other
//! than the current user, `SYSTEM` or `Administrators` refuses the file, even if a *deny* entry
//! might take it back; a file with no DACL at all (everyone has full access) is refused too, and
//! so is one with an entry of a type the rule does not read (object or compound allows).

#[cfg(windows)]
use std::path::Path;

/// `NT AUTHORITY\SYSTEM`.
pub(crate) const SYSTEM: &str = "S-1-5-18";
/// `BUILTIN\Administrators`.
pub(crate) const ADMINISTRATORS: &str = "S-1-5-32-544";
/// `OWNER RIGHTS`: whoever owns the file.
const OWNER_RIGHTS: &str = "S-1-3-4";

/// Rights that expose a secret: reading it (`FILE_READ_DATA`, `GENERIC_READ`), changing it
/// (`FILE_WRITE_DATA`, `FILE_APPEND_DATA`, `GENERIC_WRITE`), or granting oneself either
/// (`WRITE_DAC`, `WRITE_OWNER`, `GENERIC_ALL`).
pub(crate) const EXPOSING_RIGHTS: u32 = 0x0000_0001
    | 0x0000_0002
    | 0x0000_0004
    | 0x0004_0000
    | 0x0008_0000
    | 0x1000_0000
    | 0x4000_0000
    | 0x8000_0000;

/// What a DACL entry does, by its type (winnt.h `*_ACE_TYPE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AceKind {
    /// `ACCESS_ALLOWED` (0), or `ACCESS_ALLOWED_CALLBACK` (9: a conditional grant, which counts
    /// as granted): same layout, so its SID and mask are read.
    Allowed,
    /// A deny entry (1, 6, 10, 12): ignored, which errs towards refusing.
    Denied,
    /// Anything else in a DACL: object and compound allows (4, 5, 11) and types this rule does
    /// not know. Not understood, so the file counts as exposed.
    Unknown(u8),
}

/// The kind of a DACL entry of type `ace_type`.
pub(crate) fn ace_kind(ace_type: u8) -> AceKind {
    match ace_type {
        0 | 9 => AceKind::Allowed,
        1 | 6 | 10 | 12 => AceKind::Denied,
        t => AceKind::Unknown(t),
    }
}

/// One entry of a DACL (deny entries are left out).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ace {
    pub kind: AceKind,
    /// String SID, e.g. `S-1-1-0`.
    pub sid: String,
    /// `DOMAIN\name`, when it could be looked up.
    pub name: Option<String>,
    pub mask: u32,
    /// Applies only to children, not to the file itself.
    pub inherit_only: bool,
}

/// A file's discretionary ACL: only its allow entries (and ones not understood) matter here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Dacl {
    /// No DACL: everyone has full access.
    Null,
    Allowed(Vec<Ace>),
}

/// What [`read`] finds out about a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileAcl {
    pub dacl: Dacl,
    /// The file owner's SID.
    pub owner: Option<String>,
    /// The current user's SID.
    pub me: String,
}

/// The principals, besides `me`, `SYSTEM` and `Administrators`, that the ACL lets read or change
/// the file, as names for a message (deduplicated, in ACL order). Empty: the file is private.
pub(crate) fn exposed_to(acl: &FileAcl) -> Vec<String> {
    let aces = match &acl.dacl {
        Dacl::Null => return vec!["Everyone (the file has no access control list)".into()],
        Dacl::Allowed(aces) => aces,
    };
    let trusted = |sid: &str| {
        sid == acl.me
            || sid == SYSTEM
            || sid == ADMINISTRATORS
            || (sid == OWNER_RIGHTS && acl.owner.as_deref() == Some(acl.me.as_str()))
    };
    let mut out: Vec<String> = Vec::new();
    for ace in aces {
        if ace.inherit_only {
            continue;
        }
        match ace.kind {
            AceKind::Denied => continue,
            AceKind::Unknown(t) => {
                let what = format!("an access control entry dot cannot check (type {t})");
                if !out.contains(&what) {
                    out.push(what);
                }
                continue;
            }
            AceKind::Allowed => {}
        }
        if ace.mask & EXPOSING_RIGHTS == 0 || trusted(&ace.sid) {
            continue;
        }
        let name = ace
            .name
            .clone()
            .or_else(|| well_known_name(&ace.sid).map(str::to_string))
            .unwrap_or_else(|| ace.sid.clone());
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// Names of SIDs that commonly show up in ACLs, for when the lookup fails.
pub(crate) fn well_known_name(sid: &str) -> Option<&'static str> {
    Some(match sid {
        "S-1-1-0" => "Everyone",
        "S-1-5-11" => "NT AUTHORITY\\Authenticated Users",
        "S-1-5-32-545" => "BUILTIN\\Users",
        "S-1-5-32-546" => "BUILTIN\\Guests",
        "S-1-5-4" => "NT AUTHORITY\\INTERACTIVE",
        "S-1-5-7" => "NT AUTHORITY\\ANONYMOUS LOGON",
        SYSTEM => "NT AUTHORITY\\SYSTEM",
        ADMINISTRATORS => "BUILTIN\\Administrators",
        _ => return None,
    })
}

/// The refusal for a file [`exposed_to`] others, naming them and the fix. The fix names the
/// user by SID (`*S-1-5-...`), which reads the same in cmd and PowerShell (`%USERNAME%` would
/// not expand in PowerShell).
pub(crate) fn exposed_message(what: &str, path: &str, me: &str, who: &[String]) -> String {
    format!(
        "{what} {path} can be read or changed by {} (only you, SYSTEM and Administrators may \
         have access); run `icacls \"{path}\" /inheritance:r /grant:r \"*{me}:F\"`",
        who.join(", ")
    )
}

/// Reads `path`'s owner and DACL, and the current user's SID.
#[cfg(windows)]
pub(crate) fn read(path: &Path) -> Result<FileAcl, String> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce, INHERIT_ONLY_ACE,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };

    struct Descriptor(PSECURITY_DESCRIPTOR);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: allocated by GetNamedSecurityInfoW, freed once with LocalFree.
            unsafe { LocalFree(self.0) };
        }
    }

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut owner: PSID = null_mut();
    let mut dacl: *mut ACL = null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: `wide` is NUL-terminated; the out pointers are valid; the descriptor they point
    // into is freed by `Descriptor` after every use below.
    let err = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        )
    };
    if err != ERROR_SUCCESS {
        return Err(format!(
            "cannot read the access control list of {}: {}",
            path.display(),
            std::io::Error::from_raw_os_error(err as i32)
        ));
    }
    let _sd = Descriptor(sd);
    let owner = sid_string(owner);
    let dacl = if dacl.is_null() {
        Dacl::Null
    } else {
        // SAFETY: a non-null DACL returned by GetNamedSecurityInfoW, alive while `_sd` is.
        let count = unsafe { (*dacl).AceCount };
        let mut aces = Vec::new();
        for i in 0..u32::from(count) {
            let mut ace: *mut core::ffi::c_void = null_mut();
            // SAFETY: `i` is below the ACL's ACE count.
            if unsafe { GetAce(dacl, i, &mut ace) } == 0 || ace.is_null() {
                continue;
            }
            // SAFETY: every ACE starts with an ACE_HEADER.
            let header = unsafe { *(ace as *const ACE_HEADER) };
            let inherit_only = u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0;
            match ace_kind(header.AceType) {
                AceKind::Denied => continue,
                AceKind::Unknown(t) => {
                    aces.push(Ace {
                        kind: AceKind::Unknown(t),
                        sid: String::new(),
                        name: None,
                        mask: 0,
                        inherit_only,
                    });
                    continue;
                }
                AceKind::Allowed => {}
            }
            // An ACCESS_ALLOWED_CALLBACK_ACE starts like an ACCESS_ALLOWED_ACE (its condition
            // follows the SID).
            let allowed = ace as *const ACCESS_ALLOWED_ACE;
            // SAFETY: an ACCESS_ALLOWED(_CALLBACK)_ACE, whose SID starts at `SidStart`.
            let (mask, sid) =
                unsafe { ((*allowed).Mask, (&raw const (*allowed).SidStart) as PSID) };
            let Some(sid_text) = sid_string(sid) else {
                continue;
            };
            aces.push(Ace {
                kind: AceKind::Allowed,
                sid: sid_text,
                name: account_name(sid),
                mask,
                inherit_only,
            });
        }
        Dacl::Allowed(aces)
    };
    let me = current_user_sid().ok_or("cannot find out the current user's SID")?;
    Ok(FileAcl { dacl, owner, me })
}

/// `S-1-5-...` for a SID.
#[cfg(windows)]
fn sid_string(sid: windows_sys::Win32::Security::PSID) -> Option<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;

    if sid.is_null() {
        return None;
    }
    let mut text: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: `sid` is a valid SID; `text` receives a LocalAlloc'd string freed below.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 || text.is_null() {
        return None;
    }
    // SAFETY: a NUL-terminated wide string.
    let s = unsafe { wide_to_string(text) };
    // SAFETY: allocated by ConvertSidToStringSidW.
    unsafe { LocalFree(text.cast()) };
    Some(s)
}

/// `DOMAIN\name` for a SID, when it can be looked up.
#[cfg(windows)]
fn account_name(sid: windows_sys::Win32::Security::PSID) -> Option<String> {
    use windows_sys::Win32::Security::{LookupAccountSidW, SID_NAME_USE};

    let mut name = [0u16; 256];
    let mut domain = [0u16; 256];
    let (mut n, mut d) = (name.len() as u32, domain.len() as u32);
    let mut use_: SID_NAME_USE = 0;
    // SAFETY: the buffers and their lengths match; `sid` is valid.
    let ok = unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            sid,
            name.as_mut_ptr(),
            &mut n,
            domain.as_mut_ptr(),
            &mut d,
            &mut use_,
        )
    };
    if ok == 0 {
        return None;
    }
    let name = String::from_utf16_lossy(&name[..n as usize]);
    let domain = String::from_utf16_lossy(&domain[..d as usize]);
    Some(if domain.is_empty() {
        name
    } else {
        format!("{domain}\\{name}")
    })
}

/// The current process user's SID.
#[cfg(windows)]
pub(crate) fn current_user_sid() -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the current-process pseudo-handle needs no closing; `token` is a valid out pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    let mut len = 0u32;
    // SAFETY: asking for the size only.
    unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len) };
    // u64s keep the buffer aligned for TOKEN_USER.
    let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
    // SAFETY: the buffer holds at least `len` bytes.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast(),
            (buf.len() * 8) as u32,
            &mut len,
        )
    };
    // SAFETY: opened above, closed once.
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return None;
    }
    // SAFETY: GetTokenInformation filled a TOKEN_USER; its SID points into `buf`, alive here.
    let sid = unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid };
    sid_string(sid)
}

/// # Safety
/// `p` must point to a NUL-terminated UTF-16 string.
#[cfg(windows)]
unsafe fn wide_to_string(p: *const u16) -> String {
    let mut len = 0;
    // SAFETY: the caller guarantees a terminating NUL.
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` units were just read.
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "S-1-5-21-1111-2222-3333-1001";

    fn ace(sid: &str, mask: u32) -> Ace {
        Ace {
            kind: AceKind::Allowed,
            sid: sid.into(),
            name: None,
            mask,
            inherit_only: false,
        }
    }

    fn file(aces: Vec<Ace>) -> FileAcl {
        FileAcl {
            dacl: Dacl::Allowed(aces),
            owner: Some(ME.into()),
            me: ME.into(),
        }
    }

    /// `FILE_ALL_ACCESS` and `FILE_GENERIC_READ`, as icacls shows `(F)` and `(R)`.
    const FULL: u32 = 0x001F_01FF;
    const READ: u32 = 0x0012_0089;
    /// `SYNCHRONIZE | READ_CONTROL | FILE_READ_ATTRIBUTES`: can see the file, not its content.
    const ATTRIBUTES_ONLY: u32 = 0x0012_0080;

    #[test]
    fn a_file_under_the_profile_is_private() {
        // What a file in %USERPROFILE% inherits: SYSTEM, Administrators and the user, (F).
        let acl = file(vec![
            ace(SYSTEM, FULL),
            ace(ADMINISTRATORS, FULL),
            ace(ME, FULL),
        ]);
        assert!(exposed_to(&acl).is_empty());
        // OWNER RIGHTS counts as the owner, which is us.
        assert!(exposed_to(&file(vec![ace(OWNER_RIGHTS, FULL)])).is_empty());
    }

    #[test]
    fn others_who_can_read_or_change_it_are_named() {
        let acl = file(vec![
            ace(ME, FULL),
            ace("S-1-5-32-545", READ),
            ace("S-1-1-0", 0x0000_0002),
            ace("S-1-5-11", ATTRIBUTES_ONLY),
            Ace {
                name: Some("CORP\\alice".into()),
                ..ace("S-1-5-21-9-9-9-1002", 0x8000_0000)
            },
            Ace {
                inherit_only: true,
                ..ace("S-1-3-0", FULL)
            },
            ace("S-1-5-32-545", FULL),
        ]);
        assert_eq!(
            exposed_to(&acl),
            ["BUILTIN\\Users", "Everyone", "CORP\\alice"],
            "reading, writing and GENERIC_READ count; attributes and inherit-only do not"
        );
        // Someone else's file with OWNER RIGHTS: that owner is not us.
        let mut theirs = file(vec![ace(OWNER_RIGHTS, READ)]);
        theirs.owner = Some("S-1-5-21-9-9-9-1002".into());
        assert_eq!(exposed_to(&theirs), [OWNER_RIGHTS]);
        // An unknown SID without a name shows as the SID.
        assert_eq!(
            exposed_to(&file(vec![ace("S-1-5-21-7-7-7-500", 0x0004_0000)])),
            ["S-1-5-21-7-7-7-500"],
            "WRITE_DAC lets them grant themselves read"
        );
    }

    #[test]
    fn entries_of_other_types_are_not_waved_through() {
        assert_eq!(ace_kind(0), AceKind::Allowed);
        assert_eq!(
            ace_kind(9),
            AceKind::Allowed,
            "a conditional grant still grants"
        );
        assert_eq!(ace_kind(1), AceKind::Denied);
        assert_eq!(ace_kind(5), AceKind::Unknown(5));
        // Everyone may read through a conditional (callback) entry: exposed.
        let acl = file(vec![ace(ME, FULL), ace("S-1-1-0", READ)]);
        assert_eq!(exposed_to(&acl), ["Everyone"]);
        // An object or compound allow entry is not read: refused rather than trusted.
        for t in [4u8, 5, 11, 200] {
            let odd = Ace {
                kind: ace_kind(t),
                sid: String::new(),
                name: None,
                mask: 0,
                inherit_only: false,
            };
            let acl = file(vec![ace(ME, FULL), odd.clone(), odd.clone()]);
            assert_eq!(
                exposed_to(&acl),
                [format!(
                    "an access control entry dot cannot check (type {t})"
                )]
            );
            // One that applies only to children does not matter for the file.
            let only_children = Ace {
                inherit_only: true,
                ..odd
            };
            assert!(exposed_to(&file(vec![ace(ME, FULL), only_children])).is_empty());
        }
        // Deny entries are ignored (never make a file look private).
        let deny = Ace {
            kind: AceKind::Denied,
            ..ace("S-1-1-0", FULL)
        };
        assert!(exposed_to(&file(vec![ace(ME, FULL), deny])).is_empty());
    }

    #[test]
    fn no_dacl_means_everyone() {
        let acl = FileAcl {
            dacl: Dacl::Null,
            owner: None,
            me: ME.into(),
        };
        assert_eq!(
            exposed_to(&acl),
            ["Everyone (the file has no access control list)"]
        );
    }

    #[test]
    fn the_message_names_them_and_the_fix() {
        let m = exposed_message(
            "secrets file",
            r"C:\Users\me\.config\dot\secrets.env",
            "S-1-5-21-1-2-3-1001",
            &["BUILTIN\\Users".into(), "Everyone".into()],
        );
        assert!(
            m.contains("can be read or changed by BUILTIN\\Users, Everyone"),
            "{m}"
        );
        assert!(
            m.contains(
                r#"icacls "C:\Users\me\.config\dot\secrets.env" /inheritance:r /grant:r "*S-1-5-21-1-2-3-1001:F""#
            ),
            "{m}"
        );
        assert_eq!(well_known_name("S-1-1-0"), Some("Everyone"));
        assert_eq!(well_known_name("S-1-5-21-1"), None);
    }

    #[cfg(windows)]
    #[test]
    fn reads_a_real_acl() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("s.env");
        std::fs::write(&p, "").unwrap();
        let acl = read(&p).unwrap();
        assert!(acl.me.starts_with("S-1-5-"), "{acl:?}");
        assert!(matches!(acl.dacl, Dacl::Allowed(_)), "{acl:?}");
        assert!(read(&tmp.path().join("missing")).is_err());
    }
}
