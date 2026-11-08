// The region server's link to the sidecar, on its own thread. The tick only ever reads one atomic, so a slow or dead
// sidecar cannot touch the game (rule 7): the last known value stays and the next poll tries again.
//
// Only `std`, because the mod cannot add dependencies to Horizon's lockfile. Also built as the `vexmap-link` crate,
// whose tests run this code against the real sidecar.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const DEFAULT_ADDR: &str = "127.0.0.1:7878";
const GLOBAL_REGION: &str = "global";
const TIMEOUT: Duration = Duration::from_secs(2);
const POLL_EVERY: Duration = Duration::from_secs(5);
/// Far above any real answer: a bound, so a confused peer cannot make this thread buffer without limit.
const MAX_RESPONSE: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub addr: SocketAddr,
    pub region: String,
    pub every: Duration,
}

impl Config {
    /// From the values of `VEXMAP_API_ADDR` and `VEXMAP_REGION`. Both end up in a request, so both are checked.
    pub fn parse(addr: Option<&str>, region: Option<&str>) -> Result<Self, String> {
        let addr_text = addr.unwrap_or(DEFAULT_ADDR);
        let addr: SocketAddr =
            addr_text.parse().map_err(|_| format!("VEXMAP_API_ADDR is not an address: {addr_text}"))?;
        if !addr.ip().is_loopback() {
            return Err(format!("VEXMAP_API_ADDR must be a loopback address, got {addr}"));
        }
        let region = region.unwrap_or(GLOBAL_REGION);
        let valid = !region.is_empty()
            && region.len() <= 32
            && region.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
        if !valid {
            return Err("VEXMAP_REGION must be 1-32 characters of a-z, 0-9, - and _".to_owned());
        }
        Ok(Config { addr, region: region.to_owned(), every: POLL_EVERY })
    }
}

/// The module mask out of a raw HTTP response from `GET /v1/features`. Only a 200 counts. The JSON is searched rather
/// than parsed: the other fields are a validated region and fixed module names, so `"mask":` cannot occur inside them.
pub fn parse_mask(response: &[u8]) -> Result<u64, String> {
    let text = std::str::from_utf8(response).map_err(|_| "response is not UTF-8".to_owned())?;
    let (head, body) = text.split_once("\r\n\r\n").ok_or("response has no body")?;
    let status = head.lines().next().unwrap_or("");
    if status.split(' ').nth(1) != Some("200") {
        return Err(format!("sidecar answered: {status}"));
    }
    let (_, rest) = body.split_once("\"mask\":").ok_or("response has no mask")?;
    let digits = rest.trim_start();
    let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
    digits[..end].parse().map_err(|_| "mask is not a number".to_owned())
}

/// One blocking request. Never call this on the tick.
pub fn fetch_mask(config: &Config) -> Result<u64, String> {
    let failed = |error: std::io::Error| error.to_string();
    let mut stream = TcpStream::connect_timeout(&config.addr, TIMEOUT).map_err(failed)?;
    stream.set_read_timeout(Some(TIMEOUT)).map_err(failed)?;
    stream.set_write_timeout(Some(TIMEOUT)).map_err(failed)?;
    let request = format!(
        "GET /v1/features?region={} HTTP/1.1\r\nHost: {}\r\nX-VexMap-Client: region-server\r\nConnection: close\r\n\r\n",
        config.region, config.addr
    );
    stream.write_all(request.as_bytes()).map_err(failed)?;
    let mut response = Vec::new();
    stream.take(MAX_RESPONSE).read_to_end(&mut response).map_err(failed)?;
    parse_mask(&response)
}

/// Starts the thread that keeps `features` current. It logs changes of state only, not every poll.
pub fn start(config: Config, features: &'static AtomicU64) -> std::io::Result<()> {
    std::thread::Builder::new().name("vexmap-sidecar".to_owned()).spawn(move || {
        let mut last = None;
        loop {
            let result = fetch_mask(&config);
            if let Ok(mask) = result {
                features.store(mask, Ordering::Relaxed);
            }
            if last.as_ref() != Some(&result) {
                match &result {
                    Ok(mask) => eprintln!("[vexmap] sidecar: region {} modules {mask:#x}", config.region),
                    Err(error) => eprintln!("[vexmap] sidecar unavailable, keeping the last modules: {error}"),
                }
                last = Some(result);
            }
            std::thread::sleep(config.every);
        }
    })?;
    Ok(())
}
