//! Optional, checksum-pinned first-use installation in the selected workspace.
use anyhow::{Result, ensure};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bootstrap {
    pub url: String,
    pub sha256: String,
}
impl Bootstrap {
    pub fn validate(&self, origin: &str, cli: &str) -> Result<()> {
        let url = reqwest::Url::parse(&self.url)?;
        ensure!(
            url.scheme() == "https"
                && url.origin().ascii_serialization() == origin
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && self
                    .url
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b":/._-".contains(&b)),
            "Bootstrap must use a simple HTTPS URL on the gateway origin"
        );
        ensure!(
            self.sha256.len() == 64 && self.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "Bootstrap requires a pinned SHA-256"
        );
        ensure!(
            cli == "/home/coder/.local/bin/lkjyolo",
            "Bootstrap requires the standard per-user CLI path"
        );
        Ok(())
    }
    pub fn prefix(&self, cli: &str) -> String {
        // Both URL and digest were validated when the server started. No bearer
        // credential enters this script. Existing installations are never replaced.
        format!(
            r#"set -eu
if [ ! -x {cli} ]; then
  umask 077
  lkj_install_tmp=$(mktemp -d)
  trap 'rm -rf "$lkj_install_tmp"' EXIT HUP INT TERM
  curl --fail --silent --show-error --proto '=https' --tlsv1.2 --connect-timeout 10 --max-time 40 '{url}' -o "$lkj_install_tmp/lkjyolo"
  printf '%s  %s\n' '{sha256}' "$lkj_install_tmp/lkjyolo" | sha256sum --check --status
  chmod 700 "$lkj_install_tmp/lkjyolo"
  mkdir -p /home/coder/workspace
  "$lkj_install_tmp/lkjyolo" install --workspace /home/coder/workspace >/dev/null
  rm -rf "$lkj_install_tmp"
  trap - EXIT HUP INT TERM
fi
"#,
            url = self.url,
            sha256 = self.sha256
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn downloads_require_same_origin_and_fixed_digest() {
        let valid = Bootstrap {
            url: "https://coder.example.com/lkjyolo/releases/0.4.0/lkjyolo-linux-x86_64".into(),
            sha256: "a".repeat(64),
        };
        assert!(
            valid
                .validate(
                    "https://coder.example.com",
                    "/home/coder/.local/bin/lkjyolo"
                )
                .is_ok()
        );
        for url in [
            "http://coder.example.com/a",
            "https://other.example/a",
            "https://user@coder.example.com/a",
            "https://coder.example.com/a?token=secret",
            "https://coder.example.com/a'$(id)",
        ] {
            assert!(
                Bootstrap {
                    url: url.into(),
                    sha256: valid.sha256.clone()
                }
                .validate(
                    "https://coder.example.com",
                    "/home/coder/.local/bin/lkjyolo"
                )
                .is_err()
            );
        }
        assert!(
            Bootstrap {
                url: valid.url.clone(),
                sha256: "latest".into()
            }
            .validate(
                "https://coder.example.com",
                "/home/coder/.local/bin/lkjyolo"
            )
            .is_err()
        );
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn first_use_checks_digest_and_preserves_existing_installation() {
        use std::{fs, os::unix::fs::PermissionsExt, process::Command};
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let curl = bin.join("curl");
        fs::write(
            &curl,
            "#!/bin/sh\nfor last do :; done\ncp \"$TEST_DOWNLOAD\" \"$last\"\n",
        )
        .unwrap();
        fs::set_permissions(&curl, fs::Permissions::from_mode(0o755)).unwrap();
        let download = dir.path().join("download");
        fs::write(&download,"#!/bin/sh\nprintf installed > \"$TEST_MARKER\"\nmkdir -p \"$(dirname \"$TEST_CLI\")\"\ncp \"$0\" \"$TEST_CLI\"\n").unwrap();
        let cli = dir.path().join(".local/bin/lkjyolo");
        let marker = dir.path().join("installed");
        let run = |hash: String| {
            let bootstrap = Bootstrap {
                url: "https://coder.example.com/runtime".into(),
                sha256: hash,
            };
            let script = bootstrap
                .prefix("/home/coder/.local/bin/lkjyolo")
                .replace("/home/coder", dir.path().to_str().unwrap());
            Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
                .env("TEST_DOWNLOAD", &download)
                .env("TEST_MARKER", &marker)
                .env("TEST_CLI", &cli)
                .output()
                .unwrap()
        };
        let corrupt = run("0".repeat(64));
        assert!(!corrupt.status.success());
        assert!(!marker.exists());
        assert!(!cli.exists());
        let valid = run(crate::util::file_sha256(&download).unwrap());
        assert!(
            valid.status.success(),
            "{}",
            String::from_utf8_lossy(&valid.stderr)
        );
        assert!(marker.exists());
        assert!(cli.exists());
        fs::write(&marker, "preserved").unwrap();
        assert!(run("0".repeat(64)).status.success());
        assert_eq!(fs::read_to_string(&marker).unwrap(), "preserved");
    }
}
