//! Detect whether a path lives on a network filesystem.

use std::path::Path;

use anyhow::Result;

#[cfg(target_os = "linux")]
pub fn is_network_fs(path: &Path) -> Result<bool> {
    use nix::sys::statfs::{statfs, FsType};

    let st = statfs(path)?;
    let t = st.filesystem_type();

    // Magic numbers from statfs(2) for network-ish filesystems. FUSE is
    // included on purpose: sshfs/rclone/gvfs mounts all show up as FUSE and
    // should be treated as slow remote storage.
    const NETWORK_MAGICS: &[libc::__fsword_t] = &[
        0x517b,     // SMB
        0xfe534d42, // SMB2
        0xff534d42, // CIFS
        0x6969,     // NFS
        0x65735546, // FUSE
        0x19830326, // BeeGFS
        0x0bd00bd0, // Lustre
        0x47504653, // GPFS
        0x00c36400, // CephFS
        0x7461636f, // OCFS2
        0x01021997, // V9FS (9p, used by some VM shares)
    ];

    Ok(NETWORK_MAGICS.iter().any(|&m| t == FsType(m)))
}

#[cfg(not(target_os = "linux"))]
pub fn is_network_fs(_path: &Path) -> Result<bool> {
    // Conservative default on non-Linux: assume network so we stage through
    // the local temp dir (harmless, just an extra copy).
    Ok(true)
}
