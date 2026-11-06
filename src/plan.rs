//! Groups files by content and decides which file each duplicate gets cloned from.
//!
//! Within a group, files sharing a clone id are one physical copy (a cluster). The clone
//! source is the stored canonical from the index if it is still valid, otherwise a member
//! of the biggest cluster, which needs the fewest clone operations.

use crate::apfs::{self, Hash};
use crate::index::Index;
use crate::replace::FileState;
use crate::scan::{Entry, Scan, UNKNOWN};
use rayon::prelude::*;

const BLOCK: u64 = 4096;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Src {
    Local(u32),
    External(u32),
}

#[derive(Default)]
pub struct Plan {
    pub todo: Vec<(Src, u32)>,
    pub external: Vec<FileState>,
    pub new_canonicals: Vec<u32>,
    pub duplicates: u64,
    pub duplicate_bytes: u64,
    pub already_shared: u64,
    pub expected_free: u64,
}

impl Plan {
    pub fn source(&self, sc: &Scan, s: Src) -> FileState {
        match s {
            Src::Local(i) => sc.state(i as usize),
            Src::External(k) => self.external[k as usize].clone(),
        }
    }
}

fn valid_external(f: &FileState, uid: u32) -> Option<u64> {
    let owned = apfs::lstat(&f.path).is_some_and(|st| st.st_uid == uid);
    if owned && f.unchanged() { apfs::clone_info(&f.path).map(|(_, id)| id) } else { None }
}

pub fn build(sc: &Scan, hashes: &[Option<Hash>], index: Option<&Index>, uid: u32) -> rusqlite::Result<Plan> {
    let f = |i: u32| -> &Entry { &sc.files[i as usize] };
    let mut by_hash: Vec<u32> = (0..sc.files.len() as u32).filter(|&i| hashes[i as usize].is_some()).collect();
    by_hash.par_sort_unstable_by_key(|&i| (hashes[i as usize], f(i).clone_id));
    let groups: Vec<&[u32]> = by_hash.chunk_by(|&a, &b| hashes[a as usize] == hashes[b as usize]).collect();
    let stored = match index {
        Some(ix) => ix.canonicals(&groups.iter().map(|g| hashes[g[0] as usize].unwrap()).collect::<Vec<_>>())?,
        None => vec![None; groups.len()],
    };

    let mut plan = Plan::default();
    for (g, stored) in groups.into_iter().zip(stored) {
        let known = stored.and_then(|c| {
            let local = g.iter().find(|&&i| f(i).ino == c.ino && f(i).dev == c.dev).copied();
            let clone_id = valid_external(&c, uid)?;
            Some(match local {
                Some(i) => (Src::Local(i), f(i).clone_id, f(i).dev),
                None => {
                    let dev = c.dev;
                    plan.external.push(c);
                    (Src::External(plan.external.len() as u32 - 1), clone_id, dev)
                }
            })
        });
        let (src, src_clone, src_dev) = known.unwrap_or_else(|| {
            let biggest = g.chunk_by(|&a, &b| f(a).clone_id == f(b).clone_id).max_by_key(|c| c.len()).unwrap();
            plan.new_canonicals.push(biggest[0]);
            (Src::Local(biggest[0]), f(biggest[0]).clone_id, f(biggest[0]).dev)
        });
        let members = g.len() as u64 - u64::from(matches!(src, Src::Local(_)));
        plan.duplicates += members;
        plan.duplicate_bytes += members * f(g[0]).size;
        for cluster in g.chunk_by(|&a, &b| f(a).clone_id == f(b).clone_id) {
            plan.add_cluster(cluster, &f, src, src_clone, src_dev);
        }
    }
    Ok(plan)
}

impl Plan {
    fn add_cluster<'a>(&mut self, cluster: &[u32], f: &impl Fn(u32) -> &'a Entry, src: Src, src_clone: u64, src_dev: u32) {
        let first = f(cluster[0]);
        let alloc = first.size.div_ceil(BLOCK) * BLOCK;
        let measured = cluster.iter().all(|&i| f(i).private != UNKNOWN);
        let shares_src = first.clone_id == src_clone && first.clone_id != 0;
        let (mut private, mut queued) = (0, 0);
        for &i in cluster {
            if src == Src::Local(i) || f(i).dev != src_dev {
                continue;
            }
            if shares_src && (f(i).private == 0 || f(i).private == UNKNOWN) {
                self.already_shared += 1;
                continue;
            }
            self.todo.push((src, i));
            private += if measured { f(i).private } else { alloc };
            queued += 1;
        }
        if queued > 0 {
            // clones of one another report private size 0 each, yet together hold one physical copy
            self.expected_free += match (shares_src, measured) {
                (true, _) => private,
                (false, true) => private.max(alloc),
                (false, false) => alloc,
            };
        }
    }
}
