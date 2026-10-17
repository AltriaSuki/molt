//! `web.fetch` over a scripted transport: what is fetched, what is refused,
//! and what comes back.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use molt_api::web::FetchResponse;
use molt_proto::ErrorCode;
use molt_web::{Allow, Hop, Transport, Web, MAX_REDIRECTS, MAX_TEXT};
use reqwest::Url;
use serde_json::json;

/// Answers each URL from a table, and records what was asked for.
#[derive(Clone, Default)]
struct Script {
    pages: Arc<HashMap<String, Hop>>,
    asked: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Transport for Script {
    async fn get(&self, url: &Url, max_bytes: usize) -> Result<Hop, String> {
        self.asked.lock().unwrap().push(url.to_string());
        let mut hop = self.pages.get(url.as_str()).cloned().ok_or_else(|| format!("no route to {url}"))?;
        if hop.body.len() > max_bytes {
            hop.body.truncate(max_bytes);
            hop.truncated = true;
        }
        Ok(hop)
    }
}

fn page(content_type: &str, body: &str) -> Hop {
    Hop { status: 200, content_type: content_type.into(), body: body.as_bytes().to_vec(), ..Hop::default() }
}

fn redirect(to: &str) -> Hop {
    Hop { status: 302, location: Some(to.into()), ..Hop::default() }
}

fn web(pages: &[(&str, Hop)]) -> (Web, Script) {
    let script = Script {
        pages: Arc::new(pages.iter().map(|(u, h)| (u.to_string(), h.clone())).collect()),
        asked: Arc::default(),
    };
    let allow = Allow::parse(&["docs.rs", "*.python.org"]).unwrap();
    (Web::with_transport(allow, Box::new(script.clone())), script)
}

async fn fetch(web: &Web, url: &str) -> Result<FetchResponse, molt_proto::RemoteError> {
    let reply = web.handle("web.fetch", json!({ "url": url })).await?;
    Ok(serde_json::from_value(reply).unwrap())
}

#[tokio::test]
async fn an_allowed_page_comes_back_as_text_with_its_final_url() {
    let (web, script) = web(&[
        ("https://docs.rs/serde", redirect("/serde/latest/serde/")),
        (
            "https://docs.rs/serde/latest/serde/",
            page("text/html; charset=utf-8", "<h1>serde</h1><p>See <a href=\"de/index.html\">de</a>.</p>"),
        ),
        ("https://docs.python.org/3/x.json", page("application/json", "{\"a\": 1}")),
    ]);
    let r = fetch(&web, "https://docs.rs/serde#section").await.unwrap();
    assert_eq!(r.url, "https://docs.rs/serde/latest/serde/");
    assert_eq!(r.status, 200);
    assert_eq!(r.text, "# serde\n\nSee [de](https://docs.rs/serde/latest/serde/de/index.html).");
    assert!(!r.truncated);
    assert_eq!(*script.asked.lock().unwrap(), ["https://docs.rs/serde", "https://docs.rs/serde/latest/serde/"]);

    let r = fetch(&web, "https://docs.python.org/3/x.json").await.unwrap();
    assert_eq!(r.text, "{\"a\": 1}");
}

#[tokio::test]
async fn hosts_off_the_list_are_refused_before_any_request_and_on_every_redirect() {
    let (web, script) = web(&[
        ("https://docs.rs/away", redirect("https://evil.example/steal")),
        ("https://docs.rs/down", redirect("http://docs.rs/plain")),
        ("https://docs.rs/inside", redirect("https://169.254.169.254/latest/meta-data/")),
    ]);
    for (url, why) in [
        ("https://evil.example/", "not on the allowlist"),
        ("http://docs.rs/", "only https"),
        ("https://127.0.0.1/", "address"),
        ("not a url", "not a URL"),
    ] {
        let e = fetch(&web, url).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid, "{e}");
        assert!(e.message.contains(why), "{url}: {e}");
    }
    assert!(script.asked.lock().unwrap().is_empty(), "nothing refused was requested");

    for (url, why) in [
        ("https://docs.rs/away", "evil.example is not on the allowlist"),
        ("https://docs.rs/down", "only https"),
        ("https://docs.rs/inside", "address"),
    ] {
        let e = fetch(&web, url).await.unwrap_err();
        assert!(e.message.contains("redirects to") && e.message.contains(why), "{url}: {e}");
    }
    // Only the first hop of each was requested.
    assert_eq!(script.asked.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn redirect_loops_binaries_and_huge_pages_are_bounded() {
    let big = "x".repeat(MAX_TEXT + 10);
    let (web, script) = web(&[
        ("https://docs.rs/loop", redirect("/loop")),
        ("https://docs.rs/logo.png", page("image/png", "\u{89}PNG")),
        ("https://docs.rs/blob", page("", "a\0b")),
        ("https://docs.rs/big.txt", page("text/plain", &big)),
    ]);
    let e = fetch(&web, "https://docs.rs/loop").await.unwrap_err();
    assert!(e.message.contains("redirects more than"), "{e}");
    assert_eq!(script.asked.lock().unwrap().len(), MAX_REDIRECTS + 1);
    let e = fetch(&web, "https://docs.rs/logo.png").await.unwrap_err();
    assert!(e.message.contains("image/png, not text"), "{e}");
    let e = fetch(&web, "https://docs.rs/blob").await.unwrap_err();
    assert!(e.message.contains("binary, not text"), "{e}");
    let r = fetch(&web, "https://docs.rs/big.txt").await.unwrap();
    assert!(r.truncated && r.text.len() == MAX_TEXT);
}

#[tokio::test]
async fn transport_failures_say_which_url() {
    let (web, _) = web(&[]);
    let e = fetch(&web, "https://docs.rs/missing").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Failed);
    assert!(e.message.contains("fetching https://docs.rs/missing: no route"), "{e}");
    let e = web.handle("web.post", json!({})).await.unwrap_err();
    assert!(e.message.contains("unknown method web.post"), "{e}");
}
