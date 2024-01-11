use headers::HeaderMapExt;
use http::{Request, Response};

use crate::body::Body;
use crate::util::{dav_method, DavMethod};
use crate::DavResult;

impl crate::DavHandler {
    pub(crate) async fn handle_options(&self, req: &Request<()>) -> DavResult<Response<Body>> {
        let mut res = Response::new(Body::empty());

        let h = res.headers_mut();

        // We could simply not report webdav level 2 support if self.allow doesn't
        // contain LOCK/UNLOCK. However we do advertise support, since there might
        // be LOCK/UNLOCK support in another part of the URL space.
        let dav = "1,2,3,sabredav-partialupdate";
        h.insert("DAV", dav.parse().unwrap());
        h.insert("MS-Author-Via", "DAV".parse().unwrap());
        h.typed_insert(headers::ContentLength(0));

        // Helper to add method to array if method is in fact
        // allowed. If the current method is not OPTIONS, leave
        // out the current method since we're probably called
        // for DavMethodNotAllowed.
        let method = dav_method(req.method()).unwrap_or(DavMethod::OPTIONS);
        let islock = |m| m == DavMethod::LOCK || m == DavMethod::UNLOCK;
        let mm = |v: &mut Vec<String>, m: &str, y: DavMethod| {
            if (y == DavMethod::OPTIONS || (y != method || islock(y) != islock(method)))
                && (!islock(y) || self.ls.is_some())
                && self.allow.contains(y)
            {
                v.push(m.to_string());
            }
        };

        let path = self.path(req);
        let meta = self.fs.metadata(&path).await;
        let is_unmapped = meta.is_err();
        let is_file = meta.map(|m| m.is_file()).unwrap_or_default();
        let is_star = path.is_star() && method == DavMethod::OPTIONS;

        let mut v = Vec::new();
        if is_unmapped && !is_star {
            mm(&mut v, "OPTIONS", DavMethod::OPTIONS);
            mm(&mut v, "MKCOL", DavMethod::MKCOL);
            mm(&mut v, "PUT", DavMethod::PUT);
            mm(&mut v, "LOCK", DavMethod::LOCK);
        } else {
            if is_file || is_star {
                mm(&mut v, "HEAD", DavMethod::HEAD);
                mm(&mut v, "GET", DavMethod::GET);
                mm(&mut v, "PATCH", DavMethod::PATCH);
                mm(&mut v, "PUT", DavMethod::PUT);
            }
            mm(&mut v, "OPTIONS", DavMethod::OPTIONS);
            mm(&mut v, "PROPFIND", DavMethod::PROPFIND);
            mm(&mut v, "COPY", DavMethod::COPY);
            if path.as_url_string() != "/" {
                mm(&mut v, "MOVE", DavMethod::MOVE);
                mm(&mut v, "DELETE", DavMethod::DELETE);
            }
            mm(&mut v, "LOCK", DavMethod::LOCK);
            mm(&mut v, "UNLOCK", DavMethod::UNLOCK);
        }

        let a = v.join(",").parse().unwrap();
        res.headers_mut().insert("allow", a);

        Ok(res)
    }
}
