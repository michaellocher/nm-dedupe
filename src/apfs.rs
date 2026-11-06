use std::ffi::CString;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::FromRawFd;
use std::path::Path;

pub type Hash = [u8; 32];

unsafe extern "C" {
    fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32) -> libc::c_int;
    fn renamex_np(from: *const libc::c_char, to: *const libc::c_char, flags: libc::c_uint) -> libc::c_int;
}

const CLONE_NOFOLLOW: u32 = 0x1;
const CLONE_NOOWNERCOPY: u32 = 0x2;
const RENAME_SWAP: libc::c_uint = 0x2;
const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x8;
const ATTR_CMNEXT_CLONEID: u32 = 0x100;
const FSOPT_ATTR_CMN_EXTENDED: u32 = 0x20;

pub fn cpath(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).expect("path without NUL")
}

fn check(r: libc::c_int) -> std::io::Result<()> {
    if r == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

pub fn clone_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    check(unsafe { clonefile(cpath(src).as_ptr(), cpath(dst).as_ptr(), CLONE_NOFOLLOW | CLONE_NOOWNERCOPY) })
}

/// Atomically exchanges the two directory entries.
pub fn swap(a: &Path, b: &Path) -> std::io::Result<()> {
    check(unsafe { renamex_np(cpath(a).as_ptr(), cpath(b).as_ptr(), RENAME_SWAP) })
}

pub fn unlink(p: &Path) -> bool {
    unsafe { libc::unlink(cpath(p).as_ptr()) == 0 }
}

pub fn lstat(p: &Path) -> Option<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::lstat(cpath(p).as_ptr(), &mut st) } == 0).then_some(st)
}

pub fn fstat(f: &std::fs::File) -> Option<libc::stat> {
    use std::os::unix::io::AsRawFd;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::fstat(f.as_raw_fd(), &mut st) } == 0).then_some(st)
}

pub fn mtime_ns(st: &libc::stat) -> i64 {
    st.st_mtime * 1_000_000_000 + st.st_mtime_nsec
}

pub fn ctime_ns(st: &libc::stat) -> i64 {
    st.st_ctime * 1_000_000_000 + st.st_ctime_nsec
}

pub fn open_ro(p: &Path) -> Option<std::fs::File> {
    let fd = unsafe { libc::open(cpath(p).as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    (fd >= 0).then(|| unsafe { std::fs::File::from_raw_fd(fd) })
}

pub fn copy_mode_group_times(p: &Path, from: &libc::stat) -> std::io::Result<()> {
    let c = cpath(p);
    let times = [
        libc::timespec { tv_sec: from.st_atime, tv_nsec: from.st_atime_nsec },
        libc::timespec { tv_sec: from.st_mtime, tv_nsec: from.st_mtime_nsec },
    ];
    unsafe {
        check(libc::lchown(c.as_ptr(), u32::MAX, from.st_gid))?;
        check(libc::chmod(c.as_ptr(), from.st_mode & 0o7777))?;
        check(libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW))
    }
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// SHA-256 of `a` if `a` and `b` are byte-identical, read from their current offsets.
pub fn compare_hash(mut a: &std::fs::File, mut b: &std::fs::File) -> std::io::Result<Option<Hash>> {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    let (mut x, mut y) = (vec![0u8; 1 << 18], vec![0u8; 1 << 18]);
    loop {
        let n = read_full(&mut a, &mut x)?;
        let m = read_full(&mut b, &mut y)?;
        if n != m || x[..n] != y[..m] {
            return Ok(None);
        }
        if n == 0 {
            return Ok(Some(h.finalize().into()));
        }
        h.update(&x[..n]);
    }
}

pub fn hash_file(p: &Path, size_hint: u64) -> std::io::Result<Hash> {
    hash_reader(std::fs::File::open(p)?, size_hint)
}

fn hash_reader(mut r: impl Read, size_hint: u64) -> std::io::Result<Hash> {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; (size_hint as usize).saturating_add(1).min(1 << 18)];
    loop {
        match read_full(&mut r, &mut buf)? {
            0 => return Ok(h.finalize().into()),
            n => h.update(&buf[..n]),
        }
    }
}

pub fn hash_open(mut f: &std::fs::File) -> Option<Hash> {
    use std::io::Seek;
    f.seek(std::io::SeekFrom::Start(0)).ok()?;
    hash_reader(f, u64::MAX).ok()
}

pub type Xattrs = Vec<(Vec<u8>, Vec<u8>)>;

pub fn xattrs(p: &Path) -> Option<Xattrs> {
    let c = cpath(p);
    let opt = libc::XATTR_NOFOLLOW;
    let len = unsafe { libc::listxattr(c.as_ptr(), std::ptr::null_mut(), 0, opt) };
    let mut names = vec![0u8; usize::try_from(len).ok()?];
    let len = unsafe { libc::listxattr(c.as_ptr(), names.as_mut_ptr().cast(), names.len(), opt) };
    names.truncate(usize::try_from(len).ok()?);
    let mut out = Vec::new();
    for name in names.split(|&b| b == 0).filter(|n| !n.is_empty()) {
        let cn = CString::new(name).ok()?;
        let vlen = unsafe { libc::getxattr(c.as_ptr(), cn.as_ptr(), std::ptr::null_mut(), 0, 0, opt) };
        let mut v = vec![0u8; usize::try_from(vlen).ok()?];
        let vlen = unsafe { libc::getxattr(c.as_ptr(), cn.as_ptr(), v.as_mut_ptr().cast(), v.len(), 0, opt) };
        v.truncate(usize::try_from(vlen).ok()?);
        out.push((name.to_vec(), v));
    }
    out.sort();
    Some(out)
}

/// Assigned by macOS per installing app; userspace can't set it, so it is not compared.
const PROVENANCE: &[u8] = b"com.apple.provenance";

fn user_xattrs(x: Xattrs) -> Xattrs {
    x.into_iter().filter(|(n, _)| n != PROVENANCE).collect()
}

/// Makes `p`'s extended attributes equal to `want`, apart from com.apple.provenance.
pub fn set_xattrs(p: &Path, want: &Xattrs) -> bool {
    let c = cpath(p);
    let opt = libc::XATTR_NOFOLLOW;
    let want = user_xattrs(want.clone());
    let Some(have) = xattrs(p).map(user_xattrs) else { return false };
    for (n, _) in have.iter().filter(|(n, _)| !want.iter().any(|(w, _)| w == n)) {
        let cn = CString::new(n.as_slice()).unwrap();
        if unsafe { libc::removexattr(c.as_ptr(), cn.as_ptr(), opt) } != 0 {
            return false;
        }
    }
    for (n, v) in want.iter().filter(|w| !have.contains(w)) {
        let cn = CString::new(n.as_slice()).unwrap();
        if unsafe { libc::setxattr(c.as_ptr(), cn.as_ptr(), v.as_ptr().cast(), v.len(), 0, opt) } != 0 {
            return false;
        }
    }
    xattrs(p).map(user_xattrs) == Some(want)
}

/// (bytes not shared with any clone, clone id)
pub fn clone_info(p: &Path) -> Option<(u64, u64)> {
    #[repr(C, packed(4))]
    struct Buf {
        len: u32,
        private: i64,
        clone_id: u64,
    }
    let mut al: libc::attrlist = unsafe { std::mem::zeroed() };
    al.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    al.forkattr = ATTR_CMNEXT_PRIVATESIZE | ATTR_CMNEXT_CLONEID;
    let mut b = Buf { len: 0, private: 0, clone_id: 0 };
    let r = unsafe {
        libc::getattrlist(
            cpath(p).as_ptr(),
            (&raw mut al).cast(),
            (&raw mut b).cast(),
            size_of::<Buf>(),
            libc::FSOPT_NOFOLLOW | FSOPT_ATTR_CMN_EXTENDED,
        )
    };
    (r == 0).then_some((b.private as u64, b.clone_id))
}

pub fn free_bytes(p: &Path) -> u64 {
    unsafe {
        libc::sync();
        let mut s: libc::statfs = std::mem::zeroed();
        if libc::statfs(cpath(p).as_ptr(), &mut s) != 0 {
            return 0;
        }
        s.f_bavail * u64::from(s.f_bsize)
    }
}

/// APFS releases freed blocks asynchronously: poll until free space stops changing.
pub fn settled_free_bytes(p: &Path, max: std::time::Duration) -> u64 {
    let start = std::time::Instant::now();
    let mut last = free_bytes(p);
    let mut stable = 0;
    while stable < 4 && start.elapsed() < max {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let now = free_bytes(p);
        stable = if now.abs_diff(last) < 1 << 20 { stable + 1 } else { 0 };
        last = now;
    }
    last
}

pub fn peak_rss() -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    ru.ru_maxrss as u64
}

pub fn process_gone(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) != 0 && *libc::__error() == libc::ESRCH }
}

pub fn local_snapshots() -> usize {
    std::process::Command::new("tmutil")
        .args(["listlocalsnapshots", "/"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).matches("com.apple").count())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_provenance_is_ignored() {
        let x = |n: &str, v: &str| (n.as_bytes().to_vec(), v.as_bytes().to_vec());
        let all = vec![x("com.apple.provenance", "a"), x("com.apple.quarantine", "q"), x("user.k", "v")];
        assert_eq!(user_xattrs(all), vec![x("com.apple.quarantine", "q"), x("user.k", "v")]);
    }
}
