//
// This module contains the main entry point of the library,
// DavHandler.
//
use std::error::Error as StdError;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::{self, buf::Buf};
use futures_util::stream::Stream;
use headers::HeaderMapExt;
use http::{Request, Response, StatusCode};
use http_body::Body as HttpBody;

use crate::body::{Body, StreamBody};
use crate::davheaders;
use crate::davpath::DavPath;
use crate::ls::fakels::FakeLs;
use crate::ls::memls::MemLs;
use crate::util::{dav_method, DavMethod, DavMethodSet};

use crate::errors::DavError;
use crate::fs::*;
use crate::ls::*;
use crate::DavResult;

pub mod handle_copymove;
pub mod handle_delete;
pub mod handle_gethead;
use handle_gethead::READ_BUF_SIZE;
pub mod handle_lock;
pub mod handle_mkcol;
pub mod handle_options;
pub mod handle_props;
pub mod handle_put;

/// Configuration of the handler.
#[derive(Clone)]
pub struct DavBuilder {
    /// Prefix to be stripped off when handling request.
    prefix: String,
    /// Filesystem backend.
    fs: FileSystem,
    /// Locksystem backend.
    ls: Option<LockSystem>,
    /// Set of allowed methods (Defaults to "all methods")
    allow: DavMethodSet,
    /// Principal is webdav speak for "user", used to give locks an owner (if a locksystem is
    /// active).
    principal: Option<String>,
    /// Hide symbolic links? Defaults to `true`.
    hide_symlinks: bool,
    /// Does GET on a directory return indexes.
    autoindex: Option<bool>,
    /// read buffer size in bytes
    read_buf_size: usize,
    /// Does GET on a file return 302 redirect.
    redirect: bool,
}

/// File system backend.
#[derive(Clone)]
pub enum FileSystem {
    #[cfg(any(docsrs, feature = "memfs"))]
    Mem,
    #[cfg(any(docsrs, feature = "localfs"))]
    Local {
        /// Path to the root directory.
        base: PathBuf,
        public: bool,
        /// Case insensitive file names (Windows)
        case_insensitive: bool,
        /// Macos specific hacks
        macos: bool,
    },
    #[cfg(any(docsrs, feature = "localfs"))]
    LocalFile { file: PathBuf, public: bool },
}

impl FileSystem {
    /// Serve a local directory
    #[cfg(any(docsrs, feature = "localfs"))]
    pub fn local(
        path: impl Into<PathBuf>,
        public: bool,
        case_insensitive: bool,
        macos: bool,
    ) -> Self {
        FileSystem::Local {
            base: path.into(),
            public,
            case_insensitive,
            macos,
        }
    }
    /// Serve a local file
    #[cfg(any(docsrs, feature = "localfs"))]
    pub fn local_file(file: impl Into<PathBuf>, public: bool) -> Self {
        FileSystem::LocalFile {
            file: file.into(),
            public,
        }
    }
    fn build(self) -> Arc<dyn DavFileSystem> {
        match self {
            #[cfg(any(docsrs, feature = "memfs"))]
            FileSystem::Mem => crate::fs::memfs::MemFs::new(),
            #[cfg(any(docsrs, feature = "localfs"))]
            FileSystem::Local {
                base: path,
                public,
                case_insensitive,
                macos,
            } => crate::fs::localfs::LocalFs::new(path, public, case_insensitive, macos),

            #[cfg(any(docsrs, feature = "localfs"))]
            FileSystem::LocalFile { file, public } => {
                crate::fs::localfs::LocalFs::new_file(file, public)
            }
        }
    }
}

#[derive(Default, Clone, Copy)]
pub enum LockSystem {
    #[default]
    Mem,
    Fake,
}

impl LockSystem {
    fn build(self) -> Arc<dyn DavLockSystem> {
        match self {
            LockSystem::Mem => MemLs::new(),
            LockSystem::Fake => FakeLs::new(),
        }
    }
}

impl DavBuilder {
    /// Create a new configuration builder.
    pub fn new(fs: FileSystem) -> DavBuilder {
        Self {
            prefix: String::new(),
            fs,
            ls: None,
            allow: DavMethodSet::all(),
            principal: None,
            hide_symlinks: true,
            autoindex: None,
            read_buf_size: READ_BUF_SIZE,
            redirect: false,
        }
    }

    /// Use the configuration that was built to generate a DavConfig.
    pub fn build(self) -> DavHandler {
        self.into()
    }

    /// Prefix to be stripped off before translating the rest of
    /// the request path to a filesystem path.
    pub fn strip_prefix(self, prefix: impl Into<String>) -> Self {
        let mut this = self;
        this.prefix = prefix.into();
        this
    }

    /// Set the locksystem to use.
    pub fn locksystem(self, ls: LockSystem) -> Self {
        let mut this = self;
        this.ls = Some(ls);
        this
    }

    /// Which methods to allow (default is all methods).
    pub fn methods(self, allow: DavMethodSet) -> Self {
        let mut this = self;
        this.allow = allow;
        this
    }

    /// Set the name of the "webdav principal". This will be the owner of any created locks.
    pub fn principal(self, principal: impl Into<String>) -> Self {
        let mut this = self;
        this.principal = Some(principal.into());
        this
    }

    /// Hide symbolic links (default is true)
    pub fn hide_symlinks(self, hide: bool) -> Self {
        let mut this = self;
        this.hide_symlinks = hide;
        this
    }

    /// Does a GET on a directory produce a directory index.
    pub fn autoindex(self, autoindex: bool) -> Self {
        let mut this = self;
        this.autoindex = Some(autoindex);
        this
    }

    /// Read buffer size in bytes
    pub fn read_buf_size(self, size: usize) -> Self {
        let mut this = self;
        this.read_buf_size = size;
        this
    }

    pub fn redirect(self, redirect: bool) -> Self {
        let mut this = self;
        this.redirect = redirect;
        this
    }
}

/// The webdav handler struct.
///
/// The `new` and `build` etc methods are used to instantiate a handler.
///
/// The `handle` and `handle_with` methods are the methods that do the actual work.
#[derive(Clone)]
pub struct DavHandler {
    pub(crate) prefix: Arc<String>,
    pub(crate) fs: Arc<dyn DavFileSystem>,
    pub(crate) ls: Option<Arc<dyn DavLockSystem>>,
    pub(crate) allow: DavMethodSet,
    pub(crate) principal: Option<Arc<String>>,
    pub(crate) hide_symlinks: bool,
    pub(crate) autoindex: Option<bool>,
    pub(crate) read_buf_size: usize,
    pub(crate) redirect: bool,
}

impl From<DavBuilder> for DavHandler {
    fn from(cfg: DavBuilder) -> Self {
        Self {
            prefix: Arc::new(cfg.prefix),
            fs: cfg.fs.build(),
            ls: cfg.ls.map(|ls| ls.build()),
            allow: cfg.allow,
            principal: cfg.principal.map(Arc::new),
            hide_symlinks: cfg.hide_symlinks,
            autoindex: cfg.autoindex,
            read_buf_size: cfg.read_buf_size,
            redirect: cfg.redirect,
        }
    }
}

impl DavHandler {
    /// Return a configuration builder.
    pub fn builder(fs: FileSystem) -> DavBuilder {
        DavBuilder::new(fs)
    }

    /// Handle a webdav request.
    pub async fn handle<ReqBody, ReqData, ReqError>(&self, req: Request<ReqBody>) -> Response<Body>
    where
        ReqData: Buf + Send + 'static,
        ReqError: StdError + Send + Sync + 'static,
        ReqBody: HttpBody<Data = ReqData, Error = ReqError>,
    {
        self.handle_inner(req).await
    }

    /// Handle a webdav request, overriding parts of the config.
    ///
    /// For example, the `principal` can be set for this request.
    ///
    /// Or, the default config has no locksystem, and you pass in
    /// a fake locksystem (`FakeLs`) because this is a request from a
    /// windows or macos client that needs to see locking support.
    pub async fn handle_with<ReqBody, ReqData, ReqError>(
        &self,
        req: Request<ReqBody>,
        prefix: Option<String>,
        principal: Option<String>,
    ) -> Response<Body>
    where
        ReqData: Buf + Send + 'static,
        ReqError: StdError + Send + Sync + 'static,
        ReqBody: HttpBody<Data = ReqData, Error = ReqError>,
    {
        let mut this = self.clone();
        if let Some(prefix) = prefix {
            this.prefix = Arc::new(format!(
                "{}/{}",
                this.prefix.strip_suffix('/').unwrap_or(&this.prefix),
                prefix.strip_prefix('/').unwrap_or(&prefix)
            ));
        }
        if let Some(principal) = principal {
            this.principal = Some(Arc::new(principal));
        }
        this.handle_inner(req).await
    }

    /// Handles a request with a `Stream` body instead of a `HttpBody`.
    /// Used with webserver frameworks that have not
    /// opted to use the `http_body` crate just yet.
    #[doc(hidden)]
    pub async fn handle_stream<ReqBody, ReqData, ReqError>(
        &self,
        req: Request<ReqBody>,
    ) -> Response<Body>
    where
        ReqData: Buf + Send + 'static,
        ReqError: StdError + Send + Sync + 'static,
        ReqBody: Stream<Item = Result<ReqData, ReqError>>,
    {
        let req = {
            let (parts, body) = req.into_parts();
            Request::from_parts(parts, StreamBody::new(body))
        };
        self.handle_inner(req).await
    }

    /// Handles a request with a `Stream` body instead of a `HttpBody`.
    #[doc(hidden)]
    pub async fn handle_stream_with<ReqBody, ReqData, ReqError>(
        &self,
        req: Request<ReqBody>,
        prefix: Option<String>,
        principal: Option<String>,
    ) -> Response<Body>
    where
        ReqData: Buf + Send + 'static,
        ReqError: StdError + Send + Sync + 'static,
        ReqBody: Stream<Item = Result<ReqData, ReqError>>,
    {
        let req = {
            let (parts, body) = req.into_parts();
            Request::from_parts(parts, StreamBody::new(body))
        };
        let mut this = self.clone();
        if let Some(prefix) = prefix {
            this.prefix = Arc::new(prefix);
        }
        if let Some(principal) = principal {
            this.principal = Some(Arc::new(principal));
        }
        this.handle_inner(req).await
    }
}

impl DavHandler {
    // helper.
    pub(crate) async fn has_parent<'a>(&'a self, path: &'a DavPath) -> bool {
        let p = path.parent();
        self.fs
            .metadata(&p)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
    }

    // helper.
    pub(crate) fn path(&self, req: &Request<()>) -> DavPath {
        // This never fails (has been checked before)
        DavPath::from_uri_and_prefix(req.uri(), &self.prefix).unwrap()
    }

    // See if this is a directory and if so, if we have
    // to fixup the path by adding a slash at the end.
    pub(crate) fn fixpath(
        &self,
        res: &mut Response<Body>,
        path: &mut DavPath,
        meta: Box<dyn DavMetaData>,
    ) -> Box<dyn DavMetaData> {
        if meta.is_dir() && !path.is_collection() {
            path.add_slash();
            let newloc = path.with_prefix().as_url_string();
            res.headers_mut()
                .typed_insert(davheaders::ContentLocation(newloc));
        }
        meta
    }

    // drain request body and return length.
    pub(crate) async fn read_request<ReqBody, ReqData, ReqError>(
        &self,
        body: ReqBody,
        max_size: usize,
    ) -> DavResult<Vec<u8>>
    where
        ReqBody: HttpBody<Data = ReqData, Error = ReqError>,
        ReqData: Buf + Send + 'static,
        ReqError: StdError + Send + Sync + 'static,
    {
        let mut data = Vec::new();
        pin_utils::pin_mut!(body);
        while let Some(res) = body.data().await {
            let mut buf = res.map_err(|_| {
                DavError::IoError(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "UnexpectedEof",
                ))
            })?;
            while buf.has_remaining() {
                if data.len() + buf.remaining() > max_size {
                    return Err(StatusCode::PAYLOAD_TOO_LARGE.into());
                }
                let b = buf.chunk();
                let l = b.len();
                data.extend_from_slice(b);
                buf.advance(l);
            }
        }
        Ok(data)
    }

    // internal dispatcher.
    async fn handle_inner<ReqBody, ReqData, ReqError>(
        &self,
        req: Request<ReqBody>,
    ) -> Response<Body>
    where
        ReqBody: HttpBody<Data = ReqData, Error = ReqError>,
        ReqData: Buf + Send + 'static,
        ReqError: StdError + Send + Sync + 'static,
    {
        let is_ms = req
            .headers()
            .get("user-agent")
            .and_then(|s| s.to_str().ok())
            .map(|s| s.contains("Microsoft"))
            .unwrap_or(false);

        // Turn any DavError results into a HTTP error response.
        match self.handle2(req).await {
            Ok(resp) => {
                debug!("== END REQUEST result OK");
                resp
            }
            Err(err) => {
                debug!("== END REQUEST result {:?}", err);
                let mut resp = Response::builder();
                if is_ms && err.statuscode() == StatusCode::NOT_FOUND {
                    // This is an attempt to convince Windows to not
                    // cache a 404 NOT_FOUND for 30-60 seconds.
                    //
                    // That is a problem since windows caches the NOT_FOUND in a
                    // case-insensitive way. So if "www" does not exist, but "WWW" does,
                    // and you do a "dir www" and then a "dir WWW" the second one
                    // will fail.
                    //
                    // Ofcourse the below is not sufficient. Fixes welcome.
                    resp = resp
                        .header("Cache-Control", "no-store, no-cache, must-revalidate")
                        .header("Progma", "no-cache")
                        .header("Expires", "0")
                        .header("Vary", "*");
                }
                resp = resp.header("Content-Length", "0").status(err.statuscode());
                if err.must_close() {
                    resp = resp.header("connection", "close");
                }
                resp.body(Body::empty()).unwrap()
            }
        }
    }

    // internal dispatcher part 2.
    async fn handle2<ReqBody, ReqData, ReqError>(
        &self,
        req: Request<ReqBody>,
    ) -> DavResult<Response<Body>>
    where
        ReqBody: HttpBody<Data = ReqData, Error = ReqError>,
        ReqData: Buf + Send + 'static,
        ReqError: StdError + Send + Sync + 'static,
    {
        let (req, body) = {
            let (parts, body) = req.into_parts();
            (Request::from_parts(parts, ()), body)
        };

        // debug when running the webdav litmus tests.
        if log_enabled!(log::Level::Debug) {
            if let Some(t) = req.headers().typed_get::<davheaders::XLitmus>() {
                debug!("X-Litmus: {:?}", t);
            }
        }

        // translate HTTP method to Webdav method.
        let method = match dav_method(req.method()) {
            Ok(m) => m,
            Err(e) => {
                debug!("refusing method {} request {}", req.method(), req.uri());
                return Err(e);
            }
        };

        // see if method is allowed.
        if !self.allow.contains(method) {
            debug!(
                "method {} not allowed on request {}",
                req.method(),
                req.uri()
            );
            return Err(DavError::StatusClose(StatusCode::METHOD_NOT_ALLOWED));
        }

        // make sure the request path is valid.
        let path = DavPath::from_uri_and_prefix(req.uri(), &self.prefix)?;

        // PUT is the only handler that reads the body itself. All the
        // other handlers either expected no body, or a pre-read Vec<u8>.
        let (body_strm, body_data) = match method {
            DavMethod::Put | DavMethod::Patch => (Some(body), Vec::new()),
            _ => (None, self.read_request(body, 65536).await?),
        };

        // Not all methods accept a body.
        match method {
            DavMethod::Put
            | DavMethod::Patch
            | DavMethod::PropFind
            | DavMethod::PropPatch
            | DavMethod::Lock => {}
            _ => {
                if !body_data.is_empty() {
                    return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE.into());
                }
            }
        }

        debug!("== START REQUEST {:?} {}", method, path);

        match method {
            DavMethod::Options => self.handle_options(&req).await,
            DavMethod::PropFind => self.handle_propfind(&req, &body_data).await,
            DavMethod::PropPatch => self.handle_proppatch(&req, &body_data).await,
            DavMethod::MkCol => self.handle_mkcol(&req).await,
            DavMethod::Delete => self.handle_delete(&req).await,
            DavMethod::Lock => self.handle_lock(&req, &body_data).await,
            DavMethod::Unlock => self.handle_unlock(&req).await,
            DavMethod::Head | DavMethod::Get => self.handle_get(&req).await,
            DavMethod::Copy | DavMethod::Move => self.handle_copymove(&req, method).await,
            DavMethod::Put | DavMethod::Patch => self.handle_put(&req, body_strm.unwrap()).await,
        }
    }
}
