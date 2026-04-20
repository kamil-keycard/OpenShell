// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Sandbox policy configuration.

use openshell_core::proto::{
    FilesystemPolicy as ProtoFilesystemPolicy, LandlockPolicy as ProtoLandlockPolicy,
    ProcessPolicy as ProtoProcessPolicy, SandboxPolicy as ProtoSandboxPolicy,
};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    pub version: u32,
    pub filesystem: FilesystemPolicy,
    pub network: NetworkPolicy,
    pub landlock: LandlockPolicy,
    pub process: ProcessPolicy,
}

#[derive(Debug, Clone)]
pub struct FilesystemPolicy {
    /// Read-only directory allow list.
    pub read_only: Vec<PathBuf>,

    /// Read-write directory allow list.
    pub read_write: Vec<PathBuf>,

    /// Automatically include the workdir as read-write.
    pub include_workdir: bool,
}

impl Default for FilesystemPolicy {
    fn default() -> Self {
        Self {
            read_only: Vec::new(),
            read_write: Vec::new(),
            include_workdir: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NetworkPolicy {
    pub mode: NetworkMode,
    pub proxy: Option<ProxyPolicy>,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        Self {
            mode: NetworkMode::Block,
            proxy: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub enum NetworkMode {
    #[default]
    Block,
    Proxy,
    Allow,
}

#[derive(Debug, Clone)]
pub struct ProxyPolicy {
    /// TCP address for a local HTTP proxy (loopback-only).
    pub http_addr: Option<SocketAddr>,
}

#[derive(Debug, Clone, Default)]
pub struct LandlockPolicy {
    pub compatibility: LandlockCompatibility,
}

#[derive(Debug, Clone, Default)]
pub struct ProcessPolicy {
    /// User name to run the sandboxed process as.
    pub run_as_user: Option<String>,

    /// Group name to run the sandboxed process as.
    pub run_as_group: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub enum LandlockCompatibility {
    #[default]
    BestEffort,
    HardRequirement,
}

// ============================================================================
// Proto to Rust type conversions
// ============================================================================

impl TryFrom<ProtoSandboxPolicy> for SandboxPolicy {
    type Error = miette::Report;

    fn try_from(proto: ProtoSandboxPolicy) -> Result<Self, Self::Error> {
        // In cluster mode we always run with proxy networking so all egress
        // can be evaluated by OPA and `inference.local` is always addressable.
        let network = NetworkPolicy {
            mode: NetworkMode::Proxy,
            proxy: Some(ProxyPolicy { http_addr: None }),
        };

        let mut filesystem = proto
            .filesystem
            .map(FilesystemPolicy::from)
            .unwrap_or_default();

        // Auto-add host_mounts paths to Landlock coverage so the agent
        // can access mounted directories without explicit filesystem_policy
        // entries. Read-only mounts go to read_only, read-write to read_write.
        for mount in &proto.host_mounts {
            let path = PathBuf::from(openshell_policy::normalize_path(&mount.mount_path));
            if mount.read_only {
                if !filesystem.read_only.contains(&path) {
                    filesystem.read_only.push(path);
                }
            } else if !filesystem.read_write.contains(&path) {
                filesystem.read_write.push(path);
            }
        }

        Ok(Self {
            version: proto.version,
            filesystem,
            network,
            landlock: proto.landlock.map(LandlockPolicy::from).unwrap_or_default(),
            process: proto.process.map(ProcessPolicy::from).unwrap_or_default(),
        })
    }
}

impl From<ProtoFilesystemPolicy> for FilesystemPolicy {
    fn from(proto: ProtoFilesystemPolicy) -> Self {
        Self {
            read_only: proto
                .read_only
                .into_iter()
                .map(|p| PathBuf::from(openshell_policy::normalize_path(&p)))
                .collect(),
            read_write: proto
                .read_write
                .into_iter()
                .map(|p| PathBuf::from(openshell_policy::normalize_path(&p)))
                .collect(),
            include_workdir: proto.include_workdir,
        }
    }
}

impl From<ProtoLandlockPolicy> for LandlockPolicy {
    fn from(proto: ProtoLandlockPolicy) -> Self {
        let compatibility = if proto.compatibility == "hard_requirement" {
            LandlockCompatibility::HardRequirement
        } else {
            LandlockCompatibility::BestEffort
        };
        Self { compatibility }
    }
}

impl From<ProtoProcessPolicy> for ProcessPolicy {
    fn from(proto: ProtoProcessPolicy) -> Self {
        Self {
            run_as_user: if proto.run_as_user.is_empty() {
                None
            } else {
                Some(proto.run_as_user)
            },
            run_as_group: if proto.run_as_group.is_empty() {
                None
            } else {
                Some(proto.run_as_group)
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openshell_core::proto::{HostMount, SandboxPolicy as ProtoSandboxPolicy};

    fn minimal_proto() -> ProtoSandboxPolicy {
        ProtoSandboxPolicy {
            version: 1,
            ..Default::default()
        }
    }

    #[test]
    fn host_mount_rw_auto_adds_to_filesystem_read_write() {
        let mut proto = minimal_proto();
        proto.host_mounts.push(HostMount {
            host_path: "/host-projects".into(),
            mount_path: "/workspace".into(),
            read_only: false,
        });

        let policy = SandboxPolicy::try_from(proto).expect("conversion should succeed");
        assert!(
            policy.filesystem.read_write.contains(&PathBuf::from("/workspace")),
            "read-write mount should auto-add to filesystem.read_write"
        );
        assert!(
            !policy.filesystem.read_only.contains(&PathBuf::from("/workspace")),
            "read-write mount should NOT appear in filesystem.read_only"
        );
    }

    #[test]
    fn host_mount_ro_auto_adds_to_filesystem_read_only() {
        let mut proto = minimal_proto();
        proto.host_mounts.push(HostMount {
            host_path: "/host-data".into(),
            mount_path: "/data".into(),
            read_only: true,
        });

        let policy = SandboxPolicy::try_from(proto).expect("conversion should succeed");
        assert!(
            policy.filesystem.read_only.contains(&PathBuf::from("/data")),
            "read-only mount should auto-add to filesystem.read_only"
        );
        assert!(
            !policy.filesystem.read_write.contains(&PathBuf::from("/data")),
            "read-only mount should NOT appear in filesystem.read_write"
        );
    }

    #[test]
    fn host_mount_no_duplicates_in_filesystem() {
        let mut proto = minimal_proto();
        proto.host_mounts.push(HostMount {
            host_path: "/host-projects".into(),
            mount_path: "/workspace".into(),
            read_only: false,
        });
        proto.host_mounts.push(HostMount {
            host_path: "/host-projects-alt".into(),
            mount_path: "/workspace".into(),
            read_only: false,
        });

        let policy = SandboxPolicy::try_from(proto).expect("conversion should succeed");
        let count = policy
            .filesystem
            .read_write
            .iter()
            .filter(|p| *p == &PathBuf::from("/workspace"))
            .count();
        assert_eq!(count, 1, "duplicate mount_paths should not create duplicate entries");
    }

    #[test]
    fn no_host_mounts_leaves_filesystem_defaults() {
        let proto = minimal_proto();
        let policy = SandboxPolicy::try_from(proto).expect("conversion should succeed");
        assert!(policy.filesystem.read_only.is_empty());
        assert!(policy.filesystem.read_write.is_empty());
    }
}
