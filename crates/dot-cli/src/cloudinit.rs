//! `dot cloud-init`: a `#cloud-config` user-data document that, on the instance's first boot,
//! installs `dot` with `install.sh` and applies a profile of a dotfiles repository,
//! non-interactively, as the image's default user (who has passwordless sudo on cloud images).
//!
//! With `--secrets-from-file`, the secrets file is embedded (base64) as a root-only
//! `write_files` entry and moved into that user's `~/.config/dot/secrets.env` (0600) before
//! `dot` runs. User data is readable by the cloud provider and, on most clouds, by anything on
//! the instance that can reach the metadata service: the document says so, and so does `dot`
//! on stderr. Pushing secrets after boot (`dot secrets push`, `dot apply --host`) keeps them out
//! of it.
//!
//! The instance installs the release this `dot` is (`DOT_VERSION=v<version>`, with `install.sh`
//! from the same tag), so the document does the same thing whenever the VM boots; `--latest`
//! takes the latest release and `install.sh` from `main` instead.
//!
//! Strings are written as JSON strings, which YAML reads as double-quoted scalars: nothing in a
//! URL, profile or command can change the document's structure.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use clap::Args;

/// Where `install.sh` is published: `<base>/<tag or main>/install.sh`.
const RAW_BASE: &str = "https://raw.githubusercontent.com/trungngo/dot-cli";

/// This `dot`'s release tag.
pub fn this_version() -> String {
    format!("v{}", env!("CARGO_PKG_VERSION"))
}

/// Where the image fetches `install.sh` from: the tag of `version`, or `main` for the latest.
pub fn install_url(version: Option<&str>) -> String {
    format!("{RAW_BASE}/{}/install.sh", version.unwrap_or("main"))
}

/// Where the embedded secrets wait (root-only) until they are handed to the user.
const STAGED_SECRETS: &str = "/var/lib/dot/secrets.env";

#[derive(Args)]
pub struct CloudInitArgs {
    /// The dotfiles repository to clone on the instance (https, no credentials).
    #[arg(long, value_name = "URL")]
    pub repo: String,
    /// The profile to apply there.
    #[arg(long, value_name = "NAME")]
    pub profile: String,
    /// The user dot runs as (the image's default user; it needs passwordless sudo).
    #[arg(long, value_name = "NAME", default_value = "ubuntu")]
    pub user: String,
    /// Embed a secrets file (default: ~/.config/dot/secrets.env) as the user's secrets.env.
    /// The cloud provider can read user data.
    #[arg(long, value_name = "FILE", num_args = 0..=1)]
    pub secrets_from_file: Option<Option<PathBuf>>,
    /// Install the latest release (and install.sh from main) instead of this dot's version.
    #[arg(long)]
    pub latest: bool,
    /// Where the instance fetches install.sh (https; default: from this dot's release tag, or
    /// main with --latest).
    #[arg(long, value_name = "URL")]
    pub install_url: Option<String>,
}

/// What goes into the document.
#[derive(Debug, Clone)]
pub struct Spec<'a> {
    pub repo: &'a str,
    pub profile: &'a str,
    pub user: &'a str,
    pub install_url: &'a str,
    /// The release to install (`DOT_VERSION`); `None` for the latest.
    pub version: Option<&'a str>,
    /// The secrets file's content, to embed.
    pub secrets: Option<&'a [u8]>,
}

fn check_https(what: &str, url: &str) -> Result<()> {
    let Some(rest) = url.strip_prefix("https://") else {
        bail!("{what} must be an https:// URL (got `{url}`)");
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        bail!("{what} `{url}` has no host");
    }
    if authority.contains('@') {
        bail!("{what} has credentials in it; use a git credential helper instead");
    }
    if url.chars().any(|c| c.is_whitespace() || c == '\'') {
        bail!("{what} `{url}` has spaces or quotes in it");
    }
    Ok(())
}

fn check_word(what: &str, v: &str) -> Result<()> {
    let ok = !v.is_empty()
        && !v.starts_with('-')
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok {
        bail!("{what} `{v}` may only use letters, digits, `.`, `_` and `-`");
    }
    Ok(())
}

/// A YAML scalar: a JSON string.
fn s(v: &str) -> String {
    serde_json::to_string(v).expect("strings serialize")
}

/// Standard base64 with padding.
pub fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The `#cloud-config` document.
pub fn render(spec: &Spec<'_>) -> Result<String> {
    check_https("the repository URL", spec.repo)?;
    check_https("the install.sh URL", spec.install_url)?;
    check_word("the profile", spec.profile)?;
    check_word("the user", spec.user)?;
    if let Some(v) = spec.version {
        check_word("the version", v)?;
    }
    let (user, profile) = (spec.user, spec.profile);
    let mut out = String::new();
    out.push_str("#cloud-config\n");
    out.push_str(&format!(
        "# Generated by `dot cloud-init`. On first boot: installs dot, clones {repo}\n\
         # and applies its profile `{profile}` as `{user}`, non-interactively ({user} needs\n\
         # passwordless sudo, as a cloud image's default user has). Progress goes to\n\
         # /var/log/cloud-init-output.log.\n",
        repo = spec.repo
    ));
    out.push_str("package_update: true\npackages:\n  - ca-certificates\n  - curl\n  - git\n");
    let mut runcmd = Vec::new();
    if let Some(secrets) = spec.secrets {
        out.push_str(
            "# WARNING: this user data holds secrets (secrets.env). The cloud provider can read\n\
             # user data and, on most clouds, so can anything on the instance that reaches the\n\
             # metadata service. Prefer `dot secrets push` or `dot apply --host` after boot, and\n\
             # rotate these secrets if this document leaks.\n",
        );
        out.push_str("write_files:\n");
        out.push_str(&format!("  - path: {}\n", s(STAGED_SECRETS)));
        out.push_str("    owner: \"root:root\"\n");
        out.push_str("    permissions: \"0600\"\n");
        out.push_str("    encoding: b64\n");
        out.push_str(&format!("    content: {}\n", base64(secrets)));
        runcmd.push(vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                "set -e; su - {user} -c 'mkdir -p ~/.config/dot && chmod 700 ~/.config/dot'; \
                 home=$(getent passwd {user} | cut -d: -f6); \
                 install -o {user} -g \"$(id -gn {user})\" -m 0600 {STAGED_SECRETS} \
                 \"$home/.config/dot/secrets.env\"; rm -f {STAGED_SECRETS}"
            ),
        ]);
    }
    runcmd.push(vec![
        "su".to_string(),
        "-".to_string(),
        user.to_string(),
        "-c".to_string(),
        format!(
            "curl -fsSL --proto '=https' --tlsv1.2 '{}' | {}DOT_APPLY_ARGS='--yes --profile \
             {profile}' sh -s -- '{}'",
            spec.install_url,
            spec.version
                .map(|v| format!("DOT_VERSION='{v}' "))
                .unwrap_or_default(),
            spec.repo
        ),
    ]);
    out.push_str("runcmd:\n");
    for cmd in runcmd {
        let mut first = true;
        for word in cmd {
            out.push_str(if first { "  - - " } else { "    - " });
            out.push_str(&s(&word));
            out.push('\n');
            first = false;
        }
    }
    Ok(out)
}

pub fn cloud_init(a: CloudInitArgs) -> Result<ExitCode> {
    let secrets = match &a.secrets_from_file {
        None => None,
        Some(p) => {
            let path = match p {
                Some(p) => p.clone(),
                None => dirs::home_dir()
                    .context("cannot determine the home directory")?
                    .join(".config/dot/secrets.env"),
            };
            Some(
                std::fs::read(&path)
                    .with_context(|| format!("cannot read secrets from {}", path.display()))?,
            )
        }
    };
    let version = (!a.latest).then(this_version);
    let url = a
        .install_url
        .clone()
        .unwrap_or_else(|| install_url(version.as_deref()));
    let doc = render(&Spec {
        repo: &a.repo,
        profile: &a.profile,
        user: &a.user,
        install_url: &url,
        version: version.as_deref(),
        secrets: secrets.as_deref(),
    })?;
    if secrets.is_some() {
        eprintln!(
            "warning: this user data holds your secrets: the cloud provider (and, on most \
             clouds, anything on the instance that reaches the metadata service) can read it"
        );
    }
    print!("{doc}");
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN: &str = r##"#cloud-config
# Generated by `dot cloud-init`. On first boot: installs dot, clones https://github.com/me/dotfiles.git
# and applies its profile `ubuntu-server` as `ubuntu`, non-interactively (ubuntu needs
# passwordless sudo, as a cloud image's default user has). Progress goes to
# /var/log/cloud-init-output.log.
package_update: true
packages:
  - ca-certificates
  - curl
  - git
runcmd:
  - - "su"
    - "-"
    - "ubuntu"
    - "-c"
    - "curl -fsSL --proto '=https' --tlsv1.2 'https://raw.githubusercontent.com/trungngo/dot-cli/v1.2.3/install.sh' | DOT_VERSION='v1.2.3' DOT_APPLY_ARGS='--yes --profile ubuntu-server' sh -s -- 'https://github.com/me/dotfiles.git'"
"##;

    const GOLDEN_SECRETS: &str = r##"#cloud-config
# Generated by `dot cloud-init`. On first boot: installs dot, clones https://github.com/me/dotfiles.git
# and applies its profile `ubuntu-server` as `admin`, non-interactively (admin needs
# passwordless sudo, as a cloud image's default user has). Progress goes to
# /var/log/cloud-init-output.log.
package_update: true
packages:
  - ca-certificates
  - curl
  - git
# WARNING: this user data holds secrets (secrets.env). The cloud provider can read
# user data and, on most clouds, so can anything on the instance that reaches the
# metadata service. Prefer `dot secrets push` or `dot apply --host` after boot, and
# rotate these secrets if this document leaks.
write_files:
  - path: "/var/lib/dot/secrets.env"
    owner: "root:root"
    permissions: "0600"
    encoding: b64
    content: QVBJX1RPS0VOPSJhYmMiCg==
runcmd:
  - - "sh"
    - "-c"
    - "set -e; su - admin -c 'mkdir -p ~/.config/dot && chmod 700 ~/.config/dot'; home=$(getent passwd admin | cut -d: -f6); install -o admin -g \"$(id -gn admin)\" -m 0600 /var/lib/dot/secrets.env \"$home/.config/dot/secrets.env\"; rm -f /var/lib/dot/secrets.env"
  - - "su"
    - "-"
    - "admin"
    - "-c"
    - "curl -fsSL --proto '=https' --tlsv1.2 'https://raw.githubusercontent.com/trungngo/dot-cli/main/install.sh' | DOT_APPLY_ARGS='--yes --profile ubuntu-server' sh -s -- 'https://github.com/me/dotfiles.git'"
"##;

    fn spec<'a>(secrets: Option<&'a [u8]>, user: &'a str) -> Spec<'a> {
        Spec {
            repo: "https://github.com/me/dotfiles.git",
            profile: "ubuntu-server",
            user,
            install_url: "https://raw.githubusercontent.com/trungngo/dot-cli/main/install.sh",
            version: None,
            secrets,
        }
    }

    #[test]
    fn golden_documents() {
        let url = install_url(Some("v1.2.3"));
        let pinned = Spec {
            install_url: &url,
            version: Some("v1.2.3"),
            ..spec(None, "ubuntu")
        };
        assert_eq!(render(&pinned).unwrap(), GOLDEN);
        assert_eq!(
            render(&spec(Some(b"API_TOKEN=\"abc\"\n"), "admin")).unwrap(),
            GOLDEN_SECRETS
        );
    }

    #[test]
    fn base64_matches_the_standard() {
        for (raw, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(raw.as_bytes()), enc, "{raw}");
        }
        assert_eq!(base64(&[0xff, 0xfe, 0x00]), "//4A");
    }

    #[test]
    fn inputs_that_could_break_the_document_are_refused() {
        let mut s = spec(None, "ubuntu");
        for (repo, want) in [
            ("http://x/y", "https://"),
            ("https://u:t@x/y", "credentials"),
            ("https://x/y' ; rm -rf /", "spaces or quotes"),
        ] {
            s.repo = repo;
            let e = render(&s).unwrap_err().to_string();
            assert!(e.contains(want), "{repo}: {e}");
        }
        let mut s = spec(None, "ubuntu");
        s.profile = "x'; reboot";
        assert!(render(&s).is_err());
        let s = spec(None, "-root");
        assert!(render(&s).is_err());
        let s = Spec {
            version: Some("v1'; reboot"),
            ..spec(None, "ubuntu")
        };
        assert!(render(&s).is_err());
    }

    #[test]
    fn the_default_is_this_release() {
        let v = this_version();
        assert_eq!(v, format!("v{}", env!("CARGO_PKG_VERSION")));
        assert_eq!(
            install_url(Some(&v)),
            format!("https://raw.githubusercontent.com/trungngo/dot-cli/{v}/install.sh")
        );
        assert!(install_url(None).ends_with("/main/install.sh"));
    }
}
