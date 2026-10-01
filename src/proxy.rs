//! The proxy behind `network` in chest.toml, the only way out of a restricted VM.
//!
//! qemu runs `luggage network-proxy <file>` with each guest connection to `GUEST` on stdin and stdout; it serves
//! one HTTP request or `CONNECT` tunnel to a host in the allowlist `<file>`.

use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
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

/// Is `entry` a host name for `network`: labels of letters, digits and `-`, separated by dots?
pub fn is_host(entry: &str) -> bool {
    entry.len() <= 253
        && entry
            .split('.')
            .all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

/// May the guest reach `host`? An entry allows the host itself and every subdomain of it.
/// Only host names count: anything else would go to the resolver of the host as it is.
fn allows(allowed: &[String], host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    is_host(&host)
        && allowed.iter().map(|a| a.to_ascii_lowercase()).any(|a| {
            host == a || host.strip_suffix(a.as_str()).is_some_and(|sub| sub.ends_with('.'))
        })
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

/// Answers the guest with an error and a short text.
fn refuse(client: &mut UnixStream, status: &str, text: &str) {
    let text = format!("luggage: {text}\n");
    let _ = write!(
        client,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
}

/// Connects to `host` on the host side; the guest has no name resolution of its own.
fn connect(host: &str, port: u16) -> std::io::Result<TcpStream> {
    let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, "no address");
    for addr in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, TIMEOUT) {
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
    let allowed: Vec<String> =
        fs::read_to_string(file).unwrap_or_default().lines().map(str::to_owned).collect();
    if !allows(&allowed, host) {
        let text = format!("{host} is not in `network` of chest.toml");
        refuse(&mut client, "403 Forbidden", &text);
        return Ok(());
    }
    let mut server = match connect(host, *port) {
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

    #[test]
    fn hosts_and_subdomains_are_allowed() {
        let allowed = ["github.com".to_owned(), "Packagist.org".to_owned()];
        assert!(allows(&allowed, "github.com"));
        assert!(allows(&allowed, "api.github.com"));
        assert!(allows(&allowed, "repo.packagist.org."));
        assert!(!allows(&allowed, "evilgithub.com"));
        assert!(!allows(&allowed, "github.com.evil.org"));
        assert!(!allows(&[], "github.com"));
    }

    #[test]
    fn only_host_names_are_allowed() {
        let allowed = ["github.com".to_owned()];
        for host in [
            "evil@x.github.com",
            "::1%.github.com",
            "127.0.0.1\t.github.com",
            "x..github.com",
            "github.com..",
            "ä.github.com",
        ] {
            assert!(!allows(&allowed, host), "{host:?}");
        }
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
