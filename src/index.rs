//! Cross-run memory in `~/.cache/nm-dedupe/index-v2.db`: content hashes keyed by file state,
//! and one canonical clone source per hash. Lookups are merge-joins over sorted input, so
//! nothing large is loaded into memory.

use crate::apfs::Hash;
use crate::replace::FileState;
use crate::scan::Entry;
use rusqlite::{Connection, params};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

pub struct Index {
    db: Connection,
}

impl Index {
    pub fn open() -> rusqlite::Result<Index> {
        let dir = PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache/nm-dedupe");
        let _ = std::fs::create_dir_all(&dir);
        let db = Connection::open(dir.join("index-v2.db"))?;
        db.busy_timeout(std::time::Duration::from_secs(30))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS hashes(ino INTEGER, dev INTEGER, size INTEGER, mtime INTEGER, hash BLOB,
                                               PRIMARY KEY(dev, ino)) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS canon(hash BLOB PRIMARY KEY, path BLOB, ino INTEGER, dev INTEGER,
                                              size INTEGER, mtime INTEGER) WITHOUT ROWID;",
        )?;
        Ok(Index { db })
    }

    /// `files` must be sorted by (dev, ino), as the scan returns them.
    pub fn cached_hashes(&self, files: &[Entry], out: &mut [Option<Hash>]) -> rusqlite::Result<u64> {
        let mut st = self.db.prepare("SELECT dev, ino, size, mtime, hash FROM hashes ORDER BY dev, ino")?;
        let mut rows = st.query([])?;
        let (mut i, mut hits) = (0, 0);
        while i < files.len() {
            let Some(r) = rows.next()? else { break };
            let key: (i64, i64) = (r.get(0)?, r.get(1)?);
            let k = |e: &Entry| (i64::from(e.dev), e.ino as i64);
            while i < files.len() && k(&files[i]) < key {
                i += 1;
            }
            if i < files.len()
                && k(&files[i]) == key
                && r.get::<_, i64>(2)? as u64 == files[i].size
                && r.get::<_, i64>(3)? == files[i].mtime
                && let Ok(h) = r.get_ref(4)?.as_blob()?.try_into()
            {
                out[i] = Some(h);
                hits += 1;
            }
        }
        Ok(hits)
    }

    pub fn canonicals(&self, sorted: &[Hash]) -> rusqlite::Result<Vec<Option<FileState>>> {
        let mut out = vec![None; sorted.len()];
        let mut st = self.db.prepare("SELECT hash, path, ino, dev, size, mtime FROM canon ORDER BY hash")?;
        let mut rows = st.query([])?;
        let mut i = 0;
        while i < sorted.len() {
            let Some(r) = rows.next()? else { break };
            let Ok(h): Result<Hash, _> = r.get_ref(0)?.as_blob()?.try_into() else { continue };
            while i < sorted.len() && sorted[i] < h {
                i += 1;
            }
            if i < sorted.len() && sorted[i] == h {
                out[i] = Some(FileState {
                    path: PathBuf::from(OsStr::from_bytes(r.get_ref(1)?.as_blob()?)),
                    ino: r.get::<_, i64>(2)? as u64,
                    dev: r.get::<_, i64>(3)? as u32,
                    size: r.get::<_, i64>(4)? as u64,
                    mtime: r.get(5)?,
                });
            }
        }
        Ok(out)
    }

    pub fn save_hashes<'a>(&self, rows: impl Iterator<Item = (&'a Entry, &'a Hash)>) -> rusqlite::Result<()> {
        let tx = self.db.unchecked_transaction()?;
        {
            let mut put = tx.prepare("INSERT OR REPLACE INTO hashes(ino, dev, size, mtime, hash) VALUES(?,?,?,?,?)")?;
            for (e, h) in rows {
                put.execute(params![e.ino as i64, e.dev, e.size as i64, e.mtime, h.as_slice()])?;
            }
        }
        tx.commit()
    }

    pub fn save_canonicals(&self, rows: impl Iterator<Item = (Hash, FileState)>) -> rusqlite::Result<()> {
        let tx = self.db.unchecked_transaction()?;
        {
            let mut put = tx.prepare("INSERT OR REPLACE INTO canon(hash, path, ino, dev, size, mtime) VALUES(?,?,?,?,?,?)")?;
            for (h, f) in rows {
                put.execute(params![h.as_slice(), f.path.as_os_str().as_bytes(), f.ino as i64, f.dev, f.size as i64, f.mtime])?;
            }
        }
        tx.commit()
    }
}
