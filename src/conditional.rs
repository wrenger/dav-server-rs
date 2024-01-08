use std::time::{Duration, SystemTime, UNIX_EPOCH};

use headers::{HeaderMapExt, IfModifiedSince};
use http::{Method, StatusCode};

use crate::davheaders::{self, ETag, ETagList, If, IfItem, IfNoneMatch, IfRange};
use crate::davpath::DavPath;
use crate::fs::{DavFileSystem, DavMetaData};
use crate::ls::DavLockSystem;

type Request = http::Request<()>;

// SystemTime has nanosecond precision. Round it down to the
// nearest second, because an HttpDate has second precision.
fn round_time(tm: impl Into<SystemTime>) -> SystemTime {
    let tm = tm.into();
    match tm.duration_since(UNIX_EPOCH) {
        Ok(d) => UNIX_EPOCH + Duration::from_secs(d.as_secs()),
        Err(_) => tm,
    }
}

pub(crate) fn ifrange_match(hdr: &IfRange, tag: Option<&ETag>, date: Option<SystemTime>) -> bool {
    match (hdr, tag, date) {
        (IfRange::Date(d), None, Some(date)) => round_time(date) == round_time(*d),
        (IfRange::ETag(t), Some(tag), None) => t == tag,
        _ => false,
    }
}

pub(crate) fn etaglist_match(tags: &ETagList, exists: bool, tag: Option<&ETag>) -> bool {
    match tags {
        ETagList::Star => exists,
        ETagList::Tags(ref t) => match tag {
            Some(tag) => t.iter().any(|x| x == tag),
            None => false,
        },
    }
}

// Handle the if-headers: RFC 7232, HTTP/1.1 Conditional Requests.
pub(crate) fn http_if_match(req: &Request, meta: Option<&dyn DavMetaData>) -> Option<StatusCode> {
    let file_modified = meta.and_then(|m| m.modified().ok());

    if let Some(r) = req.headers().typed_get::<davheaders::IfMatch>() {
        let etag = meta.and_then(ETag::from_meta);
        if !etaglist_match(&r.0, meta.is_some(), etag.as_ref()) {
            trace!("precondition fail: If-Match {:?}", r);
            return Some(StatusCode::PRECONDITION_FAILED);
        }
    } else if let Some(r) = req.headers().typed_get::<headers::IfUnmodifiedSince>() {
        match file_modified {
            None => return Some(StatusCode::PRECONDITION_FAILED),
            Some(file_modified) => {
                if round_time(file_modified) > round_time(r) {
                    trace!("precondition fail: If-Unmodified-Since {:?}", r);
                    return Some(StatusCode::PRECONDITION_FAILED);
                }
            }
        }
    }

    if let Some(r) = req.headers().typed_get::<IfNoneMatch>() {
        let etag = meta.and_then(ETag::from_meta);
        if etaglist_match(&r.0, meta.is_some(), etag.as_ref()) {
            trace!("precondition fail: If-None-Match {:?}", r);
            if req.method() == Method::GET || req.method() == Method::HEAD {
                return Some(StatusCode::NOT_MODIFIED);
            } else {
                return Some(StatusCode::PRECONDITION_FAILED);
            }
        }
    } else if let Some(r) = req.headers().typed_get::<IfModifiedSince>() {
        if req.method() == Method::GET || req.method() == Method::HEAD {
            if let Some(file_modified) = file_modified {
                if round_time(file_modified) <= round_time(r) {
                    trace!("not-modified If-Modified-Since {:?}", r);
                    return Some(StatusCode::NOT_MODIFIED);
                }
            }
        }
    }
    None
}

// handle the If header: RFC4918, 10.4.  If Header
//
// returns true if the header was not present, or if any of the iflists
// evaluated to true. Also returns a Vec of StateTokens that we encountered.
//
// caller should set the http status to 412 PreconditionFailed if
// the return value from this function is false.
//
pub(crate) async fn dav_if_match(
    req: &Request,
    fs: &dyn DavFileSystem,
    ls: Option<&dyn DavLockSystem>,
    path: &DavPath,
) -> (bool, Vec<String>) {
    let mut tokens: Vec<String> = Vec::new();
    let mut any_list_ok = false;

    let r = match req.headers().typed_get::<If>() {
        Some(r) => r,
        None => return (true, tokens),
    };

    for iflist in r.0.iter() {
        // save and return all statetokens that we encountered.
        let toks = iflist.conditions.iter().filter_map(|c| match c.item {
            IfItem::StateToken(ref t) => Some(t.to_owned()),
            _ => None,
        });
        tokens.extend(toks);

        // skip over if a previous list already evaluated to true.
        if any_list_ok {
            continue;
        }

        // find the resource that this list is about.
        let mut pa: Option<DavPath> = None;
        let (p, valid) = match iflist.resource_tag {
            Some(ref url) => {
                match DavPath::from_str_and_prefix(url.path(), path.prefix()) {
                    Ok(p) => {
                        // anchor davpath in pa.
                        let p: &DavPath = pa.get_or_insert(p);
                        (p, true)
                    }
                    Err(_) => (path, false),
                }
            }
            None => (path, true),
        };

        // now process the conditions. they must all be true.
        let mut list_ok = false;
        for cond in iflist.conditions.iter() {
            let cond_ok = match cond.item {
                IfItem::StateToken(ref s) => {
                    // tokens in DAV: namespace always evaluate to false (10.4.8)
                    if !valid || s.starts_with("DAV:") {
                        false
                    } else {
                        match ls {
                            Some(ls) => ls.check(p, None, true, false, vec![s]).is_ok(),
                            None => false,
                        }
                    }
                }
                IfItem::ETag(ref tag) => {
                    if !valid {
                        // invalid location, so always false.
                        false
                    } else {
                        match fs.metadata(p).await {
                            Ok(meta) => {
                                // exists and may have metadata ..
                                if let Some(mtag) = ETag::from_meta(&*meta) {
                                    tag == &mtag
                                } else {
                                    false
                                }
                            }
                            Err(_) => {
                                // metadata error, fail.
                                false
                            }
                        }
                    }
                }
            };
            if cond_ok == cond.not {
                list_ok = false;
                break;
            }
            list_ok = true;
        }
        if list_ok {
            any_list_ok = true;
        }
    }
    if !any_list_ok {
        trace!("precondition fail: If {:?}", r.0);
    }
    (any_list_ok, tokens)
}

// Handle both the HTTP conditional If: headers, and the webdav If: header.
pub(crate) async fn if_match(
    req: &Request,
    meta: Option<&dyn DavMetaData>,
    fs: &dyn DavFileSystem,
    ls: Option<&dyn DavLockSystem>,
    path: &DavPath,
) -> Option<StatusCode> {
    match dav_if_match(req, fs, ls, path).await {
        (true, _) => {}
        (false, _) => return Some(StatusCode::PRECONDITION_FAILED),
    }
    http_if_match(req, meta)
}

// Like if_match, but also returns all "associated state-tokens"
pub(crate) async fn if_match_get_tokens(
    req: &Request,
    meta: Option<&dyn DavMetaData>,
    fs: &dyn DavFileSystem,
    ls: Option<&dyn DavLockSystem>,
    path: &DavPath,
) -> Result<Vec<String>, StatusCode> {
    if let Some(code) = http_if_match(req, meta) {
        return Err(code);
    }
    match dav_if_match(req, fs, ls, path).await {
        (true, v) => Ok(v),
        (false, _) => Err(StatusCode::PRECONDITION_FAILED),
    }
}
