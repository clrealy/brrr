//! IPP wire format + the tiny bit of HTTP it rides on. Hand-rolled on purpose.
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

pub const PRINT_JOB: u16 = 0x0002;
pub const CANCEL_JOB: u16 = 0x0008;
pub const GET_JOB_ATTRS: u16 = 0x0009;
pub const GET_JOBS: u16 = 0x000A;
pub const GET_PRINTER_ATTRS: u16 = 0x000B;

pub const INTEGER: u8 = 0x21;
pub const BOOLEAN: u8 = 0x22;
pub const ENUM: u8 = 0x23;
pub const RANGE: u8 = 0x33;
pub const RESOLUTION: u8 = 0x32;
pub const NAME: u8 = 0x42;
pub const KEYWORD: u8 = 0x44;
pub const URI: u8 = 0x45;
pub const CHARSET: u8 = 0x47;
pub const LANGUAGE: u8 = 0x48;
pub const MIME: u8 = 0x49;
pub const TEXT: u8 = 0x41;

trait Stream: Read + Write {}
impl<T: Read + Write> Stream for T {}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i32),
    Bool(bool),
    Text(String),
    Range(i32, i32),
    Res(i32, i32, u8),
    Other,
}

pub type Group = HashMap<String, Vec<Value>>;

#[derive(Debug, Clone)]
pub struct Uri {
    pub host: String,
    pub port: u16,
    pub path: String,
    pub tls: bool,
    /// Stable identity for certificate pinning: the printer's UUID when known.
    pub id: String,
}

impl Uri {
    pub fn parse(s: &str) -> Result<Uri, String> {
        let (rest, tls) = if let Some(r) = s.strip_prefix("ipps://").or_else(|| s.strip_prefix("https://")) {
            (r, true)
        } else if let Some(r) = s.strip_prefix("ipp://").or_else(|| s.strip_prefix("http://")) {
            (r, false)
        } else {
            return Err(format!("'{s}' isn't an ipp:// or ipps:// address"));
        };
        let (auth, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/ipp/print"),
        };
        let (host, port) = if let Some(end) = auth.strip_prefix('[').and_then(|a| a.find(']')) {
            let host = &auth[1..=end];
            let port = auth[end + 2..].strip_prefix(':').and_then(|p| p.parse().ok());
            (host.to_string(), port.unwrap_or(631))
        } else if let Some((h, p)) = auth.rsplit_once(':') {
            (h.to_string(), p.parse().map_err(|_| format!("bad port in '{s}'"))?)
        } else {
            (auth.to_string(), 631)
        };
        let id = format!("{host}:{port}");
        Ok(Uri { host, port, path: path.to_string(), tls, id })
    }

    pub fn to_ipp(&self) -> String {
        let h = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        format!("{}://{}:{}{}", if self.tls { "ipps" } else { "ipp" }, h, self.port, self.path)
    }
}

pub struct Msg {
    buf: Vec<u8>,
}

impl Msg {
    pub fn new(op: u16, version: (u8, u8), id: u32) -> Msg {
        let mut buf = vec![version.0, version.1];
        buf.extend_from_slice(&op.to_be_bytes());
        buf.extend_from_slice(&id.to_be_bytes());
        Msg { buf }
    }
    /// Every request starts with the same operation-attributes boilerplate.
    pub fn standard(op: u16, version: (u8, u8), uri: &Uri) -> Msg {
        let mut m = Msg::new(op, version, 1);
        m.group(1);
        m.s(CHARSET, "attributes-charset", "utf-8");
        m.s(LANGUAGE, "attributes-natural-language", "en");
        m.s(URI, "printer-uri", &uri.to_ipp());
        m.s(NAME, "requesting-user-name", &user());
        m
    }
    pub fn group(&mut self, tag: u8) {
        self.buf.push(tag);
    }
    fn head(&mut self, tag: u8, name: &str, len: usize) {
        self.buf.push(tag);
        self.buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
        self.buf.extend_from_slice(name.as_bytes());
        self.buf.extend_from_slice(&(len as u16).to_be_bytes());
    }
    pub fn s(&mut self, tag: u8, name: &str, v: &str) {
        self.head(tag, name, v.len());
        self.buf.extend_from_slice(v.as_bytes());
    }
    pub fn i(&mut self, tag: u8, name: &str, v: i32) {
        self.head(tag, name, 4);
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    pub fn boolean(&mut self, name: &str, v: bool) {
        self.head(BOOLEAN, name, 1);
        self.buf.push(v as u8);
    }
    pub fn res(&mut self, name: &str, dpi: i32) {
        self.head(RESOLUTION, name, 9);
        self.buf.extend_from_slice(&dpi.to_be_bytes());
        self.buf.extend_from_slice(&dpi.to_be_bytes());
        self.buf.push(3);
    }
    /// A 1setOf: first value carries the name, the rest continue it.
    pub fn many(&mut self, tag: u8, name: &str, vals: &[String]) {
        for (i, v) in vals.iter().enumerate() {
            self.s(tag, if i == 0 { name } else { "" }, v);
        }
    }
    pub fn range(&mut self, name: &str, a: i32, b: i32) {
        self.head(RANGE, name, 8);
        self.buf.extend_from_slice(&a.to_be_bytes());
        self.buf.extend_from_slice(&b.to_be_bytes());
    }
    pub fn finish(mut self) -> Vec<u8> {
        self.buf.push(3);
        self.buf
    }
}

pub fn user() -> String {
    std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_else(|_| "brrr".into())
}

#[derive(Debug, Default)]
pub struct Resp {
    pub status: u16,
    pub op: Group,
    pub job: Vec<Group>,
    pub printer: Group,
    pub unsupported: Group,
}

impl Resp {
    pub fn ok(&self) -> bool {
        self.status < 0x0100
    }
}

pub fn parse(b: &[u8]) -> Result<Resp, String> {
    parse_full(b).map(|x| x.0)
}

/// Also returns where the attributes end, which is where a document would start.
pub fn parse_full(b: &[u8]) -> Result<(Resp, usize), String> {
    let bad = || "printer sent a mangled IPP response".to_string();
    if b.len() < 9 {
        return Err(bad());
    }
    let mut r = Resp { status: u16::from_be_bytes([b[2], b[3]]), ..Default::default() };
    let mut i = 8;
    let mut group = 0u8;
    let mut last = String::new();
    let mut depth = 0i32;
    let mut cur = Group::new();

    fn store(r: &mut Resp, group: u8, g: &mut Group) {
        let g = std::mem::take(g);
        match group {
            1 => r.op = g,
            2 => r.job.push(g),
            4 => r.printer = g,
            5 => r.unsupported = g,
            _ => {}
        }
    }

    while i < b.len() {
        let tag = b[i];
        i += 1;
        if tag < 0x10 {
            store(&mut r, group, &mut cur);
            if tag == 3 {
                return Ok((r, i));
            }
            group = tag;
            continue;
        }
        let name_len = u16::from_be_bytes([*b.get(i).ok_or_else(bad)?, *b.get(i + 1).ok_or_else(bad)?]) as usize;
        i += 2;
        let name = b.get(i..i + name_len).ok_or_else(bad)?;
        i += name_len;
        let vlen = u16::from_be_bytes([*b.get(i).ok_or_else(bad)?, *b.get(i + 1).ok_or_else(bad)?]) as usize;
        i += 2;
        let v = b.get(i..i + vlen).ok_or_else(bad)?;
        i += vlen;

        // Collections are skipped: we only need the attribute's existence.
        if tag == 0x34 {
            if depth == 0 {
                if !name.is_empty() {
                    last = String::from_utf8_lossy(name).into_owned();
                }
                cur.entry(last.clone()).or_default().push(Value::Other);
            }
            depth += 1;
            continue;
        }
        if tag == 0x37 {
            depth -= 1;
            continue;
        }
        if depth > 0 {
            continue;
        }
        if !name.is_empty() {
            last = String::from_utf8_lossy(name).into_owned();
        }
        let val = match tag {
            INTEGER | ENUM if vlen == 4 => Value::Int(i32::from_be_bytes([v[0], v[1], v[2], v[3]])),
            BOOLEAN if vlen == 1 => Value::Bool(v[0] != 0),
            RANGE if vlen == 8 => Value::Range(
                i32::from_be_bytes([v[0], v[1], v[2], v[3]]),
                i32::from_be_bytes([v[4], v[5], v[6], v[7]]),
            ),
            RESOLUTION if vlen == 9 => Value::Res(
                i32::from_be_bytes([v[0], v[1], v[2], v[3]]),
                i32::from_be_bytes([v[4], v[5], v[6], v[7]]),
                v[8],
            ),
            0x35 | 0x36 => {
                // textWithLanguage: [lang-len lang text-len text]
                let ll = if vlen >= 2 { u16::from_be_bytes([v[0], v[1]]) as usize } else { 0 };
                let t = v.get(ll + 4..).unwrap_or(&[]);
                Value::Text(String::from_utf8_lossy(t).into_owned())
            }
            0x40..=0x49 => Value::Text(String::from_utf8_lossy(v).into_owned()),
            0x10 | 0x12 | 0x13 => continue, // unsupported / unknown / no-value
            _ => Value::Other,
        };
        cur.entry(last.clone()).or_default().push(val);
    }
    Err(bad())
}

pub fn explain_http(code: u32, uri: &Uri) -> String {
    match code {
        401 => format!("{} wants a login, and brrr can't do printer auth yet", uri.host),
        403 => format!("{} said forbidden. check the printer's web settings for IPP access", uri.host),
        404 => format!("{} doesn't have a print queue at {}", uri.host, uri.path),
        426 => format!("{} wants an encrypted connection. try its ipps:// address", uri.host),
        _ => format!("{} answered HTTP {code}", uri.host),
    }
}

/// POST one IPP message (plus optional document) and return the response.
pub fn call(uri: &Uri, head: &[u8], doc: Option<&Path>) -> Result<Resp, String> {
    let addr = (uri.host.as_str(), uri.port)
        .to_socket_addrs()
        .map_err(|e| format!("can't find {}: {e}", uri.host))?
        .next()
        .ok_or_else(|| format!("can't find {}", uri.host))?;
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(6))
        .map_err(|e| format!("can't reach {}:{} ({e})", uri.host, uri.port))?;
    tcp.set_read_timeout(Some(Duration::from_secs(180))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(180))).ok();
    let mut sock: Box<dyn Stream> = if uri.tls { Box::new(crate::tls::wrap(uri, tcp)?) } else { Box::new(tcp) };

    let doc_len = match doc {
        Some(p) => std::fs::metadata(p).map_err(|e| e.to_string())?.len(),
        None => 0,
    };
    let host = if uri.host.contains(':') { format!("[{}]", uri.host) } else { uri.host.clone() };
    let req = format!(
        "POST {} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: brrr/0.1\r\nContent-Type: application/ipp\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        uri.path, host, uri.port, head.len() as u64 + doc_len
    );
    let io_err = |e: io::Error| format!("connection to {} died: {e}", uri.host);
    sock.write_all(req.as_bytes()).map_err(io_err)?;
    sock.write_all(head).map_err(io_err)?;
    if let Some(p) = doc {
        let mut f = File::open(p).map_err(|e| e.to_string())?;
        io::copy(&mut f, &mut sock).map_err(io_err)?;
    }
    sock.flush().map_err(io_err)?;

    let mut r = BufReader::new(sock);
    let (code, len, chunked) = loop {
        let mut line = String::new();
        r.read_line(&mut line).map_err(io_err)?;
        let code: u32 = line.split_whitespace().nth(1).and_then(|c| c.parse().ok())
            .ok_or_else(|| format!("{} doesn't speak HTTP on port {}", uri.host, uri.port))?;
        let (mut len, mut chunked) = (None, false);
        loop {
            let mut h = String::new();
            if r.read_line(&mut h).map_err(io_err)? == 0 || h.trim().is_empty() {
                break;
            }
            let l = h.to_ascii_lowercase();
            if let Some(v) = l.strip_prefix("content-length:") {
                len = v.trim().parse::<usize>().ok();
            }
            if l.starts_with("transfer-encoding:") && l.contains("chunked") {
                chunked = true;
            }
        }
        if code != 100 {
            break (code, len, chunked);
        }
    };
    if code != 200 {
        return Err(explain_http(code, uri));
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut sz = String::new();
            r.read_line(&mut sz).map_err(io_err)?;
            let n = usize::from_str_radix(sz.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| "bad chunk from printer".to_string())?;
            if n == 0 {
                break;
            }
            let start = body.len();
            body.resize(start + n, 0);
            r.read_exact(&mut body[start..]).map_err(io_err)?;
            let mut crlf = [0u8; 2];
            r.read_exact(&mut crlf).map_err(io_err)?;
        }
    } else if let Some(n) = len {
        body.resize(n, 0);
        r.read_exact(&mut body).map_err(io_err)?;
    } else {
        r.read_to_end(&mut body).map_err(io_err)?;
    }
    parse(&body)
}

/// Runs a request, dropping to IPP/1.1 for grumpy old printers.
pub fn request(uri: &Uri, build: &dyn Fn((u8, u8)) -> Vec<u8>, doc: Option<&Path>) -> Result<Resp, String> {
    let r = call(uri, &build((2, 0)), doc)?;
    if r.status == 0x0503 {
        return call(uri, &build((1, 1)), doc);
    }
    Ok(r)
}

pub fn get_attrs(uri: &Uri, wanted: &[&str]) -> Result<Group, String> {
    let wanted: Vec<String> = wanted.iter().map(|s| s.to_string()).collect();
    let r = request(uri, &|v| {
        let mut m = Msg::standard(GET_PRINTER_ATTRS, v, uri);
        for (i, w) in wanted.iter().enumerate() {
            m.s(KEYWORD, if i == 0 { "requested-attributes" } else { "" }, w);
        }
        m.finish()
    }, None)?;
    if !r.ok() {
        return Err(format!("printer refused to describe itself ({})", crate::human::status(r.status)));
    }
    Ok(r.printer)
}

// Small attribute helpers used everywhere.
pub fn texts(g: &Group, k: &str) -> Vec<String> {
    g.get(k).map(|v| v.iter().filter_map(|x| match x { Value::Text(s) => Some(s.clone()), _ => None }).collect()).unwrap_or_default()
}
pub fn text(g: &Group, k: &str) -> Option<String> {
    texts(g, k).into_iter().next()
}
pub fn ints(g: &Group, k: &str) -> Vec<i32> {
    g.get(k).map(|v| v.iter().filter_map(|x| match x { Value::Int(i) => Some(*i), _ => None }).collect()).unwrap_or_default()
}
pub fn int(g: &Group, k: &str) -> Option<i32> {
    ints(g, k).into_iter().next()
}
pub fn boolean(g: &Group, k: &str) -> Option<bool> {
    g.get(k).and_then(|v| v.iter().find_map(|x| match x { Value::Bool(b) => Some(*b), _ => None }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_parsing() {
        let u = Uri::parse("ipp://10.0.0.5/ipp/print").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("10.0.0.5", 631, "/ipp/print"));
        let u = Uri::parse("ipp://[fe80::1]:8000/x").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("fe80::1", 8000, "/x"));
        assert!(Uri::parse("ipps://x/y").unwrap().tls);
        assert!(Uri::parse("ftp://x/y").is_err());
    }

    #[test]
    fn additional_values_and_collections() {
        let mut m = Msg::new(0, (2, 0), 1);
        m.group(4);
        m.s(KEYWORD, "sides-supported", "one-sided");
        m.s(KEYWORD, "", "two-sided-long-edge");
        m.i(INTEGER, "copies", 3);
        let r = parse(&m.finish()).unwrap();
        assert_eq!(texts(&r.printer, "sides-supported").len(), 2);
        assert_eq!(int(&r.printer, "copies"), Some(3));
    }

    #[test]
    fn truncated_input_is_an_error_not_a_panic() {
        let mut m = Msg::new(0, (2, 0), 1);
        m.group(4);
        m.s(KEYWORD, "sides-supported", "one-sided");
        let full = m.finish();
        for n in 0..full.len() {
            assert!(parse(&full[..n]).is_err());
        }
    }
}
