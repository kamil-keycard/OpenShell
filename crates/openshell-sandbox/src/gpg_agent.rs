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
//!   a `gpg.conf` that redirects agent communication to the socket in the
//!   private directory, and a symlink to the agent socket.
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

    /// Return the PID of the gpg-agent process.
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
///   ├── gpg.conf                (redirects to agent socket)
///   └── S.gpg-agent             (symlink → /run/openshell-gpg/private/S.gpg-agent)
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

    check_gpg_binaries()?;

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
    let agent_conf = format!(
        "default-cache-ttl 31536000\n\
         max-cache-ttl 31536000\n\
         allow-preset-passphrase\n"
    );
    std::fs::write(private_dir.join("gpg-agent.conf"), agent_conf)
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

    // Get the keygrip for passphrase pre-seeding.
    let keygrip = get_keygrip(private_dir)?;

    // Pre-seed the passphrase via gpg-preset-passphrase.
    preset_passphrase(private_dir, &keygrip, passphrase)?;
    info!("GPG passphrase pre-seeded");

    // Export the public key to the sandbox-accessible directory.
    export_public_key(private_dir, sandbox_gnupg_dir)?;
    chown(
        &sandbox_gnupg_dir.join("pubring.kbx"),
        Some(sandbox_uid),
        Some(sandbox_gid),
    )
    .into_diagnostic()
    .wrap_err("failed to chown public keyring")?;

    // Write gpg.conf that redirects agent to the socket in private dir.
    let socket_path = private_dir.join(SOCKET_NAME);
    let gpg_conf = format!("no-autostart\n");
    std::fs::write(sandbox_gnupg_dir.join("gpg.conf"), &gpg_conf)
        .into_diagnostic()
        .wrap_err("failed to write gpg.conf")?;
    chown(
        &sandbox_gnupg_dir.join("gpg.conf"),
        Some(sandbox_uid),
        Some(sandbox_gid),
    )
    .into_diagnostic()?;

    // Symlink the agent socket into the sandbox gnupg dir.
    let sandbox_socket = sandbox_gnupg_dir.join(SOCKET_NAME);
    if sandbox_socket.exists() {
        std::fs::remove_file(&sandbox_socket).into_diagnostic()?;
    }
    std::os::unix::fs::symlink(&socket_path, &sandbox_socket)
        .into_diagnostic()
        .wrap_err("failed to symlink gpg-agent socket")?;
    // Symlink ownership doesn't matter on Linux (Landlock checks the target).

    // Write gitconfig if signing_key_id is set.
    if let Some(key_id) = signing_key_id {
        if !key_id.is_empty() {
            write_gitconfig_signing(key_id, sandbox_uid, sandbox_gid)?;
        }
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

/// Verify that required GPG binaries are available.
fn check_gpg_binaries() -> Result<()> {
    for binary in &["gpg", "gpg-agent", "gpg-preset-passphrase"] {
        let result = Command::new("which").arg(binary).output();
        match result {
            Ok(output) if output.status.success() => {}
            _ => {
                return Err(miette::miette!(
                    "required binary '{binary}' not found in PATH; \
                     the sandbox image must include gpg, gpg-agent, and gpg-preset-passphrase"
                ));
            }
        }
    }
    Ok(())
}

/// Extract the keygrip of the first signing-capable key in the keyring.
fn get_keygrip(homedir: &Path) -> Result<String> {
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
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(grip) = trimmed.strip_prefix("Keygrip = ") {
            return Ok(grip.trim().to_string());
        }
    }

    Err(miette::miette!(
        "no keygrip found in gpg output; key import may have failed"
    ))
}

/// Pre-seed the passphrase for a key using `gpg-preset-passphrase`.
fn preset_passphrase(homedir: &Path, keygrip: &str, passphrase: &str) -> Result<()> {
    let output = Command::new("gpg-preset-passphrase")
        .args([
            "--homedir",
            &homedir.display().to_string(),
            "--preset",
            keygrip,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .into_diagnostic()
        .wrap_err("failed to spawn gpg-preset-passphrase")?;

    use std::io::Write;
    let mut child = output;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(passphrase.as_bytes())
            .into_diagnostic()
            .wrap_err("failed to write passphrase to gpg-preset-passphrase")?;
    }

    let status = child
        .wait()
        .into_diagnostic()
        .wrap_err("gpg-preset-passphrase failed")?;

    if !status.success() {
        return Err(miette::miette!("gpg-preset-passphrase exited with {status}"));
    }
    Ok(())
}

/// Export the public keyring from the private homedir to the sandbox dir.
fn export_public_key(private_dir: &Path, sandbox_dir: &Path) -> Result<()> {
    let output = Command::new("gpg")
        .args([
            "--homedir",
            &private_dir.display().to_string(),
            "--batch",
            "--export",
            "--output",
            &sandbox_dir.join("pubring.kbx").display().to_string(),
        ])
        .output()
        .into_diagnostic()
        .wrap_err("failed to export public keyring")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(stderr = %stderr, "public key export produced warnings");
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
        config.push_str(&format!("\tsigningkey = {key_id}\n"));
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
