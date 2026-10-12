//! `--dot-binary PATH` (default `DOT_HOST_BINARY`): the `dot` to install on a remote, from a file
//! here instead of a download (no GitHub, no `install.sh`).
//!
//! Before anything is copied, the file's header must say it runs on the remote's OS and CPU (as
//! the probe found them): an ELF `e_machine` for Linux, a Mach-O `cputype` (any slice of a
//! universal binary) for macOS. Read here, with no `file` command. Its `--version` is then checked
//! there, after the upload and before it is renamed into place (see [`super::Remote`]). Its
//! SHA-256 tells a dot of this version there that is this very build (left as it is) from
//! another build of the same version (replaced: every build from source says the same version).

use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use sha2::{Digest as _, Sha256};

use super::Target;

/// The environment variable `--dot-binary` defaults to.
pub const ENV: &str = "DOT_HOST_BINARY";

/// What a file's header says it runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kind {
    /// `linux`, `macos`, or another OS an ELF names.
    pub os: String,
    /// The CPUs it runs on, as [`Target`] names them (`x86_64`, `aarch64`), others by their own
    /// name: one, or each slice of a universal Mach-O.
    pub archs: Vec<String>,
}

impl Kind {
    pub fn runs_on(&self, t: Target) -> bool {
        self.os == t.os && self.archs.iter().any(|a| a == t.arch)
    }

    /// `a linux/aarch64 binary`, `a macos/x86_64+aarch64 (universal) binary`.
    pub fn describe(&self) -> String {
        format!(
            "a {}/{} {}binary",
            self.os,
            self.archs.join("+"),
            if self.archs.len() > 1 {
                "(universal) "
            } else {
                ""
            }
        )
    }
}

fn u16_at(b: &[u8], at: usize, big: bool) -> Option<u16> {
    let s: [u8; 2] = b.get(at..at + 2)?.try_into().ok()?;
    Some(if big {
        u16::from_be_bytes(s)
    } else {
        u16::from_le_bytes(s)
    })
}

fn u32_at(b: &[u8], at: usize, big: bool) -> Option<u32> {
    let s: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
    Some(if big {
        u32::from_be_bytes(s)
    } else {
        u32::from_le_bytes(s)
    })
}

/// `m` as [`Target`] names a CPU; `x86_64` and `aarch64` only for 64-bit ELF (a 32-bit one for
/// those machines is the x32 or ILP32 ABI, which dot is not built for).
fn elf_machine(m: u16, is64: bool) -> String {
    match m {
        62 if is64 => "x86_64".into(),
        62 => "x32".into(),
        183 if is64 => "aarch64".into(),
        183 => "aarch64_ilp32".into(),
        3 => "i386".into(),
        40 => "arm".into(),
        243 => "riscv".into(),
        20 => "ppc".into(),
        21 => "ppc64".into(),
        22 => "s390x".into(),
        8 => "mips".into(),
        other => format!("e_machine {other}"),
    }
}

fn macho_cpu(cpu: u32) -> String {
    const ABI64: u32 = 0x0100_0000;
    match cpu {
        0x0100_0007 => "x86_64".into(),
        0x0100_000C => "aarch64".into(),
        7 => "i386".into(),
        12 => "arm".into(),
        18 => "ppc".into(),
        c if c & ABI64 != 0 => format!("cputype {:#x}", c),
        c => format!("cputype {c}"),
    }
}

/// What the start of a file (its first few KiB) says it runs on; else what it seems to be.
pub fn parse(b: &[u8]) -> std::result::Result<Kind, String> {
    if b.starts_with(b"\x7fELF") {
        // e_ident (16 bytes), e_type and e_machine: at the same offsets in 32- and 64-bit ELF.
        if b.len() < 20 {
            return Err("a truncated ELF file".into());
        }
        let is64 = match b[4] {
            1 => false,
            2 => true,
            _ => return Err("an ELF file of unknown class".into()),
        };
        let big = match b.get(5) {
            Some(1) => false,
            Some(2) => true,
            _ => return Err("an ELF file of unknown byte order".into()),
        };
        // e_type: an executable or a position-independent one; not an object or a core dump.
        match u16_at(b, 16, big) {
            Some(2 | 3) => {}
            _ => return Err("an ELF file that is not an executable".into()),
        }
        let machine = u16_at(b, 18, big).ok_or("a truncated ELF file")?;
        let os = match b.get(7) {
            // System V (what Linux toolchains write) or GNU/Linux.
            Some(0 | 3) => "linux".to_string(),
            Some(9) => "freebsd".to_string(),
            Some(12) => "openbsd".to_string(),
            Some(2) => "netbsd".to_string(),
            Some(6) => "solaris".to_string(),
            Some(n) => format!("ELF OS/ABI {n}"),
            None => return Err("a truncated ELF file".into()),
        };
        return Ok(Kind {
            os,
            archs: vec![elf_machine(machine, is64)],
        });
    }
    // Thin Mach-O, 64- or 32-bit, little-endian (every Mac since PowerPC).
    if b.starts_with(&[0xCF, 0xFA, 0xED, 0xFE]) || b.starts_with(&[0xCE, 0xFA, 0xED, 0xFE]) {
        let cpu = u32_at(b, 4, false).ok_or("a truncated Mach-O file")?;
        // filetype: MH_EXECUTE, not a library (MH_DYLIB) or a bundle.
        match u32_at(b, 12, false) {
            Some(2) => {}
            Some(_) => return Err("a Mach-O file that is not an executable (a library?)".into()),
            None => return Err("a truncated Mach-O file".into()),
        }
        return Ok(Kind {
            os: "macos".into(),
            archs: vec![macho_cpu(cpu)],
        });
    }
    // Universal (fat) Mach-O: big-endian headers, one entry per slice (20 bytes, 32 for the
    // 64-bit variant). Java class files share 0xCAFEBABE: they have a far larger second word.
    let fat = if b.starts_with(&[0xCA, 0xFE, 0xBA, 0xBE]) {
        Some(20)
    } else if b.starts_with(&[0xCA, 0xFE, 0xBA, 0xBF]) {
        Some(32)
    } else {
        None
    };
    if let Some(size) = fat {
        let n = u32_at(b, 4, true).ok_or("a truncated Mach-O file")? as usize;
        if (1..=32).contains(&n) {
            let archs = (0..n)
                .map(|i| u32_at(b, 8 + i * size, true).map(macho_cpu))
                .collect::<Option<Vec<_>>>()
                .ok_or("a truncated universal Mach-O file")?;
            return Ok(Kind {
                os: "macos".into(),
                archs,
            });
        }
        return Err("a Java class file".into());
    }
    Err(if b.starts_with(&[0x1F, 0x8B]) {
        "a gzip archive (a release .tar.gz? pass the `dot` inside it)".into()
    } else if b.starts_with(b"PK\x03\x04") {
        "a zip archive (pass the `dot` inside it)".into()
    } else if b.starts_with(b"MZ") {
        "a Windows executable (--host supports Linux and macOS remotes)".into()
    } else if b.starts_with(b"#!") {
        "a script, not a dot binary".into()
    } else if b.is_empty() {
        "an empty file".into()
    } else {
        "neither an ELF nor a Mach-O executable".into()
    })
}

/// The Rust target a `dot` for `t` is built for (as released).
pub fn rust_target(t: Target) -> String {
    match t.os {
        "macos" => format!("{}-apple-darwin", t.arch),
        _ => format!("{}-unknown-linux-musl", t.arch),
    }
}

/// How to build a `dot` for `t` on this machine.
pub fn how_to_build(t: Target) -> String {
    match t.os {
        "linux" => format!(
            "build one with `scripts/build-linux.sh {a}` in dot's source (cargo zigbuild \
             --release -p dot-cli --target {}): a static musl build",
            rust_target(t),
            a = t.arch
        ),
        _ => format!(
            "build one with `cargo build --release -p dot-cli --target {}`",
            rust_target(t)
        ),
    }
}

/// The file `--dot-binary` (or `DOT_HOST_BINARY`) names, read and recognised.
#[derive(Debug, Clone)]
pub struct HostBinary {
    /// Absolute: scp takes `name:...` for a host.
    pub path: PathBuf,
    pub kind: Kind,
    /// Its SHA-256, lowercase hex.
    pub sha256: String,
    /// Where it was named, for messages: `--dot-binary` or `DOT_HOST_BINARY`.
    pub from: &'static str,
}

impl HostBinary {
    /// `flag`, else `DOT_HOST_BINARY` when set and not empty; `None` when neither names one.
    pub fn given(flag: Option<PathBuf>) -> Result<Option<HostBinary>> {
        let (path, from) = match flag {
            Some(p) => (p, "--dot-binary"),
            None => match std::env::var_os(ENV).filter(|v| !v.is_empty()) {
                Some(v) => (dot_core::paths::plain(PathBuf::from(v)), ENV),
                None => return Ok(None),
            },
        };
        let mut b =
            HostBinary::open(&path).with_context(|| format!("{from} {}", path.display()))?;
        b.from = from;
        Ok(Some(b))
    }

    /// Reads the file at `path`: its header, and its checksum.
    pub fn open(path: &Path) -> Result<HostBinary> {
        let abs = std::path::absolute(path).context("cannot make the path absolute")?;
        let meta = std::fs::metadata(&abs).context("cannot read it")?;
        if !meta.is_file() {
            bail!("not a file");
        }
        let mut head = Vec::with_capacity(4096);
        std::fs::File::open(&abs)
            .context("cannot read it")?
            .take(4096)
            .read_to_end(&mut head)
            .context("cannot read it")?;
        let kind = parse(&head).map_err(|what| anyhow::anyhow!("not a dot binary: {what}"))?;
        let mut hasher = Sha256::new();
        let mut file = std::fs::File::open(&abs).context("cannot read it")?;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf).context("cannot read it")?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let sha256 = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok(HostBinary {
            path: abs,
            kind,
            sha256,
            from: "--dot-binary",
        })
    }

    /// Refuses a binary that does not run on `t` (the remote, `host`).
    pub fn check(&self, t: Target, host: &str) -> Result<()> {
        if self.kind.runs_on(t) {
            return Ok(());
        }
        bail!(
            "{} {} is {}; {host} is {}/{}: {}",
            self.from,
            self.path.display(),
            self.kind.describe(),
            t.os,
            t.arch,
            how_to_build(t)
        )
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    /// The first 64 bytes of a 64-bit ELF executable for `machine` (62: x86_64, 183: aarch64).
    pub fn elf(machine: u16, big: bool) -> Vec<u8> {
        let mut b = vec![0u8; 64];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2; // 64-bit
        b[5] = if big { 2 } else { 1 };
        b[6] = 1;
        let put = |b: &mut Vec<u8>, at: usize, v: u16| {
            let bytes = if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            };
            b[at..at + 2].copy_from_slice(&bytes);
        };
        put(&mut b, 16, 3); // ET_DYN: a static-pie executable
        put(&mut b, 18, machine);
        b
    }

    /// A thin 64-bit Mach-O executable's header for `cpu`.
    pub fn macho(cpu: u32) -> Vec<u8> {
        let mut b = vec![0xCF, 0xFA, 0xED, 0xFE];
        b.extend(cpu.to_le_bytes());
        b.extend([0u8; 4]); // cpusubtype
        b.extend(2u32.to_le_bytes()); // MH_EXECUTE
        b.extend([0u8; 16]);
        b
    }

    /// A universal Mach-O header with one slice per `cpus`.
    pub fn fat(cpus: &[u32]) -> Vec<u8> {
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE];
        b.extend(u32::try_from(cpus.len()).unwrap().to_be_bytes());
        for c in cpus {
            b.extend(c.to_be_bytes());
            b.extend([0u8; 16]);
        }
        b
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    const LINUX_X86: Target = Target {
        os: "linux",
        arch: "x86_64",
    };
    const LINUX_ARM: Target = Target {
        os: "linux",
        arch: "aarch64",
    };
    const MAC_ARM: Target = Target {
        os: "macos",
        arch: "aarch64",
    };

    #[test]
    fn elf_headers_name_the_cpu() {
        let k = parse(&elf(62, false)).unwrap();
        assert_eq!(k.os, "linux");
        assert_eq!(k.archs, ["x86_64"]);
        assert!(k.runs_on(LINUX_X86) && !k.runs_on(LINUX_ARM) && !k.runs_on(MAC_ARM));
        let k = parse(&elf(183, false)).unwrap();
        assert!(k.runs_on(LINUX_ARM) && !k.runs_on(LINUX_X86));
        assert_eq!(k.describe(), "a linux/aarch64 binary");
        // Byte order is the header's own.
        assert_eq!(parse(&elf(62, true)).unwrap().archs, ["x86_64"]);
        assert_eq!(parse(&elf(3, false)).unwrap().archs, ["i386"]);
        let mut freebsd = elf(62, false);
        freebsd[7] = 9;
        assert!(!parse(&freebsd).unwrap().runs_on(LINUX_X86));
        let mut object = elf(62, false);
        object[16] = 1; // ET_REL
        assert!(parse(&object).unwrap_err().contains("not an executable"));
        assert!(parse(&elf(62, false)[..18]).is_err(), "truncated");
        assert_eq!(
            parse(b"\x7fELF\x02\x01").unwrap_err(),
            "a truncated ELF file"
        );
        // 32-bit ELF for those machines: the x32 / ILP32 ABIs, not what dot runs as.
        let mut x32 = elf(62, false);
        x32[4] = 1;
        let k = parse(&x32).unwrap();
        assert_eq!(k.archs, ["x32"]);
        assert!(!k.runs_on(LINUX_X86));
        let mut arm32 = elf(183, false);
        arm32[4] = 1;
        assert!(!parse(&arm32).unwrap().runs_on(LINUX_ARM));
        let mut classless = elf(62, false);
        classless[4] = 0;
        assert!(parse(&classless).unwrap_err().contains("unknown class"));
    }

    #[test]
    fn mach_o_headers_thin_and_universal() {
        let k = parse(&macho(0x0100_000C)).unwrap();
        assert!(k.runs_on(MAC_ARM) && !k.runs_on(LINUX_ARM));
        assert_eq!(parse(&macho(0x0100_0007)).unwrap().archs, ["x86_64"]);
        let mut dylib = macho(0x0100_000C);
        dylib[12] = 6; // MH_DYLIB
        assert!(parse(&dylib).unwrap_err().contains("not an executable"));
        assert!(
            parse(&macho(0x0100_000C)[..12])
                .unwrap_err()
                .contains("truncated")
        );
        let k = parse(&fat(&[0x0100_0007, 0x0100_000C])).unwrap();
        assert_eq!(k.describe(), "a macos/x86_64+aarch64 (universal) binary");
        assert!(k.runs_on(MAC_ARM));
        assert!(k.runs_on(Target {
            os: "macos",
            arch: "x86_64"
        }));
        // A Java class file starts the same way.
        let mut class = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
        class.extend([0u8; 32]);
        assert_eq!(parse(&class).unwrap_err(), "a Java class file");
    }

    #[test]
    fn anything_else_is_named() {
        for (bytes, says) in [
            (&[0x1F, 0x8B, 8, 0][..], "gzip archive"),
            (b"PK\x03\x04", "zip archive"),
            (b"MZ\x90\x00", "Windows executable"),
            (b"#!/bin/sh\necho dot 0.1.0\n", "a script"),
            (b"", "an empty file"),
            (b"hello", "neither an ELF nor a Mach-O"),
        ] {
            let e = parse(bytes).unwrap_err();
            assert!(e.contains(says), "{e}");
        }
    }

    #[test]
    fn a_file_is_read_and_checked_against_the_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("dot-arm");
        std::fs::write(&p, elf(183, false)).unwrap();
        let b = HostBinary::open(&p).unwrap();
        assert!(b.path.is_absolute());
        let sum: String = Sha256::digest(elf(183, false))
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(b.sha256, sum);
        b.check(LINUX_ARM, "box").unwrap();
        let e = format!("{:#}", b.check(LINUX_X86, "me@box").unwrap_err());
        assert!(
            e.contains("is a linux/aarch64 binary; me@box is linux/x86_64"),
            "{e}"
        );
        assert!(e.contains("scripts/build-linux.sh x86_64"), "{e}");
        assert!(e.contains("x86_64-unknown-linux-musl"), "{e}");
        let e = format!("{:#}", b.check(MAC_ARM, "mac").unwrap_err());
        assert!(e.contains("aarch64-apple-darwin"), "{e}");
        assert!(e.starts_with("--dot-binary "), "{e}");
        // Named by the environment: its messages say so.
        let from_env = HostBinary { from: ENV, ..b };
        let e = format!("{:#}", from_env.check(LINUX_X86, "box").unwrap_err());
        assert!(e.starts_with("DOT_HOST_BINARY "), "{e}");

        std::fs::write(&p, b"#!/bin/sh\n").unwrap();
        let e = format!("{:#}", HostBinary::open(&p).unwrap_err());
        assert!(e.contains("not a dot binary: a script"), "{e}");
        let e = format!(
            "{:#}",
            HostBinary::given(Some(tmp.path().join("missing"))).unwrap_err()
        );
        assert!(
            e.starts_with("--dot-binary ") && e.contains("cannot read it"),
            "{e}"
        );
        let e = format!(
            "{:#}",
            HostBinary::given(Some(tmp.path().to_path_buf())).unwrap_err()
        );
        assert!(e.contains("not a file"), "{e}");
    }
}
