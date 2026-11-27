//! 命名管道：助手进程（服务端）→ 界面进程（客户端）。
//!
//! 管道是**单向推流**：助手每秒写一帧，界面端只管读。这样界面进程不需要
//! 任何权限，也不用反向查询 —— 权限全部集中在助手那一侧。
//!
//! 分帧方式：每帧前面加一个 `u32` 小端长度前缀。管道是字节流，没有消息边界，
//! 光靠 [`crate::proto`] 自己的帧头也能解，但「先读长度再读整块」更不容易出错，
//! 也让读循环能按需预分配。仅当前账户和 SYSTEM 可访问，双方验证安装路径。
//! 写入有截止时间，停止读取的客户端不会永久占住采集推流。

use std::os::windows::io::IntoRawHandle;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, OPEN_EXISTING, PIPE_ACCESS_OUTBOUND, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, DisconnectNamedPipe, PeekNamedPipe, WaitNamedPipeW,
};

use crate::proto::{Frame, ProtoError, MAX_FRAME_BYTES};

const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum PipeError {
    Security(String),
    Connect(u32),
    Io(u32),
    /// 对端关闭
    Closed,
    Proto(ProtoError),
    FrameTooLarge(usize),
    Timeout,
}

impl std::fmt::Display for PipeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Security(message) => write!(f, "采集通道身份或权限核验失败：{message}"),
            Self::Connect(c) => write!(f, "连接命名管道失败（Win32 错误 {c}）"),
            Self::Io(c) => write!(f, "管道读写失败（Win32 错误 {c}）"),
            Self::Closed => write!(f, "管道已被对端关闭"),
            Self::Proto(e) => write!(f, "帧格式错误：{e}"),
            Self::FrameTooLarge(n) => write!(f, "帧长度异常：{n} 字节"),
            Self::Timeout => write!(f, "采集管道读写超时"),
        }
    }
}

impl std::error::Error for PipeError {}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn last_error() -> u32 {
    // SAFETY: 无参数
    unsafe { GetLastError() }
}

fn write_all(handle: HANDLE, data: &[u8], deadline: Instant) -> Result<(), PipeError> {
    let mut off = 0usize;
    while off < data.len() {
        if Instant::now() >= deadline {
            return Err(PipeError::Timeout);
        }
        let mut written: u32 = 0;
        // SAFETY: 缓冲区指针与长度来自同一个切片，句柄由本结构体持有
        let ok = unsafe {
            WriteFile(
                handle,
                data[off..].as_ptr(),
                (data.len() - off).min(16 * 1024) as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(PipeError::Io(last_error()));
        }
        if written == 0 {
            // PIPE_NOWAIT reports a full byte-pipe buffer as a successful zero-byte write.
            std::thread::sleep(Duration::from_millis(10));
        }
        off += written as usize;
    }
    Ok(())
}

fn read_exact(handle: HANDLE, buf: &mut [u8]) -> Result<(), PipeError> {
    let mut off = 0usize;
    while off < buf.len() {
        let mut got: u32 = 0;
        // SAFETY: 同上，目标缓冲区有效
        let ok = unsafe {
            ReadFile(
                handle,
                buf[off..].as_mut_ptr(),
                (buf.len() - off) as u32,
                &mut got,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(PipeError::Io(last_error()));
        }
        if got == 0 {
            return Err(PipeError::Closed);
        }
        off += got as usize;
    }
    Ok(())
}

/// 服务端（跑在提权的助手进程里）。
pub struct PipeServer {
    handle: HANDLE,
}

impl PipeServer {
    pub fn create() -> Result<Self, PipeError> {
        Self::create_named(crate::PIPE_NAME)
    }

    fn create_named(name: &str) -> Result<Self, PipeError> {
        let handle = crate::network_control::create_restricted_pipe(name, PIPE_ACCESS_OUTBOUND)
            .map_err(PipeError::Security)?
            .into_raw_handle();
        Ok(Self { handle })
    }

    /// 等待本目录的普通权限界面连接，未通过核验前不发送任何统计数据。
    pub fn accept(&self) -> Result<(), PipeError> {
        self.accept_connection()?;
        crate::network_control::verify_peer(self.handle, false).map_err(PipeError::Security)
    }

    fn accept_connection(&self) -> Result<(), PipeError> {
        loop {
            let ok = unsafe { ConnectNamedPipe(self.handle, std::ptr::null_mut()) };
            let err = last_error();
            // With PIPE_NOWAIT, success can mean only that the instance has
            // become available after DisconnectNamedPipe. Only CONNECTED is
            // proof of a client; otherwise keep waiting without a busy loop.
            if ok == 0 && err == ERROR_PIPE_CONNECTED {
                return Ok(());
            }
            if ok == 0 && err != ERROR_PIPE_LISTENING {
                return Err(PipeError::Connect(err));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// 写一帧（带长度前缀）。
    pub fn send_frame(&self, frame: &Frame, scratch: &mut Vec<u8>) -> Result<(), PipeError> {
        scratch.clear();
        scratch.extend_from_slice(&[0u8; 4]); // 占位，稍后回填长度
        let mut payload = Vec::new();
        frame.encode_into(&mut payload).map_err(PipeError::Proto)?;
        scratch[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        scratch.extend_from_slice(&payload);
        write_all(self.handle, scratch, Instant::now() + WRITE_TIMEOUT)
    }
}

impl Drop for PipeServer {
    fn drop(&mut self) {
        // SAFETY: 句柄由本结构体独占
        unsafe {
            DisconnectNamedPipe(self.handle);
            CloseHandle(self.handle);
        }
    }
}

/// 客户端（跑在普通权限的界面进程里）。
pub struct PipeClient {
    handle: HANDLE,
    buffered: Vec<u8>,
}

// 句柄由客户端独占，只有同步操作；移动所有权时没有挂起的 IO。
unsafe impl Send for PipeClient {}

impl PipeClient {
    /// 带超时地连接。助手还没起来时返回错误，调用方负责重试。
    pub fn connect(timeout_ms: u32) -> Result<Self, PipeError> {
        Self::connect_verified(crate::PIPE_NAME, timeout_ms)
    }

    fn connect_verified(name: &str, timeout_ms: u32) -> Result<Self, PipeError> {
        let client = Self::connect_named(name, timeout_ms)?;
        crate::network_control::verify_peer(client.handle, true).map_err(PipeError::Security)?;
        Ok(client)
    }

    fn connect_named(name: &str, timeout_ms: u32) -> Result<Self, PipeError> {
        let name = wide(name);
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);

        loop {
            // SAFETY: 名字有效；只读打开，不共享
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    windows_sys::Win32::Foundation::GENERIC_READ,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    std::ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                return Ok(Self {
                    handle,
                    buffered: Vec::new(),
                });
            }
            let err = last_error();
            if err != ERROR_PIPE_BUSY {
                return Err(PipeError::Connect(err));
            }
            if std::time::Instant::now() >= deadline {
                return Err(PipeError::Connect(err));
            }
            // 管道实例被别人占着，等一会儿再来
            // SAFETY: 名字有效
            let remaining = deadline.saturating_duration_since(Instant::now());
            unsafe { WaitNamedPipeW(name.as_ptr(), remaining.as_millis().clamp(1, 500) as u32) };
        }
    }

    /// 有界等待一帧；部分帧会保留供下一次读取。
    pub fn recv_frame(&mut self) -> Result<Frame, PipeError> {
        self.poll_frame(WRITE_TIMEOUT)
    }

    /// 保留部分帧，按可读长度消费，避免同步 ReadFile 无限阻塞退出/重连。
    pub fn poll_frame(&mut self, timeout: std::time::Duration) -> Result<Frame, PipeError> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.buffered.len() >= 4 {
                let len = u32::from_le_bytes(self.buffered[..4].try_into().unwrap()) as usize;
                if len > MAX_FRAME_BYTES {
                    return Err(PipeError::FrameTooLarge(len));
                }
                if self.buffered.len() >= len + 4 {
                    let frame =
                        Frame::decode(&self.buffered[4..len + 4]).map_err(PipeError::Proto)?;
                    self.buffered.drain(..len + 4);
                    return Ok(frame);
                }
            }
            if Instant::now() >= deadline {
                return Err(PipeError::Timeout);
            }
            let mut available = 0;
            if unsafe {
                PeekNamedPipe(
                    self.handle,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(PipeError::Io(last_error()));
            }
            if available > 0 {
                let count = (available as usize).min(64 * 1024);
                let start = self.buffered.len();
                self.buffered.resize(start + count, 0);
                read_exact(self.handle, &mut self.buffered[start..])?;
            } else {
                if std::time::Instant::now() >= deadline {
                    return Err(PipeError::Timeout);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
}

impl Drop for PipeClient {
    fn drop(&mut self) {
        // SAFETY: 句柄由本结构体独占
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    };
    use std::time::Duration;

    fn test_pipe() -> String {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        format!(
            r"\\.\pipe\tramoing-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        )
    }

    #[test]
    fn missing_collector_preserves_the_windows_error_code() {
        assert!(matches!(
            PipeClient::connect_named(&test_pipe(), 0),
            Err(PipeError::Connect(
                windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND
            ))
        ));
    }

    #[test]
    fn native_pipe_recovers_from_partial_frame_timeout_and_reports_busy_and_disconnect() {
        let name = test_pipe();
        let server_name = name.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (step_tx, step_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let server = PipeServer::create_named(&server_name).unwrap();
            ready_tx.send(()).unwrap();
            server.accept_connection().unwrap();
            let frame = Frame::new(42, Vec::new());
            let mut payload = Vec::new();
            frame.encode_into(&mut payload).unwrap();
            let header = (payload.len() as u32).to_le_bytes();
            write_all(server.handle, &header[..2], Instant::now() + WRITE_TIMEOUT).unwrap();
            ready_tx.send(()).unwrap();
            step_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            write_all(server.handle, &header[2..], Instant::now() + WRITE_TIMEOUT).unwrap();
            write_all(server.handle, &payload, Instant::now() + WRITE_TIMEOUT).unwrap();
            // 等客户端读完，再关闭；DisconnectNamedPipe 会丢弃未读缓冲。
            step_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut client = PipeClient::connect_named(&name, 0).unwrap();
        assert!(matches!(
            PipeClient::connect_named(&name, 0),
            Err(PipeError::Connect(ERROR_PIPE_BUSY))
        ));
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            client.poll_frame(Duration::from_millis(30)),
            Err(PipeError::Timeout)
        ));
        step_tx.send(()).unwrap();
        assert_eq!(client.poll_frame(Duration::from_secs(2)).unwrap().seq, 42);
        step_tx.send(()).unwrap();
        server.join().unwrap();
        assert!(matches!(
            client.poll_frame(Duration::from_millis(30)),
            Err(PipeError::Io(_)) | Err(PipeError::Closed)
        ));
    }

    #[test]
    fn statistics_pipe_acl_grants_only_system_and_the_current_account() {
        use std::ptr::{addr_of, null_mut};
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertSidToStringSidW, GetSecurityInfo, SE_KERNEL_OBJECT,
        };
        use windows_sys::Win32::Security::{GetAce, ACCESS_ALLOWED_ACE, DACL_SECURITY_INFORMATION};

        let name = test_pipe();
        let server = PipeServer::create_named(&name).unwrap();
        // A second server cannot attach to this name and intercept connections.
        assert!(PipeServer::create_named(&name).is_err());
        let mut acl = null_mut();
        let mut descriptor = null_mut();
        unsafe {
            assert_eq!(
                GetSecurityInfo(
                    server.handle,
                    SE_KERNEL_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    &mut acl,
                    null_mut(),
                    &mut descriptor
                ),
                0
            );
            assert!(!acl.is_null());
            let mut trustees = Vec::new();
            for index in 0..(*acl).AceCount {
                let mut ace = null_mut();
                assert_ne!(GetAce(acl, index as u32, &mut ace), 0);
                let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
                assert_eq!(ace.Header.AceType, 0); // ACCESS_ALLOWED_ACE_TYPE
                let mut text = null_mut();
                assert_ne!(
                    ConvertSidToStringSidW(addr_of!(ace.SidStart).cast_mut().cast(), &mut text),
                    0
                );
                let mut len = 0;
                while *text.add(len) != 0 {
                    len += 1;
                }
                trustees.push(String::from_utf16_lossy(std::slice::from_raw_parts(
                    text, len,
                )));
                LocalFree(text.cast());
            }
            LocalFree(descriptor);
            trustees.sort();
            let mut expected = vec![
                "S-1-5-18".to_owned(),
                crate::scheduled_task::current_user_sid().unwrap(),
            ];
            expected.sort();
            assert_eq!(trustees, expected);
        }
    }

    #[test]
    fn statistics_pipe_rejects_untrusted_client_and_server_executables() {
        let name = test_pipe();
        let server = PipeServer::create_named(&name).unwrap();
        let client = PipeClient::connect_named(&name, 0).unwrap();
        // The unit-test executable is neither installed tramoing.exe nor helper.
        assert!(matches!(server.accept(), Err(PipeError::Security(_))));
        drop(client);
        drop(server);
        let server = PipeServer::create_named(&name).unwrap();
        assert!(matches!(
            PipeClient::connect_verified(&name, 0),
            Err(PipeError::Security(_))
        ));
        drop(server);
        // Rejected peers do not leave the name permanently occupied.
        assert!(PipeServer::create_named(&name).is_ok());
    }

    #[test]
    fn nonreading_client_times_out_and_the_pipe_can_be_recreated() {
        let name = test_pipe();
        let server = PipeServer::create_named(&name).unwrap();
        let client = PipeClient::connect_named(&name, 0).unwrap();
        server.accept_connection().unwrap();
        let started = Instant::now();
        assert!(matches!(
            write_all(
                server.handle,
                &vec![0; 256 * 1024],
                started + Duration::from_millis(100)
            ),
            Err(PipeError::Timeout)
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(client);
        drop(server);
        assert!(PipeServer::create_named(&name).is_ok());
    }

    #[test]
    fn complete_long_path_frame_crosses_pipe_buffer_boundaries() {
        let name = test_pipe();
        let server_name = name.clone();
        let frame = Frame::new(
            42,
            vec![crate::proto::ProcEntry {
                pid: 7,
                rx: 123,
                tx: 456,
                name: "微".repeat(32760),
            }],
        );
        let expected = frame.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let server = PipeServer::create_named(&server_name).unwrap();
            ready_tx.send(()).unwrap();
            server.accept_connection().unwrap();
            server.send_frame(&frame, &mut Vec::new()).unwrap();
            done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut client = PipeClient::connect_named(&name, 0).unwrap();
        assert_eq!(client.poll_frame(Duration::from_secs(5)).unwrap(), expected);
        done_tx.send(()).unwrap();
        server.join().unwrap();
    }
    #[test]
    fn nonblocking_accept_waits_for_a_new_client_after_disconnect() {
        let name = test_pipe();
        let server_name = name.clone();
        let (state_tx, state_rx) = mpsc::channel();
        let (step_tx, step_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let server = PipeServer::create_named(&server_name).unwrap();
            state_tx.send(()).unwrap();
            server.accept_connection().unwrap();
            state_tx.send(()).unwrap();
            step_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_ne!(unsafe { DisconnectNamedPipe(server.handle) }, 0);
            state_tx.send(()).unwrap();
            server.accept_connection().unwrap();
            state_tx.send(()).unwrap();
            step_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        state_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let first = PipeClient::connect_named(&name, 0).unwrap();
        state_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(first);
        step_tx.send(()).unwrap();
        state_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            state_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        let second = PipeClient::connect_named(&name, 0).unwrap();
        state_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        step_tx.send(()).unwrap();
        server.join().unwrap();
        drop(second);
    }
}
