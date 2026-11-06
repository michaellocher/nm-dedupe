//! Parallel walk with `getattrlistbulk(2)`: one syscall returns the attributes of a whole
//! batch of directory entries, so there is no per-file `lstat`.

use crate::{apfs, replace};
use rayon::Scope;
use rayon::prelude::*;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub const UNKNOWN: u64 = u64::MAX;

/// Path = dirs[dir] + "/" + names[name_off..name_off + name_len]
#[derive(Clone, Copy, Debug)]
pub struct Entry {
    pub ino: u64,
    pub size: u64,
    pub mtime: i64,
    /// bytes not shared with any clone, or UNKNOWN if not requested
    pub private: u64,
    pub clone_id: u64,
    pub name_off: u32,
    pub dir: u32,
    pub dev: u32,
    pub name_len: u16,
}

#[derive(Default)]
pub struct Stats {
    pub node_modules: AtomicU64,
    pub symlinks: AtomicU64,
    pub hardlinks: AtomicU64,
    pub foreign_owner: AtomicU64,
    pub flagged: AtomicU64,
    pub empty: AtomicU64,
    pub leftovers_removed: AtomicU64,
    pub unreadable_dirs: AtomicU64,
}

pub struct Scan {
    pub dirs: Vec<Box<[u8]>>,
    pub names: Vec<u8>,
    pub files: Vec<Entry>,
    pub stats: Stats,
    pub leftovers_kept: Vec<String>,
}

impl Scan {
    pub fn path(&self, e: &Entry) -> PathBuf {
        let dir = &self.dirs[e.dir as usize];
        let name = &self.names[e.name_off as usize..][..e.name_len as usize];
        let mut b = Vec::with_capacity(dir.len() + 1 + name.len());
        b.extend_from_slice(dir);
        b.push(b'/');
        b.extend_from_slice(name);
        PathBuf::from(OsStr::from_bytes(&b))
    }

    pub fn state(&self, i: usize) -> replace::FileState {
        let e = &self.files[i];
        replace::FileState { path: self.path(e), ino: e.ino, dev: e.dev, size: e.size, mtime: e.mtime }
    }
}

pub struct Options {
    pub uid: u32,
    pub min_size: u64,
    pub resolve_leftovers: bool,
    pub private_sizes: bool,
}

const ATTR_CMN_NAME: u32 = 0x1;
const ATTR_CMN_DEVID: u32 = 0x2;
const ATTR_CMN_OBJTYPE: u32 = 0x8;
const ATTR_CMN_MODTIME: u32 = 0x400;
const ATTR_CMN_OWNERID: u32 = 0x8000;
const ATTR_CMN_FLAGS: u32 = 0x4_0000;
const ATTR_CMN_EXTENDED_SECURITY: u32 = 0x40_0000;
const ATTR_CMN_FILEID: u32 = 0x200_0000;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
const ATTR_FILE_LINKCOUNT: u32 = 0x1;
const ATTR_FILE_DATALENGTH: u32 = 0x200;
const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x8;
const ATTR_CMNEXT_CLONEID: u32 = 0x100;
const FSOPT_NOFOLLOW: u64 = 0x1;
const FSOPT_ATTR_CMN_EXTENDED: u64 = 0x20;
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

const REQUIRED_CMN: u32 =
    ATTR_CMN_NAME | ATTR_CMN_DEVID | ATTR_CMN_OBJTYPE | ATTR_CMN_MODTIME | ATTR_CMN_OWNERID | ATTR_CMN_FLAGS | ATTR_CMN_FILEID;
const REQUIRED_FILE: u32 = ATTR_FILE_LINKCOUNT | ATTR_FILE_DATALENGTH;

#[repr(C)]
struct AttrList {
    bitmapcount: u16,
    reserved: u16,
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

unsafe extern "C" {
    fn getattrlistbulk(
        dirfd: libc::c_int,
        al: *mut AttrList,
        buf: *mut libc::c_void,
        size: libc::size_t,
        opts: u64,
    ) -> libc::c_int;
}

struct Raw<'a> {
    name: &'a [u8],
    objtype: u32,
    dev: u32,
    mtime: i64,
    uid: u32,
    flags: u32,
    has_acl: bool,
    ino: u64,
    nlink: u32,
    size: u64,
    private: u64,
    clone_id: u64,
}

struct Reader<'a> {
    rec: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let v = self.rec.get(self.pos..self.pos + N)?.try_into().ok()?;
        self.pos += N;
        Some(v)
    }
    fn u32(&mut self) -> Option<u32> {
        self.take().map(u32::from_ne_bytes)
    }
    fn u64(&mut self) -> Option<u64> {
        self.take().map(u64::from_ne_bytes)
    }
}

/// Attributes follow in bit order, each present only if its bit is in the returned set.
fn parse(rec: &[u8]) -> Option<Raw<'_>> {
    let mut r = Reader { rec, pos: 4 };
    let (cmn, _vol, _dir, file, fork) = (r.u32()?, r.u32()?, r.u32()?, r.u32()?, r.u32()?);
    let mut e = Raw {
        name: &[],
        objtype: 0,
        dev: 0,
        mtime: 0,
        uid: u32::MAX,
        flags: u32::MAX,
        has_acl: false,
        ino: 0,
        nlink: 0,
        size: 0,
        private: UNKNOWN,
        clone_id: 0,
    };
    if cmn & ATTR_CMN_NAME != 0 {
        let at = r.pos;
        let (off, len) = (r.u32()? as i32, r.u32()? as usize);
        let start = usize::try_from(at as i64 + i64::from(off)).ok()?;
        let name = rec.get(start..start + len)?;
        e.name = name.strip_suffix(&[0]).unwrap_or(name);
    }
    if cmn & ATTR_CMN_DEVID != 0 {
        e.dev = r.u32()?
    }
    if cmn & ATTR_CMN_OBJTYPE != 0 {
        e.objtype = r.u32()?
    }
    if cmn & ATTR_CMN_MODTIME != 0 {
        e.mtime = r.u64()? as i64 * 1_000_000_000 + r.u64()? as i64
    }
    if cmn & ATTR_CMN_OWNERID != 0 {
        e.uid = r.u32()?
    }
    if cmn & ATTR_CMN_FLAGS != 0 {
        e.flags = r.u32()?
    }
    if cmn & ATTR_CMN_EXTENDED_SECURITY != 0 {
        e.has_acl = {
            r.u32()?;
            r.u32()? > 0
        }
    }
    if cmn & ATTR_CMN_FILEID != 0 {
        e.ino = r.u64()?
    }
    if file & ATTR_FILE_LINKCOUNT != 0 {
        e.nlink = r.u32()?
    }
    if file & ATTR_FILE_DATALENGTH != 0 {
        e.size = r.u64()?
    }
    if fork & ATTR_CMNEXT_PRIVATESIZE != 0 {
        e.private = r.u64()?
    }
    if fork & ATTR_CMNEXT_CLONEID != 0 {
        e.clone_id = r.u64()?
    }
    if e.objtype == VREG && (cmn & REQUIRED_CMN != REQUIRED_CMN || file & REQUIRED_FILE != REQUIRED_FILE) {
        e.objtype = 0;
    }
    Some(e)
}

fn read_dir_bulk(dir: &Path, private: bool, mut f: impl FnMut(Raw)) -> bool {
    let fd =
        unsafe { libc::open(apfs::cpath(dir).as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 {
        return false;
    }
    let mut al = AttrList {
        bitmapcount: 5,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS | REQUIRED_CMN | ATTR_CMN_EXTENDED_SECURITY,
        volattr: 0,
        dirattr: 0,
        fileattr: REQUIRED_FILE,
        // private size costs the kernel an extent walk, roughly doubling scan time
        forkattr: ATTR_CMNEXT_CLONEID | if private { ATTR_CMNEXT_PRIVATESIZE } else { 0 },
    };
    let mut buf = vec![0u8; 256 * 1024];
    let ok = loop {
        let n =
            unsafe { getattrlistbulk(fd, &mut al, buf.as_mut_ptr().cast(), buf.len(), FSOPT_NOFOLLOW | FSOPT_ATTR_CMN_EXTENDED) };
        if n <= 0 {
            break n == 0;
        }
        let mut off = 0;
        for _ in 0..n {
            let Some(len) = buf.get(off..off + 4).map(|b| u32::from_ne_bytes(b.try_into().unwrap()) as usize) else { break };
            let Some(rec) = buf.get(off..off + len).filter(|_| len >= 4) else { break };
            if let Some(raw) = parse(rec) {
                f(raw);
            }
            off += len;
        }
    };
    unsafe { libc::close(fd) };
    ok
}

struct DirOut {
    path: PathBuf,
    names: Vec<u8>,
    entries: Vec<Entry>,
}

struct Ctx {
    opt: Options,
    stats: Stats,
    leftovers_kept: Mutex<Vec<String>>,
    out: Mutex<Vec<DirOut>>,
}

impl Ctx {
    fn classify(&self, dir: &Path, e: &Raw, names: &mut Vec<u8>, entries: &mut Vec<Entry>) {
        let s = &self.stats;
        if e.name.starts_with(replace::TMP_PREFIX.as_bytes()) {
            if self.opt.resolve_leftovers && e.uid == self.opt.uid {
                match replace::resolve_leftover(&dir.join(OsStr::from_bytes(e.name))) {
                    Some(replace::Final::Removed | replace::Final::Restored) => {
                        s.leftovers_removed.fetch_add(1, Relaxed);
                    }
                    Some(kept) => self.leftovers_kept.lock().unwrap().push(format!("{kept:?}")),
                    None => {}
                }
            }
        } else if e.nlink != 1 {
            s.hardlinks.fetch_add(1, Relaxed);
        } else if e.uid != self.opt.uid {
            s.foreign_owner.fetch_add(1, Relaxed);
        } else if e.flags != 0 || e.has_acl {
            s.flagged.fetch_add(1, Relaxed);
        } else if e.size < self.opt.min_size {
            s.empty.fetch_add(1, Relaxed);
        } else {
            entries.push(Entry {
                ino: e.ino,
                size: e.size,
                mtime: e.mtime,
                private: e.private,
                clone_id: e.clone_id,
                name_off: names.len() as u32,
                dir: 0,
                dev: e.dev,
                name_len: e.name.len() as u16,
            });
            names.extend_from_slice(e.name);
        }
    }
}

fn walk_node_modules<'s>(s: &Scope<'s>, dir: PathBuf, cx: &'s Ctx) {
    let (mut names, mut entries, mut subdirs) = (Vec::new(), Vec::new(), Vec::new());
    let ok = read_dir_bulk(&dir, cx.opt.private_sizes, |e| match e.objtype {
        VDIR => subdirs.push(dir.join(OsStr::from_bytes(e.name))),
        VLNK => {
            cx.stats.symlinks.fetch_add(1, Relaxed);
        }
        VREG => cx.classify(&dir, &e, &mut names, &mut entries),
        _ => {}
    });
    if !ok {
        cx.stats.unreadable_dirs.fetch_add(1, Relaxed);
    }
    for d in subdirs {
        s.spawn(move |s| walk_node_modules(s, d, cx));
    }
    if !entries.is_empty() {
        names.shrink_to_fit();
        entries.shrink_to_fit();
        cx.out.lock().unwrap().push(DirOut { path: dir, names, entries });
    }
}

fn find_node_modules<'s>(s: &Scope<'s>, dir: PathBuf, cx: &'s Ctx) {
    if dir.file_name() == Some(OsStr::new("node_modules")) {
        cx.stats.node_modules.fetch_add(1, Relaxed);
        return walk_node_modules(s, dir, cx);
    }
    let mut subdirs = Vec::new();
    read_dir_bulk(&dir, false, |e| {
        if e.objtype == VDIR && e.name != b".git" && e.name != b".Trash" {
            subdirs.push(dir.join(OsStr::from_bytes(e.name)));
        }
    });
    for d in subdirs {
        s.spawn(move |s| find_node_modules(s, d, cx));
    }
}

/// Roots may be project dirs or node_modules dirs. Files come back sorted by (dev, ino).
pub fn scan(roots: &[PathBuf], opt: Options) -> Scan {
    let cx = Ctx { opt, stats: Stats::default(), leftovers_kept: Mutex::default(), out: Mutex::default() };
    rayon::scope(|s| {
        for r in roots {
            let (r, cx) = (r.clone(), &cx);
            s.spawn(move |s| find_node_modules(s, r, cx));
        }
    });
    let outs = cx.out.into_inner().unwrap();
    let mut sc = Scan {
        dirs: Vec::with_capacity(outs.len()),
        names: Vec::with_capacity(outs.iter().map(|d| d.names.len()).sum()),
        files: Vec::with_capacity(outs.iter().map(|d| d.entries.len()).sum()),
        stats: cx.stats,
        leftovers_kept: cx.leftovers_kept.into_inner().unwrap(),
    };
    for d in outs {
        let (dir, base) = (sc.dirs.len() as u32, sc.names.len() as u32);
        sc.dirs.push(d.path.as_os_str().as_bytes().into());
        sc.files.extend(d.entries.into_iter().map(|e| Entry { dir, name_off: e.name_off + base, ..e }));
        sc.names.extend_from_slice(&d.names);
    }
    sc.files.par_sort_unstable_by_key(|e| (i64::from(e.dev), e.ino as i64));
    sc.files.dedup_by_key(|e| (e.dev, e.ino));
    sc
}
