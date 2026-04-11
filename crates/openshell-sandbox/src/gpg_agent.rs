// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! GPG agent lifecycle management for sandbox commit signing.
//!
//! Spawns a `gpg-agent` daemon whose homedir is `/sandbox/.gnupg/`.
//! The sandbox user connects to the **main** agent socket (not the
//! extra/restricted socket) so that the pre-seeded passphrase cache
//! is available for signing — GnuPG's extra socket deliberately
//! disables passphrase cache lookups, which breaks non-interactive
//! signing.
//!
//! Private key material lives on disk (passphrase-protected) in
//! `/sandbox/.gnupg/private-keys-v1.d/`, readable only by root.
//! After setup the directory is chowned to the sandbox user, but the
//! `private-keys-v1.d/` subtree stays root-only so the sandbox user
//! cannot read the raw (encrypted) key files.
//!
//! The sandbox user gets signing capability via `gpg --sign` or
//! `git commit -S` using the cached passphrase in the agent.

use std::path::{Path, PathBuf};
use std::process::Command;

use miette::{IntoDiagnostic, Result, WrapErr};
use tracing::{debug, info, warn};

const SANDBOX_GNUPG_DIR: &str = "/sandbox/.gnupg";
const SOCKET_NAME: &str = "S.gpg-agent";
const PINENTRY_DIR: &str = "/run/openshell-gpg";

/// Handle to a running `gpg-agent` daemon.
///
/// The agent is killed when the handle is dropped.
pub(crate) struct GpgAgentHandle {
    pid: u32,
    gnupg_dir: PathBuf,
}

impl GpgAgentHandle {
    /// Return the sandbox-accessible GNUPGHOME directory.
    pub fn gnupg_dir(&self) -> &Path {
        &self.gnupg_dir
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
            .args(["--homedir", &self.gnupg_dir.display().to_string()])
            .args(["--kill", "gpg-agent"])
            .output();
    }
}

/// Start a `gpg-agent` daemon with the given key material and passphrase.
///
/// # Directory layout
///
/// ```text
/// /sandbox/.gnupg/                   (sandbox:sandbox 0700)
///   ├── gpg-agent.conf
///   ├── gpg.conf                     (no-autostart)
///   ├── pubring.kbx
///   ├── trustdb.gpg
///   ├── S.gpg-agent                  (main socket — unrestricted)
///   └── private-keys-v1.d/           (root:root 0700 — sandbox can't read)
///       └── <keygrip>.key
/// ```
#[cfg(unix)]
pub(crate) fn start_gpg_agent(
    private_key: &[u8],
    passphrase: &str,
    signing_key_id: Option<&str>,
    sandbox_uid: nix::unistd::Uid,
    sandbox_gid: nix::unistd::Gid,
) -> Result<GpgAgentHandle> {
    use std::os::unix::fs::PermissionsExt;

    let gnupg_dir = Path::new(SANDBOX_GNUPG_DIR);

    let preset_bin = check_gpg_binaries()?;

    // Create the gnupg directory as root (chowned to sandbox user later,
    // after private key material has been locked down).
    std::fs::create_dir_all(gnupg_dir)
        .into_diagnostic()
        .wrap_err("failed to create .gnupg directory")?;
    std::fs::set_permissions(gnupg_dir, std::fs::Permissions::from_mode(0o700))
        .into_diagnostic()?;

    // Install a headless pinentry that reads the passphrase from a
    // root-only file. The agent runs as root, so pinentry (launched
    // by the agent) can read the file. This guarantees signing works
    // even if the passphrase cache is invalidated.
    let pinentry_path = install_pinentry(passphrase)?;

    let agent_conf = format!(
        "default-cache-ttl 31536000\n\
         max-cache-ttl 31536000\n\
         allow-preset-passphrase\n\
         allow-loopback-pinentry\n\
         pinentry-program {}\n",
        pinentry_path.display()
    );
    std::fs::write(gnupg_dir.join("gpg-agent.conf"), &agent_conf)
        .into_diagnostic()
        .wrap_err("failed to write gpg-agent.conf")?;

    // Write the private key to a temporary file for import.
    let key_path = gnupg_dir.join("private-key.asc");
    std::fs::write(&key_path, private_key)
        .into_diagnostic()
        .wrap_err("failed to write gpg private key")?;
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
        .into_diagnostic()?;

    // Start gpg-agent daemon.
    //
    // gpg-agent --daemon forks: the parent exits quickly while the child
    // continues as the long-running daemon. We must NOT use .output() here
    // because the daemon child inherits our pipe FDs, keeping the write ends
    // open indefinitely — .output() waits for EOF and blocks forever.
    let status = Command::new("gpg-agent")
        .args([
            "--homedir",
            &gnupg_dir.display().to_string(),
            "--daemon",
            "--verbose",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .into_diagnostic()
        .wrap_err("failed to start gpg-agent")?;

    if !status.success() {
        return Err(miette::miette!("gpg-agent failed to start: {status}"));
    }
    info!("gpg-agent daemon started");

    // Import the private key.
    let import_output = Command::new("gpg")
        .args([
            "--homedir",
            &gnupg_dir.display().to_string(),
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

    // Remove the raw key file — it's now in the keyring.
    let _ = std::fs::remove_file(&key_path);

    // Pre-seed the passphrase for every keygrip (primary + subkeys).
    let keygrips = get_all_keygrips(gnupg_dir)?;
    for grip in &keygrips {
        preset_passphrase(&preset_bin, gnupg_dir, grip, passphrase)?;
    }
    info!(
        count = keygrips.len(),
        "GPG passphrase pre-seeded for all keygrips"
    );

    // Write gpg.conf for the sandbox user's gpg client.
    let gpg_conf = "no-autostart\n";
    std::fs::write(gnupg_dir.join("gpg.conf"), gpg_conf)
        .into_diagnostic()
        .wrap_err("failed to write gpg.conf")?;

    // Wait for the main socket to appear.
    let socket_path = gnupg_dir.join(SOCKET_NAME);
    wait_for_socket(&socket_path)?;

    // Lock down private key material: private-keys-v1.d/ stays root-only.
    let priv_keys_dir = gnupg_dir.join("private-keys-v1.d");
    if priv_keys_dir.exists() {
        std::fs::set_permissions(&priv_keys_dir, std::fs::Permissions::from_mode(0o700))
            .into_diagnostic()?;
    }

    // Chown everything to the sandbox user EXCEPT private-keys-v1.d/.
    chown_except_private_keys(gnupg_dir, sandbox_uid, sandbox_gid)?;

    // Write gitconfig if signing_key_id is set.
    if let Some(key_id) = signing_key_id.filter(|k| !k.is_empty()) {
        write_gitconfig_signing(key_id, sandbox_uid, sandbox_gid)?;
    }

    // Read the agent PID.
    let pid = read_agent_pid(gnupg_dir)?;

    info!(
        pid,
        gnupg_dir = %gnupg_dir.display(),
        "GPG agent ready for signing"
    );

    Ok(GpgAgentHandle {
        pid,
        gnupg_dir: gnupg_dir.to_path_buf(),
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

/// Write a headless pinentry script and its passphrase file into a
/// root-only directory.  Returns the path to the pinentry executable.
///
/// The pinentry speaks the Assuan pinentry protocol and returns the
/// passphrase for every `GETPIN` request.  Because the agent runs as
/// root the pinentry inherits root privileges and can read the
/// passphrase file that the sandbox user cannot access.
fn install_pinentry(passphrase: &str) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let dir = Path::new(PINENTRY_DIR);
    std::fs::create_dir_all(dir)
        .into_diagnostic()
        .wrap_err("failed to create pinentry directory")?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .into_diagnostic()?;

    let pass_file = dir.join("passphrase");
    std::fs::write(&pass_file, passphrase)
        .into_diagnostic()
        .wrap_err("failed to write passphrase file")?;
    std::fs::set_permissions(&pass_file, std::fs::Permissions::from_mode(0o600))
        .into_diagnostic()?;

    let pinentry_path = dir.join("pinentry-openshell");
    let script = format!(
        "#!/bin/sh\n\
         echo 'OK Pleased to meet you'\n\
         while IFS= read -r cmd; do\n\
           case \"$cmd\" in\n\
             GETPIN*)\n\
               printf 'D %s\\n' \"$(cat {pass})\"\n\
               echo OK ;;\n\
             BYE*)\n\
               echo OK; exit 0 ;;\n\
             *)\n\
               echo OK ;;\n\
           esac\n\
         done\n",
        pass = pass_file.display()
    );
    std::fs::write(&pinentry_path, &script)
        .into_diagnostic()
        .wrap_err("failed to write pinentry script")?;
    std::fs::set_permissions(&pinentry_path, std::fs::Permissions::from_mode(0o700))
        .into_diagnostic()?;

    debug!(path = %pinentry_path.display(), "Installed headless pinentry");
    Ok(pinentry_path)
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

/// Chown all entries in the gnupg directory to the sandbox user,
/// except `private-keys-v1.d/` which stays root-only.
#[cfg(unix)]
fn chown_except_private_keys(
    dir: &Path,
    uid: nix::unistd::Uid,
    gid: nix::unistd::Gid,
) -> Result<()> {
    use nix::unistd::chown;

    chown(dir, Some(uid), Some(gid))
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to chown {}", dir.display()))?;

    for entry in std::fs::read_dir(dir).into_diagnostic()? {
        let entry = entry.into_diagnostic()?;
        let path = entry.path();
        let name = entry.file_name();

        if name == "private-keys-v1.d" {
            continue;
        }

        chown(&path, Some(uid), Some(gid))
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to chown {}", path.display()))?;
        if path.is_dir() {
            chown_except_private_keys(&path, uid, gid)?;
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
        "gpg-agent socket did not appear at {} within 5 s",
        path.display()
    ))
}

/// Read the gpg-agent PID from the agent-info or pidfile.
fn read_agent_pid(homedir: &Path) -> Result<u32> {
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
            gnupg_dir: PathBuf::from(SANDBOX_GNUPG_DIR),
        };
        assert_eq!(handle.gnupg_dir(), Path::new(SANDBOX_GNUPG_DIR));
        assert_eq!(handle.pid(), 12345);
    }

    #[test]
    fn sandbox_gnupg_dir_is_under_sandbox() {
        assert!(
            SANDBOX_GNUPG_DIR.starts_with("/sandbox"),
            "sandbox gnupg dir must be under /sandbox"
        );
    }
}
