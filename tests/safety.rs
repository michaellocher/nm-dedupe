//! The invariant checked after every scenario: every file still exists with identical content
//! and metadata (content hash, size, mode, mtime, owner, group, BSD flags, xattrs, symlink
//! targets, directory modes). Only inode numbers and block sharing may change.

use nm_dedupe::apfs;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_nm-dedupe");

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        static N: AtomicUsize = AtomicUsize::new(0);
        let root =
            std::env::temp_dir().join(format!("nmdd-test-{}-{}-{name}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("w")).unwrap();
        Sandbox { home: root.join("home"), root }
    }

    fn work(&self) -> PathBuf {
        self.root.join("w")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args).env("HOME", &self.home).env_remove("NMDD_TEST_HOOK");
        c
    }

    fn spawn_paused(&self, hooks: &str) -> std::process::Child {
        let work = self.work();
        let mut c = self.command(&["-j", "1", work.to_str().unwrap()]);
        c.env("NMDD_TEST_HOOK", hooks).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn run_on(&self, dirs: &[&Path]) -> String {
        let o = self.command(&[]).args(dirs).output().unwrap();
        let out = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
        assert!(o.status.success(), "nm-dedupe failed:\n{out}");
        out
    }

    fn dedupe(&self) -> String {
        self.run_on(&[&self.work()])
    }

    fn dedupe_checked(&self, dirs: &[&Path]) -> String {
        let (before, _) = manifest(&self.root);
        let out = self.run_on(dirs);
        let (after, extra) = manifest(&self.root);
        assert_same(&before, &after);
        assert!(extra.is_empty(), "leftovers: {extra:?}\n{out}");
        out
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = Command::new("chflags").args(["-R", "nouchg"]).arg(&self.root).status();
        let _ = Command::new("chmod").args(["-R", "-N"]).arg(&self.root).stderr(Stdio::null()).status();
        let _ = Command::new("chmod").args(["-R", "u+rwx"]).arg(&self.root).status();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn write(p: &Path, data: &[u8]) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, data).unwrap();
}

/// Projects with identical node_modules trees, one unique file each and a file outside node_modules.
fn projects(sb: &Sandbox, projects: usize, files: usize, size: usize) -> Vec<PathBuf> {
    (0..projects)
        .map(|p| {
            let proj = sb.work().join(format!("proj{p}"));
            for f in 0..files {
                write(&proj.join(format!("node_modules/pkg{}/lib/f{f}.js", f % 7)), &bytes(f as u64 + 1, size + f % 13));
            }
            write(&proj.join("node_modules/unique.js"), &bytes(1000 + p as u64, 3000));
            write(&proj.join("src/index.js"), &bytes(7, 5000));
            proj
        })
        .collect()
}

#[derive(Debug, PartialEq)]
enum Item {
    File { sha: apfs::Hash, size: u64, mode: u16, mtime: (i64, i64), uid: u32, gid: u32, flags: u32, xattrs: apfs::Xattrs },
    Symlink(PathBuf),
    Dir { mode: u16 },
}

fn lstat(p: &Path) -> libc::stat {
    apfs::lstat(p).unwrap_or_else(|| panic!("lstat {}", p.display()))
}

fn chmod(p: &Path, mode: u16) {
    fs::set_permissions(p, fs::Permissions::from_mode(u32::from(mode))).unwrap();
}

/// Reads even unreadable files/dirs by granting access temporarily.
fn with_access<T>(p: &Path, need: u16, f: impl FnOnce() -> T) -> T {
    let mode = lstat(p).st_mode & 0o7777;
    let grant = mode & need != need;
    if grant {
        chmod(p, mode | need);
    }
    let r = f();
    if grant {
        chmod(p, mode);
    }
    r
}

/// Everything under root except the test HOME, plus nm-dedupe temp/recovery files separately.
fn manifest(root: &Path) -> (BTreeMap<PathBuf, Item>, Vec<PathBuf>) {
    let (mut items, mut extra) = (BTreeMap::new(), Vec::new());
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<PathBuf> =
            with_access(&dir, 0o500, || fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect());
        entries.sort();
        for p in entries {
            let name = p.file_name().unwrap().as_bytes();
            if name.starts_with(b".nmdd-") || name.ends_with(b".nm-dedupe-recovered") {
                extra.push(p);
                continue;
            }
            if dir == root && name == b"home" {
                continue;
            }
            let st = lstat(&p);
            let item = match st.st_mode & libc::S_IFMT {
                libc::S_IFDIR => {
                    stack.push(p.clone());
                    Item::Dir { mode: st.st_mode & 0o7777 }
                }
                libc::S_IFLNK => Item::Symlink(fs::read_link(&p).unwrap()),
                _ => Item::File {
                    xattrs: with_access(&p, 0o400, || apfs::xattrs(&p).unwrap()),
                    sha: with_access(&p, 0o400, || apfs::hash_file(&p, st.st_size as u64).unwrap()),
                    size: st.st_size as u64,
                    mode: st.st_mode & 0o7777,
                    mtime: (st.st_mtime, st.st_mtime_nsec),
                    uid: st.st_uid,
                    gid: st.st_gid,
                    flags: st.st_flags,
                },
            };
            items.insert(p.strip_prefix(root).unwrap().to_path_buf(), item);
        }
    }
    (items, extra)
}

fn assert_same(before: &BTreeMap<PathBuf, Item>, after: &BTreeMap<PathBuf, Item>) {
    for (k, v) in before {
        match after.get(k) {
            None => panic!("FILE LOST: {}", k.display()),
            Some(a) => assert_eq!(a, v, "FILE CHANGED: {}", k.display()),
        }
    }
    for k in after.keys() {
        assert!(before.contains_key(k), "unexpected new file {}", k.display());
    }
}

fn shared(a: &Path, b: &Path) -> bool {
    let (pa, ia) = apfs::clone_info(a).unwrap();
    let (pb, ib) = apfs::clone_info(b).unwrap();
    ia == ib && pa == 0 && pb == 0
}

fn num(out: &str, key: &str) -> u64 {
    let i = out.find(key).unwrap_or_else(|| panic!("'{key}' not in output:\n{out}")) + key.len();
    out[i..].trim_start().split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap()
}

fn cloned(out: &str) -> u64 {
    num(out, "nm-dedupe: cloned")
}

fn set_times(p: &Path, st: &libc::stat) {
    let t = [
        libc::timespec { tv_sec: st.st_atime, tv_nsec: st.st_atime_nsec },
        libc::timespec { tv_sec: st.st_mtime, tv_nsec: st.st_mtime_nsec },
    ];
    assert_eq!(unsafe { libc::utimensat(libc::AT_FDCWD, apfs::cpath(p).as_ptr(), t.as_ptr(), 0) }, 0);
}

fn sh(cmd: &str, args: &[&str], p: &Path) {
    assert!(Command::new(cmd).args(args).arg(p).status().unwrap().success(), "{cmd} {args:?} {}", p.display());
}

#[test]
fn dedupes_and_preserves_every_file() {
    let sb = Sandbox::new("basic");
    let p = projects(&sb, 3, 60, 5000);
    for (i, size) in [1usize, 4095, 4096, 4097, 65536, 1 << 20, (9 << 20) + 3].iter().enumerate() {
        for proj in &p {
            write(&proj.join(format!("node_modules/sizes/s{i}")), &bytes(500 + i as u64, *size));
        }
    }
    let out = sb.dedupe_checked(&[&sb.work()]);
    assert_eq!(num(&out, "errors"), 0);
    assert_eq!(cloned(&out), 2 * (60 + 7), "{out}");
    for f in ["node_modules/pkg0/lib/f0.js", "node_modules/sizes/s6", "node_modules/sizes/s0"] {
        assert!(shared(&p[0].join(f), &p[1].join(f)) && shared(&p[0].join(f), &p[2].join(f)), "{f} not shared");
    }
    assert!(!shared(&p[0].join("node_modules/unique.js"), &p[1].join("node_modules/unique.js")));
    assert!(!shared(&p[0].join("src/index.js"), &p[1].join("src/index.js")), "outside node_modules must stay untouched");
}

#[test]
fn dry_run_changes_nothing_not_even_inodes() {
    let sb = Sandbox::new("dry");
    projects(&sb, 2, 40, 3000);
    let inodes = || -> Vec<u64> { manifest(&sb.work()).0.keys().map(|k| lstat(&sb.work().join(k)).st_ino).collect() };
    let (before, i0) = (manifest(&sb.root).0, inodes());
    let o = sb.run(&["-n", sb.work().to_str().unwrap()]);
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success() && out.contains("would clone 40 files"), "{out}");
    assert_same(&before, &manifest(&sb.root).0);
    assert_eq!(i0, inodes());
}

#[test]
fn second_run_is_a_noop() {
    let sb = Sandbox::new("idem");
    projects(&sb, 3, 30, 2000);
    sb.dedupe();
    let out = sb.dedupe_checked(&[&sb.work()]);
    assert_eq!(cloned(&out), 0);
    assert_eq!(num(&out, "to clone:"), 0);
}

#[test]
fn later_project_matches_via_index() {
    let sb = Sandbox::new("index");
    let p = projects(&sb, 1, 30, 4000);
    sb.dedupe();
    let later = sb.root.join("later");
    for f in 0..30 {
        write(&later.join(format!("node_modules/pkg{}/lib/f{f}.js", f % 7)), &bytes(f as u64 + 1, 4000 + f % 13));
    }
    let out = sb.dedupe_checked(&[&later]);
    assert_eq!(cloned(&out), 30, "{out}");
    assert!(shared(&p[0].join("node_modules/pkg0/lib/f0.js"), &later.join("node_modules/pkg0/lib/f0.js")));
}

#[test]
fn works_without_index() {
    let sb = Sandbox::new("noindex");
    projects(&sb, 2, 20, 1000);
    let (before, _) = manifest(&sb.root);
    let o = sb.run(&["--no-index", sb.work().to_str().unwrap()]);
    assert_eq!(cloned(&String::from_utf8_lossy(&o.stdout)), 20);
    assert_same(&before, &manifest(&sb.root).0);
    assert!(!sb.home.join(".cache/nm-dedupe").exists());
}

#[test]
fn copy_on_write_edits_stay_local() {
    let sb = Sandbox::new("cow");
    let p = projects(&sb, 3, 10, 9000);
    sb.dedupe();
    let f = "node_modules/pkg1/lib/f1.js";
    let orig = fs::read(p[1].join(f)).unwrap();
    let mut edited = orig.clone();
    edited[100] ^= 0xff;
    fs::write(p[0].join(f), &edited).unwrap();
    assert_eq!(fs::read(p[1].join(f)).unwrap(), orig);
    assert_eq!(fs::read(p[2].join(f)).unwrap(), orig);
}

#[test]
fn preserves_mode_times_group_and_differing_xattrs() {
    let sb = Sandbox::new("meta");
    let p = projects(&sb, 3, 5, 6000);
    let f = |i: usize| p[i].join("node_modules/pkg0/lib/f0.js");
    sh("xattr", &["-w", "user.only-on-1", "one"], &f(1));
    sh("xattr", &["-w", "user.on-0-and-2", "zero"], &f(0));
    sh("xattr", &["-w", "user.on-0-and-2", "two!"], &f(2));
    sh("xattr", &["-wx", "user.bin", "00FF00"], &f(2));
    chmod(&f(1), 0o751);
    chmod(&f(2), 0o400);
    sh("touch", &["-t", "200001020304.05"], &f(1));
    let groups = String::from_utf8(Command::new("id").arg("-G").output().unwrap().stdout).unwrap();
    let cur = lstat(&f(1)).st_gid;
    if let Some(g) = groups.split_whitespace().filter_map(|g| g.parse::<u32>().ok()).find(|&g| g != cur) {
        unsafe { libc::lchown(apfs::cpath(&f(1)).as_ptr(), u32::MAX, g) };
    }
    let seen = apfs::xattrs(&f(2)).unwrap();
    assert!(seen.iter().filter(|(n, _)| n.starts_with(b"user.")).count() == 2, "fixture xattrs not visible");
    let out = sb.dedupe_checked(&[&sb.work()]);
    assert_eq!(num(&out, "errors"), 0, "{out}");
}

#[test]
fn leaves_symlinks_hardlinks_flags_and_acls_alone() {
    let sb = Sandbox::new("leave");
    let p = projects(&sb, 3, 4, 3000);
    let f = |i: usize| p[i].join("node_modules/pkg0/lib/f0.js");
    fs::hard_link(f(0), p[0].join("node_modules/hl.js")).unwrap();
    symlink("pkg0/lib/f0.js", p[1].join("node_modules/link.js")).unwrap();
    fs::create_dir_all(sb.work().join("proj8")).unwrap();
    symlink(p[0].join("node_modules"), sb.work().join("proj8/node_modules")).unwrap();
    sh("chflags", &["uchg"], &f(1));
    sh("chmod", &["+a", "everyone deny delete"], &f(2));
    let inodes = || [f(0), p[0].join("node_modules/hl.js"), f(1), f(2)].map(|q| lstat(&q).st_ino);
    let before = inodes();
    sb.dedupe_checked(&[&sb.work()]);
    assert_eq!(before, inodes());
}

#[test]
fn read_only_dirs_and_unreadable_files_lose_nothing() {
    let sb = Sandbox::new("ro");
    let p = projects(&sb, 3, 6, 4000);
    chmod(&p[2].join("node_modules/pkg3/lib/f3.js"), 0o000);
    chmod(&p[1].join("node_modules/pkg2/lib"), 0o555);
    let (before, _) = manifest(&sb.root);
    let o = sb.run(&[sb.work().to_str().unwrap()]);
    let (after, extra) = manifest(&sb.root);
    assert_same(&before, &after);
    assert!(extra.is_empty(), "leftovers: {extra:?}");
    assert!(cloned(&String::from_utf8_lossy(&o.stdout)) > 0);
}

#[test]
fn awkward_names_and_deep_paths() {
    let sb = Sandbox::new("names");
    let long = ["x".repeat(200), "y".repeat(255)];
    let names = [
        "with space.js",
        "tab\there.js",
        "new\nline.js",
        "-leading-dash.js",
        "ünïcödé-日本.js",
        "e\u{301}-nfd.js",
        "quote'\"$(x).js",
        "back\\slash.js",
        "~tilde.js",
        ".dotfile",
        &long[0],
        &long[1],
    ];
    let deep: PathBuf = (0..25).map(|i| format!("d{i:02}-{}", "z".repeat(20))).collect();
    for proj in 0..3 {
        let nm = sb.work().join(format!("p{proj}/node_modules"));
        for (i, n) in names.iter().enumerate() {
            write(&nm.join("pkg").join(n), &bytes(i as u64 + 77, 2000 + i));
        }
        write(&nm.join(&deep).join("deep.js"), &bytes(99, 3333));
    }
    let out = sb.dedupe_checked(&[&sb.work()]);
    assert_eq!(cloned(&out), 2 * (names.len() as u64 + 1), "{out}");
    for n in names {
        let at = |p: u8| sb.work().join(format!("p{p}/node_modules/pkg")).join(n);
        assert!(shared(&at(0), &at(2)), "{n:?}");
    }
}

#[test]
fn stale_hash_cache_with_restored_mtime_is_caught_by_byte_compare() {
    let sb = Sandbox::new("stale");
    let a = sb.work().join("a/node_modules/x/f.js");
    let b = sb.work().join("b/node_modules/x/f.js");
    write(&a, b"original content 1234\n");
    sb.dedupe();
    let st = lstat(&a);
    fs::write(&a, b"EVIL!!!! content 1234\n").unwrap();
    set_times(&a, &st);
    write(&b, b"original content 1234\n");
    sb.dedupe_checked(&[&sb.work()]);
    assert_eq!(fs::read(&b).unwrap(), b"original content 1234\n");
}

#[test]
fn edited_or_deleted_canonical_is_never_used() {
    let sb = Sandbox::new("canon");
    let file = |p: &str| sb.work().join(p).join("node_modules/x/f.js");
    write(&file("a"), b"payload one\n");
    sb.dedupe();
    fs::write(file("a"), b"payload two\n").unwrap();
    write(&file("b"), b"payload one\n");
    sb.dedupe();
    assert_eq!(fs::read(file("b")).unwrap(), b"payload one\n");
    fs::remove_file(file("a")).unwrap();
    write(&file("c"), b"payload one\n");
    sb.dedupe_checked(&[&sb.work()]);
    assert!(shared(&file("b"), &file("c")));
}

#[test]
fn overlapping_roots_count_files_once() {
    let sb = Sandbox::new("overlap");
    let p = projects(&sb, 2, 10, 1000);
    let out = sb.dedupe_checked(&[&sb.work(), &p[0], &p[0].join("node_modules")]);
    assert_eq!(cloned(&out), 10, "{out}");
    assert_eq!(num(&out, "scanned"), 2, "{out}");
}

#[test]
fn crash_at_every_critical_point_loses_nothing_and_next_run_cleans_up() {
    for point in ["crash-before-swap", "crash-after-swap"] {
        for k in [0usize, 5, 17] {
            let sb = Sandbox::new("crash");
            projects(&sb, 3, 12, 5000);
            let (before, _) = manifest(&sb.root);
            let o = sb
                .command(&["-j", "1", sb.work().to_str().unwrap()])
                .env("NMDD_TEST_HOOK", format!("{point}:{k}"))
                .output()
                .unwrap();
            assert_eq!(o.status.code(), Some(99), "hook did not fire for {point}:{k}");
            let (after, extra) = manifest(&sb.root);
            assert_same(&before, &after);
            assert!(!extra.is_empty(), "{point}:{k}: expected a leftover temp file");
            sb.dedupe_checked(&[&sb.work()]);
        }
    }
}

#[test]
fn leftover_that_differs_from_target_is_kept_as_recovered() {
    let sb = Sandbox::new("leftover");
    let dir = sb.work().join("p/node_modules/x");
    write(&dir.join("f.js"), b"current\n");
    write(&dir.join(".nmdd-99999999-0~f.js"), b"something else\n");
    write(&dir.join(".nmdd-99999999-1~gone.js"), b"orphan data\n");
    write(&dir.join(".nmdd-99999999-2~f.js"), b"current\n");
    sb.dedupe();
    assert_eq!(fs::read(dir.join("f.js")).unwrap(), b"current\n");
    assert_eq!(fs::read(dir.join("f.js.nm-dedupe-recovered")).unwrap(), b"something else\n");
    assert_eq!(fs::read(dir.join("gone.js")).unwrap(), b"orphan data\n");
    assert!(!dir.join(".nmdd-99999999-2~f.js").exists());
    let alive = dir.join(format!(".nmdd-{}-0~f.js", std::process::id()));
    write(&alive, b"in flight\n");
    sb.dedupe();
    assert_eq!(fs::read(&alive).unwrap(), b"in flight\n");
}

#[test]
fn sigkill_at_random_moments_loses_nothing() {
    for round in 0..12u64 {
        let sb = Sandbox::new("sigkill");
        projects(&sb, 4, 400, 3000);
        let (before, _) = manifest(&sb.root);
        let mut child = sb.command(&[sb.work().to_str().unwrap()]).stdout(Stdio::null()).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(5 + round * 9));
        let _ = child.kill();
        let _ = child.wait();
        assert_same(&before, &manifest(&sb.root).0);
        sb.dedupe_checked(&[&sb.work()]);
    }
}

#[test]
fn concurrent_writers_never_lose_a_write() {
    for round in 0..4u64 {
        let sb = Sandbox::new("writers");
        let p = projects(&sb, 3, 300, 8000);
        let targets: Vec<PathBuf> = p
            .iter()
            .flat_map(|proj| (0..300).map(move |f| proj.join(format!("node_modules/pkg{}/lib/f{f}.js", f % 7))))
            .collect();
        let expect = Arc::new(Mutex::new(targets.iter().map(|t| (t.clone(), fs::read(t).unwrap())).collect::<BTreeMap<_, _>>()));
        let stop = Arc::new(AtomicBool::new(false));
        let writers: Vec<_> = (0..3u64)
            .map(|w| {
                let (targets, expect, stop) = (targets.clone(), expect.clone(), stop.clone());
                std::thread::spawn(move || {
                    let mut i = round * 1000 + w * 7919;
                    while !stop.load(Ordering::Relaxed) {
                        i += 1;
                        let t = &targets[(i * 2_654_435_761 % targets.len() as u64) as usize];
                        let mut ex = expect.lock().unwrap();
                        let data = if i % 4 == 0 { ex[t].clone() } else { bytes(i, 8000 + (i % 3) as usize) };
                        match i % 3 {
                            0 => fs::write(t, &data).unwrap(),
                            1 => {
                                let st = lstat(t);
                                fs::write(t, &data).unwrap();
                                set_times(t, &st);
                            }
                            _ => {
                                let tmp = t.with_extension("writer-tmp");
                                fs::write(&tmp, &data).unwrap();
                                fs::rename(&tmp, t).unwrap();
                            }
                        }
                        ex.insert(t.clone(), data);
                        drop(ex);
                        std::thread::sleep(Duration::from_micros(200));
                    }
                })
            })
            .collect();
        let o = sb.run(&["-j", "4", sb.work().to_str().unwrap()]);
        stop.store(true, Ordering::Relaxed);
        writers.into_iter().for_each(|w| w.join().unwrap());
        let out = String::from_utf8_lossy(&o.stdout);
        for (t, data) in expect.lock().unwrap().iter() {
            assert_eq!(&fs::read(t).unwrap(), data, "round {round}: write lost in {}\n{out}", t.display());
        }
        assert!(manifest(&sb.root).1.is_empty(), "round {round}: leftovers");
    }
}

#[test]
fn write_through_open_fd_after_swap_is_restored() {
    use std::io::{Read, Seek, Write};
    let sb = Sandbox::new("openfd");
    let p = projects(&sb, 3, 3, 5000);
    let victims: Vec<PathBuf> = p.iter().map(|q| q.join("node_modules/pkg0/lib/f0.js")).collect();
    let mut handles: Vec<fs::File> = victims.iter().map(|v| fs::OpenOptions::new().write(true).open(v).unwrap()).collect();
    let mut child = sb.spawn_paused("pause-before-finalize:1500");
    let mut err = child.stderr.take().unwrap();
    let (mut buf, mut seen) = ([0u8; 64], Vec::new());
    while !String::from_utf8_lossy(&seen).contains("NMDD_PAUSED") {
        let n = err.read(&mut buf).unwrap();
        assert!(n > 0, "child exited before pausing");
        seen.extend_from_slice(&buf[..n]);
    }
    for (i, h) in handles.iter_mut().enumerate() {
        h.seek(std::io::SeekFrom::Start(0)).unwrap();
        h.write_all(format!("WRITTEN VIA OPEN FD {i}").as_bytes()).unwrap();
    }
    drop(handles);
    let out = String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).to_string();
    for (i, v) in victims.iter().enumerate() {
        let marker = format!("WRITTEN VIA OPEN FD {i}");
        let kept = [fs::read(v).unwrap(), fs::read(v.with_file_name("f0.js.nm-dedupe-recovered")).unwrap_or_default()];
        assert!(kept.iter().any(|k| k.starts_with(marker.as_bytes())), "write {i} lost:\n{out}");
    }
    assert!(out.contains("original restored"), "{out}");
}

#[test]
fn writes_to_original_and_clone_during_swap_back_both_survive() {
    use std::io::{BufRead, Seek, Write};
    let sb = Sandbox::new("swapback");
    let p = projects(&sb, 2, 3, 5000);
    let victims: Vec<PathBuf> = p.iter().map(|q| q.join("node_modules/pkg0/lib/f0.js")).collect();
    let mut originals: Vec<fs::File> = victims.iter().map(|v| fs::OpenOptions::new().write(true).open(v).unwrap()).collect();
    let mut child = sb.spawn_paused("pause-before-finalize:1000,pause-before-swap-back:1000");
    let write_at = |f: &mut fs::File, off: u64, text: String| {
        f.seek(std::io::SeekFrom::Start(off)).unwrap();
        f.write_all(text.as_bytes()).unwrap();
    };
    let mut clone_written = false;
    for line in std::io::BufReader::new(child.stderr.take().unwrap()).lines() {
        let line = line.unwrap();
        if line.contains("pause-before-finalize") {
            for (i, f) in originals.iter_mut().enumerate() {
                write_at(f, 0, format!("VIA ORIGINAL FD {i}"));
            }
        } else if line.contains("pause-before-swap-back") && !clone_written {
            for (i, v) in victims.iter().enumerate() {
                write_at(&mut fs::OpenOptions::new().write(true).open(v).unwrap(), 100, format!("VIA PATH {i}"));
            }
            clone_written = true;
        }
    }
    let out = String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).to_string();
    assert!(clone_written, "swap-back never happened:\n{out}");
    for (i, v) in victims.iter().enumerate() {
        let dir = v.parent().unwrap();
        let copies: Vec<Vec<u8>> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|q| q.file_name().unwrap().as_bytes().starts_with(b"f0.js"))
            .map(|q| fs::read(q).unwrap())
            .collect();
        for marker in [format!("VIA ORIGINAL FD {i}"), format!("VIA PATH {i}")] {
            assert!(copies.iter().any(|c| c.windows(marker.len()).any(|w| w == marker.as_bytes())), "lost '{marker}':\n{out}");
        }
    }
}
