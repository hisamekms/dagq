//! The HTTPS fetching of an OIDC issuer's documents against a real TLS
//! listener on loopback (boundary oidc-keys-fetched-from-issuer-over-tcp).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use dagq::application::oidc::{OidcDocumentSource, verify_jwt};
use dagq::domain::oidc::{
    FETCH_TIMEOUT_SECS, KeyFetchFailure, KeyMemory, MAX_DOCUMENT_BYTES, OidcConfig, discovery_url,
};
use dagq::infrastructure::oidc_keys::HttpsDocumentSource;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::json;

use crate::common::{STEP_LIMIT, within};

// A self-signed certificate for `localhost` valid until 2126, and its key,
// made for this test only and trusted by nothing but the source below.
const CERT_DER: &str = "MIIBfjCCASSgAwIBAgIUCGmV1XIadv18F8fKZTY+D/4saNkwCgYIKoZIzj0EAwIwFDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI2MTEwNjA0NDY0OFoYDzIxMjYxMDEzMDQ0NjQ4WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAASb3fyr3RNyM7nU0wKXcu/CT/FsBcx+4PTmL85b7/NNCWXhoPepe+goER61qyb1kGuoUGGxweDsHUfyCtOsa3bOo1IwUDAdBgNVHQ4EFgQUddl+z4raiDchG9LrZpWnpSRSQeIwFAYDVR0RBA0wC4IJbG9jYWxob3N0MAwGA1UdEwEB/wQCMAAwCwYDVR0PBAQDAgeAMAoGCCqGSM49BAMCA0gAMEUCIQDX2FZKv4U8FZyRD13S06d0vhqZtCuxB4ggavj/IhiJlAIgPMqaQpcaWV3gecvlBFDFOO+rgB20w+6BLR41KbeMMD4=";
const KEY_PKCS8: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgF048b9XYz6QUYrC/itoBqh2xiDSnIFCqlNZiml2rnAWhRANCAASb3fyr3RNyM7nU0wKXcu/CT/FsBcx+4PTmL85b7/NNCWXhoPepe+goER61qyb1kGuoUGGxweDsHUfyCtOsa3bO";
/// What the oversized answer offers, far past the socket buffers.
const HUGE_BYTES: u64 = 64 * 1024 * 1024;

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// What the listener saw.
#[derive(Default)]
struct Seen {
    paths: Mutex<HashMap<String, u32>>,
    huge_written: AtomicU64,
    stopping: AtomicBool,
}

/// One answer per connection: `documents` by path, `/moved` a redirect to
/// the key document, `/huge` an answer of no declared length past the limit,
/// `/stalled` an answer whose body stops coming until the test ends.
fn serve(listener: TcpListener, documents: HashMap<String, (String, String)>, seen: Arc<Seen>) {
    let certs = vec![CertificateDer::from(STANDARD.decode(CERT_DER).unwrap())];
    let key = PrivatePkcs8KeyDer::from(STANDARD.decode(KEY_PKCS8).unwrap());
    let tls = Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, PrivateKeyDer::Pkcs8(key))
        .unwrap(),
    );
    let documents = Arc::new(documents);
    for stream in listener.incoming() {
        if seen.stopping.load(Ordering::SeqCst) {
            return;
        }
        let (tls, documents, seen) = (tls.clone(), documents.clone(), seen.clone());
        let stream = stream.unwrap();
        thread::spawn(move || answer(stream, tls, &documents, &seen));
    }
}

fn answer(
    stream: TcpStream,
    tls: Arc<rustls::ServerConfig>,
    documents: &HashMap<String, (String, String)>,
    seen: &Seen,
) {
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let connection = rustls::ServerConnection::new(tls).unwrap();
    let mut tls = rustls::StreamOwned::new(connection, stream);
    let mut request = Vec::new();
    let mut byte = [0];
    while !request.ends_with(b"\r\n\r\n") {
        match tls.read(&mut byte) {
            Ok(1) => request.push(byte[0]),
            _ => return,
        }
    }
    let request = String::from_utf8(request).unwrap();
    let path = request.split(' ').nth(1).unwrap().to_string();
    *seen.paths.lock().unwrap().entry(path.clone()).or_default() += 1;
    let close = "Connection: close\r\n\r\n";
    if path == "/moved" {
        let _ = write!(tls, "HTTP/1.1 302 Found\r\nLocation: /jwks\r\n{close}");
    } else if path == "/huge" {
        let _ = write!(tls, "HTTP/1.1 200 OK\r\n{close}");
        let chunk = vec![b'x'; 64 * 1024];
        while seen.huge_written.load(Ordering::SeqCst) < HUGE_BYTES {
            if tls.write_all(&chunk).is_err() {
                return;
            }
            seen.huge_written
                .fetch_add(chunk.len() as u64, Ordering::SeqCst);
        }
    } else if path == "/stalled" {
        let _ = write!(tls, "HTTP/1.1 200 OK\r\nContent-Length: 10\r\n{close}x");
        let _ = tls.flush();
        while !seen.stopping.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(20));
        }
    } else if let Some((body, extra)) = documents.get(&path) {
        let length = body.len();
        let _ = write!(
            tls,
            "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\n{extra}{close}{body}"
        );
    } else {
        let _ = write!(
            tls,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n{close}"
        );
    }
    tls.conn.send_close_notify();
    let _ = tls.flush();
}

#[test]
fn keys_are_fetched_from_an_issuer_over_tls_within_the_limits_and_without_following_redirects() {
    let _waiting = within(STEP_LIMIT, "the fetches from the loopback issuer");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let issuer = format!("https://localhost:{port}");

    let rng = SystemRandom::new();
    let alg = &ECDSA_P256_SHA256_FIXED_SIGNING;
    let der = EcdsaKeyPair::generate_pkcs8(alg, &rng).unwrap();
    let pair = EcdsaKeyPair::from_pkcs8(alg, der.as_ref(), &rng).unwrap();
    let point = pair.public_key().as_ref();
    let jwks = json!({"keys": [{"kty": "EC", "crv": "P-256", "kid": "k1",
        "x": b64(&point[1..33]), "y": b64(&point[33..])}]});
    let discovery = json!({"issuer": issuer, "jwks_uri": format!("{issuer}/jwks")});
    let documents = HashMap::from([
        (
            "/.well-known/openid-configuration".to_string(),
            (discovery.to_string(), String::new()),
        ),
        (
            "/jwks".to_string(),
            (
                jwks.to_string(),
                "Cache-Control: public\r\nCache-Control: max-age=300\r\n".to_string(),
            ),
        ),
    ]);
    let seen = Arc::new(Seen::default());
    let server = {
        let seen = seen.clone();
        thread::spawn(move || serve(listener, documents, seen))
    };

    let header = json!({"alg": "ES256", "kid": "k1"});
    let claims = json!({"iss": issuer, "sub": "person-1", "aud": "dagq", "exp": 9_000});
    let input = format!(
        "{}.{}",
        b64(header.to_string().as_bytes()),
        b64(claims.to_string().as_bytes())
    );
    let signature = pair.sign(&rng, input.as_bytes()).unwrap();
    let token = format!("{input}.{}", b64(signature.as_ref()));

    let cert: &'static [u8] = STANDARD.decode(CERT_DER).unwrap().leak();
    let source = HttpsDocumentSource::with_roots(&[cert], Duration::from_secs(FETCH_TIMEOUT_SECS));
    let config = OidcConfig::new(&issuer, "dagq").unwrap();

    // The whole way: discovery, the key document, the signature.
    let mut memory = KeyMemory::default();
    let verified = verify_jwt(&config, &token, &mut memory, &source, 1_000).unwrap();
    assert_eq!(verified.identity.issuer, issuer);
    assert_eq!(verified.identity.subject, "person-1");
    // The lifetime of the answer's header holds: 299 s on no fetch comes.
    verify_jwt(&config, &token, &mut memory, &source, 1_299).unwrap();
    let hits = |path: &str| seen.paths.lock().unwrap().get(path).copied().unwrap_or(0);
    assert_eq!(hits("/jwks"), 1);
    assert_eq!(hits("/.well-known/openid-configuration"), 1);
    assert_eq!(
        source
            .get(&discovery_url(&issuer))
            .map(|document| document.cache_control),
        Ok(None)
    );

    // A redirect is an answer, not followed.
    assert_eq!(
        source.get(&format!("{issuer}/moved")),
        Err(KeyFetchFailure::Redirected)
    );
    assert_eq!(hits("/moved"), 1);
    assert_eq!(hits("/jwks"), 1);

    // The lifetime on the second header line ends the memory: 300 s on, a fetch.
    verify_jwt(&config, &token, &mut memory, &source, 1_300).unwrap();
    assert_eq!(hits("/jwks"), 2);

    // Too much is refused without reading the rest.
    assert_eq!(
        source.get(&format!("{issuer}/huge")),
        Err(KeyFetchFailure::TooLarge)
    );
    let written = seen.huge_written.load(Ordering::SeqCst);
    assert!(
        (MAX_DOCUMENT_BYTES / 2..HUGE_BYTES).contains(&written),
        "the listener wrote {written} bytes"
    );

    // An answer that stops coming ends at the time limit, its body included.
    let hurrying = HttpsDocumentSource::with_roots(&[cert], Duration::from_millis(500));
    assert!(matches!(
        hurrying.get(&format!("{issuer}/stalled")),
        Err(KeyFetchFailure::Unreachable(_))
    ));

    // Not https, and an issuer whose certificate is not trusted, are not fetched.
    assert!(matches!(
        source.get(&format!("http://localhost:{port}/jwks")),
        Err(KeyFetchFailure::Unreachable(_))
    ));
    let untrusting = HttpsDocumentSource::new();
    assert!(matches!(
        untrusting.get(&format!("{issuer}/jwks")),
        Err(KeyFetchFailure::Unreachable(_))
    ));

    seen.stopping.store(true, Ordering::SeqCst);
    drop(TcpStream::connect(("127.0.0.1", port)));
    server.join().unwrap();
}
