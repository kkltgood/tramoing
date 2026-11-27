//! Opt-in checks against the real macOS network collector.
//! Run outside a sandbox with `cargo test -p tm-macos --test native_runtime
//! -- --ignored --nocapture --test-threads=1`. The proxy check additionally needs
//! TM_TEST_PROXY and downloads 4 MiB of public test data.
#![cfg(target_os = "macos")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tm_macos::NettopSampler;
use tm_sampler::Sampler;

#[test]
#[ignore = "requires live nettop and local IPv4/IPv6 sockets"]
fn live_connection_details_include_real_pids_listeners_loopback_and_udp() {
    use tm_core::{AppKey, ConnectionProtocol, ConnectionState};

    let mut sampler = NettopSampler::spawn().expect("start nettop");
    let pid = std::process::id();
    let app = AppKey::new(tm_macos::identify(pid, "native_runtime").key);
    for (address, protocol, udp_protocol) in [
        (
            "127.0.0.1:0",
            ConnectionProtocol::Tcp4,
            ConnectionProtocol::Udp4,
        ),
        (
            "[::1]:0",
            ConnectionProtocol::Tcp6,
            ConnectionProtocol::Udp6,
        ),
    ] {
        let listener = TcpListener::bind(address).unwrap();
        let listener_address = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(listener_address).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        client.write_all(b"socket details").unwrap();
        server.read_exact(&mut [0; 14]).unwrap();
        let client_address = client.local_addr().unwrap();
        let udp = UdpSocket::bind(address).unwrap();
        let udp_address = udp.local_addr().unwrap();
        let mut matched = false;
        for _ in 0..6 {
            sampler.sample().expect("current socket snapshot");
            let report = sampler.connections(&app).expect("connection details");
            let rows = &report.connections;
            matched = rows.iter().any(|row| {
                row.pid == pid
                    && row.protocol == protocol
                    && row.state == ConnectionState::Listen
                    && row.local.port == listener_address.port()
                    && row.remote.is_none()
            }) && rows.iter().any(|row| {
                row.pid == pid
                    && row.protocol == protocol
                    && row.state == ConnectionState::Established
                    && row.local.port == client_address.port()
                    && row.remote.as_ref().is_some_and(|remote| {
                        remote.port == listener_address.port()
                            && remote.address == listener_address.ip().to_string()
                    })
            }) && rows.iter().any(|row| {
                row.pid == pid
                    && row.protocol == udp_protocol
                    && row.local.port == udp_address.port()
                    && row.remote.is_none()
            });
            if matched {
                break;
            }
        }
        assert!(
            matched,
            "missing live {protocol:?} listener, stream or UDP socket"
        );
        drop(client);
        drop(server);
        drop(listener);
        drop(udp);
        let mut removed = false;
        for _ in 0..6 {
            sampler.sample().expect("snapshot after sockets closed");
            let report = sampler.connections(&app).unwrap();
            removed = !report.connections.iter().any(|row| {
                (row.protocol == protocol
                    && row.local.port == listener_address.port()
                    && row.state == ConnectionState::Listen)
                    || (row.protocol == udp_protocol && row.local.port == udp_address.port())
            });
            if removed {
                break;
            }
        }
        assert!(removed, "closed listener/UDP socket remained in details");
    }
    assert_eq!(
        sampler.total_bytes(app.as_str()),
        tm_macos::Bytes::default()
    );
}

fn child_cpu_seconds() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage fills this correctly sized buffer on success.
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, usage.as_mut_ptr()) },
        0
    );
    // SAFETY: getrusage succeeded and initialized the buffer above.
    let usage = unsafe { usage.assume_init() };
    let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1_000_000.0;
    seconds(usage.ru_utime) + seconds(usage.ru_stime)
}

#[test]
#[ignore = "requires live nettop; run with --test-threads=1 for child CPU accounting"]
fn live_nettop_waits_between_samples_without_spinning_on_stdin() {
    // No input is sent to the collector. It must wait between its 1 Hz samples,
    // rather than continuously dispatching readability notifications for EOF.
    let before = child_cpu_seconds();
    let started = Instant::now();
    let mut sampler = NettopSampler::spawn().expect("start nettop");
    for _ in 0..6 {
        sampler.sample().expect("live sample");
    }
    // RUSAGE_CHILDREN includes this collector only after it has been reaped.
    drop(sampler);
    let elapsed = started.elapsed().as_secs_f64();
    let cpu = child_cpu_seconds() - before;
    let percent = cpu / elapsed * 100.0;
    eprintln!("nettop: {cpu:.3}s CPU over {elapsed:.3}s, {percent:.2}% of one core");
    // Allow substantial connection/scheduling overhead while catching the
    // approximately 100% CPU EOF loop seen on macOS.
    assert!(
        percent < 25.0,
        "nettop used {percent:.2}% CPU without input"
    );
}

#[test]
#[ignore = "requires live nettop and local socket access"]
fn live_loopback_is_excluded_and_nettop_is_reaped() {
    let mut sampler = NettopSampler::spawn().expect("start nettop");
    let children = Command::new("/usr/bin/pgrep")
        .args(["-P", &std::process::id().to_string(), "-x", "nettop"])
        .output()
        .expect("find our nettop child");
    assert!(children.status.success());
    let child: i32 = String::from_utf8(children.stdout)
        .unwrap()
        .trim()
        .parse()
        .expect("exactly one nettop child");
    sampler.sample().expect("baseline");
    let app = tm_macos::procname::executable_path(std::process::id()).unwrap();

    for address in ["127.0.0.1:0", "[::1]:0"] {
        let listener = TcpListener::bind(address).unwrap();
        let address = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(address).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let timeout = Some(Duration::from_secs(8));
        server.set_write_timeout(timeout).unwrap();
        client.set_read_timeout(timeout).unwrap();
        let writer = thread::spawn(move || {
            for _ in 0..128 {
                server.write_all(&[0x5a; 65536]).unwrap();
                thread::sleep(Duration::from_millis(25));
            }
        });
        let reader = thread::spawn(move || {
            let mut data = Vec::new();
            client.read_to_end(&mut data).unwrap();
            assert_eq!(data.len(), 8 * 1024 * 1024);
            assert!(data.iter().all(|&b| b == 0x5a));
        });
        let mut intervals = Vec::new();
        for _ in 0..6 {
            let started = Instant::now();
            sampler.sample().expect("live sample");
            intervals.push(started.elapsed().as_secs_f64());
        }
        writer.join().unwrap();
        reader.join().unwrap();
        let bytes = sampler.total_bytes(&app);
        eprintln!("{address}: transferred 8 MiB, accounted {bytes:?}, intervals {intervals:?}");
        assert_eq!((bytes.rx, bytes.tx), (0, 0));
        // Allow scheduling jitter while catching pipe-buffered three-second bursts.
        assert!(intervals.iter().all(|&dt| (0.5..1.8).contains(&dt)));
    }

    let started = Instant::now();
    drop(sampler);
    assert!(started.elapsed() < Duration::from_secs(2));
    // SAFETY: signal 0 only checks whether this known child still exists.
    assert_eq!(unsafe { libc::kill(child, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    eprintln!("nettop {child} reaped in {:?}", started.elapsed());
}

struct Download(Child);

impl Drop for Download {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "requires TM_TEST_PROXY and downloads 4 MiB from speed.cloudflare.com"]
fn live_proxy_download_is_attributed_to_client() {
    let proxy = std::env::var("TM_TEST_PROXY").expect("set TM_TEST_PROXY to the test HTTP proxy");
    let mut sampler = NettopSampler::spawn().expect("start nettop");
    sampler.sample().expect("baseline");
    let before = sampler.total_bytes("/usr/bin/curl");
    let mut download = Download(
        Command::new("/usr/bin/curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--proxy",
                &proxy,
                "--noproxy",
                "",
                "--max-time",
                "40",
                "--limit-rate",
                "256k",
                "--output",
                "/dev/null",
                "--write-out",
                "%{size_download}",
                "https://speed.cloudflare.com/__down?bytes=4194304",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .expect("start curl"),
    );
    let started = Instant::now();
    loop {
        sampler.sample().expect("live sample during download");
        if let Some(status) = download.0.try_wait().unwrap() {
            assert!(status.success(), "curl failed: {status}");
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(45));
    }
    for _ in 0..2 {
        sampler.sample().expect("drain final samples");
    }
    let mut output = String::new();
    download
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut output)
        .unwrap();
    assert_eq!(output.trim(), "4194304");
    let after = sampler.total_bytes("/usr/bin/curl");
    let (rx, tx) = (after.rx - before.rx, after.tx - before.tx);
    eprintln!("proxy download: payload=4194304, curl rx={rx}, tx={tx}");
    // nettop samples sockets: a connection closing between frames can lose its tail.
    assert!(
        (3_145_728..=4_823_450).contains(&rx),
        "download attribution: {rx}"
    );
    assert!(tx < 104_858, "download incorrectly counted as upload: {tx}");
}
