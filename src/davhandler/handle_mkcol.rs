use headers::HeaderMapExt;
use http::{Request, Response, StatusCode};

use crate::body::Body;
use crate::conditional::*;
use crate::davheaders;
use crate::fs::*;
use crate::{DavError, DavResult};

impl crate::DavHandler {
    pub(crate) async fn handle_mkcol(&self, req: &Request<()>) -> DavResult<Response<Body>> {
        let mut path = self.path(req);
        let meta = self.fs.metadata(&path).await;

        // check the If and If-* headers.
        let res = if_match_get_tokens(
            req,
            meta.ok().as_deref(),
            &*self.fs,
            self.ls.as_deref(),
            &path,
        )
        .await;
        let tokens = match res {
            Ok(t) => t,
            Err(s) => return Err(DavError::Status(s)),
        };

        // if locked check if we hold that lock.
        if let Some(locksystem) = &self.ls {
            let t = tokens.iter().map(|s| s.as_str()).collect::<Vec<&str>>();
            let principal = &self.principal;
            if locksystem.check(&path, principal, false, false, t).is_err() {
                return Err(DavError::Status(StatusCode::LOCKED));
            }
        }

        match self.fs.create_dir(&path).await {
            // RFC 4918 9.3.1 MKCOL Status Codes.
            Err(FsError::Exists) => Err(DavError::Status(StatusCode::METHOD_NOT_ALLOWED)),
            Err(FsError::NotFound) => Err(DavError::Status(StatusCode::CONFLICT)),
            Err(e) => Err(DavError::FsError(e)),
            Ok(()) => {
                let mut res = Response::new(Body::empty());
                if path.is_collection() {
                    path.add_slash();
                    res.headers_mut().typed_insert(davheaders::ContentLocation(
                        path.with_prefix().as_url_string(),
                    ));
                }
                *res.status_mut() = StatusCode::CREATED;
                Ok(res)
            }
        }
    }
}
