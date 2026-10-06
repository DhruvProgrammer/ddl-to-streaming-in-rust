//! Live-server proof that an extensionless direct-download link plays.
//!
//! `tests/integration/extensionless.rs` pins the identification rules against
//! `media::identify()` directly: no network, no server, no protocol. This file
//! closes the other half. It runs the **real** application on a real port and
//! the **real** Node fixture origin (`tests/support/fixture-origin.mjs`) as a
//! separate process, and asserts the thing a user actually experiences: the
//! probe says the link is playable, and then `/api/stream` plays it.
//!
//! The gap this exists to close is that those two answers used to be computed
//! by different code with different inputs. `Engine::probe` read a 1 KiB body
//! prefix and identified from it; `Engine::media_for` handed the same function
//! `head: None` and an extensionless path, got `Container::Unknown`, and
//! answered `415 MEDIA_NOT_SUPPORTED` for a link the probe had just promised
//! would play. Every test here fails if that disagreement comes back.
//!
//! Nothing in this file asserts on a value it computed itself: every verdict
//! comes off the wire as JSON or HTTP, so a test cannot pass by agreeing with
//! its own assumptions.
//!
//! Each test starts its own origin process *and* its own proxy, so no cache
//! entry, connection pool or counter can be shared between tests. URLs still
//! carry a per-test `t=` tag because the cache key is the whole URL: a test
//! that reuses another test's URL would inherit its verdict and prove nothing.

use std::io::BufReader;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ddl_player::config::Config;

// ---------------------------------------------------------------- fixtures

/// `tests/fixtures/media/sample-360p.mp4`, the body every DDL mode serves.
const MP4_LEN: usize = 991_017;
/// The same file behind a legal size-8 `free` box (`?fault=leading-free`).
const MP4_LEN_WITH_FREE: usize = MP4_LEN + 8;
/// `00 00 00 20` `ftyp` `isom` — what a real MP4 must open with.
const FTYP: [u8; 12] = [
    0x00, 0x00, 0x00, 0x20, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm',
];
/// `00 00 00 08` `free` — the padding box in front of it, for `leading-free`.
const FREE: [u8; 8] = [0x00, 0x00, 0x00, 0x08, b'f', b'r', b'e', b'e'];

// ---------------------------------------------------------------- origin

/// The Node fixture origin, as a child process.
///
/// A separate process on purpose: the point of these tests is that the proxy
/// copes with an origin it does not control, over a socket, with its own idea
/// of what a well-behaved response looks like.
struct Origin {
    child: Child,
    stderr: Arc<Mutex<String>>,
    base: String,
}

impl Origin {
    async fn start() -> Origin {
        let origin = tokio::task::spawn_blocking(spawn_origin)
            .await
            .expect("spawn task for the fixture origin");
        wait_until_healthy(&origin).await;
        origin
    }

    /// `http://127.0.0.1:<port>/download/37334?fault=octet`
    fn url(&self, path_and_query: &str) -> String {
        format!("{}{}", self.base, path_and_query)
    }

    /// Everything the origin wrote to stderr, for a panic message. A fixture
    /// that died on a syntax error says so here instead of "timed out".
    fn stderr(&self) -> String {
        self.stderr
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| "<stderr lock poisoned>".into())
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        // The origin is this test's own child, so killing it on the way out is
        // just tidying up after ourselves. Runs on panic too.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fixture_origin_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("tests")
        .join("support")
        .join("fixture-origin.mjs")
}

fn spawn_origin() -> Origin {
    let script = fixture_origin_script();
    assert!(
        script.exists(),
        "the Node fixture origin is missing at {}.\nThese tests drive it as a real \
         process, so they cannot run without it.",
        script.display()
    );
    let mut child = Command::new("node")
        .arg(&script)
        // Port 0: the origin binds an ephemeral port and prints the one it got,
        // so two test binaries racing for a fixed port cannot collide.
        .args(["--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| {
            panic!(
                "cannot run `node {}`: {e}\nNode is required by \
                 tests/integration/ddl_live.rs",
                script.display()
            )
        });

    // Drain both pipes on threads of their own, for as long as the origin
    // lives. Two reasons, both learned the hard way:
    //   * An undrained pipe fills after 64 KiB and would wedge the origin
    //     mid-test, turning a fixture bug into a hang.
    //   * Closing the *read* end of a pipe makes the next write fail, and an
    //     error on `process.stdout` takes node down with it — which looks
    //     exactly like an origin that refused to start.
    let stderr = child.stderr.take().expect("piped stderr");
    let sink = Arc::new(Mutex::new(String::new()));
    let captured = Arc::clone(&sink);
    std::thread::spawn(move || {
        for line in std::io::BufRead::lines(BufReader::new(stderr)).map_while(|l| l.ok()) {
            if let Ok(mut buf) = captured.lock() {
                buf.push_str(&line);
                buf.push('\n');
            }
        }
    });

    // `--port 0` means the origin picks the port, so it has to tell us which
    // one: guessing a free port and handing it to `listen` races every other
    // process on the machine.
    let stdout = child.stdout.take().expect("piped stdout");
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufRead::lines(BufReader::new(stdout)).map_while(|l| l.ok()) {
            if let Some(port) = port_in_banner(&line) {
                let _ = port_tx.send(port);
            }
        }
    });

    let port = match port_rx.recv_timeout(Duration::from_secs(30)) {
        Ok(port) => port,
        Err(e) => panic!(
            "the fixture origin never announced a port ({e}).\nstderr:\n{}",
            stderr_text(&sink)
        ),
    };
    Origin {
        child,
        stderr: sink,
        base: format!("http://127.0.0.1:{port}"),
    }
}

fn stderr_text(sink: &Arc<Mutex<String>>) -> String {
    sink.lock().map(|s| s.clone()).unwrap_or_default()
}

/// `fixture-origin listening on http://127.0.0.1:52855` -> `52855`.
fn port_in_banner(line: &str) -> Option<u16> {
    let rest = line.split("127.0.0.1:").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

async fn wait_until_healthy(origin: &Origin) {
    let client = client();
    let mut waited = Duration::ZERO;
    let mut last;
    loop {
        match client.get(format!("{}/health", origin.base)).send().await {
            Ok(r) if r.status().is_success() => return,
            Ok(r) => last = format!("status {}", r.status()),
            Err(e) => last = e.to_string(),
        }
        if waited >= Duration::from_secs(20) {
            panic!(
                "the fixture origin at {} never became healthy ({last}).\n\
                 Is `node` on PATH and the fixture media present?\nstderr:\n{}",
                origin.base,
                origin.stderr()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        waited += Duration::from_millis(50);
    }
}

// ------------------------------------------------------------------ proxy

/// Boot the real application on an ephemeral port.
///
/// `cache_ttl` and `response_timeout` are the only two knobs that matter here:
/// the first decides whether a test is looking at a warm or a cold verdict, the
/// second decides whether a slow origin gets a fair chance to send its bytes.
async fn serve(cache_ttl: Duration, response_timeout: Duration) -> String {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        static_dir: None,
        max_concurrent_streams: 64,
        stream_buffer_bytes: 256 * 1024,
        read_chunk_bytes: 16 * 1024,
        prefetch_window_bytes: 4 * 1024 * 1024,
        connect_timeout: Duration::from_secs(2),
        response_timeout,
        // Generous on purpose: a test that trips the idle timeout is testing the
        // timeout, and the ones here are testing identification.
        idle_timeout: Duration::from_secs(15),
        dns_timeout: Duration::from_secs(2),
        cache_ttl,
        // The origin is on loopback, which the production default refuses.
        allow_private_hosts: true,
        ..Default::default()
    };
    let app = ddl_player::build(cfg);
    // Bound before the task is spawned: a connection made the instant this
    // returns is answered by the kernel backlog, so there is no sleep here and
    // no race on a half-started server.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app.router).await;
    });
    format!("http://{addr}")
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .expect("client")
}

/// `POST /api/probe` -> `(status, body)`.
async fn probe(base: &str, url: &str) -> (u16, serde_json::Value) {
    probe_with(base, url, false).await
}

async fn probe_with(base: &str, url: &str, refresh: bool) -> (u16, serde_json::Value) {
    let r = client()
        .post(format!("{base}/api/probe"))
        .json(&serde_json::json!({ "url": url, "refresh": refresh }))
        .send()
        .await
        .expect("probe request");
    let status = r.status().as_u16();
    let text = r.text().await.expect("probe body");
    let json = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("probe did not return JSON ({e}): {text}"));
    (status, json)
}

/// `GET /api/stats`, for the one thing only the proxy knows: what its metadata
/// cache is holding right now.
async fn stats(base: &str) -> serde_json::Value {
    client()
        .get(format!("{base}/api/stats"))
        .send()
        .await
        .expect("stats request")
        .json()
        .await
        .expect("stats json")
}

/// What `/api/stream` answered, with the body already read.
///
/// The status and the body come back together so a refusal can be reported in
/// full: "415 MEDIA_NOT_SUPPORTED" is not actionable on its own, but
/// "415 MEDIA_NOT_SUPPORTED: {reason}" is the whole bug report.
struct Streamed {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
}

impl Streamed {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn head(&self) -> &[u8] {
        &self.body[..self.body.len().min(12)]
    }
}

async fn stream(base: &str, url: &str) -> Streamed {
    let target = format!("{base}/api/stream?url={}", encode(url));
    let r = client().get(&target).send().await.expect("stream request");
    let status = r.status().as_u16();
    let headers = r.headers().clone();
    let body = r
        .bytes()
        .await
        .unwrap_or_else(|e| panic!("reading the /api/stream body from {target}: {e}"))
        .to_vec();
    Streamed {
        status,
        headers,
        body,
    }
}

/// Percent-encode a URL for use as a query-string value.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Long enough that nothing expires mid-test unless a test asks for it.
fn generous_ttl() -> Duration {
    Duration::from_secs(300)
}

/// A response timeout that leaves room for the fixture's slowest body.
fn patient_timeout() -> Duration {
    Duration::from_secs(6)
}

// ===========================================================================
// C1 — the headline bug: probe says yes, stream refused.
// ===========================================================================

#[tokio::test]
async fn c1_a_probe_that_says_yes_is_followed_by_a_stream_that_plays() {
    let origin = Origin::start().await;
    let base = serve(generous_ttl(), patient_timeout()).await;
    // The canonical DDL: no extension, a generic type, real MP4 bytes.
    let url = origin.url("/download/37334?fault=octet&t=c1");

    let (status, probe) = probe(&base, &url).await;
    assert_eq!(status, 200, "probe body: {probe}");
    // Precondition: the origin really did declare a generic type on a path with
    // no extension. Without this the test could pass on a Content-Type it was
    // never meant to depend on.
    assert_eq!(
        probe["origin_content_type"], "application/octet-stream",
        "precondition: the origin declared a generic type"
    );
    assert!(
        probe["streamable"].as_bool() == Some(true),
        "a real MP4 behind octet-stream and an extensionless path is playable. \
         The bytes say so; nothing else can. probe body: {probe}"
    );
    assert_eq!(probe["container"], "mp4", "probe body: {probe}");
    assert_eq!(
        probe["evidence"], "magic-bytes",
        "the only evidence available here is the body, so the verdict must be \
         attributed to it. probe body: {probe}"
    );
    assert_eq!(probe["content_type"], "video/mp4", "probe body: {probe}");
    assert_eq!(probe["content_length"], MP4_LEN as u64, "probe body: {probe}");
    assert_eq!(probe["redirects"], 0, "probe body: {probe}");
    assert_eq!(probe["cached"], false, "probe body: {probe}");

    // The half that used to fail: the same URL, the same origin, the same
    // verdict — but decided by the streaming path instead of the probe.
    let s = stream(&base, &url).await;
    assert_ne!(
        s.status, 415,
        "the probe promised a playable MP4 and /api/stream refused the very \
         same URL with 415 MEDIA_NOT_SUPPORTED: {}",
        s.text()
    );
    assert_eq!(s.status, 200, "/api/stream: {}", s.text());
    assert_eq!(s.headers["content-type"], "video/mp4", "/api/stream: {}", s.text());
    assert_eq!(s.body.len(), MP4_LEN, "the whole file must arrive");
    assert_eq!(s.head(), &FTYP, "the bytes must be the MP4 that was promised");
}

#[tokio::test]
async fn c1b_a_cold_cache_stream_identifies_the_ddl_by_itself() {
    let origin = Origin::start().await;
    let base = serve(generous_ttl(), patient_timeout()).await;
    // `?id=12345` is the other canonical shape: no extension, and a query
    // string that must never be mistaken for a filename.
    let url = origin.url("/get?fault=octet&id=12345&t=c1b");

    // No probe first. Nothing is cached, nothing has been decided: the stream
    // path has to identify this resource from the response itself.
    let s = stream(&base, &url).await;
    assert_ne!(
        s.status, 415,
        "with a cold cache /api/stream must identify the resource itself; it \
         answered 415 MEDIA_NOT_SUPPORTED: {}",
        s.text()
    );
    assert_eq!(s.status, 200, "/api/stream: {}", s.text());
    assert_eq!(s.headers["content-type"], "video/mp4");
    assert_eq!(s.body.len(), MP4_LEN);
    assert_eq!(s.head(), &FTYP);

    // The stream path identifies, it does not cache: if this probe came back
    // `cached`, the verdict above may have been read out of the cache rather
    // than out of the bytes, and this test would be proving nothing.
    let (status, probe) = probe(&base, &url).await;
    assert_eq!(status, 200, "probe body: {probe}");
    assert_eq!(
        probe["cached"], false,
        "the cold stream must not have left a cached verdict behind for this \
         assertion to lean on. probe body: {probe}"
    );
    assert_eq!(probe["streamable"], true, "probe body: {probe}");
    assert_eq!(probe["container"], "mp4", "probe body: {probe}");
}

#[tokio::test]
async fn c1c_a_cache_entry_that_expires_mid_playback_does_not_become_a_415() {
    let origin = Origin::start().await;
    let base = serve(Duration::from_millis(150), patient_timeout()).await;
    let url = origin.url("/dl/A_SLu4ViVaRA1e51eV8yNiRGIQpB4_D25?fault=octet&t=c1c");

    let (status, first) = probe(&base, &url).await;
    assert_eq!(status, 200, "probe body: {first}");
    assert_eq!(first["streamable"], true, "probe body: {first}");
    assert_eq!(first["cached"], false, "probe body: {first}");

    let (_, warm) = probe(&base, &url).await;
    assert_eq!(
        warm["cached"], true,
        "precondition: the entry is in the cache, so the expiry below is a \
         real transition rather than a guess. probe body: {warm}"
    );
    assert_eq!(
        stats(&base).await["cache"]["entries"], 1,
        "precondition: exactly one entry, and it is the one we just probed"
    );

    tokio::time::sleep(Duration::from_millis(400)).await;

    // Playback resumes with the verdict gone. A cached verdict is a hint; a
    // container is a property of the bytes and outlives the metadata.
    let s = stream(&base, &url).await;
    assert_ne!(
        s.status, 415,
        "the cache entry expired between the probe and playback, and the \
         stream path turned that into 415 MEDIA_NOT_SUPPORTED: {}",
        s.text()
    );
    assert_eq!(s.status, 200, "/api/stream: {}", s.text());
    assert_eq!(s.headers["content-type"], "video/mp4");
    assert_eq!(s.body.len(), MP4_LEN);
    assert_eq!(s.head(), &FTYP);

    // The stream's own lookup is what reaped the expired entry. If it were
    // still there, the assertions above would have been satisfied by a warm
    // cache and this test would have proved nothing about expiry.
    let after = stats(&base).await;
    assert_eq!(
        after["cache"]["entries"], 0,
        "the stream must have looked in the cache and found the entry expired. \
         If it is still cached, the stream below was really reading a verdict \
         and this test proves nothing: {after}"
    );
    assert!(
        after["cache"]["expirations"].as_u64().unwrap_or(0) >= 1,
        "the expiry should have been counted: {after}"
    );
}

// ===========================================================================
// C8 — a redirect. The bytes and the headers come from the last hop, so the
// evidence has to come from there too.
// ===========================================================================

#[tokio::test]
async fn c8_an_extensionless_url_redirecting_to_an_mp4_path_is_identified_from_the_final_hop() {
    let origin = Origin::start().await;
    let base = serve(generous_ttl(), patient_timeout()).await;
    let url = origin.url("/download/37334?fault=ext-in-redirect&t=c8a");

    let (status, probe) = probe(&base, &url).await;
    assert_eq!(status, 200, "probe body: {probe}");
    assert!(
        probe["redirects"].as_u64().unwrap_or(0) >= 1,
        "precondition: the origin really did redirect. probe body: {probe}"
    );
    // The hop that served the bytes is the one that named the type and the
    // path. If this came from the URL the viewer pasted, the whole resource
    // would have been read as belonging to `/download/37334`.
    assert_eq!(
        probe["origin_content_type"], "application/octet-stream",
        "the final hop declared a generic type, so the container can only have \
         come from its body. probe body: {probe}"
    );
    assert_eq!(
        probe["final_url"].as_str().unwrap_or_default(),
        format!("{}/media/360p.mp4", origin.base),
        "the reported final URL must be the hop that served the bytes. probe body: {probe}"
    );
    assert_eq!(probe["container"], "mp4", "probe body: {probe}");
    assert_eq!(probe["evidence"], "magic-bytes", "probe body: {probe}");
    assert_eq!(probe["streamable"], true, "probe body: {probe}");

    // And the stream path agrees, redirect included.
    let s = stream(&base, &url).await;
    assert_ne!(
        s.status, 415,
        "the probe followed the redirect and identified an MP4, but the stream \
         path refused the same URL: {}",
        s.text()
    );
    assert_eq!(s.status, 200, "/api/stream: {}", s.text());
    assert_eq!(s.headers["content-type"], "video/mp4");
    assert_eq!(s.body.len(), MP4_LEN);
    assert_eq!(s.head(), &FTYP);
}

#[tokio::test]
async fn c8_a_mp4_url_redirecting_to_an_extensionless_path_is_identified_from_the_final_hop() {
    let origin = Origin::start().await;
    let base = serve(generous_ttl(), patient_timeout()).await;
    // The mirror image: the URL the viewer pasted carries `.mp4`, and the hop
    // that actually serves the bytes carries nothing at all.
    let url = origin.url("/media/360p.mp4?fault=cdnr&t=c8b");

    let (status, probe) = probe(&base, &url).await;
    assert_eq!(status, 200, "probe body: {probe}");
    assert!(
        probe["redirects"].as_u64().unwrap_or(0) >= 1,
        "precondition: the origin really did redirect. probe body: {probe}"
    );
    assert_eq!(
        probe["final_url"].as_str().unwrap_or_default(),
        format!("{}/dl/37334-8f2c1a", origin.base),
        "the final hop is extensionless, and that is the hop the verdict has to \
         describe. probe body: {probe}"
    );
    assert_eq!(probe["container"], "mp4", "probe body: {probe}");
    assert_eq!(probe["evidence"], "magic-bytes", "probe body: {probe}");
    assert_eq!(probe["streamable"], true, "probe body: {probe}");

    let s = stream(&base, &url).await;
    assert_ne!(s.status, 415, "/api/stream: {}", s.text());
    assert_eq!(s.status, 200, "/api/stream: {}", s.text());
    assert_eq!(s.body.len(), MP4_LEN);
    assert_eq!(s.head(), &FTYP);
}

/// The redirect fix, isolated.
///
/// Everything else in C8 has a body that identifies itself, so it would pass
/// even if the extension evidence were read off the pre-redirect path. Here the
/// body identifies as *nothing* (`&head=truncated`), so the final path is the
/// only evidence left — and it belongs to the hop that served the bytes, not to
/// the URL the viewer happened to paste.
#[tokio::test]
async fn c8_the_stream_path_takes_the_extension_from_the_final_url_not_the_pasted_one() {
    let origin = Origin::start().await;
    let base = serve(generous_ttl(), patient_timeout()).await;
    let url = origin.url("/download/37334?fault=ext-in-redirect&head=truncated&t=c8c");

    let s = stream(&base, &url).await;
    assert_ne!(
        s.status, 415,
        "the final hop's path carries `.mp4` and its own headers and body \
         identify as nothing, so that path is the last evidence there is — \
         reading it off the pre-redirect URL turns a playable file into a \
         refusal: {}",
        s.text()
    );
    assert_eq!(s.status, 200, "/api/stream: {}", s.text());
    assert_eq!(s.headers["content-type"], "video/mp4");
    // The origin only has four bytes to give, and all four are delivered: this
    // is about the verdict, not about the payload.
    assert_eq!(s.body.len(), 4, "/api/stream: {}", s.text());
}

/// The other half of the same rule, and the reason the rule is not symmetric.
///
/// `/media/360p.mp4` here is a *claim* — the viewer pasted it, or copied it
/// from somewhere. The hop that serves the bytes is extensionless. With a body
/// that identifies as nothing, there is no evidence left, so the link must be
/// refused rather than played on the strength of a URL.
#[tokio::test]
async fn c8_a_pasted_mp4_extension_does_not_rescue_an_extensionless_body() {
    let origin = Origin::start().await;
    let base = serve(generous_ttl(), patient_timeout()).await;
    let url = origin.url("/media/360p.mp4?fault=cdnr&head=truncated&t=c8d");

    let s = stream(&base, &url).await;
    assert_eq!(
        s.status, 415,
        "the final hop is extensionless and neither its headers nor its bytes \
         identify as media; the `.mp4` the viewer pasted is not evidence: {}",
        s.text()
    );
    let body: serde_json::Value = serde_json::from_str(&s.text()).expect("error json");
    assert_eq!(
        body["code"], "INVALID_CONTENT_TYPE",
        "the refusal must be about the content, not a transport failure: {}",
        body
    );
}

// ===========================================================================
// C7 — a slow origin. Headers are cheap; the first body byte is not.
// ===========================================================================

/// Assert the probe's answer to "headers arrived, the body did not".
///
/// Two answers are acceptable and one is not. Saying the file is an unsupported
/// container is a lie: nothing about the container was ever observed. Either
/// the bytes arrived within the configured budget, or the failure must name the
/// wait.
#[track_caller]
fn assert_identified_or_a_timeout(probe: &serde_json::Value, why: &str, response_timeout: Duration) {
    if probe["streamable"].as_bool() == Some(true) {
        assert_eq!(probe["container"], "mp4", "{why} probe body: {probe}");
        assert_eq!(probe["evidence"], "magic-bytes", "{why} probe body: {probe}");
        return;
    }
    let reason = probe["reason"]
        .as_str()
        .unwrap_or("<no reason>")
        .to_ascii_lowercase();
    let names_the_wait = ["timed out", "timeout", "no response headers within"]
        .iter()
        .any(|w| reason.contains(w));
    assert!(
        names_the_wait,
        "{why}\n  expected: either the bytes arrived within the {response_timeout:?} \
         budget, or a reason that names the wait\n  actual:   streamable=false, \
         container={:?}, evidence={:?}, reason={:?}",
        probe["container"], probe["evidence"], probe["reason"]
    );
}

#[tokio::test]
async fn c7_a_slow_origin_is_identified_within_the_configured_timeout() {
    let origin = Origin::start().await;
    // The body lands 2.5 s after the headers, so the budget has to be bigger
    // than 2.5 s: this is the "tuned for a slow CDN" configuration.
    let budget = Duration::from_secs(6);
    let base = serve(generous_ttl(), budget).await;
    let url = origin.url("/file?fault=slow-body&token=abc&t=c7");

    let (status, probe) = probe(&base, &url).await;
    assert_eq!(status, 200, "probe body: {probe}");
    assert_identified_or_a_timeout(&probe, "a slow origin with a 6s budget", budget);
    assert_eq!(
        probe["streamable"], true,
        "with a 6s budget and a 2.5s body this must simply work. probe body: {probe}"
    );
    assert_eq!(probe["container"], "mp4", "probe body: {probe}");
    assert_eq!(probe["evidence"], "magic-bytes", "probe body: {probe}");
}

/// The same origin, with a budget too small to receive its body.
///
/// This is the case the placeholder in `extensionless.rs` was written for. The
/// budget is derived from `cfg.response_timeout` now rather than hard-coded, so
/// raising the timeouts for a slow host does move it — and when the configured
/// budget is still not enough, the answer must be about the wait and not about
/// the container.
///
/// **Fails today, and the product is wrong.** Observed against a live slow
/// origin with `DDL_RESPONSE_TIMEOUT_MS=700`:
///
/// ```text
/// streamable=false, container=empty, evidence=magic-bytes,
/// reason="The origin returned an empty body; there is no video here."
/// ```
///
/// The body was not empty. Nothing came back *in time*, and
/// `read_head` (`backend/src/streaming/engine.rs:1144`) cannot tell those two
/// apart: its loop condition swallows both the elapsed budget and a transport
/// error and hands back whatever it collected — here, nothing. `identify` then
/// reads an empty `Some(&[])` as a body that identifies as nothing and reports
/// `Container::Empty` (`backend/src/media.rs:562`), whose reason
/// (`backend/src/media.rs:480`) says there is no video here.
///
/// So a viewer whose CDN is merely slow is told their file is missing. The fix
/// belongs in `read_head`: return something that distinguishes "the body ended"
/// from "the budget expired", and let the probe name the wait. Both of those
/// files are outside what this task may change, so the requirement stays on
/// record here rather than being asserted as if it held.
///
/// Run with: cargo test --test ddl_live -- --ignored
#[tokio::test]
#[ignore = "product bug: a slow body is reported as an empty body, not as a \
            timeout. read_head (backend/src/streaming/engine.rs:1144) cannot \
            distinguish an expired budget from a body that ended, so the probe \
            answers container=empty / 'there is no video here' for a file that \
            is merely late."]
async fn c7_a_body_slower_than_the_budget_is_reported_as_a_wait_not_a_format_problem() {
    let origin = Origin::start().await;
    let budget = Duration::from_millis(700);
    let base = serve(generous_ttl(), budget).await;
    let url = origin.url("/file?fault=slow-body&token=abc&t=c7b");

    let (status, probe) = probe(&base, &url).await;
    assert_eq!(status, 200, "probe body: {probe}");
    assert_identified_or_a_timeout(
        &probe,
        "a body that misses a 700ms budget must not be blamed on the container",
        budget,
    );
}

// ===========================================================================
// The remaining DDL shapes, over the wire.
// ===========================================================================

/// The harshest of them: the origin says nothing whatsoever about what it is
/// sending, on a path with no extension. Only the body can answer.
#[tokio::test]
async fn an_origin_that_declares_no_content_type_at_all_still_plays() {
    let origin = Origin::start().await;
    let base = serve(generous_ttl(), patient_timeout()).await;
    let url = origin.url("/get?fault=no-ct&id=98765&t=noct");

    let (status, probe) = probe(&base, &url).await;
    assert_eq!(status, 200, "probe body: {probe}");
    assert_eq!(
        probe["origin_content_type"], serde_json::Value::Null,
        "precondition: the origin sent no Content-Type at all. probe body: {probe}"
    );
    assert_eq!(probe["streamable"], true, "probe body: {probe}");
    assert_eq!(probe["container"], "mp4", "probe body: {probe}");
    assert_eq!(probe["evidence"], "magic-bytes", "probe body: {probe}");

    let s = stream(&base, &url).await;
    assert_ne!(s.status, 415, "/api/stream: {}", s.text());
    assert_eq!(s.status, 200, "/api/stream: {}", s.text());
    assert_eq!(s.headers["content-type"], "video/mp4");
    assert_eq!(s.body.len(), MP4_LEN);
    assert_eq!(s.head(), &FTYP);
}

/// `ftyp` at offset 4 is the common case and `ftyp` at offset 8 is legal. A
/// size-8 `free` box ahead of it is emitted by real muxers, so an identifier
/// that only looks at offset 4 misses real files — exactly the ones with no
/// extension and no useful `Content-Type` to fall back on.
#[tokio::test]
async fn an_ftyp_behind_a_leading_free_box_is_still_an_mp4() {
    let origin = Origin::start().await;
    let base = serve(generous_ttl(), patient_timeout()).await;
    let url = origin.url("/download/37334?fault=leading-free&t=freebox");

    let (status, probe) = probe(&base, &url).await;
    assert_eq!(status, 200, "probe body: {probe}");
    assert_eq!(probe["streamable"], true, "probe body: {probe}");
    assert_eq!(probe["container"], "mp4", "probe body: {probe}");
    assert_eq!(probe["evidence"], "magic-bytes", "probe body: {probe}");

    let s = stream(&base, &url).await;
    assert_ne!(s.status, 415, "/api/stream: {}", s.text());
    assert_eq!(s.status, 200, "/api/stream: {}", s.text());
    assert_eq!(s.body.len(), MP4_LEN_WITH_FREE);
    assert_eq!(
        &s.body[..8],
        &FREE,
        "precondition: the body really does open with a free box"
    );
    assert_eq!(&s.body[8..20], &FTYP);
}

/// `Content-Disposition: attachment; filename="Movie.mp4"` on an extensionless
/// DDL is the origin naming its own file. With the body truncated there is
/// nothing else to go on, and the filename is what is left — so the probe must
/// reach it rather than shrug.
#[tokio::test]
async fn an_attachment_filename_identifies_a_ddl_whose_body_is_truncated() {
    let origin = Origin::start().await;
    let base = serve(generous_ttl(), patient_timeout()).await;
    let url = origin.url("/dl/A_SLu4ViVaRA1e51eV8yNiRGIQpB4_D25?fault=attachment&head=truncated&t=cd");

    let (status, probe) = probe(&base, &url).await;
    assert_eq!(status, 200, "probe body: {probe}");
    assert_eq!(probe["origin_content_type"], "application/octet-stream");
    assert_eq!(probe["streamable"], true, "probe body: {probe}");
    assert_eq!(probe["container"], "mp4", "probe body: {probe}");
    assert_eq!(
        probe["evidence"], "content-disposition",
        "with a body that identifies as nothing, the origin's own filename is \
         the evidence there is. probe body: {probe}"
    );

    // Cold, so this is the streaming path's own `Content-Disposition` branch and
    // not a cached verdict from the probe above.
    let cold = origin.url("/dl/B_9f3a2b7c1d4e5f6a8b0c2d4e6f8a0b1c?fault=attachment&head=truncated&t=cd2");
    let s = stream(&base, &cold).await;
    assert_ne!(
        s.status, 415,
        "the origin named its own file and the body was too short to say \
         otherwise; that must be enough: {}",
        s.text()
    );
    assert_eq!(s.status, 200, "/api/stream: {}", s.text());
    assert_eq!(s.headers["content-type"], "video/mp4");
}

/// The counterweight: a filename is a claim, the body is the evidence.
#[tokio::test]
async fn the_body_outranks_a_content_disposition_filename_that_lies() {
    let origin = Origin::start().await;
    let base = serve(generous_ttl(), patient_timeout()).await;
    let url = origin.url("/download/37334?fault=attachment&name=clip.webm&t=cdlie");

    let (status, probe) = probe(&base, &url).await;
    assert_eq!(status, 200, "probe body: {probe}");
    assert_eq!(
        probe["container"], "mp4",
        "the body is an MP4 whatever the filename claims. probe body: {probe}"
    );
    assert_eq!(probe["evidence"], "magic-bytes", "probe body: {probe}");

    let s = stream(&base, &url).await;
    assert_ne!(s.status, 415, "/api/stream: {}", s.text());
    assert_eq!(s.status, 200, "/api/stream: {}", s.text());
    assert_eq!(
        s.headers["content-type"], "video/mp4",
        "/api/stream: {}",
        s.text()
    );
}

/// The other direction: a body that identifies as nothing is not media, and no
/// URL rescues it. This is the rule that makes the ones above worth having.
#[tokio::test]
async fn a_body_that_identifies_as_nothing_is_not_rescued_by_an_mp4_extension() {
    let origin = Origin::start().await;
    let base = serve(generous_ttl(), patient_timeout()).await;
    // Same body, both shapes of path: the only difference is the extension.
    let extensionless = origin.url("/download/37334?fault=truncated-head&t=trunc1");
    let with_extension = origin.url("/media/360p.mp4?fault=truncated-head&t=trunc2");

    for (url, why) in [
        (&extensionless, "extensionless"),
        (&with_extension, "with a .mp4 extension"),
    ] {
        let (status, probe) = probe(&base, url).await;
        assert_eq!(status, 200, "probe body: {probe}");
        assert_eq!(
            probe["streamable"], false,
            "{why}: a four-byte body identifies no container, so the link is not \
             playable. probe body: {probe}"
        );
        assert_eq!(probe["container"], "unknown", "{why}: probe body: {probe}");
        assert_ne!(
            probe["evidence"], "extension",
            "{why}: the extension is the last resort and only when no body was \
             read; a body was read. probe body: {probe}"
        );
        assert!(
            probe["reason"].as_str().is_some_and(|r| !r.is_empty()),
            "{why}: a refusal has to explain itself. probe body: {probe}"
        );
    }

    // The extensionless one is refused outright rather than played on the
    // strength of the URL.
    let s = stream(&base, &extensionless).await;
    assert_eq!(
        s.status, 415,
        "an extensionless link whose body identifies as nothing must be \
         refused, not streamed on the off-chance that it is media: {}",
        s.text()
    );
}