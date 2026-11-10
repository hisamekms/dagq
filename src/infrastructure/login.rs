//! Terminal interaction for public OIDC login. The loopback listener has an
//! ephemeral port, accepts only /callback and lives at most five minutes.
use crate::application::login::{LoginInteraction, Params};
use anyhow::{Result, ensure};
use std::{
    io::{Read, Write},
    net::TcpListener,
    time::{Duration, Instant, SystemTime},
};

pub fn form_encode(params: &Params) -> String {
    fn encode(s: &str) -> String {
        let mut out = String::new();
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                b' ' => out.push('+'),
                _ => {
                    use std::fmt::Write;
                    let _ = write!(out, "%{b:02X}");
                }
            }
        }
        out
    }
    params
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}
pub fn form_decode(query: &str) -> Result<Params> {
    fn decode(s: &str) -> Result<String> {
        let mut bytes = Vec::new();
        let mut source = s.bytes();
        while let Some(b) = source.next() {
            bytes.push(match b {
                b'+' => b' ',
                b'%' => {
                    let a = source.next().and_then(|b| (b as char).to_digit(16));
                    let b = source.next().and_then(|b| (b as char).to_digit(16));
                    let (Some(a), Some(b)) = (a, b) else {
                        anyhow::bail!("invalid redirect encoding")
                    };
                    (a * 16 + b) as u8
                }
                _ => b,
            });
        }
        String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("invalid redirect encoding"))
    }
    query
        .split('&')
        .map(|part| {
            let (k, v) = part.split_once('=').unwrap_or((part, ""));
            Ok((decode(k)?, decode(v)?))
        })
        .collect()
}
pub struct TerminalLogin;
impl LoginInteraction for TerminalLogin {
    fn now(&self) -> i64 {
        crate::application::unix_seconds(SystemTime::now())
    }
    fn sleep(&self, seconds: u64) {
        std::thread::sleep(Duration::from_secs(seconds));
    }
    fn entropy(&self) -> Result<[u8; 32]> {
        let mut bytes = [0; 32];
        getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("login entropy unavailable"))?;
        Ok(bytes)
    }
    fn device(&self, url: &str, user_code: &str) -> Result<()> {
        // A verification URL and a user code are the intended human prompt,
        // never a device_code, token, or verification_uri_complete.
        eprintln!("Open {url} and enter code {user_code}");
        Ok(())
    }
    fn authorize(&self, endpoint: &str, params: &Params, state: &str) -> Result<(String, String)> {
        let listener = TcpListener::bind(crate::application::login::callback_address())?;
        listener.set_nonblocking(true)?;
        let redirect_uri = format!(
            "http://127.0.0.1:{}/callback",
            listener.local_addr()?.port()
        );
        let mut params = params.clone();
        params.push(("redirect_uri".into(), redirect_uri.clone()));
        let separator = if endpoint.contains('?') { '&' } else { '?' };
        let url = format!("{endpoint}{separator}{}", form_encode(&params));
        #[cfg(target_os = "macos")]
        let program = "open";
        #[cfg(not(target_os = "macos"))]
        let program = "xdg-open";
        let mut browser = std::process::Command::new(program)
            .arg(&url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|_| anyhow::anyhow!("cannot open login browser"))?;
        let end = Instant::now() + Duration::from_secs(300);
        let result = (|| -> Result<(String, String)> {
            let mut stream = loop {
                ensure!(Instant::now() < end, "login redirect timed out");
                match listener.accept() {
                    Ok((stream, peer)) => {
                        ensure!(
                            peer.ip().is_loopback(),
                            "login redirect peer is not loopback"
                        );
                        break stream;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(25))
                    }
                    Err(_) => anyhow::bail!("login redirect unavailable"),
                }
            };
            // Accepted sockets inherit the listener's O_NONBLOCK on macOS.
            stream.set_nonblocking(false)?;
            stream.set_read_timeout(Some(Duration::from_secs(10)))?;
            stream.set_write_timeout(Some(Duration::from_secs(10)))?;
            let mut head = Vec::new();
            while head.len() < 16 * 1024 && !head.ends_with(b"\r\n\r\n") {
                ensure!(Instant::now() < end, "login redirect timed out");
                let mut byte = [0];
                ensure!(stream.read(&mut byte)? == 1, "incomplete login redirect");
                head.push(byte[0]);
            }
            ensure!(head.ends_with(b"\r\n\r\n"), "login redirect too large");
            let head = std::str::from_utf8(&head)
                .map_err(|_| anyhow::anyhow!("invalid login redirect"))?;
            let path = head
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("GET "))
                .and_then(|line| line.strip_suffix(" HTTP/1.1"))
                .ok_or_else(|| anyhow::anyhow!("invalid login redirect method"))?;
            let query = path
                .strip_prefix("/callback?")
                .ok_or_else(|| anyhow::anyhow!("invalid login redirect path"))?;
            let code = crate::application::login::redirect(&form_decode(query)?, state);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 30\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nReturn to the dagq terminal.\r\n");
            Ok((code?, redirect_uri))
        })();
        // Do not leave even a hung browser launcher behind the CLI.
        if browser.try_wait()?.is_none() {
            let _ = browser.kill();
        }
        let _ = browser.wait();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authorization_values_are_form_encoded_and_redirect_encoding_is_strict() {
        let params = vec![
            ("scope".into(), "openid profile".into()),
            ("value".into(), "a&=+日本語".into()),
        ];
        let encoded = form_encode(&params);
        assert!(encoded.contains("scope=openid+profile"));
        assert!(encoded.contains("a%26%3D%2B"));
        assert_eq!(form_decode(&encoded).unwrap(), params);
        for malformed in ["x=%", "x=%xz", "x=%FF"] {
            assert!(form_decode(malformed).is_err());
        }
    }
}
