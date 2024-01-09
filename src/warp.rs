//! Adapter for the `warp` HTTP server framework.
//!
//! The filters in this module will always succeed and never
//! return an error. For example, if a file is not found, the
//! filter will return a 404 reply, and not an internal
//! rejection.
//!
use std::convert::Infallible;
use std::path::Path;

use crate::{DavHandler, FileSystem, LockSystem};
use warp::{filters::BoxedFilter, Filter, Reply};

/// Reply-filter that runs a DavHandler.
///
/// Just pass in a pre-configured DavHandler. If a prefix was not
/// configured, it will be the request path up to this point.
pub fn dav_handler(handler: DavHandler) -> BoxedFilter<(impl Reply,)> {
    use http::header::HeaderMap;
    use http::uri::Uri;
    use http::Response;
    use warp::path::{FullPath, Tail};

    warp::method()
        .and(warp::path::full())
        .and(warp::path::tail())
        .and(warp::header::headers_cloned())
        .and(warp::body::stream())
        .and_then(
            move |method, path_full: FullPath, path_tail: Tail, headers: HeaderMap, body| {
                let handler = handler.clone();

                async move {
                    // rebuild an http::Request struct.
                    let path_str = path_full.as_str();
                    let uri = path_str.parse::<Uri>().unwrap();
                    let mut builder = http::Request::builder().method(method).uri(uri);
                    for (k, v) in headers.iter() {
                        builder = builder.header(k, v);
                    }
                    let request = builder.body(body).unwrap();

                    let path_len = path_str.len();
                    let tail_len = path_tail.as_str().len();
                    let prefix = path_str[..path_len - tail_len].to_string();
                    let response = handler
                        .handle_stream_with(request, Some(prefix), None)
                        .await;

                    // Need to remap the http_body::Body to a hyper::Body.
                    let (parts, body) = response.into_parts();
                    let response = Response::from_parts(parts, hyper::Body::wrap_stream(body));
                    Ok::<_, Infallible>(response)
                }
            },
        )
        .boxed()
}

/// Creates a Filter that serves files and directories at the
/// base path joined with the remainder of the request path,
/// like `warp::filters::fs::dir`.
///
/// The behaviour for serving a directory depends on the flags:
///
/// - `index_html`: if an `index.html` file is found, serve it.
/// - `auto_index_over_get`: Create a directory index page when accessing over HTTP `GET` (but NOT
///   affecting WebDAV `PROPFIND` method currently). In the current implementation, this only
///   affects HTTP `GET` method (commonly used for listing the directories when accessing through a
///   `http://` or `https://` URL for a directory in a browser), but NOT WebDAV listing of a
///   directory (HTTP `PROPFIND`). BEWARE: The name and behaviour of this parameter variable may
///   change, and later it may control WebDAV `PROPFIND`, too (but not as of now).
///
///   In release mode, if `auto_index_over_get` is `true`, then this executes as described above
///   (currently affecting only HTTP `GET`), but beware of this current behaviour.
///
///   In debug mode, if `auto_index_over_get` is `false`, this _panics_. That is so that it alerts
///   the developers to this current limitation, so they don't accidentally expect
///   `auto_index_over_get` to control WebDAV.
/// - no flags set: 404.
pub fn dav_dir(base: impl AsRef<Path>, auto_index_over_get: bool) -> BoxedFilter<(impl Reply,)> {
    debug_assert!(
        auto_index_over_get,
        "See documentation of dav_server::warp::dav_dir(...)."
    );
    dav_handler(
        DavHandler::builder()
            .filesystem(FileSystem::local(base.as_ref(), false, false, false))
            .locksystem(LockSystem::Fake)
            .autoindex(auto_index_over_get)
            .build(),
    )
}

/// Creates a Filter that serves a single file, ignoring the request path,
/// like `warp::filters::fs::file`.
pub fn dav_file(file: impl AsRef<Path>) -> BoxedFilter<(impl Reply,)> {
    dav_handler(
        DavHandler::builder()
            .filesystem(FileSystem::local_file(file.as_ref(), false))
            .locksystem(LockSystem::Fake)
            .build(),
    )
}
