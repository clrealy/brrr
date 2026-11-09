//! One-shot mDNS discovery of IPP printers, plus a localhost probe for ipp-usb.
use crate::ipp::{self, Uri};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

const MDNS: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(224, 0, 0, 251), 5353);
const IPP: &str = "_ipp._tcp.local";
const IPPS: &str = "_ipps._tcp.local";

#[derive(Debug, Clone)]
pub struct Printer {
    pub name: String,
    pub uri: Option<Uri>,
    /// Unencrypted address, for printers whose TLS turns out to be broken.
    pub plain: Option<Uri>,
    pub via: &'static str,
}

fn read_name(p: &[u8], mut i: usize) -> Option<(Vec<String>, usize)> {
    let mut labels = Vec::new();
    let mut end = None;
    let mut hops = 0;
    loop {
        let len = *p.get(i)? as usize;
        if len == 0 {
            return Some((labels, end.unwrap_or(i + 1)));
        }
        if len & 0xC0 == 0xC0 {
            let lo = *p.get(i + 1)? as usize;
            end.get_or_insert(i + 2);
            i = ((len & 0x3F) << 8) | lo;
            hops += 1;
            if hops > 32 {
                return None;
            }
            continue;
        }
        labels.push(String::from_utf8_lossy(p.get(i + 1..i + 1 + len)?).into_owned());
        i += 1 + len;
    }
}

fn key(l: &[String]) -> String {
    l.join(".").to_ascii_lowercase()
}

#[derive(Default)]
struct Seen {
    instances: HashMap<String, (String, bool)>, // key -> (display name, is_ipps)
    srv: HashMap<String, (u16, String)>,
    txt: HashMap<String, HashMap<String, String>>,
    a: HashMap<String, Ipv4Addr>,
}

fn ingest(s: &mut Seen, p: &[u8]) -> Option<()> {
    if p.len() < 12 {
        return None;
    }
    let qd = u16::from_be_bytes([p[4], p[5]]) as usize;
    let rr: usize = [6, 8, 10].iter().map(|&n| u16::from_be_bytes([p[n], p[n + 1]]) as usize).sum();
    let mut i = 12;
    for _ in 0..qd {
        let (_, n) = read_name(p, i)?;
        i = n + 4;
    }
    for _ in 0..rr {
        let (name, n) = read_name(p, i)?;
        let ty = u16::from_be_bytes([*p.get(n)?, *p.get(n + 1)?]);
        let len = u16::from_be_bytes([*p.get(n + 8)?, *p.get(n + 9)?]) as usize;
        let d = n + 10;
        p.get(d..d + len)?;
        let k = key(&name);
        match ty {
            12 if k == IPP || k == IPPS => {
                let (inst, _) = read_name(p, d)?;
                let display = inst.first()?.clone();
                s.instances.insert(key(&inst), (display, k == IPPS));
            }
            33 if len > 6 => {
                let port = u16::from_be_bytes([p[d + 4], p[d + 5]]);
                let (target, _) = read_name(p, d + 6)?;
                s.srv.insert(k, (port, key(&target)));
            }
            16 => {
                let mut map = HashMap::new();
                let mut j = d;
                while j < d + len {
                    let l = p[j] as usize;
                    if let Some(e) = p.get(j + 1..(j + 1 + l).min(d + len)) {
                        let e = String::from_utf8_lossy(e);
                        if let Some((a, b)) = e.split_once('=') {
                            map.insert(a.to_ascii_lowercase(), b.to_string());
                        }
                    }
                    j += 1 + l;
                }
                s.txt.insert(k, map);
            }
            1 if len == 4 => {
                s.a.insert(k, Ipv4Addr::new(p[d], p[d + 1], p[d + 2], p[d + 3]));
            }
            _ => {}
        }
        i = d + len;
    }
    Some(())
}

fn query(names: &[(String, u16)], unicast: bool) -> Vec<u8> {
    let mut b = vec![0, 0, 0, 0];
    b.extend_from_slice(&(names.len() as u16).to_be_bytes());
    b.extend_from_slice(&[0; 6]);
    for (name, ty) in names {
        // Split on dots only in the suffix so instance names can contain them.
        let suffix = [IPP, IPPS].iter().find_map(|s| name.strip_suffix(&format!(".{s}")).map(|inst| (inst, *s)));
        let labels: Vec<&str> = match suffix {
            Some((inst, s)) => std::iter::once(inst).chain(s.split('.')).collect(),
            None => name.split('.').collect(),
        };
        for l in labels {
            b.push(l.len().min(63) as u8);
            b.extend_from_slice(&l.as_bytes()[..l.len().min(63)]);
        }
        b.push(0);
        b.extend_from_slice(&ty.to_be_bytes());
        b.extend_from_slice(&(if unicast { 0x8001u16 } else { 1 }).to_be_bytes());
    }
    b
}

fn local_ips() -> Vec<Ipv4Addr> {
    if_addrs::get_if_addrs().unwrap_or_default().into_iter()
        .filter_map(|i| match i.ip() { std::net::IpAddr::V4(v) => Some(v), _ => None }).collect()
}

/// One throwaway socket per interface. Replies come back unicast, which gets
/// through most firewalls, but responders may truncate them on busy networks.
fn oneshot_sockets() -> Vec<UdpSocket> {
    local_ips().into_iter().filter_map(|ip| {
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).ok()?;
        s.bind(&SocketAddrV4::new(ip, 0).into()).ok()?;
        s.set_multicast_if_v4(&ip).ok()?;
        s.set_multicast_ttl_v4(255).ok()?;
        s.set_nonblocking(true).ok()?;
        Some(UdpSocket::from(s))
    }).collect()
}

/// A real port-5353 listener, shared with avahi, for complete multicast answers.
fn multicast_socket() -> Option<Socket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).ok()?;
    s.set_reuse_address(true).ok()?;
    s.set_reuse_port(true).ok()?;
    s.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 5353).into()).ok()?;
    s.set_multicast_ttl_v4(255).ok()?;
    s.set_nonblocking(true).ok()?;
    Some(s)
}

/// ipp-usb exposes USB printers as plain IPP on these localhost ports.
fn probe_usb() -> Vec<Printer> {
    (60000..60004u16).filter_map(|port| {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        TcpStream::connect_timeout(&addr, Duration::from_millis(120)).ok()?;
        let uri = Uri { host: "127.0.0.1".into(), port, path: "/ipp/print".into(), tls: false, id: format!("usb:{port}") };
        let a = ipp::get_attrs(&uri, &["printer-make-and-model", "printer-name"]).ok()?;
        let name = ipp::text(&a, "printer-make-and-model").or_else(|| ipp::text(&a, "printer-name"))?;
        Some(Printer { name, uri: Some(uri), plain: None, via: "usb" })
    }).collect()
}

pub fn discover(wait: Duration) -> Vec<Printer> {
    let socks = oneshot_sockets();
    let mc = multicast_socket();
    let ips = local_ips();
    if let Some(m) = &mc {
        for ip in &ips {
            let _ = m.join_multicast_v4(&Ipv4Addr::new(224, 0, 0, 251), ip);
        }
    }
    let mc_recv = mc.as_ref().and_then(|m| m.try_clone().ok()).map(UdpSocket::from);
    let mut seen = Seen::default();
    let start = Instant::now();
    let mut next_send = start;
    let mut buf = [0u8; 9000];
    while start.elapsed() < wait {
        if Instant::now() >= next_send {
            let mut q = vec![(IPP.to_string(), 12), (IPPS.to_string(), 12)];
            for (inst, (display, ipps)) in &seen.instances {
                let full = format!("{display}.{}", if *ipps { IPPS } else { IPP });
                if !seen.srv.contains_key(inst) { q.push((full.clone(), 33)); }
                if !seen.txt.contains_key(inst) { q.push((full, 16)); }
            }
            let hosts: HashSet<_> = seen.srv.values().map(|(_, h)| h.clone()).collect();
            q.extend(hosts.into_iter().filter(|h| !seen.a.contains_key(h)).map(|h| (h, 1)));
            let pkt = query(&q, true);
            for s in &socks {
                let _ = s.send_to(&pkt, MDNS);
            }
            if let Some(m) = &mc {
                let pkt = query(&q, false);
                for ip in &ips {
                    if m.set_multicast_if_v4(ip).is_ok() {
                        let _ = m.send_to(&pkt, &MDNS.into());
                    }
                }
            }
            next_send = Instant::now() + Duration::from_millis(350);
        }
        let mut got = false;
        for s in socks.iter().chain(mc_recv.iter()) {
            while let Ok((n, _)) = s.recv_from(&mut buf) {
                got = true;
                let _ = ingest(&mut seen, &buf[..n]);
            }
        }
        if !got {
            std::thread::sleep(Duration::from_millis(15));
        }
    }
    let mut out = probe_usb();
    // Group each printer's ipp and ipps adverts under its name.
    let mut by_name: HashMap<String, (String, Option<Uri>, Option<Uri>)> = HashMap::new();
    for (k, (display, ipps)) in &seen.instances {
        let e = by_name.entry(display.to_ascii_lowercase()).or_insert((display.clone(), None, None));
        let uri = seen.srv.get(k).and_then(|(port, host)| {
            let ip = seen.a.get(host)?;
            let txt = seen.txt.get(k);
            let rp = txt.and_then(|t| t.get("rp")).map(|s| s.as_str()).unwrap_or("ipp/print");
            let id = txt.and_then(|t| t.get("uuid")).cloned().unwrap_or_else(|| display.to_ascii_lowercase());
            Some(Uri { host: ip.to_string(), port: *port, path: format!("/{rp}"), tls: *ipps, id })
        });
        if *ipps { e.2 = uri; } else { e.1 = uri; }
    }
    for (_, (name, plain, tls)) in by_name {
        // Encrypted wins whenever the printer offers it.
        let (uri, via, plain) = match (tls, plain) {
            (Some(t), p) => (Some(t), "encrypted", p),
            (None, Some(p)) => (Some(p), "network", None),
            (None, None) => (None, "network", None),
        };
        out.push(Printer { name, uri, plain, via });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn nm(b: &mut Vec<u8>, s: &[&str]) {
        for l in s { b.push(l.len() as u8); b.extend_from_slice(l.as_bytes()); }
        b.push(0);
    }
    fn rr(b: &mut Vec<u8>, name: &[&str], ty: u16, data: &[u8]) {
        nm(b, name);
        b.extend_from_slice(&ty.to_be_bytes());
        b.extend_from_slice(&[0x80, 1, 0, 0, 0, 120]);
        b.extend_from_slice(&(data.len() as u16).to_be_bytes());
        b.extend_from_slice(data);
    }

    pub fn answer(inst: &str, port: u16, ip: [u8; 4]) -> Vec<u8> {
        let svc = ["_ipp", "_tcp", "local"];
        let full = [inst, "_ipp", "_tcp", "local"];
        let mut b = vec![0, 0, 0x84, 0, 0, 0, 0, 4, 0, 0, 0, 0];
        let mut d = vec![]; nm(&mut d, &full);
        rr(&mut b, &svc, 12, &d);
        let mut d = vec![0,0,0,0]; d.extend_from_slice(&port.to_be_bytes()); nm(&mut d, &["box", "local"]);
        rr(&mut b, &full, 33, &d);
        let t = b"rp=ipp/print"; let mut d = vec![t.len() as u8]; d.extend_from_slice(t);
        rr(&mut b, &full, 16, &d);
        rr(&mut b, &["box", "local"], 1, &ip);
        b
    }

    #[test]
    fn parses_a_full_answer() {
        let mut s = Seen::default();
        ingest(&mut s, &answer("Office. Printer", 631, [10, 0, 0, 9])).unwrap();
        let (name, ipps) = s.instances.values().next().unwrap();
        assert_eq!(name, "Office. Printer");
        assert!(!ipps);
        let k = s.instances.keys().next().unwrap();
        assert_eq!(s.srv[k], (631, "box.local".to_string()));
        assert_eq!(s.txt[k]["rp"], "ipp/print");
        assert_eq!(s.a["box.local"], Ipv4Addr::new(10, 0, 0, 9));
    }

    #[test]
    fn pointer_loop_does_not_hang() {
        let p = [0u8, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0xC0, 12];
        let mut s = Seen::default();
        assert!(ingest(&mut s, &p).is_none());
    }

    #[test]
    fn garbage_never_panics() {
        let base = answer("x", 631, [1, 2, 3, 4]);
        for n in 0..base.len() {
            let _ = ingest(&mut Seen::default(), &base[..n]);
            let mut b = base.clone();
            b[n] ^= 0xFF;
            let _ = ingest(&mut Seen::default(), &b);
        }
    }
}
