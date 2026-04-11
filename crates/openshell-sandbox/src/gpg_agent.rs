// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! GPG agent lifecycle management for sandbox commit signing.
//!
//! Spawns a `gpg-agent` daemon with a split directory layout:
//!
//! - `/run/openshell-gpg/private/` — root-only (0700), holds the private key
//!   and the agent's keyring. The sandbox user cannot access this directory.
//!
//! - `/sandbox/.gnupg/` — sandbox-accessible, holds only the public keyring,
//!   a `gpg.conf` with `no-autostart`, and the agent's extra socket.
//!   The extra socket is a real file (not a symlink) so Landlock allows access.
//!
//! The sandbox user gets signing capability via `gpg --sign` or `git commit -S`
//! without ever seeing private key material.

use std::path::{Path, PathBuf};
use std::process::Command;

use miette::{IntoDiagnostic, Result, WrapErr};
use tracing::{debug, info, warn};

const PRIVATE_DIR: &str = "/run/openshell-gpg/private";
const SANDBOX_GNUPG_DIR: &str = "/sandbox/.gnupg";
const SOCKET_NAME: &str = "S.gpg-agent";

/// Handle to a running `gpg-agent` daemon.
///
/// The agent is killed when the handle is dropped.
pub(crate) struct GpgAgentHandle {
    pid: u32,
    private_dir: PathBuf,
    sandbox_gnupg_dir: PathBuf,
}

impl GpgAgentHandle {
    /// Return the sandbox-accessible GNUPGHOME directory.
    pub fn gnupg_dir(&self) -> &Path {
        &self.sandbox_gnupg_dir
    }

    /// Return the PID of the gpg-agent process (used on Linux for SIGCHLD reaper).
    #[allow(dead_code)]
    pub fn pid(&self) -> u32 {
        self.pid
    }
}

impl Drop for GpgAgentHandle {
    fn drop(&mut self) {
        debug!(pid = self.pid, "Shutting down gpg-agent");
        let _ = Command::new("gpgconf")
            .args(["--homedir", &self.private_dir.display().to_string()])
            .args(["--kill", "gpg-agent"])
            .output();
    }
}

/// Start a `gpg-agent` daemon with the given key material and passphrase.
///
/// # Directory layout
///
/// ```text
/// /run/openshell-gpg/private/   (root:root 0700)
///   ├── private-key.asc         (imported into keyring)
///   ├── gpg-agent.conf
///   ├── pubring.kbx
///   ├── private-keys-v1.d/
///   └── S.gpg-agent
///
/// /sandbox/.gnupg/              (sandbox:sandbox 0700)
///   ├── pubring.kbx             (copy of public keyring)
///   ├── gpg.conf                (no-autostart)
///   └── S.gpg-agent             (extra socket, chowned to sandbox)
/// ```
#[cfg(unix)]
pub(crate) fn start_gpg_agent(
    private_key: &[u8],
    passphrase: &str,
    signing_key_id: Option<&str>,
    sandbox_uid: nix::unistd::Uid,
    sandbox_gid: nix::unistd::Gid,
) -> Result<GpgAgentHandle> {
    use nix::unistd::chown;
    use std::os::unix::fs::PermissionsExt;

    let private_dir = Path::new(PRIVATE_DIR);
    let sandbox_gnupg_dir = Path::new(SANDBOX_GNUPG_DIR);

    let preset_bin = check_gpg_binaries()?;

    // Create private directory (root-only).
    std::fs::create_dir_all(private_dir)
        .into_diagnostic()
        .wrap_err("failed to create gpg private directory")?;
    std::fs::set_permissions(private_dir, std::fs::Permissions::from_mode(0o700))
        .into_diagnostic()?;

    // Create sandbox-accessible gnupg directory.
    std::fs::create_dir_all(sandbox_gnupg_dir)
        .into_diagnostic()
        .wrap_err("failed to create sandbox .gnupg directory")?;
    chown(sandbox_gnupg_dir, Some(sandbox_uid), Some(sandbox_gid))
        .into_diagnostic()
        .wrap_err("failed to chown sandbox .gnupg directory")?;
    std::fs::set_permissions(sandbox_gnupg_dir, std::fs::Permissions::from_mode(0o700))
        .into_diagnostic()?;

    // Write gpg-agent.conf to private dir.
    // extra-socket creates a real socket in the sandbox dir (not a symlink),
    // so Landlock + DAC both allow the sandbox user to connect.
    let extra_socket_path = sandbox_gnupg_dir.join(SOCKET_NAME);
    let agent_conf = format!(
        "default-cache-ttl 31536000\n\
         max-cache-ttl 31536000\n\
         allow-preset-passphrase\n\
         extra-socket {}\n",
        extra_socket_path.display()
    );
    std::fs::write(private_dir.join("gpg-agent.conf"), &agent_conf)
        .into_diagnostic()
        .wrap_err("failed to write gpg-agent.conf")?;

    // Write the private key to the private directory.
    let key_path = private_dir.join("private-key.asc");
    std::fs::write(&key_path, private_key)
        .into_diagnostic()
        .wrap_err("failed to write gpg private key")?;
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
        .into_diagnostic()?;

    // Start gpg-agent daemon.
    let output = Command::new("gpg-agent")
        .args([
            "--homedir",
            &private_dir.display().to_string(),
            "--daemon",
            "--verbose",
        ])
        .output()
        .into_diagnostic()
        .wrap_err("failed to start gpg-agent")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(miette::miette!("gpg-agent failed to start: {stderr}"));
    }
    info!("gpg-agent daemon started");

    // Import the private key.
    let import_output = Command::new("gpg")
        .args([
            "--homedir",
            &private_dir.display().to_string(),
            "--batch",
            "--import",
        ])
        .arg(&key_path)
        .output()
        .into_diagnostic()
        .wrap_err("failed to import gpg private key")?;

    if !import_output.status.success() {
        let stderr = String::from_utf8_lossy(&import_output.stderr);
        warn!(stderr = %stderr, "gpg key import produced warnings");
    }
    info!("GPG private key imported");

    // Pre-seed the passphrase for every keygrip (primary + subkeys).
    let keygrips = get_all_keygrips(private_dir)?;
    for grip in &keygrips {
        preset_passphrase(&preset_bin, private_dir, grip, passphrase)?;
    }
    info!(
        count = keygrips.len(),
        "GPG passphrase pre-seeded for all keygrips"
    );

    // Export the public key and import into the sandbox-accessible keyring.
    export_public_key(private_dir, sandbox_gnupg_dir)?;

    // Write gpg.conf for the sandbox user's gpg client.
    let gpg_conf = "no-autostart\n";
    std::fs::write(sandbox_gnupg_dir.join("gpg.conf"), gpg_conf)
        .into_diagnostic()
        .wrap_err("failed to write gpg.conf")?;

    // Wait for the extra socket to appear (created by gpg-agent via extra-socket
    // directive), then chown it so the sandbox user can connect.
    wait_for_socket(&extra_socket_path)?;

    // Chown everything in the sandbox gnupg dir (keyring files, trustdb,
    // gpg.conf, and the extra socket) to the sandbox user.
    chown_recursive(sandbox_gnupg_dir, sandbox_uid, sandbox_gid)?;

    // Write gitconfig if signing_key_id is set.
    if let Some(key_id) = signing_key_id.filter(|k| !k.is_empty()) {
        write_gitconfig_signing(key_id, sandbox_uid, sandbox_gid)?;
    }

    // Read the agent PID from the socket directory.
    let pid = read_agent_pid(private_dir)?;

    info!(
        pid,
        private_dir = %private_dir.display(),
        sandbox_gnupg_dir = %sandbox_gnupg_dir.display(),
        "GPG agent ready for signing"
    );

    Ok(GpgAgentHandle {
        pid,
        private_dir: private_dir.to_path_buf(),
        sandbox_gnupg_dir: sandbox_gnupg_dir.to_path_buf(),
    })
}

/// Verify that required GPG binaries are available and return the full
/// path to `gpg-preset-passphrase` (which lives in the GnuPG libexecdir
/// rather than on PATH).
fn check_gpg_binaries() -> Result<PathBuf> {
    for binary in &["gpg", "gpg-agent", "gpgconf"] {
        let result = Command::new("which").arg(binary).output();
        match result {
            Ok(output) if output.status.success() => {}
            _ => {
                return Err(miette::miette!(
                    "required binary '{binary}' not found in PATH; \
                     the sandbox image must include gnupg"
                ));
            }
        }
    }

    let libexec = Command::new("gpgconf")
        .args(["--list-dirs", "libexecdir"])
        .output()
        .into_diagnostic()
        .wrap_err("failed to query gpgconf libexecdir")?;

    let libexec_dir = String::from_utf8_lossy(&libexec.stdout).trim().to_string();
    let preset_path = PathBuf::from(&libexec_dir).join("gpg-preset-passphrase");

    if !preset_path.exists() {
        return Err(miette::miette!(
            "gpg-preset-passphrase not found at {}; \
             the sandbox image must include gnupg-utils",
            preset_path.display()
        ));
    }
    debug!(path = %preset_path.display(), "Found gpg-preset-passphrase");
    Ok(preset_path)
}

/// Extract all keygrips from the keyring. Keys with signing subkeys have
/// multiple keygrips; each needs its passphrase pre-seeded.
fn get_all_keygrips(homedir: &Path) -> Result<Vec<String>> {
    let output = Command::new("gpg")
        .args([
            "--homedir",
            &homedir.display().to_string(),
            "--batch",
            "--with-keygrip",
            "--list-secret-keys",
        ])
        .output()
        .into_diagnostic()
        .wrap_err("failed to list secret keys")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let grips: Vec<String> = stdout
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("Keygrip = ")
                .map(|g| g.trim().to_string())
        })
        .collect();

    if grips.is_empty() {
        return Err(miette::miette!(
            "no keygrip found in gpg output; key import may have failed"
        ));
    }
    Ok(grips)
}

/// Pre-seed the passphrase for a key using `gpg-preset-passphrase`.
///
/// Uses `GNUPGHOME` env var to locate the agent socket rather than
/// `--homedir`, which not all gpg-preset-passphrase builds support.
fn preset_passphrase(
    preset_bin: &Path,
    homedir: &Path,
    keygrip: &str,
    passphrase: &str,
) -> Result<()> {
    use std::io::Write;

    let mut child = Command::new(preset_bin)
        .env("GNUPGHOME", homedir)
        .args(["--preset", keygrip])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .into_diagnostic()
        .wrap_err("failed to spawn gpg-preset-passphrase")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(passphrase.as_bytes())
            .into_diagnostic()
            .wrap_err("failed to write passphrase to gpg-preset-passphrase")?;
    }

    let output = child
        .wait_with_output()
        .into_diagnostic()
        .wrap_err("gpg-preset-passphrase failed")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(miette::miette!(
            "gpg-preset-passphrase exited with {}: {stderr}",
            output.status
        ));
    }
    Ok(())
}

/// Export public keys from the private homedir and import them into the
/// sandbox keyring so that the sandbox user's `gpg` recognises the key.
fn export_public_key(private_dir: &Path, sandbox_dir: &Path) -> Result<()> {
    use std::io::Write;

    let export = Command::new("gpg")
        .args([
            "--homedir",
            &private_dir.display().to_string(),
            "--batch",
            "--export",
        ])
        .output()
        .into_diagnostic()
        .wrap_err("failed to export public key")?;

    if !export.status.success() {
        let stderr = String::from_utf8_lossy(&export.stderr);
        warn!(stderr = %stderr, "public key export produced warnings");
    }

    let mut child = Command::new("gpg")
        .args([
            "--homedir",
            &sandbox_dir.display().to_string(),
            "--batch",
            "--import",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .into_diagnostic()
        .wrap_err("failed to spawn gpg --import for sandbox keyring")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&export.stdout)
            .into_diagnostic()
            .wrap_err("failed to pipe public key to sandbox gpg")?;
    }

    let output = child
        .wait_with_output()
        .into_diagnostic()
        .wrap_err("gpg --import into sandbox keyring failed")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(stderr = %stderr, "gpg --import into sandbox keyring produced warnings");
    }
    Ok(())
}

/// Write git signing configuration to the sandbox user's gitconfig.
#[cfg(unix)]
fn write_gitconfig_signing(
    key_id: &str,
    sandbox_uid: nix::unistd::Uid,
    sandbox_gid: nix::unistd::Gid,
) -> Result<()> {
    use nix::unistd::chown;
    use std::fmt::Write;

    let gitconfig_path = Path::new("/sandbox/.gitconfig");
    let mut config = if gitconfig_path.exists() {
        std::fs::read_to_string(gitconfig_path)
            .into_diagnostic()
            .wrap_err("failed to read existing .gitconfig")?
    } else {
        String::new()
    };

    if !config.contains("[user]") {
        config.push_str("[user]\n");
    }
    if !config.contains("signingkey") {
        let _ = writeln!(config, "\tsigningkey = {key_id}");
    }

    if !config.contains("[commit]") {
        config.push_str("[commit]\n");
        config.push_str("\tgpgsign = true\n");
    }

    std::fs::write(gitconfig_path, &config)
        .into_diagnostic()
        .wrap_err("failed to write .gitconfig")?;
    chown(gitconfig_path, Some(sandbox_uid), Some(sandbox_gid))
        .into_diagnostic()
        .wrap_err("failed to chown .gitconfig")?;

    info!(key_id, "Wrote git signing configuration");
    Ok(())
}

/// Recursively chown a directory and all its contents.
#[cfg(unix)]
fn chown_recursive(dir: &Path, uid: nix::unistd::Uid, gid: nix::unistd::Gid) -> Result<()> {
    use nix::unistd::chown;

    chown(dir, Some(uid), Some(gid))
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to chown {}", dir.display()))?;

    for entry in std::fs::read_dir(dir).into_diagnostic()? {
        let entry = entry.into_diagnostic()?;
        let path = entry.path();
        chown(&path, Some(uid), Some(gid))
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to chown {}", path.display()))?;
        if path.is_dir() {
            chown_recursive(&path, uid, gid)?;
        }
    }
    Ok(())
}

/// Poll until the socket file exists, with a bounded retry.
fn wait_for_socket(path: &Path) -> Result<()> {
    for _ in 0..50 {
        if path.exists() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(miette::miette!(
        "gpg-agent extra socket did not appear at {} within 5 s",
        path.display()
    ))
}

/// Read the gpg-agent PID from the agent-info or pidfile.
fn read_agent_pid(homedir: &Path) -> Result<u32> {
    // gpg-agent writes a pidfile at $GNUPGHOME/S.gpg-agent when started
    // with --daemon. We can also parse the output from gpg-connect-agent.
    let output = Command::new("gpg-connect-agent")
        .args([
            "--homedir",
            &homedir.display().to_string(),
            "GETINFO pid",
            "/bye",
        ])
        .output()
        .into_diagnostic()
        .wrap_err("failed to query gpg-agent pid")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("D ") {
            if let Ok(pid) = rest.trim().parse::<u32>() {
                return Ok(pid);
            }
        }
    }

    Err(miette::miette!(
        "could not determine gpg-agent PID from gpg-connect-agent output"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpg_agent_handle_has_correct_paths() {
        let handle = GpgAgentHandle {
            pid: 12345,
            private_dir: PathBuf::from(PRIVATE_DIR),
            sandbox_gnupg_dir: PathBuf::from(SANDBOX_GNUPG_DIR),
        };
        assert_eq!(handle.gnupg_dir(), Path::new(SANDBOX_GNUPG_DIR));
        assert_eq!(handle.pid(), 12345);
    }

    #[test]
    fn private_dir_constant_is_outside_sandbox() {
        assert!(
            !PRIVATE_DIR.starts_with("/sandbox"),
            "private dir must not be under /sandbox"
        );
    }

    #[test]
    fn sandbox_gnupg_dir_is_under_sandbox() {
        assert!(
            SANDBOX_GNUPG_DIR.starts_with("/sandbox"),
            "sandbox gnupg dir must be under /sandbox"
        );
    }
}
