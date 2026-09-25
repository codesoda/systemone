use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write as _},
    net::TcpListener,
    sync::{Arc, Mutex},
    thread,
};

use sha2::{Digest, Sha256};

use super::*;

/// What the local server sends for one path.
#[derive(Clone)]
enum Reply {
    Body(Vec<u8>),
    /// Sends a Content-Length of `declared` but only `body`, then closes.
    Truncated {
        body: Vec<u8>,
        declared: usize,
    },
    Status(u16),
    Redirect(String),
}

struct Server {
    base: String,
    hits: Arc<Mutex<Vec<String>>>,
}

fn serve(routes: BTreeMap<String, Reply>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&hits);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header == "\r\n" || header.is_empty() {
                    break;
                }
            }
            let path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .to_owned();
            log.lock().unwrap().push(path.clone());
            let reply = routes.get(&path).cloned().unwrap_or(Reply::Status(404));
            let _ = match reply {
                Reply::Body(body) => {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    stream
                        .write_all(head.as_bytes())
                        .and_then(|()| stream.write_all(&body))
                }
                Reply::Truncated { body, declared } => {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
                    );
                    stream
                        .write_all(head.as_bytes())
                        .and_then(|()| stream.write_all(&body))
                }
                Reply::Status(code) => stream.write_all(
                    format!("HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                ),
                Reply::Redirect(location) => stream.write_all(
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                ),
            };
        }
    });
    Server { base, hits }
}

fn sha(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn pin(server: &Server, path: &str, body: &[u8]) -> PinnedFile {
    PinnedFile {
        path: path.to_owned(),
        url: format!("{}/{path}", server.base),
        bytes: body.len() as u64,
        sha256: sha(body),
    }
}

fn plan(files: Vec<PinnedFile>) -> DownloadPlan {
    DownloadPlan {
        label: "test".to_owned(),
        files,
    }
}

fn downloader() -> Downloader {
    Downloader::build(true).unwrap()
}

fn leftovers(directory: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for entry in walk(directory) {
        names.push(entry.strip_prefix(directory).unwrap().display().to_string());
    }
    names.sort();
    names
}

fn walk(directory: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(directory) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[test]
fn downloads_verify_and_land_in_nested_paths() {
    let a = b"alpha".to_vec();
    let b = vec![7_u8; 3 * CHUNK_BYTES + 5];
    let server = serve(BTreeMap::from([
        ("/a.json".to_owned(), Reply::Body(a.clone())),
        ("/dir/b.bin".to_owned(), Reply::Body(b.clone())),
    ]));
    let plan = plan(vec![
        pin(&server, "a.json", &a),
        pin(&server, "dir/b.bin", &b),
    ]);
    let root = tempfile::tempdir().unwrap();
    let report = downloader()
        .download(&plan, root.path(), &mut NoProgress)
        .unwrap();
    assert_eq!(report.downloaded_files, 2);
    assert_eq!(report.downloaded_bytes, plan.total_bytes());
    assert_eq!(fs::read(root.path().join("a.json")).unwrap(), a);
    assert_eq!(fs::read(root.path().join("dir/b.bin")).unwrap(), b);
    assert_eq!(leftovers(root.path()), ["a.json", "dir/b.bin"]);
    assert!(
        inspect(&plan, root.path(), Check::Full)
            .unwrap()
            .iter()
            .all(|state| *state == FileState::Verified)
    );
}

#[test]
fn verified_files_are_skipped_and_corrupt_files_replaced() {
    let good = b"good".to_vec();
    let fixed = b"right".to_vec();
    let server = serve(BTreeMap::from([
        ("/good".to_owned(), Reply::Body(good.clone())),
        ("/fixed".to_owned(), Reply::Body(fixed.clone())),
    ]));
    let plan = plan(vec![
        pin(&server, "good", &good),
        pin(&server, "fixed", &fixed),
    ]);
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("good"), &good).unwrap();
    // Same size as the pin, wrong content.
    fs::write(root.path().join("fixed"), b"wrong").unwrap();
    assert_eq!(missing_bytes(&plan, root.path()).unwrap(), 0);

    let report = downloader()
        .download(&plan, root.path(), &mut NoProgress)
        .unwrap();
    assert_eq!(report.verified_files, 1);
    assert_eq!(report.downloaded_files, 1);
    assert_eq!(fs::read(root.path().join("fixed")).unwrap(), fixed);
    assert_eq!(*server.hits.lock().unwrap(), ["/fixed"]);
}

#[test]
fn wrong_checksum_leaves_no_file() {
    let served = b"tampered".to_vec();
    let server = serve(BTreeMap::from([("/f".to_owned(), Reply::Body(served))]));
    let mut file = pin(&server, "f", b"original");
    file.sha256 = sha(b"original");
    let root = tempfile::tempdir().unwrap();
    let error = downloader()
        .download(&plan(vec![file]), root.path(), &mut NoProgress)
        .unwrap_err();
    assert!(matches!(error, WeightsError::Checksum { .. }), "{error}");
    assert!(leftovers(root.path()).is_empty());
}

#[test]
fn wrong_size_leaves_no_file() {
    let server = serve(BTreeMap::from([(
        "/f".to_owned(),
        Reply::Body(b"longer than pinned".to_vec()),
    )]));
    let file = pin(&server, "f", b"short");
    let root = tempfile::tempdir().unwrap();
    let error = downloader()
        .download(&plan(vec![file]), root.path(), &mut NoProgress)
        .unwrap_err();
    assert!(matches!(error, WeightsError::Size { .. }), "{error}");
    assert!(leftovers(root.path()).is_empty());
}

#[test]
fn interrupted_stream_leaves_no_file() {
    let body = vec![1_u8; 4096];
    let server = serve(BTreeMap::from([(
        "/f".to_owned(),
        Reply::Truncated {
            body: body[..1000].to_vec(),
            declared: body.len(),
        },
    )]));
    let file = pin(&server, "f", &body);
    let root = tempfile::tempdir().unwrap();
    let error = downloader()
        .download(&plan(vec![file]), root.path(), &mut NoProgress)
        .unwrap_err();
    assert!(
        matches!(
            error,
            WeightsError::Download { .. } | WeightsError::Size { .. }
        ),
        "{error}"
    );
    assert!(leftovers(root.path()).is_empty());
}

#[test]
fn http_errors_are_reported() {
    let server = serve(BTreeMap::from([("/f".to_owned(), Reply::Status(404))]));
    let file = pin(&server, "f", b"x");
    let root = tempfile::tempdir().unwrap();
    let error = downloader()
        .download(&plan(vec![file]), root.path(), &mut NoProgress)
        .unwrap_err();
    assert!(error.to_string().contains("404"), "{error}");
}

#[test]
fn redirects_off_the_hub_are_refused() {
    let server = serve(BTreeMap::from([(
        "/f".to_owned(),
        Reply::Redirect("https://example.com/f".to_owned()),
    )]));
    let file = pin(&server, "f", b"x");
    let root = tempfile::tempdir().unwrap();
    let error = downloader()
        .download(&plan(vec![file]), root.path(), &mut NoProgress)
        .unwrap_err();
    assert!(error.to_string().contains("refused a redirect"), "{error}");
}

#[test]
fn production_downloader_only_accepts_hub_urls() {
    let downloader = Downloader::new().unwrap();
    let file = PinnedFile {
        path: "f".to_owned(),
        url: "https://example.com/f".to_owned(),
        bytes: 1,
        sha256: "a".repeat(64),
    };
    let root = tempfile::tempdir().unwrap();
    let error = downloader
        .download(&plan(vec![file]), root.path(), &mut NoProgress)
        .unwrap_err();
    assert!(
        error.to_string().contains("not a Hugging Face Hub URL"),
        "{error}"
    );
    assert!(host_allowed("huggingface.co"));
    assert!(host_allowed("cas-bridge.xethub.hf.co"));
    assert!(!host_allowed("huggingface.co.example.com"));
    assert!(!host_allowed("nothf.co"));
}

#[test]
fn available_space_walks_up_to_an_existing_directory() {
    let root = tempfile::tempdir().unwrap();
    let space = available_space(&root.path().join("not/yet/created"));
    assert!(space.is_some_and(|bytes| bytes > 0));
}
