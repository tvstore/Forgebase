//! The static desk: the strict allow-list, the headers and the Host check (M4.1).

mod common;

use std::path::PathBuf;

use axum::http::StatusCode;
use common::{Env, Reply, get, post};
use forgebase_http::AppConfig;

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
                   img-src 'self' data:; connect-src 'self'; font-src 'self'; object-src 'none'; \
                   base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// A throwaway desk directory, removed on drop. `secret.txt` and `secret.js` (an allowed
/// extension) sit next to the desk directory, in its parent, which no request may ever reach;
/// `index.js` and `top.js` sit in the desk root, outside `assets/`, which is not served either.
struct DeskDir {
    root: PathBuf,
    desk: PathBuf,
}

impl DeskDir {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "forgebase_desk_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let desk = root.join("dist");
        std::fs::create_dir_all(desk.join("assets/dir.js")).expect("directories");
        std::fs::write(root.join("secret.txt"), "TOP SECRET").expect("secret");
        std::fs::write(root.join("secret.js"), "TOP SECRET").expect("secret js");
        std::fs::write(desk.join("index.js"), "NOT AN ASSET").expect("root js");
        std::fs::write(desk.join("top.js"), "NOT AN ASSET").expect("root js");
        std::fs::write(
            desk.join("index.html"),
            "<!doctype html><title>desk</title>",
        )
        .expect("index");
        std::fs::write(desk.join("secret.txt"), "NOT AN ASSET").expect("not an asset");
        for (name, content) in [
            ("app-AbC123.js", "console.log(1)"),
            ("app-AbC123.css", "body{}"),
            ("logo.svg", "<svg/>"),
            ("data.exe", "MZ"),
            ("noextension", "x"),
            (".hidden.js", "x"),
            ("..js", "x"),
            ("app.js.map", "{}"),
        ] {
            std::fs::write(desk.join("assets").join(name), content).expect("asset");
        }
        Self { root, desk }
    }

    async fn env(&self) -> Env {
        Env::with_config(AppConfig {
            desk_dir: Some(self.desk.clone()),
            ..AppConfig::default()
        })
        .await
    }
}

impl Drop for DeskDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn assert_desk_headers(reply: &Reply) {
    assert_eq!(
        reply.header("content-security-policy").as_deref(),
        Some(CSP)
    );
    assert_eq!(reply.header("x-frame-options").as_deref(), Some("DENY"));
    assert_eq!(
        reply.header("referrer-policy").as_deref(),
        Some("same-origin")
    );
    assert_eq!(
        reply.header("x-content-type-options").as_deref(),
        Some("nosniff")
    );
    assert!(reply.header("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn the_index_and_the_assets_are_served_with_their_headers() {
    let dir = DeskDir::new();
    let env = dir.env().await;

    for path in ["/desk/", "/desk/index.html"] {
        let reply = env.send(get(path)).await;
        assert_eq!(reply.status, StatusCode::OK, "{path}");
        assert_eq!(reply.text, "<!doctype html><title>desk</title>");
        assert_eq!(
            reply.header("content-type").as_deref(),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(reply.header("cache-control").as_deref(), Some("no-store"));
        assert_desk_headers(&reply);
    }

    for (name, content_type, body) in [
        (
            "app-AbC123.js",
            "text/javascript; charset=utf-8",
            "console.log(1)",
        ),
        ("app-AbC123.css", "text/css; charset=utf-8", "body{}"),
        ("logo.svg", "image/svg+xml", "<svg/>"),
        ("app.js.map", "application/json", "{}"),
    ] {
        let reply = env.send(get(&format!("/desk/assets/{name}"))).await;
        assert_eq!(reply.status, StatusCode::OK, "{name}");
        assert_eq!(reply.text, body);
        assert_eq!(reply.header("content-type").as_deref(), Some(content_type));
        assert_eq!(
            reply.header("cache-control").as_deref(),
            Some("public, max-age=31536000, immutable")
        );
        assert_desk_headers(&reply);
    }

    // The API keeps `no-store` and gets no desk headers.
    let reply = env.send(get("/api/v1/meta")).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.header("cache-control").as_deref(), Some("no-store"));
    assert!(reply.header("content-security-policy").is_none());
}

#[tokio::test]
async fn only_the_allow_list_is_served() {
    let dir = DeskDir::new();
    let env = dir.env().await;
    let hostile = [
        // Traversal in every spelling.
        "/desk/assets/../secret.js",
        "/desk/assets/../../secret.js",
        "/desk/assets/%2e%2e%2fsecret.js",
        "/desk/assets/%2E%2E%2Fsecret.js",
        "/desk/assets/..%2fsecret.js",
        "/desk/assets/a%2f..%2f..%2fsecret.js",
        "/desk/assets/dir.js%2f..%2f..%2f..%2fsecret.js",
        "/desk/assets/..%5csecret.js",
        "/desk/assets/%2e%2e%5csecret.js",
        "/desk/assets/..\\secret.js",
        "/desk/assets/%2e%2e/secret.js",
        "/desk/assets/..%2findex.js",
        "/desk/assets/..%2f..%2fsecret.js",
        "/desk/index.js",
        "/desk/top.js",
        "/desk/assets/../index.js",
        "/desk/assets/../top.js",
        "/desk/assets/../secret.txt",
        "/desk/assets/../../secret.txt",
        "/desk/assets/%2e%2e/secret.txt",
        "/desk/assets/%2e%2e%2fsecret.txt",
        "/desk/assets/%2E%2E%2Fsecret.txt",
        "/desk/assets/..%2fsecret.txt",
        "/desk/assets/..%5csecret.txt",
        "/desk/assets/%2e%2e%5csecret.txt",
        "/desk/assets/..%00.js",
        "/desk/assets/app-AbC123.js%00.png",
        "/desk/assets/%2fetc%2fpasswd",
        "/desk/assets//etc/passwd",
        "/desk/assets/./app-AbC123.js",
        "/desk/assets/sub/app-AbC123.js",
        "/desk/assets/%61pp-AbC123.js",
        "/desk/assets/app-AbC123.js/",
        "/desk/assets/",
        "/desk/assets",
        // Not in the allow-list: other files of the desk directory, hidden files, unknown
        // extensions, a directory named like a script, the empty name pieces.
        "/desk/secret.txt",
        "/desk/assets/secret.txt",
        "/desk/assets/data.exe",
        "/desk/assets/noextension",
        "/desk/assets/.hidden.js",
        "/desk/assets/..js",
        "/desk/assets/dir.js",
        "/desk/assets/missing.js",
        "/desk/missing.html",
        "/desk/assets/index.html",
        "/desk/assets/..",
        "/desk/%2e%2e/secret.txt",
        "/desk/..%2fsecret.txt",
    ];
    for path in hostile {
        let reply = env.send(get(path)).await;
        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "{path}: {}",
            reply.text
        );
        assert!(!reply.text.contains("SECRET"), "{path}");
        assert!(!reply.text.contains("NOT AN ASSET"), "{path}");
        assert_eq!(reply.code(), "not_found", "{path}");
    }
}

#[tokio::test]
async fn desk_routes_take_get_only() {
    let dir = DeskDir::new();
    let env = dir.env().await;
    for path in ["/desk/", "/desk/index.html", "/desk/assets/app-AbC123.js"] {
        let reply = env.send(post(path)).await;
        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED, "{path}");
    }
}

#[tokio::test]
async fn a_desk_directory_without_index_is_a_404() {
    let dir = DeskDir::new();
    let env = dir.env().await;
    std::fs::remove_file(dir.desk.join("index.html")).expect("remove");
    let reply = env.send(get("/desk/")).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn without_a_desk_directory_nothing_is_served() {
    let env = Env::new().await;
    for path in [
        "/desk/",
        "/desk/index.html",
        "/desk/assets/app.js",
        "/desk",
        "/",
    ] {
        let reply = env.send(get(path)).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(reply.code(), "not_found");
        assert!(reply.header("content-security-policy").is_none());
    }
}

#[tokio::test]
async fn the_host_check_applies_to_the_desk() {
    let dir = DeskDir::new();
    let env = dir.env().await;
    for path in ["/desk/", "/desk/index.html", "/desk/assets/app-AbC123.js"] {
        for host in [None, Some("evil.example"), Some("localhost.evil.example")] {
            let reply = env.send(get(path).host(host)).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{path} {host:?}");
            assert_eq!(reply.code(), "invalid_host");
            assert!(!reply.text.contains("desk"), "{}", reply.text);
        }
    }
}
