//! Picking the release asset for this machine, and finding a checksum for it.
//!
//! Without an `asset:` glob the choice is made from the asset names, the way people name them:
//! the OS (`linux`, `darwin`/`macos`/`apple`, `windows`/`win64`/`.exe`) must be named; the
//! architecture should be (`x86_64`/`amd64`/`x64`, `aarch64`/`arm64`), and an asset naming a
//! different one is never picked; on Linux a static `musl` build beats one naming no libc, which
//! beats `gnu`. Checksums, signatures, SBOMs and OS packages (`.deb`, `.msi`, ...) are never
//! picked. Remaining ties go to the preferred archive format, then the shortest name, except
//! that a single executable published both raw and archived (`yq_linux_amd64` and
//! `yq_linux_amd64.tar.gz`) is taken raw: the archive holds the same file under its platform
//! name.

use dot_core::config::Extract;
use dot_core::facts::Os;
use globset::GlobMatcher;

use super::api::Asset;

/// Whether `word` occurs in `name` (both lower case) on its own: not preceded or followed by a
/// letter or digit.
fn mentions(name: &str, word: &str) -> bool {
    let bytes = name.as_bytes();
    name.match_indices(word).any(|(i, _)| {
        let before = i.checked_sub(1).map(|j| bytes[j]);
        let after = bytes.get(i + word.len()).copied();
        !before.is_some_and(|b| b.is_ascii_alphanumeric())
            && !after.is_some_and(|b| b.is_ascii_alphanumeric())
    })
}

/// The name as matched: lower case, with `x86_64`/`x86-64` spelled `amd64` so that `x86` alone
/// means 32-bit.
fn normalize(name: &str) -> String {
    name.to_ascii_lowercase()
        .replace("x86_64", "amd64")
        .replace("x86-64", "amd64")
}

const OS_WORDS: [(&str, &[&str]); 3] = [
    ("linux", &["linux"]),
    (
        "macos",
        &["darwin", "macos", "macosx", "osx", "apple", "mac"],
    ),
    (
        "windows",
        &["windows", "win", "win32", "win64", "msvc", "mingw"],
    ),
];

/// Systems no asset is ever picked for.
const OTHER_OS_WORDS: [&str; 10] = [
    "freebsd",
    "netbsd",
    "openbsd",
    "dragonfly",
    "android",
    "illumos",
    "solaris",
    "aix",
    "ios",
    "wasm",
];

/// Architecture families and their spellings (after [`normalize`]).
const ARCH_WORDS: [(&str, &[&str]); 9] = [
    ("x86_64", &["amd64", "x64", "64bit", "64-bit"]),
    ("aarch64", &["aarch64", "arm64", "armv8"]),
    (
        "x86",
        &["x86", "i386", "i586", "i686", "386", "32bit", "32-bit"],
    ),
    (
        "arm",
        &[
            "arm", "armv5", "armv6", "armv7", "armv7l", "armhf", "armel", "arm32",
        ],
    ),
    (
        "powerpc64",
        &["ppc64", "ppc64le", "powerpc64", "powerpc64le"],
    ),
    ("s390x", &["s390x"]),
    ("riscv64", &["riscv64", "riscv64gc"]),
    ("loongarch64", &["loong64", "loongarch64"]),
    ("mips", &["mips", "mipsle", "mips64", "mips64le"]),
];

const UNIVERSAL_WORDS: [&str; 3] = ["universal", "universal2", "all"];

/// Assets that are never the program itself.
const AUX_SUFFIXES: [&str; 29] = [
    ".sha256",
    ".sha256sum",
    ".sha512",
    ".sha512sum",
    ".sha1",
    ".md5",
    ".b3",
    ".asc",
    ".sig",
    ".minisig",
    ".pem",
    ".crt",
    ".cert",
    ".sbom",
    ".spdx",
    ".json",
    ".txt",
    ".yml",
    ".yaml",
    ".jsonl",
    ".deb",
    ".rpm",
    ".apk",
    ".msi",
    ".pkg",
    ".dmg",
    ".snap",
    ".vsix",
    ".nupkg",
];

/// Compressed formats `dot` cannot unpack.
const UNSUPPORTED_SUFFIXES: [&str; 11] = [
    ".tar.bz2", ".tbz", ".tbz2", ".tar.zst", ".tzst", ".7z", ".rar", ".gz", ".bz2", ".zst", ".xz",
];

/// What a name says about how to unpack it; `None`: a format `dot` cannot unpack.
pub fn format_of(name: &str) -> Option<Extract> {
    let n = name.to_ascii_lowercase();
    if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
        Some(Extract::TarGz)
    } else if n.ends_with(".tar.xz") || n.ends_with(".txz") {
        Some(Extract::TarXz)
    } else if n.ends_with(".zip") {
        Some(Extract::Zip)
    } else if UNSUPPORTED_SUFFIXES.iter().any(|s| n.ends_with(s)) || n.ends_with(".tar") {
        None
    } else {
        Some(Extract::Raw)
    }
}

fn is_auxiliary(n: &str) -> bool {
    AUX_SUFFIXES.iter().any(|s| n.ends_with(s))
        || n.contains("checksum")
        || n.contains("sha256sum")
        || n.contains("sbom")
}

/// A candidate's rank; higher is better, compared field by field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Score {
    os: u8,
    arch: u8,
    libc: u8,
    format: u8,
}

/// Scores `name` for `os`/`arch`. `None`: not for this machine (or not installable). With
/// `strict: false` (the user's glob already chose), a missing OS or architecture only lowers
/// the score.
fn score(name: &str, os: Os, arch: &str, strict: bool) -> Option<Score> {
    let n = normalize(name);
    if is_auxiliary(&n) && strict {
        return None;
    }
    let format = format_of(name)?;
    let exe = n.ends_with(".exe");
    let os_hit = |key: &str| {
        OS_WORDS
            .iter()
            .find(|(k, _)| *k == key)
            .is_some_and(|(_, words)| words.iter().any(|w| mentions(&n, w)))
    };
    let ours = os_hit(os.as_str()) || (os == Os::Windows && exe);
    // `aarch64-linux-android` names Linux, but is not for it.
    if OTHER_OS_WORDS.iter().any(|w| mentions(&n, w)) {
        return None;
    }
    let others = OS_WORDS
        .iter()
        .filter(|(k, _)| *k != os.as_str())
        .any(|(k, _)| os_hit(k))
        || (os != Os::Windows && exe);
    if others && !ours {
        return None;
    }
    if !ours && strict {
        return None;
    }
    let arch_hit = |family: &str| {
        ARCH_WORDS
            .iter()
            .find(|(k, _)| *k == family)
            .is_some_and(|(_, words)| words.iter().any(|w| mentions(&n, w)))
    };
    let other_arch = ARCH_WORDS
        .iter()
        .filter(|(k, _)| *k != arch)
        .any(|(k, _)| arch_hit(k));
    let arch_score = if arch_hit(arch) {
        3
    } else if other_arch {
        if strict {
            return None;
        }
        0
    } else if UNIVERSAL_WORDS.iter().any(|w| mentions(&n, w)) {
        2
    } else {
        1
    };
    let libc = match os {
        Os::Linux if mentions(&n, "musl") => 2,
        Os::Linux if mentions(&n, "gnu") || mentions(&n, "glibc") => 0,
        Os::Windows if mentions(&n, "msvc") => 2,
        Os::Windows if mentions(&n, "gnu") || mentions(&n, "mingw") => 0,
        _ => 1,
    };
    let format = match (os, format) {
        (Os::Windows, Extract::Zip) => 3,
        (Os::Windows, Extract::Raw) if exe => 3,
        (Os::Windows, _) => 1,
        (_, Extract::TarGz | Extract::TarXz) => 3,
        (_, Extract::Zip) => 2,
        _ => 1,
    };
    Some(Score {
        os: u8::from(ours),
        arch: arch_score,
        libc,
        format,
    })
}

/// `name` without an archive extension, if it has one.
fn archive_stem(name: &str) -> Option<&str> {
    let lower = name.to_ascii_lowercase();
    [".tar.gz", ".tgz", ".tar.xz", ".txz", ".zip"]
        .iter()
        .find(|ext| lower.ends_with(*ext))
        .map(|ext| &name[..name.len() - ext.len()])
}

/// Whether `text` (e.g. `linux_amd64`, the rest of `yq_linux_amd64`) names an OS or an
/// architecture.
pub fn names_platform(text: &str) -> bool {
    let n = normalize(text);
    OS_WORDS
        .iter()
        .flat_map(|(_, words)| words.iter())
        .chain(ARCH_WORDS.iter().flat_map(|(_, words)| words.iter()))
        .any(|w| mentions(&n, w))
}

/// The best candidate. With `single` (one executable wanted), an archive that is just a raw
/// candidate's name plus an archive extension gives way to the raw one.
fn best<'a>(scored: impl Iterator<Item = (Score, &'a Asset)>, single: bool) -> Option<&'a Asset> {
    let scored: Vec<(Score, &Asset)> = scored.collect();
    let raw_twin = |a: &Asset| {
        archive_stem(&a.name).is_some_and(|stem| {
            scored.iter().any(|(_, r)| {
                format_of(&r.name) == Some(Extract::Raw)
                    && (r.name == stem || r.name.strip_suffix(".exe") == Some(stem))
            })
        })
    };
    scored
        .iter()
        .filter(|(_, a)| !(single && raw_twin(a)))
        .max_by(|(sa, a), (sb, b)| {
            sa.cmp(sb)
                // Shorter, then alphabetically first, wins: reversed because `max_by`.
                .then_with(|| b.name.len().cmp(&a.name.len()))
                .then_with(|| b.name.cmp(&a.name))
        })
        .map(|(_, a)| *a)
}

fn names(assets: &[Asset]) -> String {
    if assets.is_empty() {
        return "none".into();
    }
    let v: Vec<&str> = assets.iter().map(|a| a.name.as_str()).collect();
    v.join(", ")
}

/// The asset to install on `os`/`arch`, or why there is none. `single`: one executable is
/// wanted, so a raw asset can serve.
pub fn select<'a>(
    assets: &'a [Asset],
    glob: Option<(&str, &GlobMatcher)>,
    os: Os,
    arch: &str,
    single: bool,
) -> Result<&'a Asset, String> {
    match glob {
        Some((pattern, m)) => {
            let hits: Vec<&Asset> = assets.iter().filter(|a| m.is_match(&a.name)).collect();
            match hits.as_slice() {
                [] => Err(format!(
                    "no asset matches `{pattern}`; the release has: {}",
                    names(assets)
                )),
                [one] => Ok(one),
                many => best(
                    many.iter()
                        .filter_map(|a| score(&a.name, os, arch, false).map(|s| (s, *a))),
                    single,
                )
                .ok_or_else(|| {
                    let v: Vec<&str> = many.iter().map(|a| a.name.as_str()).collect();
                    format!(
                        "`{pattern}` matches several assets and none can be installed here: {}",
                        v.join(", ")
                    )
                }),
            }
        }
        None => best(
            assets
                .iter()
                .filter_map(|a| score(&a.name, os, arch, true).map(|s| (s, a))),
            single,
        )
        .ok_or_else(|| {
            format!(
                "no asset for {}/{arch} (set `asset:` to choose one); the release has: {}",
                os.as_str(),
                names(assets)
            )
        }),
    }
}

/// The OS package (`.deb` / `.rpm`, by `ext`) to install on `arch`, or why there is none: an
/// asset naming this architecture wins over an architecture-independent one (`_all.deb`,
/// `.noarch.rpm`, or none named); one naming another architecture is never picked. `glob`
/// narrows the candidates first.
pub fn select_package<'a>(
    assets: &'a [Asset],
    glob: Option<(&str, &GlobMatcher)>,
    ext: &str,
    arch: &str,
) -> Result<&'a Asset, String> {
    let suffix = format!(".{ext}");
    let candidates: Vec<&Asset> = assets
        .iter()
        .filter(|a| a.name.to_ascii_lowercase().ends_with(&suffix))
        .filter(|a| glob.is_none_or(|(_, m)| m.is_match(&a.name)))
        .collect();
    let family_hit = |n: &str, family: &str| {
        ARCH_WORDS
            .iter()
            .find(|(k, _)| *k == family)
            .is_some_and(|(_, words)| words.iter().any(|w| mentions(n, w)))
    };
    candidates
        .iter()
        .filter_map(|a| {
            let n = normalize(&a.name);
            if family_hit(&n, arch) {
                return Some((2u8, *a));
            }
            let other = ARCH_WORDS
                .iter()
                .any(|(k, _)| *k != arch && family_hit(&n, k));
            (!other).then_some((1u8, *a))
        })
        .max_by(|(sa, a), (sb, b)| sa.cmp(sb).then_with(|| b.name.len().cmp(&a.name.len())))
        .map(|(_, a)| a)
        .ok_or_else(|| {
            let what = match glob {
                Some((p, _)) => format!("no .{ext} asset for {arch} matches `{p}`"),
                None => format!("no .{ext} asset for {arch} (set `asset:` to choose one)"),
            };
            format!("{what}; the release has: {}", names(assets))
        })
}

/// Where the expected SHA-256 of an asset comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChecksumSource {
    /// GitHub's own `digest` for the asset.
    Digest(String),
    /// A file among the release assets: `<asset>.sha256`, or a list such as `checksums.txt`.
    File { name: String, url: String },
}

/// Finds a published SHA-256 for `chosen`.
pub fn checksum_source(assets: &[Asset], chosen: &Asset) -> Option<ChecksumSource> {
    if let Some(hex) = chosen
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .filter(|h| is_sha256(h))
    {
        return Some(ChecksumSource::Digest(hex.to_ascii_lowercase()));
    }
    let file = |a: &Asset| ChecksumSource::File {
        name: a.name.clone(),
        url: a.browser_download_url.clone(),
    };
    let own = [
        format!("{}.sha256", chosen.name),
        format!("{}.sha256sum", chosen.name),
    ];
    if let Some(a) = assets.iter().find(|a| own.contains(&a.name)) {
        return Some(file(a));
    }
    let lists: Vec<&Asset> = assets
        .iter()
        .filter(|a| {
            let n = a.name.to_ascii_lowercase();
            let signature = [".sig", ".asc", ".pem", ".minisig", ".crt"]
                .iter()
                .any(|s| n.ends_with(s));
            let listing = n.contains("checksums") || n.contains("sha256sums") || n == "sha256.txt";
            listing && !signature && !n.contains("sha512") && !n.contains("sha1")
        })
        .collect();
    lists
        .iter()
        .find(|a| a.name.to_ascii_lowercase().contains("sha256"))
        .or(lists.first())
        .map(|a| file(a))
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The SHA-256 for `asset` in a checksum file: `<hex>  <name>` lines (`sha256sum` output, with
/// or without `*` for binary mode), or a bare `<hex>` in a single-asset `.sha256` file.
pub fn find_checksum(text: &str, asset: &str) -> Option<String> {
    let all: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    for line in &all {
        let mut parts = line.split_whitespace();
        let (Some(hex), name) = (parts.next(), parts.next()) else {
            continue;
        };
        if !is_sha256(hex) {
            continue;
        }
        match name {
            Some(n) => {
                let n = n.trim_start_matches('*').trim_start_matches("./");
                let base = n.rsplit('/').next().unwrap_or(n);
                if n == asset || base == asset {
                    return Some(hex.to_ascii_lowercase());
                }
            }
            None if all.len() == 1 => return Some(hex.to_ascii_lowercase()),
            None => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assets(names: &[&str]) -> Vec<Asset> {
        names
            .iter()
            .map(|n| Asset {
                name: n.to_string(),
                browser_download_url: format!("https://example.invalid/{n}"),
                size: 0,
                digest: None,
                id: None,
            })
            .collect()
    }

    fn pick(names: &[&str], os: Os, arch: &str) -> Result<String, String> {
        let a = assets(names);
        select(&a, None, os, arch, true).map(|a| a.name.clone())
    }

    #[test]
    fn a_raw_executable_beats_its_own_archive() {
        const YQ: [&str; 9] = [
            "checksums",
            "yq_darwin_arm64",
            "yq_darwin_arm64.tar.gz",
            "yq_linux_amd64",
            "yq_linux_amd64.tar.gz",
            "yq_linux_arm64",
            "yq_linux_arm64.tar.gz",
            "yq_windows_amd64.exe",
            "yq_windows_amd64.zip",
        ];
        assert_eq!(pick(&YQ, Os::Linux, "x86_64").unwrap(), "yq_linux_amd64");
        assert_eq!(pick(&YQ, Os::Macos, "aarch64").unwrap(), "yq_darwin_arm64");
        assert_eq!(
            pick(&YQ, Os::Windows, "x86_64").unwrap(),
            "yq_windows_amd64.exe"
        );
        // Several executables wanted: only the archive can hold them.
        let a = assets(&YQ);
        assert_eq!(
            select(&a, None, Os::Linux, "x86_64", false).unwrap().name,
            "yq_linux_amd64.tar.gz"
        );
    }

    const RIPGREP: [&str; 12] = [
        "ripgrep-14.1.1-aarch64-apple-darwin.tar.gz",
        "ripgrep-14.1.1-aarch64-apple-darwin.tar.gz.sha256",
        "ripgrep-14.1.1-aarch64-unknown-linux-gnu.tar.gz",
        "ripgrep-14.1.1-armv7-unknown-linux-musleabihf.tar.gz",
        "ripgrep-14.1.1-i686-pc-windows-msvc.zip",
        "ripgrep-14.1.1-x86_64-apple-darwin.tar.gz",
        "ripgrep-14.1.1-x86_64-pc-windows-gnu.zip",
        "ripgrep-14.1.1-x86_64-pc-windows-msvc.zip",
        "ripgrep-14.1.1-x86_64-unknown-linux-musl.tar.gz",
        "ripgrep-14.1.1-x86_64-unknown-linux-musl.tar.gz.sha256",
        "ripgrep_14.1.1-1_amd64.deb",
        "ripgrep-14.1.1-i686-unknown-linux-gnu.tar.gz",
    ];

    #[test]
    fn rust_style_triples() {
        assert_eq!(
            pick(&RIPGREP, Os::Linux, "x86_64").unwrap(),
            "ripgrep-14.1.1-x86_64-unknown-linux-musl.tar.gz"
        );
        assert_eq!(
            pick(&RIPGREP, Os::Linux, "aarch64").unwrap(),
            "ripgrep-14.1.1-aarch64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            pick(&RIPGREP, Os::Macos, "aarch64").unwrap(),
            "ripgrep-14.1.1-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            pick(&RIPGREP, Os::Windows, "x86_64").unwrap(),
            "ripgrep-14.1.1-x86_64-pc-windows-msvc.zip"
        );
        assert_eq!(
            pick(&RIPGREP, Os::Linux, "x86").unwrap(),
            "ripgrep-14.1.1-i686-unknown-linux-gnu.tar.gz"
        );
    }

    #[test]
    fn go_style_names() {
        let names = [
            "lazygit_0.44.1_Darwin_arm64.tar.gz",
            "lazygit_0.44.1_Darwin_x86_64.tar.gz",
            "lazygit_0.44.1_Linux_32-bit.tar.gz",
            "lazygit_0.44.1_Linux_arm64.tar.gz",
            "lazygit_0.44.1_Linux_armv6.tar.gz",
            "lazygit_0.44.1_Linux_x86_64.tar.gz",
            "lazygit_0.44.1_Windows_x86_64.zip",
            "lazygit_0.44.1_freebsd_x86_64.tar.gz",
            "checksums.txt",
        ];
        assert_eq!(
            pick(&names, Os::Linux, "x86_64").unwrap(),
            "lazygit_0.44.1_Linux_x86_64.tar.gz"
        );
        assert_eq!(
            pick(&names, Os::Linux, "aarch64").unwrap(),
            "lazygit_0.44.1_Linux_arm64.tar.gz"
        );
        assert_eq!(
            pick(&names, Os::Macos, "x86_64").unwrap(),
            "lazygit_0.44.1_Darwin_x86_64.tar.gz"
        );
        assert_eq!(
            pick(&names, Os::Windows, "x86_64").unwrap(),
            "lazygit_0.44.1_Windows_x86_64.zip"
        );
    }

    #[test]
    fn plain_binaries_universal_and_missing_arch() {
        let names = [
            "tool-linux-amd64",
            "tool-linux-arm64",
            "tool-darwin-universal",
            "tool-windows-amd64.exe",
            "tool.exe",
        ];
        assert_eq!(
            pick(&names, Os::Linux, "x86_64").unwrap(),
            "tool-linux-amd64"
        );
        assert_eq!(
            pick(&names, Os::Macos, "aarch64").unwrap(),
            "tool-darwin-universal"
        );
        assert_eq!(
            pick(&names, Os::Windows, "x86_64").unwrap(),
            "tool-windows-amd64.exe"
        );
        // An asset naming the OS but no architecture is used when nothing better exists.
        assert_eq!(
            pick(
                &["t-macos.zip", "t-linux-arm64.tar.gz"],
                Os::Macos,
                "x86_64"
            )
            .unwrap(),
            "t-macos.zip"
        );
        // A different architecture is never picked.
        let e = pick(
            &["t-linux-arm64.tar.gz", "t_amd64.deb"],
            Os::Linux,
            "x86_64",
        )
        .unwrap_err();
        assert!(e.contains("no asset for linux/x86_64"), "{e}");
        assert!(e.contains("t-linux-arm64.tar.gz, t_amd64.deb"), "{e}");
        // Other systems and unsupported formats are never picked.
        assert!(pick(&["t-linux-android-arm64.tar.gz"], Os::Linux, "aarch64").is_err());
        assert!(pick(&["t-linux-amd64.tar.bz2"], Os::Linux, "x86_64").is_err());
        assert!(pick(&["t-linux-amd64.tar.zst"], Os::Linux, "x86_64").is_err());
        // `mac` inside another word is not macOS.
        assert!(pick(&["machine-linux-amd64.tar.gz"], Os::Macos, "x86_64").is_err());
    }

    #[test]
    fn ties_prefer_format_then_the_shortest_name() {
        let names = [
            "t-linux-amd64.zip",
            "t-linux-amd64-v3.tar.gz",
            "t-linux-amd64.tar.gz",
        ];
        assert_eq!(
            pick(&names, Os::Linux, "x86_64").unwrap(),
            "t-linux-amd64.tar.gz"
        );
        let names = ["t-windows-amd64.tar.gz", "t-windows-amd64.zip"];
        assert_eq!(
            pick(&names, Os::Windows, "x86_64").unwrap(),
            "t-windows-amd64.zip"
        );
    }

    #[test]
    fn a_glob_narrows_the_choice() {
        let a = assets(&RIPGREP);
        let glob = |p: &str| globset::Glob::new(p).unwrap().compile_matcher();
        let g = glob("*-x86_64-unknown-linux-musl.tar.gz");
        let chosen = select(&a, Some(("p", &g)), Os::Macos, "aarch64", true).unwrap();
        assert_eq!(
            chosen.name,
            "ripgrep-14.1.1-x86_64-unknown-linux-musl.tar.gz"
        );
        // Several matches: the heuristics pick among them.
        let g = glob("*linux*.tar.gz");
        let chosen = select(&a, Some(("p", &g)), Os::Linux, "aarch64", true).unwrap();
        assert_eq!(
            chosen.name,
            "ripgrep-14.1.1-aarch64-unknown-linux-gnu.tar.gz"
        );
        let g = glob("*.dmg");
        let e = select(&a, Some(("*.dmg", &g)), Os::Linux, "x86_64", true).unwrap_err();
        assert!(e.contains("no asset matches `*.dmg`"), "{e}");
    }

    #[test]
    fn formats() {
        assert_eq!(format_of("a.tar.gz"), Some(Extract::TarGz));
        assert_eq!(format_of("a.TGZ"), Some(Extract::TarGz));
        assert_eq!(format_of("a.tar.xz"), Some(Extract::TarXz));
        assert_eq!(format_of("a.zip"), Some(Extract::Zip));
        assert_eq!(format_of("a-linux-amd64"), Some(Extract::Raw));
        assert_eq!(format_of("a.exe"), Some(Extract::Raw));
        assert_eq!(format_of("a.tar.bz2"), None);
        assert_eq!(format_of("a.gz"), None);
    }

    #[test]
    fn checksum_sources() {
        let mut a = assets(&[
            "t-linux-amd64.tar.gz",
            "t-linux-amd64.tar.gz.sha256",
            "checksums.txt",
            "checksums.txt.sig",
        ]);
        let chosen = a[0].clone();
        assert_eq!(
            checksum_source(&a, &chosen),
            Some(ChecksumSource::File {
                name: "t-linux-amd64.tar.gz.sha256".into(),
                url: "https://example.invalid/t-linux-amd64.tar.gz.sha256".into()
            })
        );
        a.remove(1);
        assert!(matches!(
            checksum_source(&a, &chosen),
            Some(ChecksumSource::File { name, .. }) if name == "checksums.txt"
        ));
        let hex = "ab".repeat(32);
        let mut with_digest = chosen.clone();
        with_digest.digest = Some(format!("sha256:{}", hex.to_uppercase()));
        assert_eq!(
            checksum_source(&a, &with_digest),
            Some(ChecksumSource::Digest(hex))
        );
        let none = assets(&["t-linux-amd64.tar.gz", "t.sha512"]);
        assert_eq!(checksum_source(&none, &none[0]), None);
        let many = assets(&["t.tar.gz", "SHA512SUMS", "SHA256SUMS"]);
        assert!(matches!(
            checksum_source(&many, &many[0]),
            Some(ChecksumSource::File { name, .. }) if name == "SHA256SUMS"
        ));
    }

    #[test]
    fn checksum_file_formats() {
        let h1 = "a".repeat(64);
        let h2 = "B".repeat(64);
        let list = format!("{h1}  other.tar.gz\n{h2} *./dist/t.tar.gz\n");
        assert_eq!(find_checksum(&list, "t.tar.gz"), Some("b".repeat(64)));
        assert_eq!(find_checksum(&list, "missing"), None);
        assert_eq!(
            find_checksum(&format!("{h1}\n"), "t.tar.gz"),
            Some(h1.clone())
        );
        assert_eq!(find_checksum("not a checksum  t.tar.gz", "t.tar.gz"), None);
    }
}
