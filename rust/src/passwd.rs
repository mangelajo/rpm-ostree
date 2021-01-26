use crate::cxxrsutil;
use crate::ffiutil;
use crate::nameservice;
use anyhow::{anyhow, Result};
use nix::unistd::{Gid, Uid};
use openat_ext::OpenatDirExt;
use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

static PWGRP_SHADOW_FILES: &[&str] = &["shadow", "gshadow", "subuid", "subgid"];
static USRLIB_PWGRP_FILES: &[&str] = &["passwd", "group"];

// Lock/backup files that should not be in the base commit (TODO fix).
static PWGRP_LOCK_AND_BACKUP_FILES: &[&str] = &[
    ".pwd.lock",
    "passwd-",
    "group-",
    "shadow-",
    "gshadow-",
    "subuid-",
    "subgid-",
];

/// Populate a new DB with content from `passwd` and `group` files.
pub fn passwddb_open(rootfs: i32) -> Result<Box<PasswdDB>> {
    let fd = ffiutil::ffi_view_openat_dir(rootfs);
    PasswdDB::populate_new(&fd).map(Box::new)
}

/// Prepare passwd content before layering RPMs.
///
/// We actually want RPM to inject to /usr/lib/passwd - we
/// accomplish this by temporarily renaming /usr/lib/passwd -> /usr/etc/passwd
/// (Which appears as /etc/passwd via our compatibility symlink in the bubblewrap
/// script runner). We also copy the merge deployment's /etc/passwd to
/// /usr/lib/passwd, so that %pre scripts are aware of newly added system users
/// not in the tree's /usr/lib/passwd (through nss-altfiles in the container).
pub fn prepare_rpm_layering(rootfs_dfd: i32, merge_passwd_dir: &str) -> Result<bool> {
    passwd_cleanup(rootfs_dfd)?;
    let rootfs = ffiutil::ffi_view_openat_dir(rootfs_dfd);
    let dir: Option<PathBuf> = cxxrsutil::opt_string(merge_passwd_dir).map(|d| d.into());

    // Break hardlinks for the shadow files, since shadow-utils currently uses
    // O_RDWR unconditionally.
    for filename in PWGRP_SHADOW_FILES {
        let src = format!("etc/{}", filename);
        if rootfs.exists(&src)? {
            ostree::break_hardlink(rootfs.as_raw_fd(), &src, true, gio::NONE_CANCELLABLE)?;
        };
    }

    let has_usrlib_passwd = has_usrlib_passwd(&rootfs)?;
    if has_usrlib_passwd {
        prepare_pwgrp(&rootfs, dir)?;
    }

    Ok(has_usrlib_passwd)
}

pub fn complete_rpm_layering(rootfs_dfd: i32) -> Result<()> {
    let rootfs = ffiutil::ffi_view_openat_dir(rootfs_dfd);
    complete_pwgrp(&rootfs)?;

    Ok(())
}

/// Clean up passwd files.
///
/// This may be leftover in the tree from an older version of rpm-ostree that
/// didn't clean them up at compose time, and having them exist will mean
/// rofiles-fuse will prevent useradd from opening it for write.
pub fn passwd_cleanup(rootfs_dfd: i32) -> Result<()> {
    let rootfs = ffiutil::ffi_view_openat_dir(rootfs_dfd);
    for filename in PWGRP_LOCK_AND_BACKUP_FILES {
        let target = format!("usr/etc/{}", filename);
        rootfs.remove_file_optional(target)?;
    }

    Ok(())
}

/// Passwd splitting logic.
///
/// This function is taking the /etc/passwd generated in the install root (really
/// in /usr/etc at this point), and splitting it into two streams: a new
/// /etc/passwd that just contains the root entry, and /usr/lib/passwd which
/// contains everything else.
pub fn migrate_passwd_except_root(rootfs_dfd: i32) -> Result<()> {
    static ETCSRC_PATH: &str = "usr/etc/passwd";
    static ETCTMP_PATH: &str = "usr/etc/passwd.tmp";
    static USRDEST_PATH: &str = "usr/lib/passwd";

    let rootfs = ffiutil::ffi_view_openat_dir(rootfs_dfd);
    let entries = {
        let src_rd = rootfs.open_file(ETCSRC_PATH).map(BufReader::new)?;
        nameservice::passwd::parse_passwd_content(src_rd)?
    };

    {
        let mut etcdest_stream = rootfs.write_file(ETCTMP_PATH, 0o664).map(BufWriter::new)?;
        let mut usrdest_stream = rootfs
            .append_file(USRDEST_PATH, 0o664)
            .map(BufWriter::new)?;

        for user in entries {
            let mut target = match user.uid {
                0 => &mut etcdest_stream,
                _ => &mut usrdest_stream,
            };
            user.to_writer(&mut target)?;
        }

        etcdest_stream.flush()?;
        usrdest_stream.flush()?;
    }

    rootfs.local_rename(ETCTMP_PATH, ETCSRC_PATH)?;
    Ok(())
}

/// Group splitting logic.
///
/// This function is taking the /etc/group generated in the install root (really
/// in /usr/etc at this point), and splitting it into two streams: a new
/// /etc/group that just contains the root entry, and /usr/lib/group which
/// contains everything else.
pub fn migrate_group_except_root(rootfs_dfd: i32, preserved_groups: &Vec<String>) -> Result<()> {
    static ETCSRC_PATH: &str = "usr/etc/group";
    static ETCTMP_PATH: &str = "usr/etc/group.tmp";
    static USRDEST_PATH: &str = "usr/lib/group";

    let rootfs = ffiutil::ffi_view_openat_dir(rootfs_dfd);
    let entries = {
        let src_rd = rootfs.open_file(ETCSRC_PATH).map(BufReader::new)?;
        nameservice::group::parse_group_content(src_rd)?
    };

    {
        let mut etcdest_stream = rootfs.write_file(ETCTMP_PATH, 0o664).map(BufWriter::new)?;
        let mut usrdest_stream = rootfs
            .append_file(USRDEST_PATH, 0o664)
            .map(BufWriter::new)?;

        for group in entries {
            let mut target = match group.gid {
                0 => &mut etcdest_stream,
                _ => &mut usrdest_stream,
            };
            group.to_writer(&mut target)?;

            // If it's marked in the preserve group, we need to write to
            // *both* /etc and /usr/lib in order to preserve semantics for
            // upgraded systems from before we supported the preserve concept.
            if preserved_groups.contains(&group.name) {
                // We should never be trying to preserve the root entry, it
                // should always be only in /etc.
                anyhow::ensure!(group.gid != 0, "cannot preserve root entry");
                group.to_writer(&mut etcdest_stream)?;
            }
        }

        etcdest_stream.flush()?;
        usrdest_stream.flush()?;
    }

    rootfs.local_rename(ETCTMP_PATH, ETCSRC_PATH)?;
    Ok(())
}

fn has_usrlib_passwd(rootfs: &openat::Dir) -> Result<bool> {
    // Does this rootfs have a usr/lib/passwd? We might be doing a
    // container or something else.
    Ok(rootfs.exists("usr/lib/passwd")?)
}

fn prepare_pwgrp(rootfs: &openat::Dir, merge_passwd_dir: Option<PathBuf>) -> Result<()> {
    for filename in USRLIB_PWGRP_FILES {
        let etc_file = format!("etc/{}", filename);
        let etc_backup = format!("{}.rpmostreesave", etc_file);
        let usrlib_file = format!("usr/lib/{}", filename);
        let usrlib_file_tmp = format!("{}.tmp", &usrlib_file);

        // Retain the current copies in /etc as backups.
        rootfs.local_rename(&etc_file, &etc_backup)?;

        // Copy /usr/lib/{passwd,group} -> /etc (breaking hardlinks).
        rootfs.copy_file(&usrlib_file, &etc_file)?;

        // Copy the merge's passwd/group to usr/lib (breaking hardlinks).
        if let Some(ref merge_dir) = merge_passwd_dir {
            {
                let current_root = openat::Dir::open("/")?;
                let merge_file = format!("{}/{}", merge_dir.display(), &filename);
                current_root.copy_file_at(&merge_file, rootfs, &usrlib_file_tmp)?;
            }
            rootfs.local_rename(&usrlib_file_tmp, &usrlib_file)?;
        }
    }

    Ok(())
}

fn complete_pwgrp(rootfs: &openat::Dir) -> Result<()> {
    for filename in USRLIB_PWGRP_FILES {
        // And now the inverse: /etc/passwd -> /usr/lib/passwd
        let etc_file = format!("etc/{}", filename);
        let usrlib_file = format!("usr/lib/{}", filename);
        rootfs.local_rename(&etc_file, &usrlib_file)?;

        // /etc/passwd.rpmostreesave -> /etc/passwd */
        let etc_backup = format!("{}.rpmostreesave", etc_file);
        rootfs.local_rename(&etc_backup, &etc_file)?;
    }

    // However, we leave the (potentially modified) shadow files in place.
    // In actuality, nothing should change /etc/shadow or /etc/gshadow, so
    // we'll just have to pay the (tiny) cost of re-checksumming.

    Ok(())
}

/// Database holding users and groups.
#[derive(Debug, Default)]
pub struct PasswdDB {
    users: HashMap<Uid, String>,
    groups: HashMap<Gid, String>,
}

impl PasswdDB {
    /// Populate a new DB with content from `passwd` and `group` files.
    pub fn populate_new(rootfs: &openat::Dir) -> anyhow::Result<Self> {
        let mut db = Self::default();
        db.add_passwd_content(rootfs.as_raw_fd(), "usr/etc/passwd")?;
        db.add_passwd_content(rootfs.as_raw_fd(), "usr/lib/passwd")?;
        db.add_group_content(rootfs.as_raw_fd(), "usr/etc/group")?;
        db.add_group_content(rootfs.as_raw_fd(), "usr/lib/group")?;
        Ok(db)
    }

    /// Lookup user name by ID.
    pub fn lookup_user(&self, uid: u32) -> anyhow::Result<String> {
        let key = Uid::from_raw(uid);
        self.users
            .get(&key)
            .cloned()
            .ok_or_else(|| anyhow!("failed to find user ID '{}'", uid))
    }

    /// Lookup group name by ID.
    pub fn lookup_group(&self, gid: u32) -> anyhow::Result<String> {
        let key = Gid::from_raw(gid);
        self.groups
            .get(&key)
            .cloned()
            .ok_or_else(|| anyhow!("failed to find group ID '{}'", gid))
    }

    /// Add content from a `group` file.
    fn add_group_content(&mut self, rootfs_dfd: i32, group_path: &str) -> anyhow::Result<()> {
        let rootfs = ffiutil::ffi_view_openat_dir(rootfs_dfd);
        let db = rootfs.open_file(group_path)?;
        let entries = nameservice::group::parse_group_content(BufReader::new(db))?;

        for group in entries {
            let id = Gid::from_raw(group.gid);
            self.groups.insert(id, group.name);
        }
        Ok(())
    }

    /// Add content from a `passwd` file.
    fn add_passwd_content(&mut self, rootfs_dfd: i32, passwd_path: &str) -> anyhow::Result<()> {
        let rootfs = ffiutil::ffi_view_openat_dir(rootfs_dfd);
        let db = rootfs.open_file(passwd_path)?;
        let entries = nameservice::passwd::parse_passwd_content(BufReader::new(db))?;

        for user in entries {
            let id = Uid::from_raw(user.uid);
            self.users.insert(id, user.name);
        }
        Ok(())
    }
}
