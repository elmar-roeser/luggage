//! The proxy behind `network` in chest.toml, the only way out of a restricted VM.
//!
//! qemu runs `luggage network-proxy <file>` with each guest connection to `GUEST` on stdin and stdout; it serves
//! one HTTP request or `CONNECT` tunnel to a host in the allowlist `<file>`.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;

/// Address of the proxy in the guest; qemu forwards it to this program.
pub const GUEST: &str = "10.0.2.100:3128";
/// Longest request head the proxy reads.
const HEAD_LIMIT: usize = 64 * 1024;
/// How long the guest may take to send the whole request head, and the proxy to reach the target.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Most proxy processes that may run at the same time for one chest.
const SLOTS: usize = 256;
/// Ports that an entry without its own port allows.
const DEFAULT_PORTS: [u16; 2] = [80, 443];

/// Is `entry` a host name for `network`: labels of letters, digits and `-`, separated by dots?
fn is_host(entry: &str) -> bool {
    entry.len() <= 253
        && entry
            .split('.')
            .all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

/// What an entry of `network` names.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    /// A host name and all its subdomains
    Name(String),
    /// Exactly this address
    Ip(IpAddr),
}

/// An entry of `network`: a target and the one port it allows, or 80 and 443 without a port.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    /// The host name or address
    target: Target,
    /// The only allowed port, if any
    port: Option<u16>,
}

/// A port from 1 to 65535, digits only.
fn port(p: &str) -> Option<u16> {
    p.bytes().all(|b| b.is_ascii_digit()).then(|| p.parse().ok()).flatten().filter(|&p| p != 0)
}

/// Reads an entry of `network`: `host`, `host:port`, an IP address, `ip:port` or `[v6]:port`.
fn entry(s: &str) -> Option<Entry> {
    if let Ok(ip) = s.parse() {
        return Some(Entry { target: Target::Ip(ip), port: None });
    }
    if let Some(rest) = s.strip_prefix('[') {
        let (ip, tail) = rest.split_once(']')?;
        let port = if tail.is_empty() { None } else { Some(port(tail.strip_prefix(':')?)?) };
        return Some(Entry { target: Target::Ip(IpAddr::V6(ip.parse().ok()?)), port });
    }
    let (host, port) = match s.rsplit_once(':') {
        Some((h, p)) => (h, Some(port(p)?)),
        None => (s, None),
    };
    let target = match host.parse() {
        Ok(ip @ IpAddr::V4(_)) => Target::Ip(ip),
        _ if is_host(host) => Target::Name(host.to_ascii_lowercase()),
        _ => return None,
    };
    Some(Entry { target, port })
}

/// Is `entry` valid in `network`?
pub fn is_entry(entry: &str) -> bool {
    self::entry(entry).is_some()
}

/// May the guest reach `host` on `port`? `None` if not, else whether private addresses are fine too.
/// Only an IP address or `localhost` in `network` opens private addresses: a name could point anywhere.
fn allows(entries: &[Entry], host: &str, port: u16) -> Option<bool> {
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    let ip = host.parse::<IpAddr>().ok();
    entries
        .iter()
        .filter(|e| e.port.map_or(DEFAULT_PORTS.contains(&port), |p| p == port))
        .filter_map(|e| match &e.target {
            Target::Ip(a) => (ip == Some(*a)).then_some(true),
            Target::Name(n) => (ip.is_none()
                && is_host(&host)
                && (host == *n || host.strip_suffix(n.as_str()).is_some_and(|s| s.ends_with('.'))))
            .then_some(n == "localhost"),
        })
        .max()
}

/// Can every host reach `ip` the same way, or is it loopback, the LAN, link-local or otherwise special?
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            let [first, second, ..] = a.octets();
            !(a.is_loopback()
                || a.is_private()
                || a.is_link_local()
                || a.is_unspecified()
                || a.is_multicast()
                || a.is_broadcast()
                || first == 0
                || first >= 240
                || (first == 100 && second & 0xc0 == 64))
        }
        IpAddr::V6(a) => {
            if let Some(v4) = a.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let first = a.segments()[0];
            // ::/96 holds ::, ::1 and the old IPv4-compatible addresses
            !(a.segments()[..6].iter().all(|&s| s == 0)
                || a.is_multicast()
                || first & 0xfe00 == 0xfc00
                || first & 0xffc0 == 0xfe80
                || first & 0xffc0 == 0xfec0)
        }
    }
}

/// Resolves `host` once and keeps the addresses the guest may reach; private ones only with `private`.
/// The proxy connects to exactly these, so a second lookup cannot swap in another address.
fn addresses(host: &str, port: u16, private: bool) -> std::io::Result<Vec<SocketAddr>> {
    Ok((host, port).to_socket_addrs()?.filter(|a| private || is_public(a.ip())).collect())
}

/// Host and port of `authority` (`host:port`, `[v6]:port` or `host`).
fn host_port(authority: &str, default: u16) -> Option<(String, u16)> {
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !p.contains(']') => (h, p.parse().ok()?),
        _ => (authority, default),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    (!host.is_empty()).then(|| (host.to_owned(), port))
}

/// What the guest asks for.
#[derive(Debug, PartialEq, Eq)]
enum Request {
    /// A tunnel to host and port (HTTPS, `CONNECT host:443`)
    Tunnel(String, u16),
    /// A plain HTTP request to host and port; the head is already rewritten for the server
    Plain(String, u16, Vec<u8>),
}

/// Reads the request head: the request line and the headers.
/// Plain requests go to the server with the path only and `Connection: close`, because one process serves one request.
fn parse(head: &str) -> Option<Request> {
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, target, version) = (first.next()?, first.next()?, first.next()?);
    if method == "CONNECT" {
        let (host, port) = host_port(target, 443)?;
        return Some(Request::Tunnel(host, port));
    }
    let rest = target.strip_prefix("http://")?;
    let (authority, path) = rest.find('/').map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
    let (host, port) = host_port(authority, 80)?;
    let mut out = format!("{method} {path} {version}\r\n");
    for line in lines.filter(|l| !l.is_empty()) {
        let name = line.split(':').next().unwrap_or_default().to_ascii_lowercase();
        if !["connection", "proxy-connection", "keep-alive", "proxy-authorization"]
            .contains(&name.as_str())
        {
            out.push_str(line);
            out.push_str("\r\n");
        }
    }
    out.push_str("Connection: close\r\n\r\n");
    Some(Request::Plain(host, port, out.into_bytes()))
}

/// Reads up to the end of the request head within `within`; returns the head and what came after it.
fn read_head(client: &mut UnixStream, within: Duration) -> Option<(String, Vec<u8>)> {
    let deadline = Instant::now() + within;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(end + 4);
            return Some((String::from_utf8_lossy(&buf[..end]).into_owned(), rest));
        }
        if buf.len() > HEAD_LIMIT {
            return None;
        }
        // a timeout per read alone would let a guest that sends a byte now and then hold the process forever
        let left = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero())?;
        client.set_read_timeout(Some(left)).ok()?;
        let n = client.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Locks one of `max` slot files in `dir`, or none if all are taken.
/// The lock ends with the process, also when it crashes, so no slot is ever lost.
fn take_slot(dir: &Path, max: usize) -> Option<File> {
    fs::create_dir_all(dir).ok()?;
    (0..max).find_map(|i| {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(i.to_string()))
            .ok()?;
        file.try_lock().ok().map(|()| file)
    })
}

/// Answers the guest with an error and a short text.
fn refuse(client: &mut UnixStream, status: &str, text: &str) {
    let text = format!("luggage: {text}\n");
    let _ = write!(
        client,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
}

/// Connects to the first of `addrs` that answers.
fn connect(addrs: &[SocketAddr]) -> std::io::Result<TcpStream> {
    let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, "no address");
    for addr in addrs {
        match TcpStream::connect_timeout(addr, TIMEOUT) {
            Ok(s) => return Ok(s),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Serves the one connection on stdin and stdout with the allowlist in `file`.
/// It never fails loudly: stderr is the connection to the guest as well.
///
/// # Errors
/// If stdin is not a socket.
pub fn serve(file: &Path) -> Result<()> {
    let mut client = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned()?);
    let Some(_slot) = take_slot(&file.with_file_name("proxy-slots"), SLOTS) else {
        refuse(&mut client, "503 Service Unavailable", "too many connections at once");
        return Ok(());
    };
    let Some((head, rest)) = read_head(&mut client, TIMEOUT) else {
        refuse(&mut client, "400 Bad Request", "cannot read the request");
        return Ok(());
    };
    client.set_read_timeout(None)?;
    let Some(request) = parse(&head) else {
        refuse(&mut client, "400 Bad Request", "only http:// requests and CONNECT go through");
        return Ok(());
    };
    let (Request::Tunnel(host, port) | Request::Plain(host, port, _)) = &request;
    let entries: Vec<Entry> =
        fs::read_to_string(file).unwrap_or_default().lines().filter_map(entry).collect();
    let Some(private) = allows(&entries, host, *port) else {
        let text = format!("{host}:{port} is not in `network` of chest.toml");
        refuse(&mut client, "403 Forbidden", &text);
        return Ok(());
    };
    let addrs = match addresses(host, *port, private) {
        Ok(a) if a.is_empty() => {
            let text = format!("{host} has only private addresses; list the address in `network`");
            refuse(&mut client, "403 Forbidden", &text);
            return Ok(());
        }
        Ok(a) => a,
        Err(e) => {
            refuse(&mut client, "502 Bad Gateway", &format!("{host}:{port}: {e}"));
            return Ok(());
        }
    };
    let mut server = match connect(&addrs) {
        Ok(s) => s,
        Err(e) => {
            refuse(&mut client, "502 Bad Gateway", &format!("{host}:{port}: {e}"));
            return Ok(());
        }
    };
    match &request {
        Request::Tunnel(..) => client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?,
        Request::Plain(_, _, head) => server.write_all(head)?,
    }
    server.write_all(&rest)?;
    let (mut up_from, mut up_to) = (client.try_clone()?, server.try_clone()?);
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut up_from, &mut up_to);
        let _ = up_to.shutdown(Shutdown::Write);
    });
    let _ = std::io::copy(&mut server, &mut client);
    let _ = client.shutdown(Shutdown::Write);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(list: &[&str]) -> Vec<Entry> {
        list.iter().map(|e| entry(e).unwrap()).collect()
    }

    #[test]
    fn hosts_and_subdomains_are_allowed() {
        let allowed = entries(&["github.com", "Packagist.org"]);
        assert_eq!(allows(&allowed, "github.com", 443), Some(false));
        assert_eq!(allows(&allowed, "api.github.com", 80), Some(false));
        assert_eq!(allows(&allowed, "repo.packagist.org.", 443), Some(false));
        assert_eq!(allows(&allowed, "evilgithub.com", 443), None);
        assert_eq!(allows(&allowed, "github.com.evil.org", 443), None);
        assert_eq!(allows(&[], "github.com", 443), None);
    }

    #[test]
    fn only_host_names_are_allowed() {
        let allowed = entries(&["github.com"]);
        for host in [
            "evil@x.github.com",
            "::1%.github.com",
            "127.0.0.1\t.github.com",
            "x..github.com",
            "github.com..",
            "ä.github.com",
        ] {
            assert_eq!(allows(&allowed, host, 443), None, "{host:?}");
        }
    }

    #[test]
    fn ports_are_80_and_443_unless_listed() {
        let allowed = entries(&["github.com", "git.example.com:22"]);
        assert_eq!(allows(&allowed, "github.com", 22), None);
        assert_eq!(allows(&allowed, "github.com", 8443), None);
        assert_eq!(allows(&allowed, "git.example.com", 22), Some(false));
        assert_eq!(allows(&allowed, "a.git.example.com", 22), Some(false));
        assert_eq!(allows(&allowed, "git.example.com", 443), None);
        assert_eq!(allows(&allowed, "example.com", 22), None);
    }

    #[test]
    fn only_addresses_and_localhost_open_private_targets() {
        let allowed = entries(&["localhost", "10.0.0.5", "[::1]:8080", "192.168.1.2:8000"]);
        assert_eq!(allows(&allowed, "localhost", 443), Some(true));
        assert_eq!(allows(&allowed, "10.0.0.5", 80), Some(true));
        assert_eq!(allows(&allowed, "::1", 8080), Some(true));
        assert_eq!(allows(&allowed, "::1", 443), None);
        assert_eq!(allows(&allowed, "192.168.1.2", 8000), Some(true));
        assert_eq!(allows(&allowed, "x.10.0.0.5", 80), None);
        assert_eq!(allows(&allowed, "127.0.0.1", 443), None);
        // a name entry that looks like the end of an address does not match the address
        assert_eq!(allows(&entries(&["0.0.5"]), "10.0.0.5", 443), None);
    }

    #[test]
    fn parses_entries() {
        let name = |n: &str, port| Some(Entry { target: Target::Name(n.into()), port });
        let ip = |a: &str, port| Some(Entry { target: Target::Ip(a.parse().unwrap()), port });
        assert_eq!(entry("GitHub.com"), name("github.com", None));
        assert_eq!(entry("git.example.com:22"), name("git.example.com", Some(22)));
        assert_eq!(entry("localhost:8080"), name("localhost", Some(8080)));
        assert_eq!(entry("127.0.0.1"), ip("127.0.0.1", None));
        assert_eq!(entry("127.0.0.1:8080"), ip("127.0.0.1", Some(8080)));
        assert_eq!(entry("::1"), ip("::1", None));
        assert_eq!(entry("[::1]"), ip("::1", None));
        assert_eq!(entry("[fd00::1]:8080"), ip("fd00::1", Some(8080)));
        for bad in [
            "",
            "*.github.com",
            ".github.com",
            "https://github.com",
            "github.com:",
            "github.com:0",
            "github.com:65536",
            "github.com:+22",
            "github.com:22:22",
            "[::1]8080",
            "[::1]:",
            "[127.0.0.1]:80",
            "[github.com]:80",
        ] {
            assert_eq!(entry(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn private_addresses_are_not_public() {
        for a in [
            "127.0.0.1",
            "127.1.2.3",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "169.254.169.254",
            "0.0.0.0",
            "0.1.2.3",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::127.0.0.1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "febf::1",
            "fec0::1",
            "ff02::1",
        ] {
            assert!(!is_public(a.parse().unwrap()), "{a}");
        }
        for a in [
            "1.1.1.1",
            "140.82.121.4",
            "172.32.0.1",
            "100.128.0.1",
            "2606:4700::1111",
            "::ffff:1.1.1.1",
        ] {
            assert!(is_public(a.parse().unwrap()), "{a}");
        }
    }

    #[test]
    fn private_addresses_need_an_explicit_entry() {
        assert_eq!(addresses("localhost", 443, false).unwrap(), []);
        assert_eq!(addresses("127.0.0.1", 443, false).unwrap(), []);
        let ok = addresses("127.0.0.1", 8080, true).unwrap();
        assert_eq!(ok, [SocketAddr::from(([127, 0, 0, 1], 8080))]);
        assert_eq!(addresses("1.1.1.1", 443, false).unwrap().len(), 1);
    }

    #[test]
    fn slots_run_out_and_come_back() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().join("slots");
        let mut taken: Vec<File> = (0..3).map(|_| take_slot(&dir, 3).unwrap()).collect();
        assert!(take_slot(&dir, 3).is_none());
        taken.pop();
        assert!(take_slot(&dir, 3).is_some());
    }

    #[test]
    fn a_slow_head_ends_in_time() {
        let (mut client, mut guest) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            while guest.write_all(b"G").is_ok() {
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let start = Instant::now();
        assert_eq!(read_head(&mut client, Duration::from_millis(200)), None);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn host_names() {
        assert!(is_host("packagist.org"));
        assert!(is_host("localhost"));
        assert!(!is_host(""));
        assert!(!is_host("*.github.com"));
        assert!(!is_host(".github.com"));
        assert!(!is_host("https://github.com"));
        assert!(!is_host("github.com:443"));
    }

    #[test]
    fn parses_tunnels() {
        let r = parse("CONNECT repo.packagist.org:443 HTTP/1.1\r\nHost: repo.packagist.org:443");
        assert_eq!(r, Some(Request::Tunnel("repo.packagist.org".into(), 443)));
        let r = parse("CONNECT [::1]:8443 HTTP/1.1");
        assert_eq!(r, Some(Request::Tunnel("::1".into(), 8443)));
    }

    #[test]
    fn plain_requests_get_the_path_and_close() {
        let head = "GET http://example.com:8080/a?b HTTP/1.1\r\nHost: example.com:8080\r\nProxy-Connection: keep-alive\r\nAccept: */*";
        let out = "GET /a?b HTTP/1.1\r\nHost: example.com:8080\r\nAccept: */*\r\nConnection: close\r\n\r\n";
        assert_eq!(parse(head), Some(Request::Plain("example.com".into(), 8080, out.into())));
        let out = "GET / HTTP/1.0\r\nConnection: close\r\n\r\n";
        assert_eq!(
            parse("GET http://example.com HTTP/1.0"),
            Some(Request::Plain("example.com".into(), 80, out.into()))
        );
        assert_eq!(parse("GET /local HTTP/1.1"), None);
    }
}
