//! Replacing a file by an APFS clone without ever risking its data.
//!
//! 1. Clone the source to a temp name next to the target, copy the target's metadata onto it.
//! 2. Compare clone and target byte for byte; remember the content hash H.
//! 3. Atomically swap clone and target; the original stays under the temp name.
//! 4. At the end of the run (`finalize`) delete the original only if it still holds H.
//!
//! The path always holds either the original or a byte-identical clone, and an original
//! that received writes meanwhile (e.g. through an open fd) is put back or kept.

use crate::apfs::{self, Hash};
use std::{ffi::OsStr, os::unix::ffi::OsStrExt, path::Path, path::PathBuf};

pub const TMP_PREFIX: &str = ".nmdd-";
const RECOVERED_SUFFIX: &str = ".nm-dedupe-recovered";

#[derive(Clone, Debug)]
pub struct FileState {
    pub path: PathBuf,
    pub ino: u64,
    pub dev: u32,
    pub size: u64,
    pub mtime: i64,
}

impl FileState {
    fn matches(&self, st: &libc::stat) -> bool {
        (st.st_mode & libc::S_IFMT) == libc::S_IFREG
            && st.st_ino == self.ino
            && st.st_dev as u32 == self.dev
            && st.st_size as u64 == self.size
            && apfs::mtime_ns(st) == self.mtime
            && st.st_nlink == 1
            && st.st_flags == 0
    }

    pub fn unchanged(&self) -> bool {
        apfs::lstat(&self.path).is_some_and(|st| self.matches(&st))
    }
}

fn same_but_ctime(a: &libc::stat, b: &libc::stat) -> bool {
    a.st_ino == b.st_ino
        && a.st_size == b.st_size
        && apfs::mtime_ns(a) == apfs::mtime_ns(b)
        && a.st_mode == b.st_mode
        && a.st_flags == b.st_flags
}

/// ctime can't be set from userspace: any write shows up, even one that restores the mtime.
fn same_state(a: &libc::stat, b: &libc::stat) -> bool {
    same_but_ctime(a, b) && apfs::ctime_ns(a) == apfs::ctime_ns(b)
}

#[derive(Debug, PartialEq)]
pub struct Pending {
    pub new_ino: u64,
    pub new_mtime: i64,
    old_ino: u64,
    hash: Hash,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Swapped(Pending),
    Skip(&'static str),
    Err(String),
}

/// `.nmdd-<pid>-<n>~<target name>`; the target name lets a later run verify a leftover.
pub fn tmp_path(dst: &Path, n: usize) -> PathBuf {
    let target = dst.file_name().unwrap_or_default().as_bytes();
    let mut name = format!("{TMP_PREFIX}{}-{n}~", std::process::id()).into_bytes();
    if name.len() + target.len() <= 255 {
        name.extend_from_slice(target);
    }
    dst.with_file_name(OsStr::from_bytes(&name))
}

/// Fault injection for tests, `NMDD_TEST_HOOK=<point>:<n>[,…]`: `crash-*` points exit hard
/// (like SIGKILL) in the n-th replacement, `pause-*` points sleep n ms.
pub fn hook(point: &str, n: usize) {
    static HOOKS: std::sync::OnceLock<Vec<(String, usize)>> = std::sync::OnceLock::new();
    let hooks = HOOKS.get_or_init(|| {
        let v = std::env::var("NMDD_TEST_HOOK").unwrap_or_default();
        v.split(',').filter_map(|h| h.rsplit_once(':')).filter_map(|(p, k)| Some((p.to_string(), k.parse().ok()?))).collect()
    });
    for (p, k) in hooks.iter().filter(|(p, _)| p == point) {
        if p.starts_with("pause-") {
            eprintln!("NMDD_PAUSED {point}");
            std::thread::sleep(std::time::Duration::from_millis(*k as u64));
        } else if *k == n {
            unsafe { libc::_exit(99) }
        }
    }
}

pub fn clone_over(src: &FileState, dst: &FileState, n: usize) -> Outcome {
    if src.dev != dst.dev {
        return Outcome::Skip("crossdevice");
    }
    let Some(dfile) = apfs::open_ro(&dst.path) else { return Outcome::Skip("unreadable") };
    let Some(dst0) = apfs::fstat(&dfile).filter(|st| dst.matches(st)) else { return Outcome::Skip("changed") };
    let Some(dst_xattrs) = apfs::xattrs(&dst.path) else { return Outcome::Skip("xattr") };

    let tmp = tmp_path(&dst.path, n);
    if let Err(e) = apfs::clone_file(&src.path, &tmp) {
        return Outcome::Err(format!("clonefile: {e}"));
    }
    let undo = |o: Outcome| {
        apfs::unlink(&tmp);
        o
    };
    if !apfs::set_xattrs(&tmp, &dst_xattrs) {
        return undo(Outcome::Skip("xattr"));
    }
    if let Err(e) = apfs::copy_mode_group_times(&tmp, &dst0) {
        return undo(Outcome::Err(format!("metadata: {e}")));
    }
    let Some(tfile) = apfs::open_ro(&tmp) else { return undo(Outcome::Err("reopen clone".into())) };
    let hash = match apfs::compare_hash(&tfile, &dfile) {
        Ok(Some(h)) => h,
        Ok(None) => return undo(Outcome::Skip("content differs")),
        Err(e) => return undo(Outcome::Err(format!("compare: {e}"))),
    };
    let meta_ok = apfs::fstat(&tfile)
        .is_some_and(|t| t.st_size == dst0.st_size && t.st_mode == dst0.st_mode && apfs::mtime_ns(&t) == apfs::mtime_ns(&dst0));
    if !meta_ok {
        return undo(Outcome::Err("clone metadata mismatch".into()));
    }
    let still = |st: Option<libc::stat>| st.is_some_and(|s| same_state(&s, &dst0));
    if !still(apfs::fstat(&dfile)) || !still(apfs::lstat(&dst.path)) {
        return undo(Outcome::Skip("changed"));
    }

    hook("crash-before-swap", n);
    if let Err(e) = apfs::swap(&tmp, &dst.path) {
        return undo(Outcome::Err(format!("swap: {e}")));
    }
    hook("crash-after-swap", n);

    // early exit for writes that raced the swap; finalize would catch them as well
    let orig_ok = apfs::fstat(&dfile).is_some_and(|a| same_but_ctime(&a, &dst0));
    let orig_parked = apfs::lstat(&tmp).is_some_and(|o| o.st_ino == dst0.st_ino);
    if !orig_ok || !orig_parked {
        return match apfs::swap(&tmp, &dst.path) {
            Ok(()) => undo(Outcome::Skip("changed")),
            Err(_) => Outcome::Err(format!("swap-back failed, original kept at {}", tmp.display())),
        };
    }
    match apfs::lstat(&dst.path) {
        Some(st) => Outcome::Swapped(Pending { new_ino: st.st_ino, new_mtime: apfs::mtime_ns(&st), old_ino: dst0.st_ino, hash }),
        None => Outcome::Err("clone vanished after swap".into()),
    }
}

#[derive(Debug, PartialEq)]
pub enum Final {
    Removed,
    Restored,
    Recovered(PathBuf),
    Kept(String),
}

/// | original now | clone now      | action                                 |
/// |--------------|----------------|----------------------------------------|
/// | H            | anything       | delete original                        |
/// | not H        | H, same inode  | swap original back, delete stale clone |
/// | not H        | otherwise      | keep original as `<name>.nm-dedupe-recovered` |
pub fn finalize(dst: &Path, n: usize, p: &Pending) -> Final {
    let tmp = tmp_path(dst, n);
    for _ in 0..3 {
        let Some(orig) = apfs::open_ro(&tmp) else { return Final::Kept("original vanished from temp name".into()) };
        let Some(o1) = apfs::fstat(&orig) else { continue };
        if o1.st_ino != p.old_ino {
            return Final::Kept(format!("unexpected file at {}", tmp.display()));
        }
        let Some(h_orig) = apfs::hash_open(&orig) else { continue };
        if !apfs::fstat(&orig).is_some_and(|o2| same_state(&o1, &o2)) {
            continue;
        }
        if h_orig == p.hash {
            apfs::unlink(&tmp);
            return Final::Removed;
        }
        let stale_clone = apfs::open_ro(dst)
            .filter(|c| apfs::fstat(c).is_some_and(|st| st.st_ino == p.new_ino) && apfs::hash_open(c) == Some(p.hash));
        let Some(clone) = stale_clone else { return recover(&tmp, dst) };
        hook("pause-before-swap-back", n);
        if apfs::swap(&tmp, dst).is_ok() && apfs::hash_open(&clone) == Some(p.hash) {
            apfs::unlink(&tmp);
            return Final::Restored;
        }
        return recover(&tmp, dst);
    }
    recover(&tmp, dst)
}

fn recover(tmp: &Path, dst: &Path) -> Final {
    let mut name = dst.file_name().unwrap_or_default().to_os_string();
    name.push(RECOVERED_SUFFIX);
    let base = name.clone();
    let mut i = 1;
    while apfs::lstat(&dst.with_file_name(&name)).is_some() {
        name = base.clone();
        name.push(format!(".{i}"));
        i += 1;
    }
    let rec = dst.with_file_name(&name);
    match std::fs::rename(tmp, &rec) {
        Ok(()) => Final::Recovered(rec),
        Err(e) => Final::Kept(format!("{} ({e})", tmp.display())),
    }
}

/// A temp file left by an interrupted run whose process is gone. It holds either a clone or
/// the original of its target: deleted only if byte-identical to the target, else kept.
pub fn resolve_leftover(tmp: &Path) -> Option<Final> {
    let rest = tmp.file_name()?.as_bytes().strip_prefix(TMP_PREFIX.as_bytes())?;
    let pid = std::str::from_utf8(rest.split(|&c| c == b'-').next()?).ok()?.parse().ok()?;
    if !apfs::process_gone(pid) {
        return None;
    }
    let target = &rest[rest.iter().position(|&c| c == b'~')? + 1..];
    if target.is_empty() {
        return Some(Final::Kept(format!("{} (no target name)", tmp.display())));
    }
    let dst = tmp.with_file_name(OsStr::from_bytes(target));
    let identical = match (apfs::open_ro(tmp), apfs::open_ro(&dst)) {
        (Some(a), Some(b)) => matches!(apfs::compare_hash(&a, &b), Ok(Some(_))),
        _ => false,
    };
    if identical {
        apfs::unlink(tmp);
        Some(Final::Removed)
    } else if apfs::lstat(&dst).is_none() {
        std::fs::rename(tmp, &dst).ok()?;
        Some(Final::Restored)
    } else {
        Some(recover(tmp, &dst))
    }
}
