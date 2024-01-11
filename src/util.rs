use std::io::{Cursor, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use bitflags::bitflags;
use bytes::Bytes;
use headers::Header;
use http::method::InvalidMethod;
use time::format_description::well_known::Rfc3339;
use time::macros::offset;

use crate::body::Body;
use crate::errors::DavError;
use crate::DavResult;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct DavMethod: u32 {
        const HEAD = 0x0001;
        const GET = 0x0002;
        const PUT = 0x0004;
        const PATCH = 0x0008;
        const OPTIONS = 0x0010;
        const PROPFIND = 0x0020;
        const PROPPATCH = 0x0040;
        const MKCOL = 0x0080;
        const COPY = 0x0100;
        const MOVE = 0x0200;
        const DELETE = 0x0400;
        const LOCK = 0x0800;
        const UNLOCK = 0x1000;

        const HTTP_RO = Self::HEAD.bits() | Self::GET.bits() | Self::OPTIONS.bits();
        const HTTP_RW = Self::HTTP_RO.bits() | Self::PUT.bits();
        const WEBDAV_RO = Self::HTTP_RO.bits() | Self::PROPFIND.bits();
        const WEBDAV_BODY = Self::PUT.bits() | Self::PATCH.bits()
            | Self::PROPFIND.bits() | Self::PROPPATCH.bits() | Self::LOCK.bits();
        // const WEBDAV_RW = Self::all().bits();
    }
}
impl DavMethod {
    pub const WEBDAV_RW: Self = Self::all();
}

// translate method into our own enum that has webdav methods as well.
pub fn dav_method(m: &http::Method) -> DavResult<DavMethod> {
    let m = match *m {
        http::Method::HEAD => DavMethod::HEAD,
        http::Method::GET => DavMethod::GET,
        http::Method::PUT => DavMethod::PUT,
        http::Method::PATCH => DavMethod::PATCH,
        http::Method::DELETE => DavMethod::DELETE,
        http::Method::OPTIONS => DavMethod::OPTIONS,
        _ => match m.as_str() {
            "PROPFIND" => DavMethod::PROPFIND,
            "PROPPATCH" => DavMethod::PROPPATCH,
            "MKCOL" => DavMethod::MKCOL,
            "COPY" => DavMethod::COPY,
            "MOVE" => DavMethod::MOVE,
            "LOCK" => DavMethod::LOCK,
            "UNLOCK" => DavMethod::UNLOCK,
            _ => {
                return Err(DavError::UnknownDavMethod);
            }
        },
    };
    Ok(m)
}

// for external use.
impl std::convert::TryFrom<&http::Method> for DavMethod {
    type Error = InvalidMethod;

    fn try_from(value: &http::Method) -> Result<Self, Self::Error> {
        dav_method(value).map_err(|_| {
            // A trick to get at the value of http::method::InvalidMethod.
            http::method::Method::from_bytes(b"").unwrap_err()
        })
    }
}

pub fn dav_xml_error(body: &str) -> Body {
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\" ?>\n\
        <D:error xmlns:D=\"DAV:\">\n\
        {body}\n\
        </D:error>\n"
    );
    Body::from(xml)
}

pub fn systemtime_to_offsetdatetime(t: SystemTime) -> time::OffsetDateTime {
    match t.duration_since(UNIX_EPOCH) {
        Ok(t) => {
            let tm = time::OffsetDateTime::from_unix_timestamp(t.as_secs() as i64).unwrap();
            tm.to_offset(offset!(UTC))
        }
        Err(_) => time::OffsetDateTime::UNIX_EPOCH.to_offset(offset!(UTC)),
    }
}

pub fn systemtime_to_httpdate(t: SystemTime) -> String {
    let d = headers::Date::from(t);
    let mut v = Vec::new();
    d.encode(&mut v);
    v[0].to_str().unwrap().to_owned()
}

pub fn systemtime_to_rfc3339(t: SystemTime) -> String {
    // 1996-12-19T16:39:57Z
    systemtime_to_offsetdatetime(t).format(&Rfc3339).unwrap()
}

// A buffer that implements "Write".
#[derive(Clone)]
pub struct MemBuffer(Cursor<Vec<u8>>);

impl MemBuffer {
    pub fn new() -> MemBuffer {
        MemBuffer(Cursor::new(Vec::new()))
    }

    pub fn take(&mut self) -> Bytes {
        let buf = std::mem::take(self.0.get_mut());
        self.0.set_position(0);
        Bytes::from(buf)
    }
}

impl Write for MemBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn test_rfc3339() {
        assert!(systemtime_to_rfc3339(UNIX_EPOCH) == "1970-01-01T00:00:00Z");
    }
}
