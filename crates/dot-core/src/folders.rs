//! Known folders, as fact-level variables, so one templated line works on every OS:
//! `"{{ config_dir }}/Code/User/settings.json"` is VS Code's settings file on macOS, Windows
//! and Linux alike.
//!
//! | variable | Linux | macOS | Windows |
//! |---|---|---|---|
//! | `config_dir` | `$XDG_CONFIG_HOME`, `~/.config` | `~/Library/Application Support` | `%APPDATA%` |
//! | `data_dir` | `$XDG_DATA_HOME`, `~/.local/share` | `~/Library/Application Support` | `%APPDATA%` |
//! | `cache_dir` | `$XDG_CACHE_HOME`, `~/.cache` | `~/Library/Caches` | `%LOCALAPPDATA%` |
//! | `state_dir` | `$XDG_STATE_HOME`, `~/.local/state` | `~/Library/Application Support` | `%LOCALAPPDATA%` |
//! | `xdg_config_home`, `xdg_data_home`, `xdg_cache_home`, `xdg_state_home` | as above | the XDG values (`~/.config`, ...) | none |
//! | `appdata`, `localappdata`, `userprofile`, `programfiles` | none | none | `%APPDATA%`, `%LOCALAPPDATA%`, `%USERPROFILE%`, `%ProgramFiles%` |
//! | `documents` | the XDG documents directory, `~/Documents` | `~/Documents` | the Documents known folder (redirected to OneDrive or not), `%USERPROFILE%\Documents` |
//! | `winget_links` | none | none | `%LOCALAPPDATA%\Microsoft\WinGet\Links` (winget's portable-app links) |
//! | `homebrew_prefix` | `/home/linuxbrew/.linuxbrew` | `/opt/homebrew` (Apple silicon), `/usr/local` (Intel) | none |
//!
//! `documents` is the platform's own answer where there is one (Windows' known folder API,
//! `xdg-user-dir`), passed in as [`DOCUMENTS_KEY`] by the caller.
//!
//! "none" is a null value, like a fact that is unknown here: `{% if appdata %}` works, and a
//! step that uses one should be guarded by `when: { os: windows }`. An environment variable that
//! is set but not an absolute path is ignored (the XDG spec says so). Values use the OS's own
//! separator with no trailing one, so appending `/Code/...` gives a path the directives normalise
//! (on Windows `C:\Users\me\AppData\Roaming/Code` becomes `C:\Users\me\AppData\Roaming\Code`).
//!
//! Everything here is a pure function of the OS, the home directory and an environment lookup,
//! so every OS's answer is tested on any host.

use crate::facts::Os;

/// The lookup key under which the caller passes the platform's Documents folder (Windows'
/// `FOLDERID_Documents`, the XDG documents directory): not an environment variable.
pub const DOCUMENTS_KEY: &str = "FOLDERID_Documents";

/// Every known-folder variable, in display order.
pub const NAMES: [&str; 15] = [
    "config_dir",
    "data_dir",
    "cache_dir",
    "state_dir",
    "xdg_config_home",
    "xdg_data_home",
    "xdg_cache_home",
    "xdg_state_home",
    "appdata",
    "localappdata",
    "userprofile",
    "programfiles",
    "documents",
    "winget_links",
    "homebrew_prefix",
];

/// The known folders for `os`, with `home` its home directory (in its own spelling) and `env`
/// looking up environment variables (case-insensitively on Windows, as Windows does). `None`:
/// not applicable on `os`.
pub fn known_folders(
    os: Os,
    home: &str,
    env: impl Fn(&str) -> Option<String>,
) -> Vec<(&'static str, Option<String>)> {
    known_folders_on(os, os, std::env::consts::ARCH, home, env)
}

/// [`known_folders`] for `os` as seen from a machine running `host`. They differ only when
/// Windows is simulated on a Unix host (`DOT_FACTS=os=windows`): the folders are then laid out
/// under `home` as Windows lays them out under the profile (`<home>/AppData/Roaming`, ...), in
/// the host's own path syntax, so the paths built from them are real paths there (a plan
/// validates them, a sandbox apply writes inside `home`). The environment is not read then:
/// the host's variables do not describe a Windows machine.
pub fn known_folders_on(
    os: Os,
    host: Os,
    arch: &str,
    home: &str,
    env: impl Fn(&str) -> Option<String>,
) -> Vec<(&'static str, Option<String>)> {
    let simulated = os == Os::Windows && host != Os::Windows;
    // The syntax paths are written in: the host's when simulating.
    let syntax = if simulated { host } else { os };
    let from_env = |k: &str| {
        if simulated {
            return None;
        }
        env(k)
            .filter(|v| !v.is_empty() && is_absolute(syntax, v))
            .map(|v| normalize(syntax, &v))
    };
    let under = |parts: &[&str]| join(syntax, home, parts);
    let mut out: Vec<(&'static str, Option<String>)> = NAMES.iter().map(|n| (*n, None)).collect();
    let mut set = |name: &str, value: String| {
        if let Some(slot) = out.iter_mut().find(|(n, _)| *n == name) {
            slot.1 = Some(value);
        }
    };
    match os {
        Os::Linux | Os::Macos => {
            let xdg = [
                ("xdg_config_home", "XDG_CONFIG_HOME", &[".config"][..]),
                ("xdg_data_home", "XDG_DATA_HOME", &[".local", "share"][..]),
                ("xdg_cache_home", "XDG_CACHE_HOME", &[".cache"][..]),
                ("xdg_state_home", "XDG_STATE_HOME", &[".local", "state"][..]),
            ];
            for (name, var, default) in xdg {
                let v = from_env(var).unwrap_or_else(|| under(default));
                if os == Os::Linux {
                    set(
                        &name.replace("xdg_", "").replace("_home", "_dir"),
                        v.clone(),
                    );
                }
                set(name, v);
            }
            if os == Os::Macos {
                let support = under(&["Library", "Application Support"]);
                set("config_dir", support.clone());
                set("data_dir", support.clone());
                set("state_dir", support);
                set("cache_dir", under(&["Library", "Caches"]));
            }
            set(
                "documents",
                from_env(DOCUMENTS_KEY).unwrap_or_else(|| under(&["Documents"])),
            );
            let brew = match (os, arch) {
                (Os::Macos, "aarch64") => "/opt/homebrew",
                (Os::Macos, _) => "/usr/local",
                _ => "/home/linuxbrew/.linuxbrew",
            };
            set("homebrew_prefix", brew.to_string());
        }
        Os::Windows => {
            let appdata = from_env("APPDATA").unwrap_or_else(|| under(&["AppData", "Roaming"]));
            let local = from_env("LOCALAPPDATA").unwrap_or_else(|| under(&["AppData", "Local"]));
            set("config_dir", appdata.clone());
            set("data_dir", appdata.clone());
            set("cache_dir", local.clone());
            set("state_dir", local.clone());
            set("appdata", appdata);
            set(
                "winget_links",
                join(syntax, &local, &["Microsoft", "WinGet", "Links"]),
            );
            set("localappdata", local);
            let profile = from_env("USERPROFILE").unwrap_or_else(|| normalize(syntax, home));
            set(
                "documents",
                from_env(DOCUMENTS_KEY).unwrap_or_else(|| join(syntax, &profile, &["Documents"])),
            );
            set("userprofile", profile);
            let program_files = if simulated {
                under(&["Program Files"])
            } else {
                r"C:\Program Files".to_string()
            };
            set(
                "programfiles",
                from_env("ProgramFiles").unwrap_or(program_files),
            );
        }
    }
    out
}

/// `path` under `os`'s rules: `/` on Unix, `C:\`, `C:/` or `\\server\share` on Windows.
pub fn is_absolute(os: Os, path: &str) -> bool {
    match os {
        Os::Windows => {
            let b = path.as_bytes();
            (b.len() >= 3
                && b[0].is_ascii_alphabetic()
                && b[1] == b':'
                && matches!(b[2], b'\\' | b'/'))
                || path.starts_with(r"\\")
                || path.starts_with("//")
        }
        Os::Linux | Os::Macos => path.starts_with('/'),
    }
}

/// `path` with `os`'s separator throughout (Windows: `/` becomes `\`), repeated separators
/// collapsed (a leading `\\` of a UNC path kept) and no trailing separator (except a root such
/// as `/` or `C:\`).
pub fn normalize(os: Os, path: &str) -> String {
    let sep = separator(os);
    let unified: String = match os {
        Os::Windows => path.replace('/', "\\"),
        Os::Linux | Os::Macos => path.to_string(),
    };
    let unc = os == Os::Windows && unified.starts_with(r"\\");
    let mut out = String::with_capacity(unified.len());
    let mut prev_sep = false;
    for c in unified.chars() {
        let is_sep = c == sep;
        if !(is_sep && prev_sep) {
            out.push(c);
        }
        prev_sep = is_sep;
    }
    if unc {
        out.insert(0, sep);
    }
    let root_len = match os {
        Os::Windows if out.len() == 3 && out.as_bytes()[1] == b':' => 3,
        _ => 1,
    };
    while out.len() > root_len && out.ends_with(sep) {
        out.pop();
    }
    out
}

/// `base` and `parts` joined with `os`'s separator, normalised.
pub fn join(os: Os, base: &str, parts: &[&str]) -> String {
    let sep = separator(os).to_string();
    let mut s = base.to_string();
    for p in parts {
        s.push_str(&sep);
        s.push_str(p);
    }
    normalize(os, &s)
}

fn separator(os: Os) -> char {
    match os {
        Os::Windows => '\\',
        Os::Linux | Os::Macos => '/',
    }
}

/// WSL: the Linux path of a Windows path, as `wslpath -u` gives it with the default automount
/// root (`C:\Users\me` is `/mnt/c/Users/me`). `None` for anything but a drive-letter path.
pub fn wsl_path(windows: &str) -> Option<String> {
    let w = windows.trim();
    let b = w.as_bytes();
    if b.len() < 2 || !b[0].is_ascii_alphabetic() || b[1] != b':' {
        return None;
    }
    let rest = w[2..].replace('\\', "/");
    let rest = rest.trim_matches('/');
    let drive = (b[0] as char).to_ascii_lowercase();
    Some(if rest.is_empty() {
        format!("/mnt/{drive}")
    } else {
        format!("/mnt/{drive}/{rest}")
    })
}

/// WSL: whether `path` is on a Windows drive mounted into Linux (`/mnt/c/...`), where Windows
/// programs live on `PATH` (interop).
pub fn is_windows_mount(path: &str) -> bool {
    let rest = match path.strip_prefix("/mnt/") {
        Some(r) => r,
        None => return false,
    };
    let mut chars = rest.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some(d), None | Some('/')) if d.is_ascii_alphabetic()
    )
}

/// WSL: whether a link at `dest` to `src` would be a WSL-only symlink that Windows programs
/// cannot follow: `dest` on a Windows drive (`/mnt/c/...`, e.g. under `windows_home`) and `src`
/// in the Linux filesystem. WSL can only store such a link as an LX symlink, which Windows VS
/// Code or Windows Terminal read as an unreadable file. `wsl`: the `env` fact says WSL.
pub fn wsl_link_to_windows_side(wsl: bool, dest: &str, src: &str) -> bool {
    wsl && is_windows_mount(dest) && !is_windows_mount(src)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn get(v: &[(&'static str, Option<String>)], name: &str) -> Option<String> {
        v.iter().find(|(n, _)| *n == name).unwrap().1.clone()
    }

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn linux_follows_xdg() {
        let v = known_folders(Os::Linux, "/home/me", env(&[]));
        assert_eq!(get(&v, "config_dir").as_deref(), Some("/home/me/.config"));
        assert_eq!(
            get(&v, "data_dir").as_deref(),
            Some("/home/me/.local/share")
        );
        assert_eq!(get(&v, "cache_dir").as_deref(), Some("/home/me/.cache"));
        assert_eq!(
            get(&v, "state_dir").as_deref(),
            Some("/home/me/.local/state")
        );
        assert_eq!(get(&v, "xdg_config_home"), get(&v, "config_dir"));
        for n in ["appdata", "localappdata", "userprofile", "programfiles"] {
            assert_eq!(get(&v, n), None, "{n}");
        }
        let v = known_folders(
            Os::Linux,
            "/home/me/",
            env(&[
                ("XDG_CONFIG_HOME", "/cfg//x/"),
                ("XDG_CACHE_HOME", "relative"),
            ]),
        );
        assert_eq!(get(&v, "config_dir").as_deref(), Some("/cfg/x"));
        assert_eq!(get(&v, "xdg_config_home").as_deref(), Some("/cfg/x"));
        assert_eq!(
            get(&v, "cache_dir").as_deref(),
            Some("/home/me/.cache"),
            "a relative XDG value is ignored"
        );
        assert_eq!(v.len(), NAMES.len());
    }

    #[test]
    fn macos_uses_library_and_keeps_xdg_separately() {
        let v = known_folders(Os::Macos, "/Users/me", env(&[("XDG_CONFIG_HOME", "/x")]));
        assert_eq!(
            get(&v, "config_dir").as_deref(),
            Some("/Users/me/Library/Application Support")
        );
        assert_eq!(
            get(&v, "cache_dir").as_deref(),
            Some("/Users/me/Library/Caches")
        );
        assert_eq!(get(&v, "xdg_config_home").as_deref(), Some("/x"));
        assert_eq!(
            get(&v, "xdg_data_home").as_deref(),
            Some("/Users/me/.local/share")
        );
        assert_eq!(get(&v, "appdata"), None);
    }

    #[test]
    fn windows_uses_the_known_folder_variables() {
        let v = known_folders(
            Os::Windows,
            r"C:\Users\me",
            env(&[
                ("APPDATA", r"C:\Users\me\AppData\Roaming\"),
                ("LOCALAPPDATA", "D:/Local"),
                ("ProgramFiles", r"C:\Program Files"),
            ]),
        );
        assert_eq!(
            get(&v, "config_dir").as_deref(),
            Some(r"C:\Users\me\AppData\Roaming")
        );
        assert_eq!(get(&v, "appdata"), get(&v, "config_dir"));
        assert_eq!(get(&v, "cache_dir").as_deref(), Some(r"D:\Local"));
        assert_eq!(get(&v, "localappdata").as_deref(), Some(r"D:\Local"));
        assert_eq!(get(&v, "userprofile").as_deref(), Some(r"C:\Users\me"));
        assert_eq!(
            get(&v, "programfiles").as_deref(),
            Some(r"C:\Program Files")
        );
        assert_eq!(get(&v, "xdg_config_home"), None);
        // Nothing set: derived from home.
        let v = known_folders(Os::Windows, r"C:\Users\me", env(&[]));
        assert_eq!(
            get(&v, "localappdata").as_deref(),
            Some(r"C:\Users\me\AppData\Local")
        );
        assert_eq!(
            get(&v, "programfiles").as_deref(),
            Some(r"C:\Program Files")
        );
    }

    #[test]
    fn vs_code_settings_is_one_line_everywhere() {
        let cases = [
            (
                Os::Linux,
                "/home/me",
                "/home/me/.config/Code/User/settings.json",
            ),
            (
                Os::Macos,
                "/Users/me",
                "/Users/me/Library/Application Support/Code/User/settings.json",
            ),
            (
                Os::Windows,
                r"C:\Users\me",
                r"C:\Users\me\AppData\Roaming\Code\User\settings.json",
            ),
        ];
        for (os, home, want) in cases {
            let v = known_folders(os, home, env(&[]));
            let rendered = format!("{}/Code/User/settings.json", get(&v, "config_dir").unwrap());
            assert_eq!(normalize(os, &rendered), want, "{os:?}");
        }
    }

    #[test]
    fn windows_simulated_on_a_unix_host_lives_under_home() {
        let v = known_folders_on(
            Os::Windows,
            Os::Macos,
            "x86_64",
            "/tmp/sbx/home",
            env(&[("APPDATA", r"C:\Elsewhere"), ("LOCALAPPDATA", "/x")]),
        );
        assert_eq!(
            get(&v, "config_dir").as_deref(),
            Some("/tmp/sbx/home/AppData/Roaming")
        );
        assert_eq!(
            get(&v, "localappdata").as_deref(),
            Some("/tmp/sbx/home/AppData/Local"),
            "the host's environment is not Windows'"
        );
        assert_eq!(get(&v, "userprofile").as_deref(), Some("/tmp/sbx/home"));
        assert_eq!(
            get(&v, "programfiles").as_deref(),
            Some("/tmp/sbx/home/Program Files")
        );
        assert!(is_absolute(Os::Linux, &get(&v, "appdata").unwrap()));
        assert_eq!(get(&v, "xdg_config_home"), None);
        // Not simulated: as before.
        assert_eq!(
            known_folders_on(
                Os::Windows,
                Os::Windows,
                std::env::consts::ARCH,
                r"C:\Users\me",
                env(&[])
            ),
            known_folders(Os::Windows, r"C:\Users\me", env(&[]))
        );
    }

    #[test]
    fn documents_winget_links_and_homebrew() {
        let win = known_folders(
            Os::Windows,
            r"C:\Users\me",
            env(&[
                ("LOCALAPPDATA", r"C:\Users\me\AppData\Local"),
                (DOCUMENTS_KEY, r"C:\Users\me\OneDrive\Documents"),
            ]),
        );
        assert_eq!(
            get(&win, "documents").as_deref(),
            Some(r"C:\Users\me\OneDrive\Documents"),
            "redirected to OneDrive"
        );
        assert_eq!(
            get(&win, "winget_links").as_deref(),
            Some(r"C:\Users\me\AppData\Local\Microsoft\WinGet\Links")
        );
        assert_eq!(get(&win, "homebrew_prefix"), None);
        let plain = known_folders(Os::Windows, r"C:\Users\me", env(&[]));
        assert_eq!(
            get(&plain, "documents").as_deref(),
            Some(r"C:\Users\me\Documents")
        );
        let arm = known_folders_on(Os::Macos, Os::Macos, "aarch64", "/Users/me", env(&[]));
        assert_eq!(
            get(&arm, "homebrew_prefix").as_deref(),
            Some("/opt/homebrew")
        );
        assert_eq!(
            get(&arm, "documents").as_deref(),
            Some("/Users/me/Documents")
        );
        let intel = known_folders_on(Os::Macos, Os::Macos, "x86_64", "/Users/me", env(&[]));
        assert_eq!(
            get(&intel, "homebrew_prefix").as_deref(),
            Some("/usr/local")
        );
        let linux = known_folders(Os::Linux, "/home/me", env(&[]));
        assert_eq!(
            get(&linux, "homebrew_prefix").as_deref(),
            Some("/home/linuxbrew/.linuxbrew")
        );
        assert_eq!(get(&linux, "winget_links"), None);
    }

    #[test]
    fn normalizing_separators() {
        assert_eq!(normalize(Os::Windows, "C:/a//b/"), r"C:\a\b");
        assert_eq!(normalize(Os::Windows, "C:/"), r"C:\");
        assert_eq!(normalize(Os::Windows, r"\\srv\share\x\"), r"\\srv\share\x");
        assert_eq!(normalize(Os::Linux, "/"), "/");
        assert_eq!(normalize(Os::Linux, "/a//b/"), "/a/b");
        assert_eq!(
            normalize(Os::Linux, r"/a\b"),
            r"/a\b",
            "a backslash is a name"
        );
        assert!(is_absolute(Os::Windows, "c:/x") && is_absolute(Os::Windows, r"\\s\x"));
        assert!(!is_absolute(Os::Windows, "/x") && !is_absolute(Os::Windows, "x"));
        assert!(is_absolute(Os::Linux, "/x") && !is_absolute(Os::Linux, "C:/x"));
        assert_eq!(
            join(Os::Windows, r"C:\Users\me\", &["AppData", "Local"]),
            r"C:\Users\me\AppData\Local"
        );
    }

    #[test]
    fn wsl_paths() {
        assert_eq!(
            wsl_path("C:\\Users\\me\r\n").as_deref(),
            Some("/mnt/c/Users/me")
        );
        assert_eq!(wsl_path("D:\\").as_deref(), Some("/mnt/d"));
        assert_eq!(wsl_path("%USERPROFILE%"), None);
        assert_eq!(wsl_path(r"\\wsl$\x"), None);
        assert!(is_windows_mount("/mnt/c/Windows/System32/winget.exe"));
        assert!(is_windows_mount("/mnt/d"));
        assert!(!is_windows_mount("/mnt/data/bin/apt"));
        assert!(!is_windows_mount("/usr/bin/apt"));
        let settings = "/mnt/c/Users/me/AppData/Roaming/Code/User/settings.json";
        assert!(wsl_link_to_windows_side(
            true,
            settings,
            "/home/me/.dot/bundles/code/settings.json"
        ));
        assert!(!wsl_link_to_windows_side(false, settings, "/home/me/x"));
        assert!(!wsl_link_to_windows_side(
            true,
            "/home/me/.rc",
            "/home/me/x"
        ));
        assert!(
            !wsl_link_to_windows_side(true, settings, "/mnt/c/dotfiles/x"),
            "both on the Windows side"
        );
    }
}
