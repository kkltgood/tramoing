//! 管理员原生验收。只阻断临时目录里的测试程序副本，绝不修改用户软件规则。
//! cargo test -p tm-windows --test network_blocking wfp_blocks -- --ignored --nocapture

#![cfg(windows)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tm_windows::firewall::Firewall;
use tm_windows::network_policy::same_path;

const IO_TIMEOUT: Duration = Duration::from_millis(600);

enum Connection {
    Tcp(TcpStream),
    Udp(UdpSocket),
}
impl Connection {
    fn connect(target: SocketAddr, udp: bool) -> std::io::Result<Self> {
        if udp {
            let socket = UdpSocket::bind(if target.is_ipv4() {
                "127.0.0.1:0"
            } else {
                "[::1]:0"
            })?;
            socket.set_read_timeout(Some(IO_TIMEOUT))?;
            socket.set_write_timeout(Some(IO_TIMEOUT))?;
            socket.connect(target)?;
            Ok(Self::Udp(socket))
        } else {
            let stream = TcpStream::connect_timeout(&target, IO_TIMEOUT)?;
            stream.set_read_timeout(Some(IO_TIMEOUT))?;
            stream.set_write_timeout(Some(IO_TIMEOUT))?;
            Ok(Self::Tcp(stream))
        }
    }
    fn exchange(&mut self) -> std::io::Result<()> {
        let mut response = [0u8; 4];
        match self {
            Self::Tcp(stream) => {
                stream.write_all(b"ping")?;
                stream.read_exact(&mut response)?;
            }
            Self::Udp(socket) => {
                socket.send(b"ping")?;
                if socket.recv(&mut response)? != 4 {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
            }
        }
        if response == *b"ping" {
            Ok(())
        } else {
            Err(std::io::ErrorKind::InvalidData.into())
        }
    }
}

fn wait_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn write_result(path: &Path, text: &str) {
    let temporary = path.with_extension("writing");
    std::fs::write(&temporary, text).unwrap();
    std::fs::rename(temporary, path).unwrap();
}

#[test]
#[ignore = "child entry point used only by the isolated administrator test"]
fn network_probe_child() {
    let Some(target) = std::env::var("TM_PROBE_TARGET").ok() else {
        return;
    };
    let output = PathBuf::from(std::env::var_os("TM_PROBE_OUTPUT").unwrap());
    let udp = std::env::var("TM_PROBE_UDP").unwrap() == "true";
    let mut connection = Connection::connect(target.parse().unwrap(), udp);
    let allowed = connection.as_mut().is_ok_and(|c| c.exchange().is_ok());
    if let Some(gate) = std::env::var_os("TM_PROBE_GATE") {
        assert!(allowed, "baseline persistent connection must work");
        write_result(&output.with_extension("ready"), "ready");
        wait_file(Path::new(&gate));
        let allowed = connection.as_mut().is_ok_and(|c| c.exchange().is_ok());
        write_result(&output, if allowed { "allowed" } else { "blocked" });
    } else {
        write_result(&output, if allowed { "allowed" } else { "blocked" });
    }
}

struct EchoServers {
    stop: Arc<AtomicBool>,
    targets: Vec<(SocketAddr, bool)>,
}
impl EchoServers {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let mut targets = Vec::new();
        for bind in ["127.0.0.1:0", "[::1]:0"] {
            let listener = TcpListener::bind(bind).unwrap();
            listener.set_nonblocking(true).unwrap();
            targets.push((listener.local_addr().unwrap(), false));
            let halt = stop.clone();
            std::thread::spawn(move || {
                while !halt.load(Ordering::Relaxed) {
                    if let Ok((mut stream, _)) = listener.accept() {
                        let halt = halt.clone();
                        std::thread::spawn(move || {
                            stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
                            stream.set_write_timeout(Some(IO_TIMEOUT)).unwrap();
                            let mut buf = [0u8; 256];
                            while !halt.load(Ordering::Relaxed) {
                                match stream.read(&mut buf) {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        if stream.write_all(&buf[..n]).is_err() {
                                            break;
                                        }
                                    }
                                    Err(e)
                                        if matches!(
                                            e.kind(),
                                            std::io::ErrorKind::TimedOut
                                                | std::io::ErrorKind::WouldBlock
                                        ) => {}
                                    Err(_) => break,
                                }
                            }
                        });
                    } else {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            });
            let socket = UdpSocket::bind(bind).unwrap();
            socket.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
            targets.push((socket.local_addr().unwrap(), true));
            let halt = stop.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 256];
                while !halt.load(Ordering::Relaxed) {
                    if let Ok((n, peer)) = socket.recv_from(&mut buf) {
                        let _ = socket.send_to(&buf[..n], peer);
                    }
                }
            });
        }
        Self { stop, targets }
    }
}
impl Drop for EchoServers {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

struct Probe(Child);
impl Drop for Probe {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_probe(
    exe: &Path,
    target: SocketAddr,
    udp: bool,
    output: &Path,
    gate: Option<&Path>,
) -> Probe {
    let mut command = Command::new(exe);
    command
        .args(["--exact", "network_probe_child", "--ignored", "--nocapture"])
        .env("TM_PROBE_TARGET", target.to_string())
        .env("TM_PROBE_UDP", udp.to_string())
        .env("TM_PROBE_OUTPUT", output)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(gate) = gate {
        command.env("TM_PROBE_GATE", gate);
    } else {
        command.env_remove("TM_PROBE_GATE");
    }
    Probe(command.spawn().unwrap())
}

fn probe(exe: &Path, target: SocketAddr, udp: bool, output: &Path) -> bool {
    let _child = spawn_probe(exe, target, udp, output, None);
    wait_file(output);
    std::fs::read_to_string(output).unwrap() == "allowed"
}

struct Restore(String);
impl Drop for Restore {
    fn drop(&mut self) {
        if let Err(error) = Firewall::open().and_then(|f| f.set(&self.0, false)) {
            eprintln!("TEST CLEANUP FAILED for {}: {error}", self.0);
        }
    }
}

#[test]
#[ignore = "requires administrator; temporarily blocks only isolated test exe copies"]
fn wfp_blocks_only_selected_exe_including_loopback_and_existing_connections() {
    let firewall = Firewall::open().expect("run this test as administrator");
    let baseline = firewall.list().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let exe = directory.path().join("probe.exe");
    let other_dir = directory.path().join("other");
    std::fs::create_dir(&other_dir).unwrap();
    let other = other_dir.join("probe.exe");
    for target in [&exe, &other] {
        std::fs::copy(std::env::current_exe().unwrap(), target).unwrap();
    }
    let path = exe.to_str().unwrap();
    let restore = Restore(path.into());
    let servers = EchoServers::start();
    let mut pending = Vec::new();
    for (i, (target, udp)) in servers.targets.iter().enumerate() {
        assert!(probe(
            &exe,
            *target,
            *udp,
            &directory.path().join(format!("baseline-{i}"))
        ));
        let output = directory.path().join(format!("existing-{i}"));
        let gate = directory.path().join(format!("gate-{i}"));
        let child = spawn_probe(&exe, *target, *udp, &output, Some(&gate));
        wait_file(&output.with_extension("ready"));
        pending.push((child, output, gate));
    }
    let rules = firewall.set(path, true).unwrap();
    assert!(rules.iter().any(|r| same_path(&r.path, path) && r.complete));
    // 重复点击幂等，不产生多份规则；关闭控制连接不撤销持久规则。
    firewall.set(path, true).unwrap();
    drop(firewall);
    for (i, (target, udp)) in servers.targets.iter().enumerate() {
        assert!(
            !probe(
                &exe,
                *target,
                *udp,
                &directory.path().join(format!("blocked-{i}"))
            ),
            "new {target} UDP={udp}"
        );
        assert!(
            probe(
                &other,
                *target,
                *udp,
                &directory.path().join(format!("unaffected-{i}"))
            ),
            "same-name program at another path"
        );
    }
    for (_child, output, gate) in pending {
        std::fs::write(gate, b"continue").unwrap();
        wait_file(&output);
        assert_eq!(
            std::fs::read_to_string(output).unwrap(),
            "blocked",
            "already-established flow must be blocked"
        );
    }
    let firewall = Firewall::open().unwrap();
    firewall.set(path, false).unwrap();
    for (i, (target, udp)) in servers.targets.iter().enumerate() {
        assert!(
            probe(
                &exe,
                *target,
                *udp,
                &directory.path().join(format!("restored-{i}"))
            ),
            "restored {target}"
        );
    }
    firewall.set(path, true).unwrap();
    std::fs::remove_file(&exe).unwrap();
    firewall.set(path, false).unwrap();
    assert_eq!(
        firewall.list().unwrap(),
        baseline,
        "only test rules may change"
    );
    drop(restore);
    println!("PASS: TCP/UDP IPv4/IPv6 loopback, existing flows, path isolation, persistence across engine close, restore and deleted exe cleanup");
}

/// 复制本测试二进制为同目录的 tramoing-helper.exe（提升权限）和 tramoing.exe
/// （普通权限）后，通过这两个入口验证真实路径校验、完整性标签及应答确认。
#[test]
#[ignore = "paired manual test entry; requires TM_CONTROL_VERIFY and elevated helper copy"]
fn control_pipe_server_child() {
    if std::env::var_os("TM_CONTROL_VERIFY").is_none() {
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let root = exe.parent().unwrap();
    let target = root.join("probe.exe");
    assert!(target.is_file());
    let server = tm_windows::network_control::Server::create().unwrap();
    std::thread::spawn(move || server.run(|message| eprintln!("{message}")));
    write_result(&root.join("ready"), "ready");
    let deadline = Instant::now() + Duration::from_secs(90);
    while !root.join("stop").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    Firewall::open()
        .unwrap()
        .set(target.to_str().unwrap(), false)
        .unwrap();
}

#[test]
#[ignore = "paired manual test entry; run the tramoing.exe copy at normal user integrity"]
fn control_pipe_client_child() {
    if std::env::var_os("TM_CONTROL_VERIFY").is_none() {
        return;
    }
    use tm_windows::network_policy::Request;
    let exe = std::env::current_exe().unwrap();
    let target = exe.parent().unwrap().join("probe.exe");
    let call = |request| {
        let response = tm_windows::network_control::request(&request).unwrap();
        assert!(response.error.is_none(), "{:?}", response.error);
        response.rules
    };
    let before = call(Request::List {});
    let path = target.to_str().unwrap().to_owned();
    let rules = call(Request::Set {
        path: path.clone(),
        blocked: true,
    });
    assert!(rules
        .iter()
        .any(|r| same_path(&r.path, &path) && r.complete));
    let restored = call(Request::Set {
        path,
        blocked: false,
    });
    assert_eq!(restored, before);
    println!("PASS: normal-user client talks to elevated helper; confirmed block and restore");
}
