//! Local filesystem access.
//!
//! This implementation is stateless. So the easiest way to use it
//! is to create a new instance in your handler every time
//! you need one.

use std::collections::VecDeque;
use std::future::Future;
use std::io;
#[cfg(unix)]
use std::os::unix::{
    ffi::OsStrExt,
    fs::{MetadataExt, PermissionsExt},
};
#[cfg(target_os = "windows")]
use std::os::windows::prelude::*;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::{Buf, Bytes, BytesMut};
use futures_util::{future, future::BoxFuture, FutureExt, Stream};
use pin_utils::pin_mut;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use super::localfs_macos::DUCacheBuilder;
use crate::davpath::DavPath;
use crate::fs::*;

#[derive(Debug, Clone)]
struct LocalFsMetaData(std::fs::Metadata);

/// Local Filesystem implementation.
pub(crate) struct LocalFs {
    pub basedir: PathBuf,
    #[allow(dead_code)]
    pub public: bool,
    pub case_insensitive: bool,
    pub macos: bool,
    pub is_file: bool,
}

#[derive(Debug)]
struct LocalFsFile(tokio::fs::File);

struct ReadDir {
    do_meta: ReadDirMeta,
    buffer: VecDeque<io::Result<DirEntry>>,
    dir_cache: Option<DUCacheBuilder>,
    iterator: Option<tokio::fs::ReadDir>,
    fut: Option<BoxFuture<'static, ReadDirBatch>>,
}

// Items from the readdir stream.
struct DirEntry {
    meta: io::Result<std::fs::Metadata>,
    entry: tokio::fs::DirEntry,
}

impl LocalFs {
    /// Create a new LocalFs DavFileSystem, serving "base".
    ///
    /// If "public" is set to true, all files and directories created will be
    /// publically readable (mode 644/755), otherwise they will be private
    /// (mode 600/700). Umask still overrides this.
    ///
    /// If "case_insensitive" is set to true, all filesystem lookups will
    /// be case insensitive. Note that this has a _lot_ of overhead!
    pub fn new(base: PathBuf, public: bool, case_insensitive: bool, macos: bool) -> Arc<LocalFs> {
        Arc::new(Self {
            basedir: base,
            public,
            macos,
            case_insensitive,
            is_file: false,
        })
    }

    /// Create a new LocalFs DavFileSystem, serving "file".
    ///
    /// This is like `new()`, but it always serves this single file.
    /// The request path is ignored.
    pub fn new_file(file: PathBuf, public: bool) -> Arc<LocalFs> {
        Arc::new(LocalFs {
            basedir: file,
            public,
            macos: false,
            case_insensitive: false,
            is_file: true,
        })
    }

    fn abs_path(&self, path: &DavPath) -> PathBuf {
        if self.case_insensitive {
            super::localfs_windows::resolve(&self.basedir, path)
        } else {
            let mut pathbuf = self.basedir.clone();
            if !self.is_file {
                pathbuf.push(path.as_rel_ospath());
            }
            pathbuf
        }
    }
}

// This implementation is basically a bunch of boilerplate to
// wrap the std::fs call in self.blocking() calls.
impl DavFileSystem for LocalFs {
    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        async move {
            if let Some(meta) = self.is_virtual(path) {
                return Ok(meta);
            }
            let path = self.abs_path(path);
            if self.is_notfound(&path) {
                return Err(FsError::NotFound);
            }

            let meta = tokio::fs::metadata(path).await?;
            Ok(Box::new(meta) as _)
        }
        .boxed()
    }

    fn symlink_metadata<'a>(&'a self, davpath: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        async move {
            if let Some(meta) = self.is_virtual(davpath) {
                return Ok(meta);
            }
            let path = self.abs_path(davpath);
            if self.is_notfound(&path) {
                return Err(FsError::NotFound);
            }
            let meta = tokio::fs::symlink_metadata(path).await?;
            Ok(Box::new(meta) as _)
        }
        .boxed()
    }

    // read_dir is a bit more involved - but not much - than a simple wrapper,
    // because it returns a stream.
    fn read_dir<'a>(
        &'a self,
        davpath: &'a DavPath,
        meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        async move {
            trace!("FS: read_dir {davpath:?}");
            let path = self.abs_path(davpath);
            let iter = tokio::fs::read_dir(&path).await?;
            let stream = ReadDir {
                do_meta: meta,
                buffer: VecDeque::new(),
                dir_cache: self.dir_cache_builder(path),
                iterator: Some(iter),
                fut: None,
            };
            Ok(Box::pin(stream) as _)
        }
        .boxed()
    }

    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        async move {
            trace!("FS: open {path:?}");
            if self.is_forbidden(path) {
                return Err(FsError::Forbidden);
            }
            let path = self.abs_path(path);
            #[cfg(unix)]
            let res = tokio::fs::OpenOptions::new()
                .read(options.read)
                .write(options.write)
                .append(options.append)
                .truncate(options.truncate)
                .create(options.create)
                .create_new(options.create_new)
                .mode(if self.public { 0o644 } else { 0o600 })
                .open(path)
                .await;
            #[cfg(windows)]
            let res = tokio::fs::OpenOptions::new()
                .read(options.read)
                .write(options.write)
                .append(options.append)
                .truncate(options.truncate)
                .create(options.create)
                .create_new(options.create_new)
                .open(path)
                .await;
            match res {
                Ok(file) => Ok(Box::new(LocalFsFile(file)) as Box<dyn DavFile>),
                Err(e) => Err(e.into()),
            }
        }
        .boxed()
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            trace!("FS: create_dir {path:?}");
            if self.is_forbidden(path) {
                return Err(FsError::Forbidden);
            }
            let _public = self.public;
            let path = self.abs_path(path);
            #[cfg(unix)]
            {
                tokio::fs::DirBuilder::new()
                    .mode(if _public { 0o755 } else { 0o700 })
                    .create(path)
                    .map_err(|e| e.into())
                    .await
            }
            #[cfg(windows)]
            {
                tokio::fs::DirBuilder::new()
                    .create(path)
                    .map_err(|e| e.into())
                    .await
            }
        }
        .boxed()
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            trace!("FS: remove_dir {path:?}");
            let path = self.abs_path(path);
            Ok(tokio::fs::remove_dir(path).await?)
        }
        .boxed()
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            trace!("FS: remove_file {path:?}");
            if self.is_forbidden(path) {
                return Err(FsError::Forbidden);
            }
            let path = self.abs_path(path);
            Ok(tokio::fs::remove_file(path).await?)
        }
        .boxed()
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            trace!("FS: rename {from:?} {to:?}");
            if self.is_forbidden(from) || self.is_forbidden(to) {
                return Err(FsError::Forbidden);
            }
            let p_from = self.abs_path(from);
            let p_to = self.abs_path(to);
            match tokio::fs::rename(&p_from, &p_to).await {
                Ok(v) => Ok(v),
                Err(e) => {
                    // webdav allows a rename from a directory to a file.
                    // note that this check is racy, and I'm not quite sure what
                    // we should do if the source is a symlink. anyway ...
                    if e.raw_os_error() == Some(libc::ENOTDIR) && p_from.is_dir() {
                        // remove and try again.
                        let _ = tokio::fs::remove_file(&p_to).await;
                        Ok(tokio::fs::rename(p_from, p_to).await?)
                    } else {
                        Err(e.into())
                    }
                }
            }
        }
        .boxed()
    }

    fn copy<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            trace!("FS: copy {from:?} {to:?}");
            if self.is_forbidden(from) || self.is_forbidden(to) {
                return Err(FsError::Forbidden);
            }
            let p_from = self.abs_path(from);
            let p_to = self.abs_path(to);
            if let Err(e) = tokio::fs::copy(p_from, p_to).await {
                debug!("copy({from:?}, {to:?}) failed: {e}",);
                Err(e.into())
            } else {
                Ok(())
            }
        }
        .boxed()
    }
}

// read_batch() result.
struct ReadDirBatch {
    iterator: Option<tokio::fs::ReadDir>,
    buffer: VecDeque<io::Result<DirEntry>>,
}

// Read the next batch of LocalFsDirEntry structs (up to 256).
// This is sync code, must be run in `blocking()`.
async fn read_batch(iterator: Option<tokio::fs::ReadDir>, do_meta: ReadDirMeta) -> ReadDirBatch {
    let mut buffer = VecDeque::new();
    let mut iterator = match iterator {
        Some(i) => i,
        None => {
            return ReadDirBatch {
                buffer,
                iterator: None,
            }
        }
    };
    for _ in 0..256 {
        match iterator.next_entry().await {
            Ok(Some(entry)) => {
                let meta = match do_meta {
                    ReadDirMeta::Data => std::fs::metadata(entry.path()),
                    ReadDirMeta::DataSymlink => entry.metadata().await,
                };
                let d = DirEntry { meta, entry };
                buffer.push_back(Ok(d))
            }
            Err(e) => {
                buffer.push_back(Err(e));
                break;
            }
            Ok(None) => break,
        }
    }
    ReadDirBatch {
        buffer,
        iterator: Some(iterator),
    }
}

impl ReadDir {
    // Create a future that calls read_batch().
    //
    // The 'iterator' is moved into the future, and returned when it completes,
    // together with a list of directory entries.
    fn read_batch(&mut self) -> BoxFuture<'static, ReadDirBatch> {
        let iterator = self.iterator.take();
        let do_meta = self.do_meta;

        read_batch(iterator, do_meta).boxed()
    }
}

// The stream implementation tries to be smart and batch I/O operations
impl Stream for ReadDir {
    type Item = Box<dyn DavDirEntry>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = Pin::into_inner(self);

        // If the buffer is empty, fill it.
        if this.buffer.is_empty() {
            // If we have no pending future, create one.
            if this.fut.is_none() {
                if this.iterator.is_none() {
                    return Poll::Ready(None);
                }
                this.fut = Some(this.read_batch());
            }

            // Poll the future.
            let fut = this.fut.as_mut().unwrap();
            pin_mut!(fut);
            match Pin::new(&mut fut).poll(cx) {
                Poll::Ready(batch) => {
                    this.fut.take();
                    if let Some(ref mut nb) = this.dir_cache {
                        batch.buffer.iter().for_each(|e| {
                            if let Ok(ref e) = e {
                                nb.add(e.entry.file_name());
                            }
                        });
                    }
                    this.buffer = batch.buffer;
                    this.iterator = batch.iterator;
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        // we filled the buffer, now pop from the buffer.
        match this.buffer.pop_front() {
            Some(Ok(item)) => Poll::Ready(Some(Box::new(item))),
            Some(Err(_)) | None => {
                // fuse the iterator.
                this.iterator.take();
                // finish the cache.
                if let Some(ref mut nb) = this.dir_cache {
                    nb.finish();
                }
                // return end-of-stream.
                Poll::Ready(None)
            }
        }
    }
}

enum Is {
    File,
    Dir,
    Symlink,
}

impl DirEntry {
    async fn is_a(&self, is: Is) -> FsResult<bool> {
        match &self.meta {
            Ok(meta) => Ok(match is {
                Is::File => meta.file_type().is_file(),
                Is::Dir => meta.file_type().is_dir(),
                Is::Symlink => meta.file_type().is_symlink(),
            }),
            Err(e) => Err(e.into()),
        }
    }
}

impl DavDirEntry for DirEntry {
    fn metadata(&self) -> FsFuture<Box<dyn DavMetaData>> {
        let m = match &self.meta {
            Ok(meta) => Ok(Box::new(meta.clone()) as _),
            Err(e) => Err(e.into()),
        };
        Box::pin(future::ready(m))
    }

    #[cfg(unix)]
    fn name(&self) -> Vec<u8> {
        self.entry.file_name().as_bytes().to_vec()
    }

    #[cfg(windows)]
    fn name(&self) -> Vec<u8> {
        self.entry.file_name().to_str().unwrap().as_bytes().to_vec()
    }

    fn is_dir(&self) -> FsFuture<bool> {
        Box::pin(self.is_a(Is::Dir))
    }

    fn is_file(&self) -> FsFuture<bool> {
        Box::pin(self.is_a(Is::File))
    }

    fn is_symlink(&self) -> FsFuture<bool> {
        Box::pin(self.is_a(Is::Symlink))
    }
}

impl DavFile for LocalFsFile {
    fn metadata(&mut self) -> FsFuture<Box<dyn DavMetaData>> {
        async move {
            let file = &self.0;
            let meta = file.metadata().await?;
            Ok(Box::new(meta) as _)
        }
        .boxed()
    }

    fn write_bytes(&mut self, buf: Bytes) -> FsFuture<()> {
        async move { Ok(self.0.write_all(&buf).await?) }.boxed()
    }

    fn write_buf(&mut self, mut buf: Box<dyn Buf + Send>) -> FsFuture<()> {
        async move {
            while buf.remaining() > 0 {
                let n = self.0.write(buf.chunk()).await?;
                buf.advance(n);
            }
            Ok(())
        }
        .boxed()
    }

    fn read_bytes(&mut self, count: usize) -> FsFuture<Bytes> {
        async move {
            let mut buf = BytesMut::with_capacity(count);
            while self.0.read_buf(&mut buf).await? > 0 {}
            Ok(buf.freeze())
        }
        .boxed()
    }

    fn seek(&mut self, pos: SeekFrom) -> FsFuture<u64> {
        self.0.seek(pos).map_err(Into::into).boxed()
    }

    fn flush(&mut self) -> FsFuture<()> {
        self.0.sync_all().map_err(Into::into).boxed()
    }
}

impl DavMetaData for std::fs::Metadata {
    fn len(&self) -> u64 {
        self.len()
    }
    fn created(&self) -> FsResult<SystemTime> {
        self.created().map_err(|e| e.into())
    }
    fn modified(&self) -> FsResult<SystemTime> {
        self.modified().map_err(|e| e.into())
    }
    fn accessed(&self) -> FsResult<SystemTime> {
        self.accessed().map_err(|e| e.into())
    }

    #[cfg(unix)]
    fn status_changed(&self) -> FsResult<SystemTime> {
        Ok(UNIX_EPOCH + Duration::new(self.ctime() as u64, 0))
    }

    #[cfg(windows)]
    fn status_changed(&self) -> FsResult<SystemTime> {
        Ok(UNIX_EPOCH + Duration::from_nanos(self.creation_time() - 116444736000000000))
    }

    fn is_dir(&self) -> bool {
        self.is_dir()
    }
    fn is_file(&self) -> bool {
        self.is_file()
    }
    fn is_symlink(&self) -> bool {
        self.file_type().is_symlink()
    }

    #[cfg(unix)]
    fn executable(&self) -> FsResult<bool> {
        if self.is_file() {
            return Ok((self.permissions().mode() & 0o100) > 0);
        }
        Err(FsError::NotImplemented)
    }

    #[cfg(windows)]
    fn executable(&self) -> FsResult<bool> {
        // FIXME: implement
        Err(FsError::NotImplemented)
    }

    // same as the default apache etag.
    #[cfg(unix)]
    fn etag(&self) -> Option<String> {
        let modified = self.modified().ok()?;
        let t = modified.duration_since(UNIX_EPOCH).ok()?;
        let t = t.as_secs() * 1000000 + t.subsec_nanos() as u64 / 1000;
        if self.is_file() {
            Some(format!("{:x}-{:x}-{:x}", self.ino(), self.len(), t))
        } else {
            Some(format!("{:x}-{:x}", self.ino(), t))
        }
    }

    // same as the default apache etag.
    #[cfg(windows)]
    fn etag(&self) -> Option<String> {
        let modified = self.modified().ok()?;
        let t = modified.duration_since(UNIX_EPOCH).ok()?;
        let t = t.as_secs() * 1000000 + t.subsec_nanos() as u64 / 1000;
        if self.is_file() {
            Some(format!("{:x}-{:x}", self.len(), t))
        } else {
            Some(format!("{:x}", t))
        }
    }
}
