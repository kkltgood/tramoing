use super::*;
use sha2::{Digest, Sha256};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::channel;

fn serve(
    count: usize,
    mut respond: impl FnMut(usize, String, &mut TcpStream) + Send + 'static,
) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        for attempt in 0..count {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "missing request {attempt}");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 8192);
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            respond(
                attempt,
                String::from_utf8(request).unwrap().to_ascii_lowercase(),
                &mut stream,
            );
        }
    });
    (format!("http://{address}/setup.exe"), server)
}

fn asset(url: String, body: &[u8]) -> Asset {
    Asset {
        url,
        size: body.len() as u64,
        sha256: format!("{:x}", Sha256::digest(body)),
    }
}

fn run_fast(
    asset: &Asset,
    root: &Path,
    tx: &Sender<Event>,
    stop: &Receiver<()>,
) -> Result<Option<PathBuf>, String> {
    run_with_agent(
        asset,
        "1.2.3",
        tx,
        root,
        &download_agent(Duration::from_millis(500)),
        stop,
        |_| Duration::from_millis(10),
    )
}

#[test]
fn repeated_disconnects_resume_from_the_last_written_byte() {
    let (url, server) = serve(3, |attempt, request, stream| {
        assert!(request.contains("accept-encoding: identity\r\n"));
        if attempt == 0 {
            assert!(!request.contains("\r\nrange:"));
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nETag: \"release\"\r\nConnection: close\r\n\r\nabc").unwrap();
        } else {
            let offset = attempt * 3;
            assert!(
                request.contains(&format!("range: bytes={offset}-\r\n")),
                "{request}"
            );
            assert!(request.contains("if-range: \"release\"\r\n"));
            let body = if attempt == 1 { "def" } else { "ghi" };
            write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {offset}-8/9\r\nETag: \"release\"\r\nConnection: close\r\n\r\n{body}", 9-offset).unwrap();
        }
    });
    let asset = asset(url, b"abcdefghi");
    let root = tempfile::tempdir().unwrap();
    let (tx, rx) = channel();
    let (_cancel, stop) = channel();
    let result = run_fast(&asset, root.path(), &tx, &stop);
    server.join().unwrap();
    let path = result.unwrap().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"abcdefghi");
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1
    );
    let events: Vec<_> = rx.try_iter().collect();
    assert_eq!(
        events
            .iter()
            .filter(|ev| matches!(ev, Event::DownloadRetrying { .. }))
            .count(),
        2
    );
    let progress: Vec<_> = events
        .iter()
        .filter_map(|ev| {
            if let Event::Progress(p) = ev {
                Some(*p)
            } else {
                None
            }
        })
        .collect();
    assert!(progress.windows(2).all(|p| p[0] <= p[1]), "{progress:?}");
}

#[test]
fn a_new_download_job_resumes_saved_bytes_even_when_the_url_changes() {
    let root = tempfile::tempdir().unwrap();
    let (tx, _rx) = channel();
    let (url, first) = serve(1, |_, _, stream| {
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"same\"\r\nConnection: close\r\n\r\nabc").unwrap();
    });
    let mut asset = asset(url, b"abcdef");
    assert!(download_to(&asset, "1.2.3", &tx, root.path()).is_err());
    first.join().unwrap();
    let (url, second) = serve(1, |_, request, stream| {
        assert!(request.contains("range: bytes=3-\r\n"), "{request}");
        assert!(request.contains("if-range: \"same\"\r\n"));
        stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nETag: \"same\"\r\nConnection: close\r\n\r\ndef").unwrap();
    });
    asset.url = url;
    let path = download_to(&asset, "1.2.3", &tx, root.path()).unwrap();
    second.join().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"abcdef");
}

#[test]
fn a_full_response_to_a_range_restarts_instead_of_appending() {
    let (url, server) = serve(2, |attempt, request, stream| {
        if attempt == 0 {
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"old\"\r\nConnection: close\r\n\r\nabc").unwrap();
        } else {
            assert!(request.contains("range: bytes=3-\r\n"));
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"new\"\r\nConnection: close\r\n\r\nabcdef").unwrap();
        }
    });
    let root = tempfile::tempdir().unwrap();
    let (tx, _rx) = channel();
    let (_cancel, stop) = channel();
    let path = run_fast(&asset(url, b"abcdef"), root.path(), &tx, &stop)
        .unwrap()
        .unwrap();
    server.join().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"abcdef");
}

#[test]
fn invalid_ranges_and_changed_etags_never_append_or_install() {
    for headers in [
        "Content-Range: bytes 2-5/6\r\nContent-Length: 4\r\n",
        "Content-Range: bytes 3-5/7\r\nContent-Length: 3\r\n",
        "Content-Range: bytes 3-5/6\r\nContent-Length: 2\r\n",
        "Content-Range: bytes 3-5/6\r\nContent-Length: 3\r\nETag: \"different\"\r\n",
        "Content-Length: 3\r\n",
    ] {
        let (url, server) = serve(1, move |_, _, stream| {
            write!(
                stream,
                "HTTP/1.1 206 Partial Content\r\n{headers}Connection: close\r\n\r\ndef"
            )
            .unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        let asset = asset(url, b"abcdef");
        let mut partial = PartialDownload::open(&asset, "1.2.3", root.path()).unwrap();
        partial.file.write_all(b"abc").unwrap();
        partial.state.etag = Some("\"original\"".into());
        partial.save_state().unwrap();
        let part_path = partial.path.clone();
        let dest = partial.dest.clone();
        drop(partial);
        let (tx, _rx) = channel();
        assert!(download_to(&asset, "1.2.3", &tx, root.path()).is_err());
        server.join().unwrap();
        assert!(!dest.exists());
        let expected = if headers.contains("/7") || headers.contains("different") {
            b"".as_slice()
        } else {
            b"abc".as_slice()
        };
        assert_eq!(std::fs::read(part_path).unwrap(), expected);
    }
}

#[test]
fn replaced_assets_do_not_reuse_partial_files_from_the_old_hash() {
    let (url, server) = serve(1, |_, request, stream| {
        assert!(!request.contains("\r\nrange:"));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nnewnew")
            .unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let mut old =
        PartialDownload::open(&asset(url.clone(), b"oldold"), "1.2.3", root.path()).unwrap();
    old.file.write_all(b"old").unwrap();
    drop(old);
    let (tx, _rx) = channel();
    let path = download_to(&asset(url, b"newnew"), "1.2.3", &tx, root.path()).unwrap();
    server.join().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"newnew");
}

#[test]
fn waiting_for_network_can_be_paused_without_losing_progress() {
    let (url, server) = serve(1, |_, _, stream| {
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc")
            .unwrap();
    });
    let asset = asset(url, b"abcdef");
    let root = tempfile::tempdir().unwrap();
    let (tx, rx) = channel();
    let (cancel, stop) = channel();
    let root_path = root.path().to_owned();
    let worker = std::thread::spawn(move || {
        run_with_agent(
            &asset,
            "1.2.3",
            &tx,
            &root_path,
            &download_agent(Duration::from_millis(500)),
            &stop,
            |_| Duration::from_secs(30),
        )
    });
    loop {
        if matches!(
            rx.recv_timeout(Duration::from_secs(3)).unwrap(),
            Event::DownloadRetrying { .. }
        ) {
            break;
        }
    }
    let paused_at = Instant::now();
    cancel.send(()).unwrap();
    assert_eq!(worker.join().unwrap().unwrap(), None);
    assert!(paused_at.elapsed() < Duration::from_secs(2));
    server.join().unwrap();
    let partial = std::fs::read_dir(root.path().join("1.2.3"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "part"))
        .unwrap();
    assert_eq!(std::fs::read(partial).unwrap(), b"abc");
}

#[test]
fn retryable_http_errors_recover_but_permanent_errors_stop() {
    let (url, server) = serve(3, |attempt, _, stream| {
        let response = match attempt {
            0 => "HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            1 => "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            _ => "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc",
        };
        stream.write_all(response.as_bytes()).unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let (tx, _rx) = channel();
    let (_cancel, stop) = channel();
    assert!(run_fast(&asset(url, b"abc"), root.path(), &tx, &stop)
        .unwrap()
        .is_some());
    server.join().unwrap();
    let (url, server) = serve(1, |_, _, stream| {
        stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
    });
    assert!(run_fast(&asset(url, b"abc"), root.path(), &tx, &stop)
        .unwrap_err()
        .contains("404"));
    server.join().unwrap();
}

#[test]
fn completed_partial_is_verified_without_a_network_request() {
    let root = tempfile::tempdir().unwrap();
    let asset = asset("http://127.0.0.1:1/setup.exe".into(), b"abc");
    let mut partial = PartialDownload::open(&asset, "1.2.3", root.path()).unwrap();
    partial.file.write_all(b"abc").unwrap();
    drop(partial);
    let (tx, _rx) = channel();
    let path = download_to(&asset, "1.2.3", &tx, root.path()).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"abc");
}

#[test]
fn corrupt_completed_cache_is_replaced_and_unknown_416_resets_incomplete_cache() {
    for known_size in [true, false] {
        let (url, server) = serve(
            if known_size { 1 } else { 2 },
            move |attempt, request, stream| {
                if !known_size && attempt == 0 {
                    assert!(request.contains("range: bytes=3-\r\n"));
                    stream.write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */6\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    assert!(!request.contains("\r\nrange:"));
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabcdef").unwrap();
                }
            },
        );
        let mut asset = asset(url, b"abcdef");
        if !known_size {
            asset.size = 0;
        }
        let root = tempfile::tempdir().unwrap();
        let mut partial = PartialDownload::open(&asset, "1.2.3", root.path()).unwrap();
        partial
            .file
            .write_all(if known_size { b"broken" } else { b"abc" })
            .unwrap();
        drop(partial);
        let (tx, _rx) = channel();
        let path = download_to(&asset, "1.2.3", &tx, root.path()).unwrap();
        server.join().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"abcdef");
    }
}

#[test]
fn short_valid_ranges_continue_until_the_whole_file_is_verified() {
    let (url, server) = serve(2, |attempt, request, stream| {
        if attempt == 0 {
            assert!(!request.contains("\r\nrange:"));
            stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-2/6\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc").unwrap();
        } else {
            assert!(request.contains("range: bytes=3-\r\n"));
            stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 3-5/6\r\nContent-Length: 3\r\nConnection: close\r\n\r\ndef").unwrap();
        }
    });
    let root = tempfile::tempdir().unwrap();
    let (tx, rx) = channel();
    let (_cancel, stop) = channel();
    let path = run_fast(&asset(url, b"abcdef"), root.path(), &tx, &stop)
        .unwrap()
        .unwrap();
    server.join().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"abcdef");
    assert!(!rx
        .try_iter()
        .any(|ev| matches!(ev, Event::DownloadRetrying { .. })));
}

#[test]
fn resumed_bytes_still_require_the_full_sha256_before_replacing_the_installer() {
    let (url, server) = serve(1, |_, request, stream| {
        assert!(request.contains("range: bytes=3-\r\n"));
        stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 3-5/6\r\nContent-Length: 3\r\nConnection: close\r\n\r\nBAD").unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let asset = asset(url, b"abcdef");
    let mut partial = PartialDownload::open(&asset, "1.2.3", root.path()).unwrap();
    partial.file.write_all(b"abc").unwrap();
    std::fs::write(&partial.dest, b"previous verified installer").unwrap();
    let dest = partial.dest.clone();
    let path = partial.path.clone();
    drop(partial);
    let (tx, _rx) = channel();
    let (_cancel, stop) = channel();
    assert!(run_fast(&asset, root.path(), &tx, &stop)
        .unwrap_err()
        .contains("sha256"));
    server.join().unwrap();
    assert_eq!(std::fs::read(dest).unwrap(), b"previous verified installer");
    assert!(std::fs::read(path).unwrap().is_empty());
}

#[test]
fn a_second_worker_cannot_write_the_same_partial() {
    let root = tempfile::tempdir().unwrap();
    let asset = asset("http://127.0.0.1:1/setup.exe".into(), b"abcdef");
    let mut first = PartialDownload::open(&asset, "1.2.3", root.path()).unwrap();
    first.file.write_all(b"abc").unwrap();
    assert!(PartialDownload::open(&asset, "1.2.3", root.path()).is_err());
    drop(first);
    let next = PartialDownload::open(&asset, "1.2.3", root.path()).unwrap();
    assert_eq!(next.len().unwrap(), 3);
}

#[test]
fn redirected_resume_requests_keep_range_headers_and_use_the_original_url_each_time() {
    let (cdn_url, cdn) = serve(1, |_, request, stream| {
        assert!(request.contains("range: bytes=3-\r\n"), "{request}");
        assert!(request.contains("if-range: \"release\"\r\n"));
        stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nETag: \"release\"\r\nConnection: close\r\n\r\ndef").unwrap();
    });
    let (url, origin) = serve(2, move |attempt, request, stream| {
        if attempt == 0 {
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"release\"\r\nConnection: close\r\n\r\nabc").unwrap();
        } else {
            assert!(request.contains("range: bytes=3-\r\n"));
            write!(stream, "HTTP/1.1 302 Found\r\nLocation: {cdn_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        }
    });
    let root = tempfile::tempdir().unwrap();
    let (tx, _rx) = channel();
    let (_cancel, stop) = channel();
    let path = run_fast(&asset(url, b"abcdef"), root.path(), &tx, &stop)
        .unwrap()
        .unwrap();
    origin.join().unwrap();
    cdn.join().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"abcdef");
}

#[test]
fn compressed_range_responses_are_rejected_before_writing_bytes() {
    let (url, server) = serve(1, |_, _, stream| {
        stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Encoding: gzip\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nConnection: close\r\n\r\ndef").unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let asset = asset(url, b"abcdef");
    let mut partial = PartialDownload::open(&asset, "1.2.3", root.path()).unwrap();
    partial.file.write_all(b"abc").unwrap();
    let path = partial.path.clone();
    drop(partial);
    let (tx, _rx) = channel();
    assert!(download_to(&asset, "1.2.3", &tx, root.path())
        .unwrap_err()
        .contains("压缩编码"));
    server.join().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"abc");
}

#[test]
fn a_failed_commit_preserves_verified_bytes_for_a_later_retry() {
    let root = tempfile::tempdir().unwrap();
    let asset = asset("http://127.0.0.1:1/setup.exe".into(), b"abc");
    let mut partial = PartialDownload::open(&asset, "1.2.3", root.path()).unwrap();
    partial.file.write_all(b"abc").unwrap();
    let path = partial.path.clone();
    let destination = partial.dest.clone();
    std::fs::create_dir(&destination).unwrap();
    drop(partial);
    let (tx, _rx) = channel();
    assert!(download_to(&asset, "1.2.3", &tx, root.path())
        .unwrap_err()
        .contains("保存安装包失败"));
    assert_eq!(std::fs::read(path).unwrap(), b"abc");
    std::fs::remove_dir(destination).unwrap();
    assert!(download_to(&asset, "1.2.3", &tx, root.path()).is_ok());
}

#[test]
fn disconnecting_a_chunked_response_keeps_only_decoded_payload_offsets() {
    let (url, server) = serve(2, |attempt, request, stream| {
        if attempt == 0 {
            // The terminating chunk is missing because the connection was lost.
            stream.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n").unwrap();
        } else {
            assert!(request.contains("range: bytes=3-\r\n"));
            stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nConnection: close\r\n\r\ndef").unwrap();
        }
    });
    let root = tempfile::tempdir().unwrap();
    let (tx, _rx) = channel();
    let (_cancel, stop) = channel();
    let path = run_fast(&asset(url, b"abcdef"), root.path(), &tx, &stop)
        .unwrap()
        .unwrap();
    server.join().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"abcdef");
}

#[test]
fn unknown_length_disconnects_resume_and_416_can_confirm_completion() {
    let (url, server) = serve(2, |attempt, request, stream| {
        if attempt == 0 {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nabc")
                .unwrap();
        } else {
            assert!(request.contains("range: bytes=3-\r\n"));
            stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 3-5/6\r\nContent-Length: 3\r\nConnection: close\r\n\r\ndef").unwrap();
        }
    });
    let mut asset = asset(url, b"abcdef");
    asset.size = 0;
    let root = tempfile::tempdir().unwrap();
    let (tx, _rx) = channel();
    let (_cancel, stop) = channel();
    let path = run_fast(&asset, root.path(), &tx, &stop).unwrap().unwrap();
    server.join().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"abcdef");

    let (url, server) = serve(1, |_, request, stream| {
        assert!(request.contains("range: bytes=6-\r\n"));
        stream.write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */6\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    asset.url = url;
    let mut partial = PartialDownload::open(&asset, "1.2.3", root.path()).unwrap();
    partial.file.write_all(b"abcdef").unwrap();
    drop(partial);
    assert_eq!(
        download_to(&asset, "1.2.3", &tx, root.path()).unwrap(),
        path
    );
    server.join().unwrap();
}

#[test]
fn ranges_and_retry_delays_are_bounded() {
    assert_eq!(parse_range("bytes 3-5/6"), Some((3, 5, 6)));
    for invalid in [
        "bytes 5-3/6",
        "bytes 0-6/6",
        "bytes 0-1/*",
        "bytes */6",
        "bytes 0-18446744073709551615/0",
    ] {
        assert_eq!(parse_range(invalid), None);
    }
    assert_eq!(retry_delay(0), Duration::from_secs(2));
    assert_eq!(retry_delay(1), Duration::from_secs(4));
    assert_eq!(retry_delay(u32::MAX), Duration::from_secs(30));
    assert!(strong_etag("\"abc\""));
    assert!(!strong_etag("W/\"abc\""));
    assert!(!strong_etag("\"abc\r\n\""));
}
