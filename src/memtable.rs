use crate::entry::Entry;
use crate::format::get_ts;
use crate::util::Comparator;
use crate::value::{self, Value};
use crate::wal::Wal;
use crate::AgateOptions;
use crate::Result;
use bytes::Bytes;
use skiplist::Skiplist;
use std::collections::VecDeque;
use std::mem::{self, ManuallyDrop, MaybeUninit};

use parking_lot::RwLock;
use std::ptr;

const MEMTABLE_VIEW_MAX: usize = 20;

/// MemTableCore guards WAL and max_version.
/// These data will only be modified on memtable put.
/// Therefore, separating wal and max_version enables
/// concurrent read/write of MemTable.
struct MemTableCore {
    wal: Option<Wal>,
    max_version: u64,
}

pub struct MemTable {
    pub(crate) skl: Skiplist<Comparator>,
    opt: AgateOptions,
    core: RwLock<MemTableCore>,
}

impl MemTable {
    pub fn new(skl: Skiplist<Comparator>, wal: Option<Wal>, opt: AgateOptions) -> Self {
        Self {
            skl,
            opt,
            core: RwLock::new(MemTableCore {
                wal,
                max_version: 0,
            }),
        }
    }

    pub fn update_skip_list(&self) -> Result<()> {
        let mut core = self.core.write();
        let mut max_version = core.max_version;
        if let Some(ref mut wal) = core.wal {
            let mut it = wal.iter()?;
            while let Some(entry) = it.next()? {
                let ts = get_ts(entry.key);
                if ts > max_version {
                    max_version = ts;
                }
                let v = Value {
                    value: Bytes::copy_from_slice(entry.value),
                    meta: entry.meta,
                    user_meta: entry.user_meta,
                    expires_at: entry.expires_at,
                    version: 0,
                };
                self.skl.put(Bytes::copy_from_slice(entry.key), v);
            }
        }
        core.max_version = max_version;
        Ok(())
    }

    pub fn put(&self, key: Bytes, value: Value) -> Result<()> {
        let mut core = self.core.write();
        if let Some(ref mut wal) = core.wal {
            let entry = Entry {
                key: key.clone(),
                value: value.value.clone(),
                expires_at: value.expires_at,
                version: value.version,
                user_meta: value.user_meta,
                meta: value.meta,
            };
            wal.write_entry(&entry)?;
        }

        // only insert finish marker in WAL
        if value.meta & value::VALUE_FIN_TXN != 0 {
            return Ok(());
        }

        // write to skiplist
        let ts = get_ts(&key);
        self.skl.put(key, value);

        // update max version
        core.max_version = ts;

        Ok(())
    }

    pub fn sync_wal(&self) -> Result<()> {
        let mut core = self.core.write();
        if let Some(ref mut wal) = core.wal {
            wal.sync()?;
        }
        Ok(())
    }
}

pub struct MemTablesView {
    tables: ManuallyDrop<[Skiplist<Comparator>; MEMTABLE_VIEW_MAX]>,
    len: usize,
}

impl MemTablesView {
    pub fn tables(&self) -> &[Skiplist<Comparator>] {
        &self.tables[0..self.len]
    }
}

impl Drop for MemTablesView {
    fn drop(&mut self) {
        for i in 0..self.len {
            unsafe {
                ptr::drop_in_place(&mut self.tables[i]);
            }
        }
    }
}

pub struct MemTables {
    mutable: MemTable,
    immutable: VecDeque<MemTable>,
}

impl MemTables {
    pub(crate) fn new(mutable: MemTable, immutable: VecDeque<MemTable>) -> Self {
        Self { mutable, immutable }
    }

    /// Get view of all current memtables
    pub fn view(&self) -> MemTablesView {
        // Maybe flush is better.
        assert!(self.immutable.len() + 1 <= MEMTABLE_VIEW_MAX);
        let mut array: [MaybeUninit<Skiplist<Comparator>>; MEMTABLE_VIEW_MAX] =
            unsafe { MaybeUninit::uninit().assume_init() };
        array[0] = MaybeUninit::new(self.mutable.skl.clone());
        for (i, s) in self.immutable.iter().enumerate() {
            array[i + 1] = MaybeUninit::new(s.skl.clone());
        }
        MemTablesView {
            tables: unsafe { ManuallyDrop::new(mem::transmute(array)) },
            len: self.immutable.len() + 1,
        }
    }

    /// Get mutable memtable
    pub fn table_mut(&self) -> &MemTable {
        &self.mutable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skiplist::FixedLengthSuffixComparator;

    fn get_comparator() -> FixedLengthSuffixComparator {
        FixedLengthSuffixComparator::new(0)
    }

    fn get_memtable(data: Vec<(String, String)>) -> MemTable {
        let skl = Skiplist::with_capacity(get_comparator(), 4 * 1024 * 1024);
        for (k, v) in data {
            assert!(skl.put(k, v).is_none());
        }
        MemTable::new(skl, None, AgateOptions::default())
    }

    #[test]
    fn test_memtable_put() {
        let mut data = vec![];
        for i in 0..1000 {
            data.push((i.to_string(), i.to_string()));
        }
        let (d1, dx) = data.split_at(250);
        let (d2, dx) = dx.split_at(250);
        let (d3, dx) = dx.split_at(250);
        let (d4, _) = dx.split_at(250);
        let mem_tables = MemTables {
            mutable: get_memtable(d1.to_vec()),
            immutable: VecDeque::from(
                [d2, d3, d4]
                    .iter()
                    .map(|x| get_memtable(x.to_vec()))
                    .collect::<Vec<MemTable>>(),
            ),
        };
        let view = mem_tables.view();
        for k in 0..4 {
            for i in k * 250..(k + 1) * 250 {
                assert_eq!(
                    view.tables()[k].get(i.to_string().as_bytes()),
                    Some(&Bytes::from(i.to_string()))
                );
            }
        }
    }
}
