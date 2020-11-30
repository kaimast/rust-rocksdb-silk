// Copyright 2020 Tyler Neely
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

use crate::{
    ffi,
    ffi_util::{from_cstr, opt_bytes_to_ptr, raw_data, to_cpath},
    ColumnFamily, ColumnFamilyDescriptor, CompactOptions, DBIterator, DBPinnableSlice,
    DBRawIterator, Direction, Error, FlushOptions, IngestExternalFileOptions,
    IteratorMode, Options, ReadOptions, Snapshot, WriteBatch, WriteOptions,
    DEFAULT_COLUMN_FAMILY_NAME,
};

use libc::{self, c_char, c_int, c_uchar, c_void, size_t};
use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::fmt;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::ptr;
use std::slice;
use std::str;

/// A RocksDB database.
///
/// See crate level documentation for a simple usage example.
pub struct DB {
    pub(crate) inner: *mut ffi::rocksdb_silk_t,
    cfs: BTreeMap<String, ColumnFamily>,
    path: PathBuf,
}

// Safety note: auto-implementing Send on most db-related types is prevented by the inner FFI
// pointer. In most cases, however, this pointer is Send-safe because it is never aliased and
// rocksdb internally does not rely on thread-local information for its user-exposed types.
unsafe impl Send for DB {}

// Sync is similarly safe for many types because they do not expose interior mutability, and their
// use within the rocksdb library is generally behind a const reference
unsafe impl Sync for DB {}

// Specifies whether open DB for read only.
enum AccessType {
    ReadWrite,
    ReadOnly { error_if_log_file_exist: bool }
}

impl DB {
    /// Opens a database with default options.
    pub fn open_default<P: AsRef<Path>>(path: P) -> Result<DB, Error> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        DB::open(&opts, path)
    }

    /// Opens the database with the specified options.
    pub fn open<P: AsRef<Path>>(opts: &Options, path: P) -> Result<DB, Error> {
        DB::open_cf(opts, path, None::<&str>)
    }

    /// Opens the database for read only with the specified options.
    pub fn open_for_read_only<P: AsRef<Path>>(
        opts: &Options,
        path: P,
        error_if_log_file_exist: bool,
    ) -> Result<DB, Error> {
        DB::open_cf_for_read_only(opts, path, None::<&str>, error_if_log_file_exist)
    }

    /// Opens a database with the given database options and column family names.
    ///
    /// Column families opened using this function will be created with default `Options`.
    pub fn open_cf<P, I, N>(opts: &Options, path: P, cfs: I) -> Result<DB, Error>
    where
        P: AsRef<Path>,
        I: IntoIterator<Item = N>,
        N: AsRef<str>,
    {
        let cfs = cfs
            .into_iter()
            .map(|name| ColumnFamilyDescriptor::new(name.as_ref(), Options::default()));

        DB::open_cf_descriptors_internal(opts, path, cfs, &AccessType::ReadWrite)
    }

    /// Opens a database for read only with the given database options and column family names.
    pub fn open_cf_for_read_only<P, I, N>(
        opts: &Options,
        path: P,
        cfs: I,
        error_if_log_file_exist: bool,
    ) -> Result<DB, Error>
    where
        P: AsRef<Path>,
        I: IntoIterator<Item = N>,
        N: AsRef<str>,
    {
        let cfs = cfs
            .into_iter()
            .map(|name| ColumnFamilyDescriptor::new(name.as_ref(), Options::default()));

        DB::open_cf_descriptors_internal(
            opts,
            path,
            cfs,
            &AccessType::ReadOnly {
                error_if_log_file_exist,
            },
        )
    }

    /// Opens a database with the given database options and column family descriptors.
    pub fn open_cf_descriptors<P, I>(opts: &Options, path: P, cfs: I) -> Result<DB, Error>
    where
        P: AsRef<Path>,
        I: IntoIterator<Item = ColumnFamilyDescriptor>,
    {
        DB::open_cf_descriptors_internal(opts, path, cfs, &AccessType::ReadWrite)
    }

    /// Internal implementation for opening RocksDB.
    fn open_cf_descriptors_internal<P, I>(
        opts: &Options,
        path: P,
        cfs: I,
        access_type: &AccessType,
    ) -> Result<DB, Error>
    where
        P: AsRef<Path>,
        I: IntoIterator<Item = ColumnFamilyDescriptor>,
    {
        let cfs: Vec<_> = cfs.into_iter().collect();

        let cpath = to_cpath(&path)?;

        if let Err(e) = fs::create_dir_all(&path) {
            return Err(Error::new(format!(
                "Failed to create RocksDB directory: `{:?}`.",
                e
            )));
        }

        let db: *mut ffi::rocksdb_silk_t;
        let mut cf_map = BTreeMap::new();

        if cfs.is_empty() {
            db = DB::open_raw(opts, &cpath, access_type)?;
        } else {
            let mut cfs_v = cfs;
            // Always open the default column family.
            if !cfs_v.iter().any(|cf| cf.name == DEFAULT_COLUMN_FAMILY_NAME) {
                cfs_v.push(ColumnFamilyDescriptor {
                    name: String::from(DEFAULT_COLUMN_FAMILY_NAME),
                    options: Options::default(),
                });
            }
            // We need to store our CStrings in an intermediate vector
            // so that their pointers remain valid.
            let c_cfs: Vec<CString> = cfs_v
                .iter()
                .map(|cf| CString::new(cf.name.as_bytes()).unwrap())
                .collect();

            let mut cfnames: Vec<_> = c_cfs.iter().map(|cf| cf.as_ptr()).collect();

            // These handles will be populated by DB.
            let mut cfhandles: Vec<_> = cfs_v.iter().map(|_| ptr::null_mut()).collect();

            let mut cfopts: Vec<_> = cfs_v
                .iter()
                .map(|cf| cf.options.inner as *const _)
                .collect();

            db = DB::open_cf_raw(
                opts,
                &cpath,
                &cfs_v,
                &mut cfnames,
                &mut cfopts,
                &mut cfhandles,
                &access_type,
            )?;
            for handle in &cfhandles {
                if handle.is_null() {
                    return Err(Error::new(
                        "Received null column family handle from DB.".to_owned(),
                    ));
                }
            }

            for (cf_desc, inner) in cfs_v.iter().zip(cfhandles) {
                cf_map.insert(cf_desc.name.clone(), ColumnFamily { inner });
            }
        }

        if db.is_null() {
            return Err(Error::new("Could not initialize database.".to_owned()));
        }

        Ok(DB {
            inner: db,
            cfs: cf_map,
            path: path.as_ref().to_path_buf(),
        })
    }

    fn open_raw(
        opts: &Options,
        cpath: &CString,
        access_type: &AccessType,
    ) -> Result<*mut ffi::rocksdb_silk_t, Error> {
        let db = unsafe {
            match *access_type {
                AccessType::ReadOnly {
                    error_if_log_file_exist,
                } => ffi_try!(ffi::rocksdb_silk_open_for_read_only(
                    opts.inner,
                    cpath.as_ptr() as *const _,
                    error_if_log_file_exist as c_uchar,
                )),
                AccessType::ReadWrite => {
                    ffi_try!(ffi::rocksdb_silk_open(opts.inner, cpath.as_ptr() as *const _))
                }
            }
        };
        Ok(db)
    }

    fn open_cf_raw(
        opts: &Options,
        cpath: &CString,
        cfs_v: &[ColumnFamilyDescriptor],
        cfnames: &mut [*const c_char],
        cfopts: &mut [*const ffi::rocksdb_silk_options_t],
        cfhandles: &mut Vec<*mut ffi::rocksdb_silk_column_family_handle_t>,
        access_type: &AccessType,
    ) -> Result<*mut ffi::rocksdb_silk_t, Error> {
        let db = unsafe {
            match *access_type {
                AccessType::ReadOnly {
                    error_if_log_file_exist,
                } => ffi_try!(ffi::rocksdb_silk_open_for_read_only_column_families(
                    opts.inner,
                    cpath.as_ptr(),
                    cfs_v.len() as c_int,
                    cfnames.as_mut_ptr(),
                    cfopts.as_mut_ptr(),
                    cfhandles.as_mut_ptr(),
                    error_if_log_file_exist as c_uchar,
                )),
                AccessType::ReadWrite => ffi_try!(ffi::rocksdb_silk_open_column_families(
                    opts.inner,
                    cpath.as_ptr(),
                    cfs_v.len() as c_int,
                    cfnames.as_mut_ptr(),
                    cfopts.as_mut_ptr(),
                    cfhandles.as_mut_ptr(),
                ))
            }
        };
        Ok(db)
    }

    pub fn list_cf<P: AsRef<Path>>(opts: &Options, path: P) -> Result<Vec<String>, Error> {
        let cpath = to_cpath(path)?;
        let mut length = 0;

        unsafe {
            let ptr = ffi_try!(ffi::rocksdb_silk_list_column_families(
                opts.inner,
                cpath.as_ptr() as *const _,
                &mut length,
            ));

            let vec = slice::from_raw_parts(ptr, length)
                .iter()
                .map(|ptr| CStr::from_ptr(*ptr).to_string_lossy().into_owned())
                .collect();
            ffi::rocksdb_silk_list_column_families_destroy(ptr, length);
            Ok(vec)
        }
    }

    pub fn destroy<P: AsRef<Path>>(opts: &Options, path: P) -> Result<(), Error> {
        let cpath = to_cpath(path)?;
        unsafe {
            ffi_try!(ffi::rocksdb_silk_destroy_db(opts.inner, cpath.as_ptr()));
        }
        Ok(())
    }

    pub fn repair<P: AsRef<Path>>(opts: &Options, path: P) -> Result<(), Error> {
        let cpath = to_cpath(path)?;
        unsafe {
            ffi_try!(ffi::rocksdb_silk_repair_db(opts.inner, cpath.as_ptr()));
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path.as_path()
    }

    /// Flushes database memtables to SST files on the disk.
    pub fn flush_opt(&self, flushopts: &FlushOptions) -> Result<(), Error> {
        unsafe {
            ffi_try!(ffi::rocksdb_silk_flush(self.inner, flushopts.inner));
        }
        Ok(())
    }

    /// Flushes database memtables to SST files on the disk using default options.
    pub fn flush(&self) -> Result<(), Error> {
        self.flush_opt(&FlushOptions::default())
    }

    pub fn write_opt(&self, batch: WriteBatch, writeopts: &WriteOptions) -> Result<(), Error> {
        unsafe {
            ffi_try!(ffi::rocksdb_silk_write(self.inner, writeopts.inner, batch.inner));
        }
        Ok(())
    }

    pub fn write(&self, batch: WriteBatch) -> Result<(), Error> {
        self.write_opt(batch, &WriteOptions::default())
    }

    pub fn write_without_wal(&self, batch: WriteBatch) -> Result<(), Error> {
        let mut wo = WriteOptions::new();
        wo.disable_wal(true);
        self.write_opt(batch, &wo)
    }

    /// Return the bytes associated with a key value with read options. If you only intend to use
    /// the vector returned temporarily, consider using [`get_pinned_opt`](#method.get_pinned_opt)
    /// to avoid unnecessary memory copy.
    pub fn get_opt<K: AsRef<[u8]>>(
        &self,
        key: K,
        readopts: &ReadOptions,
    ) -> Result<Option<Vec<u8>>, Error> {
        self.get_pinned_opt(key, readopts)
            .map(|x| x.map(|v| v.as_ref().to_vec()))
    }

    /// Return the bytes associated with a key value. If you only intend to use the vector returned
    /// temporarily, consider using [`get_pinned`](#method.get_pinned) to avoid unnecessary memory
    /// copy.
    pub fn get<K: AsRef<[u8]>>(&self, key: K) -> Result<Option<Vec<u8>>, Error> {
        self.get_opt(key.as_ref(), &ReadOptions::default())
    }

    /// Return the bytes associated with a key value and the given column family with read options.
    /// If you only intend to use the vector returned temporarily, consider using
    /// [`get_pinned_cf_opt`](#method.get_pinned_cf_opt) to avoid unnecessary memory.
    pub fn get_cf_opt<K: AsRef<[u8]>>(
        &self,
        cf: &ColumnFamily,
        key: K,
        readopts: &ReadOptions,
    ) -> Result<Option<Vec<u8>>, Error> {
        self.get_pinned_cf_opt(cf, key, readopts)
            .map(|x| x.map(|v| v.as_ref().to_vec()))
    }

    /// Return the bytes associated with a key value and the given column family. If you only
    /// intend to use the vector returned temporarily, consider using
    /// [`get_pinned_cf`](#method.get_pinned_cf) to avoid unnecessary memory.
    pub fn get_cf<K: AsRef<[u8]>>(
        &self,
        cf: &ColumnFamily,
        key: K,
    ) -> Result<Option<Vec<u8>>, Error> {
        self.get_cf_opt(cf, key.as_ref(), &ReadOptions::default())
    }

    /// Return the value associated with a key using RocksDB's PinnableSlice
    /// so as to avoid unnecessary memory copy.
    pub fn get_pinned_opt<K: AsRef<[u8]>>(
        &self,
        key: K,
        readopts: &ReadOptions,
    ) -> Result<Option<DBPinnableSlice>, Error> {
        if readopts.inner.is_null() {
            return Err(Error::new(
                "Unable to create RocksDB read options. This is a fairly trivial call, and its \
                 failure may be indicative of a mis-compiled or mis-loaded RocksDB library."
                    .to_owned(),
            ));
        }

        let key = key.as_ref();
        unsafe {
            let val = ffi_try!(ffi::rocksdb_silk_get_pinned(
                self.inner,
                readopts.inner,
                key.as_ptr() as *const c_char,
                key.len() as size_t,
            ));
            if val.is_null() {
                Ok(None)
            } else {
                Ok(Some(DBPinnableSlice::from_c(val)))
            }
        }
    }

    /// Return the value associated with a key using RocksDB's PinnableSlice
    /// so as to avoid unnecessary memory copy. Similar to get_pinned_opt but
    /// leverages default options.
    pub fn get_pinned<K: AsRef<[u8]>>(&self, key: K) -> Result<Option<DBPinnableSlice>, Error> {
        self.get_pinned_opt(key, &ReadOptions::default())
    }

    /// Return the value associated with a key using RocksDB's PinnableSlice
    /// so as to avoid unnecessary memory copy. Similar to get_pinned_opt but
    /// allows specifying ColumnFamily
    pub fn get_pinned_cf_opt<K: AsRef<[u8]>>(
        &self,
        cf: &ColumnFamily,
        key: K,
        readopts: &ReadOptions,
    ) -> Result<Option<DBPinnableSlice>, Error> {
        if readopts.inner.is_null() {
            return Err(Error::new(
                "Unable to create RocksDB read options. This is a fairly trivial call, and its \
                 failure may be indicative of a mis-compiled or mis-loaded RocksDB library."
                    .to_owned(),
            ));
        }

        let key = key.as_ref();
        unsafe {
            let val = ffi_try!(ffi::rocksdb_silk_get_pinned_cf(
                self.inner,
                readopts.inner,
                cf.inner,
                key.as_ptr() as *const c_char,
                key.len() as size_t,
            ));
            if val.is_null() {
                Ok(None)
            } else {
                Ok(Some(DBPinnableSlice::from_c(val)))
            }
        }
    }

    /// Return the value associated with a key using RocksDB's PinnableSlice
    /// so as to avoid unnecessary memory copy. Similar to get_pinned_cf_opt but
    /// leverages default options.
    pub fn get_pinned_cf<K: AsRef<[u8]>>(
        &self,
        cf: &ColumnFamily,
        key: K,
    ) -> Result<Option<DBPinnableSlice>, Error> {
        self.get_pinned_cf_opt(cf, key, &ReadOptions::default())
    }

    pub fn create_cf<N: AsRef<str>>(&mut self, name: N, opts: &Options) -> Result<(), Error> {
        let cf_name = if let Ok(c) = CString::new(name.as_ref().as_bytes()) {
            c
        } else {
            return Err(Error::new(
                "Failed to convert path to CString when creating cf".to_owned(),
            ));
        };
        unsafe {
            let inner = ffi_try!(ffi::rocksdb_silk_create_column_family(
                self.inner,
                opts.inner,
                cf_name.as_ptr(),
            ));

            self.cfs
                .insert(name.as_ref().to_string(), ColumnFamily { inner });
        };
        Ok(())
    }

    pub fn drop_cf(&mut self, name: &str) -> Result<(), Error> {
        if let Some(cf) = self.cfs.remove(name) {
            unsafe {
                ffi_try!(ffi::rocksdb_silk_drop_column_family(self.inner, cf.inner));
            }
            Ok(())
        } else {
            Err(Error::new(format!("Invalid column family: {}", name)))
        }
    }

    /// Return the underlying column family handle.
    pub fn cf_handle(&self, name: &str) -> Option<&ColumnFamily> {
        self.cfs.get(name)
    }

    pub fn iterator<'a: 'b, 'b>(&'a self, mode: IteratorMode) -> DBIterator<'b> {
        let readopts = ReadOptions::default();
        self.iterator_opt(mode, readopts)
    }

    pub fn iterator_opt<'a: 'b, 'b>(
        &'a self,
        mode: IteratorMode,
        readopts: ReadOptions,
    ) -> DBIterator<'b> {
        DBIterator::new(self, readopts, mode)
    }

    /// Opens an iterator using the provided ReadOptions.
    /// This is used when you want to iterate over a specific ColumnFamily with a modified ReadOptions
    pub fn iterator_cf_opt<'a: 'b, 'b>(
        &'a self,
        cf_handle: &ColumnFamily,
        readopts: ReadOptions,
        mode: IteratorMode,
    ) -> DBIterator<'b> {
        DBIterator::new_cf(self, cf_handle, readopts, mode)
    }

    /// Opens an iterator with `set_total_order_seek` enabled.
    /// This must be used to iterate across prefixes when `set_memtable_factory` has been called
    /// with a Hash-based implementation.
    pub fn full_iterator<'a: 'b, 'b>(&'a self, mode: IteratorMode) -> DBIterator<'b> {
        let mut opts = ReadOptions::default();
        opts.set_total_order_seek(true);
        DBIterator::new(self, opts, mode)
    }

    pub fn prefix_iterator<'a: 'b, 'b, P: AsRef<[u8]>>(&'a self, prefix: P) -> DBIterator<'b> {
        let opts = ReadOptions::default();
        DBIterator::new(
            self,
            opts,
            IteratorMode::From(prefix.as_ref(), Direction::Forward),
        )
    }

    pub fn iterator_cf<'a: 'b, 'b>(
        &'a self,
        cf_handle: &ColumnFamily,
        mode: IteratorMode,
    ) -> DBIterator<'b> {
        let opts = ReadOptions::default();
        DBIterator::new_cf(self, cf_handle, opts, mode)
    }

    pub fn full_iterator_cf<'a: 'b, 'b>(
        &'a self,
        cf_handle: &ColumnFamily,
        mode: IteratorMode,
    ) -> DBIterator<'b> {
        let mut opts = ReadOptions::default();
        opts.set_total_order_seek(true);
        DBIterator::new_cf(self, cf_handle, opts, mode)
    }

    pub fn prefix_iterator_cf<'a: 'b, 'b, P: AsRef<[u8]>>(
        &'a self,
        cf_handle: &ColumnFamily,
        prefix: P,
    ) -> DBIterator<'b> {
        let opts = ReadOptions::default();
        DBIterator::new_cf(
            self,
            cf_handle,
            opts,
            IteratorMode::From(prefix.as_ref(), Direction::Forward),
        )
    }

    /// Opens a raw iterator over the database, using the default read options
    pub fn raw_iterator<'a: 'b, 'b>(&'a self) -> DBRawIterator<'b> {
        let opts = ReadOptions::default();
        DBRawIterator::new(self, opts)
    }

    /// Opens a raw iterator over the given column family, using the default read options
    pub fn raw_iterator_cf<'a: 'b, 'b>(&'a self, cf_handle: &ColumnFamily) -> DBRawIterator<'b> {
        let opts = ReadOptions::default();
        DBRawIterator::new_cf(self, cf_handle, opts)
    }

    /// Opens a raw iterator over the database, using the given read options
    pub fn raw_iterator_opt<'a: 'b, 'b>(&'a self, readopts: ReadOptions) -> DBRawIterator<'b> {
        DBRawIterator::new(self, readopts)
    }

    /// Opens a raw iterator over the given column family, using the given read options
    pub fn raw_iterator_cf_opt<'a: 'b, 'b>(
        &'a self,
        cf_handle: &ColumnFamily,
        readopts: ReadOptions,
    ) -> DBRawIterator<'b> {
        DBRawIterator::new_cf(self, cf_handle, readopts)
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot::new(self)
    }

    pub fn put_opt<K, V>(&self, key: K, value: V, writeopts: &WriteOptions) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        let key = key.as_ref();
        let value = value.as_ref();

        unsafe {
            ffi_try!(ffi::rocksdb_silk_put(
                self.inner,
                writeopts.inner,
                key.as_ptr() as *const c_char,
                key.len() as size_t,
                value.as_ptr() as *const c_char,
                value.len() as size_t,
            ));
            Ok(())
        }
    }

    pub fn put_cf_opt<K, V>(
        &self,
        cf: &ColumnFamily,
        key: K,
        value: V,
        writeopts: &WriteOptions,
    ) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        let key = key.as_ref();
        let value = value.as_ref();

        unsafe {
            ffi_try!(ffi::rocksdb_silk_put_cf(
                self.inner,
                writeopts.inner,
                cf.inner,
                key.as_ptr() as *const c_char,
                key.len() as size_t,
                value.as_ptr() as *const c_char,
                value.len() as size_t,
            ));
            Ok(())
        }
    }

    pub fn merge_opt<K, V>(&self, key: K, value: V, writeopts: &WriteOptions) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        let key = key.as_ref();
        let value = value.as_ref();

        unsafe {
            ffi_try!(ffi::rocksdb_silk_merge(
                self.inner,
                writeopts.inner,
                key.as_ptr() as *const c_char,
                key.len() as size_t,
                value.as_ptr() as *const c_char,
                value.len() as size_t,
            ));
            Ok(())
        }
    }

    pub fn merge_cf_opt<K, V>(
        &self,
        cf: &ColumnFamily,
        key: K,
        value: V,
        writeopts: &WriteOptions,
    ) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        let key = key.as_ref();
        let value = value.as_ref();

        unsafe {
            ffi_try!(ffi::rocksdb_silk_merge_cf(
                self.inner,
                writeopts.inner,
                cf.inner,
                key.as_ptr() as *const c_char,
                key.len() as size_t,
                value.as_ptr() as *const c_char,
                value.len() as size_t,
            ));
            Ok(())
        }
    }

    pub fn delete_opt<K: AsRef<[u8]>>(
        &self,
        key: K,
        writeopts: &WriteOptions,
    ) -> Result<(), Error> {
        let key = key.as_ref();

        unsafe {
            ffi_try!(ffi::rocksdb_silk_delete(
                self.inner,
                writeopts.inner,
                key.as_ptr() as *const c_char,
                key.len() as size_t,
            ));
            Ok(())
        }
    }

    pub fn delete_cf_opt<K: AsRef<[u8]>>(
        &self,
        cf: &ColumnFamily,
        key: K,
        writeopts: &WriteOptions,
    ) -> Result<(), Error> {
        let key = key.as_ref();

        unsafe {
            ffi_try!(ffi::rocksdb_silk_delete_cf(
                self.inner,
                writeopts.inner,
                cf.inner,
                key.as_ptr() as *const c_char,
                key.len() as size_t,
            ));
            Ok(())
        }
    }

    pub fn put<K, V>(&self, key: K, value: V) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        self.put_opt(key.as_ref(), value.as_ref(), &WriteOptions::default())
    }

    pub fn put_cf<K, V>(&self, cf: &ColumnFamily, key: K, value: V) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        self.put_cf_opt(cf, key.as_ref(), value.as_ref(), &WriteOptions::default())
    }

    pub fn merge<K, V>(&self, key: K, value: V) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        self.merge_opt(key.as_ref(), value.as_ref(), &WriteOptions::default())
    }

    pub fn merge_cf<K, V>(&self, cf: &ColumnFamily, key: K, value: V) -> Result<(), Error>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        self.merge_cf_opt(cf, key.as_ref(), value.as_ref(), &WriteOptions::default())
    }

    pub fn delete<K: AsRef<[u8]>>(&self, key: K) -> Result<(), Error> {
        self.delete_opt(key.as_ref(), &WriteOptions::default())
    }

    pub fn delete_cf<K: AsRef<[u8]>>(&self, cf: &ColumnFamily, key: K) -> Result<(), Error> {
        self.delete_cf_opt(cf, key.as_ref(), &WriteOptions::default())
    }

    /// Runs a manual compaction on the Range of keys given. This is not likely to be needed for typical usage.
    pub fn compact_range<S: AsRef<[u8]>, E: AsRef<[u8]>>(&self, start: Option<S>, end: Option<E>) {
        unsafe {
            let start = start.as_ref().map(AsRef::as_ref);
            let end = end.as_ref().map(AsRef::as_ref);

            ffi::rocksdb_silk_compact_range(
                self.inner,
                opt_bytes_to_ptr(start),
                start.map_or(0, |s| s.len()) as size_t,
                opt_bytes_to_ptr(end),
                end.map_or(0, |e| e.len()) as size_t,
            );
        }
    }

    /// Same as `compact_range` but with custom options.
    pub fn compact_range_opt<S: AsRef<[u8]>, E: AsRef<[u8]>>(
        &self,
        start: Option<S>,
        end: Option<E>,
        opts: &CompactOptions,
    ) {
        unsafe {
            let start = start.as_ref().map(AsRef::as_ref);
            let end = end.as_ref().map(AsRef::as_ref);

            ffi::rocksdb_silk_compact_range_opt(
                self.inner,
                opts.inner,
                opt_bytes_to_ptr(start),
                start.map_or(0, |s| s.len()) as size_t,
                opt_bytes_to_ptr(end),
                end.map_or(0, |e| e.len()) as size_t,
            );
        }
    }

    /// Runs a manual compaction on the Range of keys given on the
    /// given column family. This is not likely to be needed for typical usage.
    pub fn compact_range_cf<S: AsRef<[u8]>, E: AsRef<[u8]>>(
        &self,
        cf: &ColumnFamily,
        start: Option<S>,
        end: Option<E>,
    ) {
        unsafe {
            let start = start.as_ref().map(AsRef::as_ref);
            let end = end.as_ref().map(AsRef::as_ref);

            ffi::rocksdb_silk_compact_range_cf(
                self.inner,
                cf.inner,
                opt_bytes_to_ptr(start),
                start.map_or(0, |s| s.len()) as size_t,
                opt_bytes_to_ptr(end),
                end.map_or(0, |e| e.len()) as size_t,
            );
        }
    }

    /// Same as `compact_range_cf` but with custom options.
    pub fn compact_range_cf_opt<S: AsRef<[u8]>, E: AsRef<[u8]>>(
        &self,
        cf: &ColumnFamily,
        start: Option<S>,
        end: Option<E>,
        opts: &CompactOptions,
    ) {
        unsafe {
            let start = start.as_ref().map(AsRef::as_ref);
            let end = end.as_ref().map(AsRef::as_ref);

            ffi::rocksdb_silk_compact_range_cf_opt(
                self.inner,
                cf.inner,
                opts.inner,
                opt_bytes_to_ptr(start),
                start.map_or(0, |s| s.len()) as size_t,
                opt_bytes_to_ptr(end),
                end.map_or(0, |e| e.len()) as size_t,
            );
        }
    }

    pub fn set_options(&self, opts: &[(&str, &str)]) -> Result<(), Error> {
        let copts = convert_options(opts)?;
        let cnames: Vec<*const c_char> = copts.iter().map(|opt| opt.0.as_ptr()).collect();
        let cvalues: Vec<*const c_char> = copts.iter().map(|opt| opt.1.as_ptr()).collect();
        let count = opts.len() as i32;
        unsafe {
            ffi_try!(ffi::rocksdb_silk_set_options(
                self.inner,
                count,
                cnames.as_ptr(),
                cvalues.as_ptr(),
            ));
        }
        Ok(())
    }

    /// Retrieves a RocksDB property by name.
    ///
    /// Full list of properties could be find
    /// [here](https://github.com/facebook/rocksdb/blob/08809f5e6cd9cc4bc3958dd4d59457ae78c76660/include/rocksdb/db.h#L428-L634).
    pub fn property_value(&self, name: &str) -> Result<Option<String>, Error> {
        let prop_name = match CString::new(name) {
            Ok(c) => c,
            Err(e) => {
                return Err(Error::new(format!(
                    "Failed to convert property name to CString: {}",
                    e
                )));
            }
        };

        unsafe {
            let value = ffi::rocksdb_silk_property_value(self.inner, prop_name.as_ptr());
            if value.is_null() {
                return Ok(None);
            }

            let str_value = match CStr::from_ptr(value).to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => {
                    return Err(Error::new(format!(
                        "Failed to convert property value to string: {}",
                        e
                    )));
                }
            };

            libc::free(value as *mut c_void);
            Ok(Some(str_value))
        }
    }

    /// Retrieves a RocksDB property by name, for a specific column family.
    ///
    /// Full list of properties could be find
    /// [here](https://github.com/facebook/rocksdb/blob/08809f5e6cd9cc4bc3958dd4d59457ae78c76660/include/rocksdb/db.h#L428-L634).
    pub fn property_value_cf(
        &self,
        cf: &ColumnFamily,
        name: &str,
    ) -> Result<Option<String>, Error> {
        let prop_name = match CString::new(name) {
            Ok(c) => c,
            Err(e) => {
                return Err(Error::new(format!(
                    "Failed to convert property name to CString: {}",
                    e
                )));
            }
        };

        unsafe {
            let value = ffi::rocksdb_silk_property_value_cf(self.inner, cf.inner, prop_name.as_ptr());
            if value.is_null() {
                return Ok(None);
            }

            let str_value = match CStr::from_ptr(value).to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => {
                    return Err(Error::new(format!(
                        "Failed to convert property value to string: {}",
                        e
                    )));
                }
            };

            libc::free(value as *mut c_void);
            Ok(Some(str_value))
        }
    }

    /// Retrieves a RocksDB property and casts it to an integer.
    ///
    /// Full list of properties that return int values could be find
    /// [here](https://github.com/facebook/rocksdb/blob/08809f5e6cd9cc4bc3958dd4d59457ae78c76660/include/rocksdb/db.h#L654-L689).
    pub fn property_int_value(&self, name: &str) -> Result<Option<u64>, Error> {
        match self.property_value(name) {
            Ok(Some(value)) => match value.parse::<u64>() {
                Ok(int_value) => Ok(Some(int_value)),
                Err(e) => Err(Error::new(format!(
                    "Failed to convert property value to int: {}",
                    e
                ))),
            },
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Retrieves a RocksDB property for a specific column family and casts it to an integer.
    ///
    /// Full list of properties that return int values could be find
    /// [here](https://github.com/facebook/rocksdb/blob/08809f5e6cd9cc4bc3958dd4d59457ae78c76660/include/rocksdb/db.h#L654-L689).
    pub fn property_int_value_cf(
        &self,
        cf: &ColumnFamily,
        name: &str,
    ) -> Result<Option<u64>, Error> {
        match self.property_value_cf(cf, name) {
            Ok(Some(value)) => match value.parse::<u64>() {
                Ok(int_value) => Ok(Some(int_value)),
                Err(e) => Err(Error::new(format!(
                    "Failed to convert property value to int: {}",
                    e
                ))),
            },
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Loads a list of external SST files created with SstFileWriter into the DB with default opts
    pub fn ingest_external_file<P: AsRef<Path>>(&self, paths: Vec<P>) -> Result<(), Error> {
        let opts = IngestExternalFileOptions::default();
        self.ingest_external_file_opts(&opts, paths)
    }

    /// Loads a list of external SST files created with SstFileWriter into the DB
    pub fn ingest_external_file_opts<P: AsRef<Path>>(
        &self,
        opts: &IngestExternalFileOptions,
        paths: Vec<P>,
    ) -> Result<(), Error> {
        let paths_v: Vec<CString> = paths
            .iter()
            .map(|path| to_cpath(&path))
            .collect::<Result<Vec<_>, _>>()?;

        let cpaths: Vec<_> = paths_v.iter().map(|path| path.as_ptr()).collect();

        self.ingest_external_file_raw(&opts, &paths_v, &cpaths)
    }

    /// Loads a list of external SST files created with SstFileWriter into the DB for given Column Family
    /// with default opts
    pub fn ingest_external_file_cf<P: AsRef<Path>>(
        &self,
        cf: &ColumnFamily,
        paths: Vec<P>,
    ) -> Result<(), Error> {
        let opts = IngestExternalFileOptions::default();
        self.ingest_external_file_cf_opts(&cf, &opts, paths)
    }

    /// Loads a list of external SST files created with SstFileWriter into the DB for given Column Family
    pub fn ingest_external_file_cf_opts<P: AsRef<Path>>(
        &self,
        cf: &ColumnFamily,
        opts: &IngestExternalFileOptions,
        paths: Vec<P>,
    ) -> Result<(), Error> {
        let paths_v: Vec<CString> = paths
            .iter()
            .map(|path| to_cpath(&path))
            .collect::<Result<Vec<_>, _>>()?;

        let cpaths: Vec<_> = paths_v.iter().map(|path| path.as_ptr()).collect();

        self.ingest_external_file_raw_cf(&cf, &opts, &paths_v, &cpaths)
    }

    fn ingest_external_file_raw(
        &self,
        opts: &IngestExternalFileOptions,
        paths_v: &[CString],
        cpaths: &[*const c_char],
    ) -> Result<(), Error> {
        unsafe {
            ffi_try!(ffi::rocksdb_silk_ingest_external_file(
                self.inner,
                cpaths.as_ptr(),
                paths_v.len(),
                opts.inner as *const _
            ));
            Ok(())
        }
    }

    fn ingest_external_file_raw_cf(
        &self,
        cf: &ColumnFamily,
        opts: &IngestExternalFileOptions,
        paths_v: &[CString],
        cpaths: &[*const c_char],
    ) -> Result<(), Error> {
        unsafe {
            ffi_try!(ffi::rocksdb_silk_ingest_external_file_cf(
                self.inner,
                cf.inner,
                cpaths.as_ptr(),
                paths_v.len(),
                opts.inner as *const _
            ));
            Ok(())
        }
    }

    /// Returns a list of all table files with their level, start key
    /// and end key
    pub fn live_files(&self) -> Result<Vec<LiveFile>, Error> {
        unsafe {
            let files = ffi::rocksdb_silk_livefiles(self.inner);
            if files.is_null() {
                Err(Error::new("Could not get live files".to_owned()))
            } else {
                let n = ffi::rocksdb_silk_livefiles_count(files);

                let mut livefiles = Vec::with_capacity(n as usize);
                let mut key_size: usize = 0;

                for i in 0..n {
                    let name = from_cstr(ffi::rocksdb_silk_livefiles_name(files, i));
                    let size = ffi::rocksdb_silk_livefiles_size(files, i);
                    let level = ffi::rocksdb_silk_livefiles_level(files, i) as i32;

                    // get smallest key inside file
                    let smallest_key = ffi::rocksdb_silk_livefiles_smallestkey(files, i, &mut key_size);
                    let smallest_key = raw_data(smallest_key, key_size);

                    // get largest key inside file
                    let largest_key = ffi::rocksdb_silk_livefiles_largestkey(files, i, &mut key_size);
                    let largest_key = raw_data(largest_key, key_size);

                    livefiles.push(LiveFile {
                        name,
                        size,
                        level,
                        start_key: smallest_key,
                        end_key: largest_key
                    })
                }

                // destroy livefiles metadata(s)
                ffi::rocksdb_silk_livefiles_destroy(files);

                // return
                Ok(livefiles)
            }
        }
    }

    /// Delete sst files whose keys are entirely in the given range.
    ///
    /// Could leave some keys in the range which are in files which are not
    /// entirely in the range.
    ///
    /// Note: L0 files are left regardless of whether they're in the range.
    ///  
    /// Snapshots before the delete might not see the data in the given range.
    pub fn delete_file_in_range<K: AsRef<[u8]>>(&self, from: K, to: K) -> Result<(), Error> {
        let from = from.as_ref();
        let to = to.as_ref();
        unsafe {
            ffi_try!(ffi::rocksdb_silk_delete_file_in_range(
                self.inner,
                from.as_ptr() as *const c_char,
                from.len() as size_t,
                to.as_ptr() as *const c_char,
                to.len() as size_t,
            ));
            Ok(())
        }
    }

    /// Same as `delete_file_in_range` but only for specific column family
    pub fn delete_file_in_range_cf<K: AsRef<[u8]>>(
        &self,
        cf: &ColumnFamily,
        from: K,
        to: K,
    ) -> Result<(), Error> {
        let from = from.as_ref();
        let to = to.as_ref();
        unsafe {
            ffi_try!(ffi::rocksdb_silk_delete_file_in_range_cf(
                self.inner,
                cf.inner,
                from.as_ptr() as *const c_char,
                from.len() as size_t,
                to.as_ptr() as *const c_char,
                to.len() as size_t,
            ));
            Ok(())
        }
    }
}

impl Drop for DB {
    fn drop(&mut self) {
        unsafe {
            for cf in self.cfs.values() {
                ffi::rocksdb_silk_column_family_handle_destroy(cf.inner);
            }
            ffi::rocksdb_silk_close(self.inner);
        }
    }
}

impl fmt::Debug for DB {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "RocksDB {{ path: {:?} }}", self.path())
    }
}

/// The metadata that describes a SST file
#[derive(Debug, Clone)]
pub struct LiveFile {
    /// Name of the file
    pub name: String,
    /// Size of the file
    pub size: usize,
    /// Level at which this file resides
    pub level: i32,
    /// Smallest user defined key in the file
    pub start_key: Option<Vec<u8>>,
    /// Largest user defined key in the file
    pub end_key: Option<Vec<u8>>,
}

fn convert_options(opts: &[(&str, &str)]) -> Result<Vec<(CString, CString)>, Error> {
    opts.iter()
        .map(|(name, value)| {
            let cname = match CString::new(name.as_bytes()) {
                Ok(cname) => cname,
                Err(e) => return Err(Error::new(format!("Invalid option name `{}`", e))),
            };
            let cvalue = match CString::new(value.as_bytes()) {
                Ok(cvalue) => cvalue,
                Err(e) => return Err(Error::new(format!("Invalid option value: `{}`", e))),
            };
            Ok((cname, cvalue))
        })
        .collect()
}
