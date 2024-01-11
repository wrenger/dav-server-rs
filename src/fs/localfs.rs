//! Local filesystem access.
//!
//! This implementation is stateless. So the easiest way to use it
//! is to create a new instance in your handler every time
//! you need one.

use std::io;
#[cfg(unix)]
use std::os::unix::{
    ffi::OsStrExt,
    fs::{MetadataExt, PermissionsExt},
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_stream::stream;
use bytes::{Buf, Bytes, BytesMut};
use futures_util::{future, FutureExt};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

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

    fn read_dir<'a>(
        &'a self,
        davpath: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        async move {
            trace!("FS: read_dir {davpath:?}");
            let path = self.abs_path(davpath);
            let mut read_dir = tokio::fs::read_dir(&path).await?;
            let mut dir_cache = self.dir_cache_builder(path);
            Ok(Box::pin(stream! {
                loop {
                    match read_dir.next_entry().await {
                        Ok(Some(entry)) => {
                            if let Some(cache) = &mut dir_cache {
                                cache.add(entry.file_name());
                            }
                            let meta = entry.metadata().await;
                            yield Box::new(DirEntry { meta, entry }) as Box<dyn DavDirEntry>;
                        }
                        Ok(None) => break,
                        Err(e) => {
                            debug!("read_dir failed {e}");
                            break;
                        }
                    }
                }
                dir_cache.map(|mut cache| cache.finish());
            }) as _)
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
            let mut opt = tokio::fs::OpenOptions::new();
            opt.read(options.read)
                .write(options.write)
                .append(options.append)
                .truncate(options.truncate)
                .create(options.create)
                .create_new(options.create_new);
            #[cfg(unix)]
            if self.public {
                opt.mode(0o644);
            } else {
                opt.mode(0o600);
            }
            match opt.open(path).await {
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
            let path = self.abs_path(path);
            #[allow(unused_mut)]
            let mut dir = tokio::fs::DirBuilder::new();
            #[cfg(unix)]
            dir.mode(if self.public { 0o755 } else { 0o700 });
            Ok(dir.create(path).await?)
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
    fn is_dir(&self) -> bool {
        self.is_dir()
    }
    fn is_file(&self) -> bool {
        self.is_file()
    }
    fn is_symlink(&self) -> bool {
        self.file_type().is_symlink()
    }
    fn executable(&self) -> FsResult<bool> {
        #[cfg(unix)]
        if self.is_file() {
            return Ok((self.permissions().mode() & 0o100) > 0);
        }
        // FIXME: implement
        Err(FsError::NotImplemented)
    }

    // same as the default apache etag.
    fn etag(&self) -> Option<String> {
        let modified = self.modified().ok()?;
        let t = modified.duration_since(UNIX_EPOCH).ok()?;
        let t = t.as_secs() * 1000000 + t.subsec_nanos() as u64 / 1000;
        #[cfg(unix)]
        if self.is_file() {
            Some(format!("{:x}-{:x}-{:x}", self.ino(), self.len(), t))
        } else {
            Some(format!("{:x}-{:x}", self.ino(), t))
        }
        #[cfg(windows)]
        if self.is_file() {
            Some(format!("{:x}-{:x}", self.len(), t))
        } else {
            Some(format!("{:x}", t))
        }
    }
}
