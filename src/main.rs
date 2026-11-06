use nm_dedupe::apfs::{self, Hash};
use nm_dedupe::index::Index;
use nm_dedupe::plan::{self, Plan};
use nm_dedupe::replace::{self, Final, Outcome};
use nm_dedupe::scan::{self, Scan};
use rayon::prelude::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

const USAGE: &str = "usage: nm-dedupe [options] [dir ...]

Finds every node_modules folder under the given dirs (default: current dir)
and replaces duplicate files with APFS clones, also across previous runs.

  -n, --dry-run      measure only, change nothing
  -q, --quiet        print only the summary line
      --min-size N   ignore files smaller than N bytes (default 1)
      --no-index     don't use or update the index in ~/.cache/nm-dedupe
  -j, --jobs N       parallel I/O threads (default 8)
  hook               print a shell function that runs nm-dedupe after npm install/ci
  -V, --version
  -h, --help";

const HOOK: &str = r#"# nm-dedupe: dedupe node_modules after successful npm installs
npm() {
  command npm "$@"; local rc=$?
  if [ $rc -eq 0 ]; then
    case "$1" in install|i|ci|add|update|up|upgrade) nm-dedupe --quiet . >&2 ;; esac
  fi
  return $rc
}"#;

/// Like println!, but a closed stdout (`nm-dedupe | head`) must not abort a run halfway.
macro_rules! out {
    ($($t:tt)*) => {{ use std::io::Write; let _ = writeln!(std::io::stdout(), $($t)*); }}
}

struct Opts {
    dry: bool,
    quiet: bool,
    min_size: u64,
    index: bool,
    jobs: usize,
    dirs: Vec<PathBuf>,
}

fn parse_args() -> Opts {
    let mut o = Opts { dry: false, quiet: false, min_size: 1, index: true, jobs: 8, dirs: vec![] };
    let mut args = std::env::args().skip(1);
    let fail = |msg: String| -> ! {
        eprintln!("nm-dedupe: {msg}\n\n{USAGE}");
        std::process::exit(2)
    };
    while let Some(a) = args.next() {
        let mut number = |name: &str| {
            args.next()
                .and_then(|v| v.parse().ok())
                .filter(|&v| v > 0)
                .unwrap_or_else(|| fail(format!("{name} needs a positive number")))
        };
        match a.as_str() {
            "hook" => exit_with(HOOK),
            "-V" | "--version" => exit_with(concat!("nm-dedupe ", env!("CARGO_PKG_VERSION"))),
            "-h" | "--help" => exit_with(USAGE),
            "-n" | "--dry-run" => o.dry = true,
            "-q" | "--quiet" => o.quiet = true,
            "--no-index" => o.index = false,
            "--min-size" => o.min_size = number("--min-size"),
            "-j" | "--jobs" => o.jobs = number("--jobs") as usize,
            s if s.starts_with('-') => fail(format!("unknown option {s}")),
            d => o.dirs.push(std::fs::canonicalize(d).unwrap_or_else(|e| fail(format!("{d}: {e}")))),
        }
    }
    if o.dirs.is_empty() {
        o.dirs.push(std::env::current_dir().expect("current dir"));
    }
    o.dirs.sort();
    o.dirs.dedup();
    let roots = o.dirs.clone();
    o.dirs.retain(|d| !roots.iter().any(|r| r != d && d.starts_with(r)));
    o
}

fn exit_with(text: &str) -> ! {
    out!("{text}");
    std::process::exit(0)
}

fn fmt(bytes: f64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut i) = (bytes, 0);
    while v.abs() >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{v:.0} B") } else { format!("{v:.1} {}", units[i]) }
}

struct Report {
    quiet: bool,
    t0: Instant,
}

impl Report {
    fn line(&self, text: String) {
        if !self.quiet {
            out!("{text}  [{:.1}s, peak {}]", self.t0.elapsed().as_secs_f64(), fmt(apfs::peak_rss() as f64));
        }
    }
}

fn report_scan(r: &Report, sc: &Scan) {
    let s = &sc.stats;
    let total: u64 = sc.files.iter().map(|f| f.size).sum();
    r.line(format!(
        "scanned {} node_modules, {} files, {} logical",
        s.node_modules.load(Relaxed),
        sc.files.len(),
        fmt(total as f64)
    ));
    let ignored = [
        ("symlinks", &s.symlinks),
        ("hardlinked", &s.hardlinks),
        ("other owner", &s.foreign_owner),
        ("flags/ACL", &s.flagged),
        ("empty/small", &s.empty),
        ("unreadable dirs", &s.unreadable_dirs),
        ("leftovers of interrupted runs removed", &s.leftovers_removed),
    ];
    let ignored: Vec<String> =
        ignored.iter().map(|(k, v)| (k, v.load(Relaxed))).filter(|x| x.1 > 0).map(|(k, v)| format!("{v} {k}")).collect();
    if !ignored.is_empty() && !r.quiet {
        out!("  left alone: {}", ignored.join(", "));
    }
    for kept in &sc.leftovers_kept {
        out!("  leftover of an interrupted run kept: {kept}");
    }
}

/// Hash every file the index doesn't know (without index: only files with a same-size twin).
fn hash_all(sc: &Scan, index: Option<&Index>, r: &Report) -> rusqlite::Result<(Vec<Option<Hash>>, Vec<bool>)> {
    let n = sc.files.len();
    let mut hashes = vec![None; n];
    let hits = match index {
        Some(ix) => ix.cached_hashes(&sc.files, &mut hashes)?,
        None => 0,
    };
    let need: Vec<u32> = if index.is_some() {
        (0..n as u32).filter(|&i| hashes[i as usize].is_none()).collect()
    } else {
        let mut by_size: Vec<u32> = (0..n as u32).collect();
        by_size.par_sort_unstable_by_key(|&i| sc.files[i as usize].size);
        by_size
            .chunk_by(|&a, &b| sc.files[a as usize].size == sc.files[b as usize].size)
            .filter(|g| g.len() > 1)
            .flatten()
            .copied()
            .collect()
    };
    let bytes = AtomicU64::new(0);
    let computed: Vec<(u32, Hash)> = need
        .par_iter()
        .filter_map(|&i| {
            let e = &sc.files[i as usize];
            let h = apfs::hash_file(&sc.path(e), e.size).ok()?;
            bytes.fetch_add(e.size, Relaxed);
            Some((i, h))
        })
        .collect();
    let mut fresh = vec![false; n];
    r.line(format!("hashed {} files ({}), {hits} from cache", computed.len(), fmt(bytes.load(Relaxed) as f64)));
    for (i, h) in computed {
        hashes[i as usize] = Some(h);
        fresh[i as usize] = true;
    }
    Ok((hashes, fresh))
}

struct Tally {
    ok: u64,
    skipped: u64,
    errors: u64,
    reasons: Vec<(String, u64)>,
}

impl Tally {
    fn note(&mut self, error: bool, reason: String) {
        if error {
            self.errors += 1
        } else {
            self.skipped += 1
        }
        match self.reasons.iter_mut().find(|x| x.0 == reason) {
            Some(x) => x.1 += 1,
            None => self.reasons.push((reason, 1)),
        }
    }
}

fn execute(sc: &mut Scan, plan: &Plan, fresh: &mut [bool]) -> Tally {
    let todo = &plan.todo;
    let mut res: Vec<Outcome> = todo
        .par_iter()
        .enumerate()
        .map(|(k, &(s, d))| replace::clone_over(&plan.source(sc, s), &sc.state(d as usize), k))
        .collect();
    replace::hook("pause-before-finalize", usize::MAX);
    let finals: Vec<(usize, Final)> = res
        .par_iter()
        .enumerate()
        .filter_map(|(k, o)| match o {
            Outcome::Swapped(p) => Some((k, replace::finalize(&sc.path(&sc.files[todo[k].1 as usize]), k, p))),
            _ => None,
        })
        .collect();
    for (k, f) in finals {
        res[k] = match f {
            Final::Removed => continue,
            Final::Restored => Outcome::Skip("written during run, original restored"),
            Final::Recovered(p) => Outcome::Err(format!("written during run, data kept at {}", p.display())),
            Final::Kept(w) => Outcome::Err(format!("original kept: {w}")),
        };
    }
    let mut t = Tally { ok: 0, skipped: 0, errors: 0, reasons: vec![] };
    for (k, o) in res.into_iter().enumerate() {
        match o {
            Outcome::Swapped(p) => {
                let d = todo[k].1 as usize;
                sc.files[d].ino = p.new_ino;
                sc.files[d].mtime = p.new_mtime;
                fresh[d] = true;
                t.ok += 1;
            }
            Outcome::Skip(why) => t.note(false, format!("skipped: {why}")),
            Outcome::Err(e) => t.note(true, format!("error: {e}")),
        }
    }
    t.reasons.sort_by_key(|x| std::cmp::Reverse(x.1));
    t
}

fn save(
    index: Option<&Index>,
    sc: &Scan,
    plan: &Plan,
    hashes: &[Option<Hash>],
    fresh: &[bool],
    dry: bool,
) -> rusqlite::Result<()> {
    let Some(ix) = index else { return Ok(()) };
    ix.save_hashes(sc.files.iter().zip(hashes).zip(fresh).filter(|x| *x.1).filter_map(|((e, h), _)| Some((e, h.as_ref()?))))?;
    if dry {
        return Ok(());
    }
    ix.save_canonicals(plan.new_canonicals.iter().map(|&i| (hashes[i as usize].unwrap(), sc.state(i as usize))))
}

fn main() -> rusqlite::Result<()> {
    let o = parse_args();
    rayon::ThreadPoolBuilder::new().num_threads(o.jobs).build_global().expect("thread pool");
    let r = Report { quiet: o.quiet, t0: Instant::now() };
    let uid = unsafe { libc::getuid() };

    let mut sc =
        scan::scan(&o.dirs, scan::Options { uid, min_size: o.min_size, resolve_leftovers: !o.dry, private_sizes: o.dry });
    report_scan(&r, &sc);
    let index = if o.index { Some(Index::open()?) } else { None };
    let (hashes, mut fresh) = hash_all(&sc, index.as_ref(), &r)?;
    let plan = plan::build(&sc, &hashes, index.as_ref(), uid)?;
    let basis = if o.dry { "from APFS private sizes" } else { "estimate; -n gives the precise forecast" };
    r.line(format!(
        "duplicates: {} files, {} logical; already cloned: {}; to clone: {}, expected to free {} ({basis})",
        plan.duplicates,
        fmt(plan.duplicate_bytes as f64),
        plan.already_shared,
        plan.todo.len(),
        fmt(plan.expected_free as f64)
    ));

    if o.dry {
        out!("dry-run: would clone {} files, expected to free {}", plan.todo.len(), fmt(plan.expected_free as f64));
        return save(index.as_ref(), &sc, &plan, &hashes, &fresh, true);
    }

    let before = apfs::free_bytes(&o.dirs[0]);
    let t = execute(&mut sc, &plan, &mut fresh);
    let after = apfs::settled_free_bytes(&o.dirs[0], std::time::Duration::from_secs(30));
    save(index.as_ref(), &sc, &plan, &hashes, &fresh, false)?;

    for (why, n) in t.reasons.iter().take(10) {
        if !o.quiet || why.starts_with("error") {
            out!("  {n}× {why}");
        }
    }
    let gained = after as f64 - before as f64;
    r.line(format!(
        "nm-dedupe: cloned {}, skipped {}, errors {}; free space {} → {} (measured {}{}, expected {})",
        t.ok,
        t.skipped,
        t.errors,
        fmt(before as f64),
        fmt(after as f64),
        if gained >= 0.0 { "+" } else { "" },
        fmt(gained),
        fmt(plan.expected_free as f64)
    ));
    if o.quiet {
        out!("nm-dedupe: cloned {}, freed {}", t.ok, fmt(gained));
    }
    if apfs::local_snapshots() > 0 {
        out!("note: local Time Machine snapshots still hold the old blocks; the space returns when they expire");
    }
    if t.errors > 0 {
        std::process::exit(1);
    }
    Ok(())
}
