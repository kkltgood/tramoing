//! 命名管道：助手进程（服务端）→ 界面进程（客户端）。
//!
//! 管道是**单向推流**：助手每秒写一帧，界面端只管读。这样界面进程不需要
//! 任何权限，也不用反向查询 —— 权限全部集中在助手那一侧。
//!
//! 分帧方式：每帧前面加一个 `u32` 小端长度前缀。管道是字节流，没有消息边界，
//! 光靠 [`crate::proto`] 自己的帧头也能解，但「先读长度再读整块」更不容易出错，
//! 也让读循环能按需预分配。

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, OPEN_EXISTING, PIPE_ACCESS_OUTBOUND,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW, PIPE_READMODE_BYTE,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};

use crate::proto::{Frame, ProtoError};

/// 单帧上限，防止被异常数据骗着分配几个 G。
const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug)]
pub enum PipeError {
    Create(u32),
    Connect(u32),
    Io(u32),
    /// 对端关闭
    Closed,
    Proto(ProtoError),
    FrameTooLarge(usize),
}

impl std::fmt::Display for PipeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Create(c) => write!(f, "创建命名管道失败（Win32 错误 {c}）"),
            Self::Connect(c) => write!(f, "连接命名管道失败（Win32 错误 {c}）"),
            Self::Io(c) => write!(f, "管道读写失败（Win32 错误 {c}）"),
            Self::Closed => write!(f, "管道已被对端关闭"),
            Self::Proto(e) => write!(f, "帧格式错误：{e}"),
            Self::FrameTooLarge(n) => write!(f, "帧长度异常：{n} 字节"),
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

fn write_all(handle: HANDLE, data: &[u8]) -> Result<(), PipeError> {
    let mut off = 0usize;
    while off < data.len() {
        let mut written: u32 = 0;
        // SAFETY: 缓冲区指针与长度来自同一个切片，句柄由本结构体持有
        let ok = unsafe {
            WriteFile(
                handle,
                data[off..].as_ptr(),
                (data.len() - off) as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(PipeError::Io(last_error()));
        }
        if written == 0 {
            return Err(PipeError::Closed);
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
        let name = wide(crate::PIPE_NAME);
        // 出方向即可：我们只写不读
        // SAFETY: 名字是有效的以 NUL 结尾的宽字符串
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_OUTBOUND,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,      // 只允许一个客户端
                64 * 1024,
                64 * 1024,
                0,
                std::ptr::null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(PipeError::Create(last_error()));
        }
        Ok(Self { handle })
    }

    /// 阻塞等待界面进程连上来。
    pub fn accept(&self) -> Result<(), PipeError> {
        // SAFETY: 句柄有效；同步模式不需要 OVERLAPPED
        let ok = unsafe { ConnectNamedPipe(self.handle, std::ptr::null_mut()) };
        if ok != 0 {
            return Ok(());
        }
        let err = last_error();
        // 客户端在我们调用 ConnectNamedPipe 之前就连接上了，这是正常情况
        if err == ERROR_PIPE_CONNECTED {
            return Ok(());
        }
        Err(PipeError::Connect(err))
    }

    /// 写一帧（带长度前缀）。
    pub fn send_frame(&self, frame: &Frame, scratch: &mut Vec<u8>) -> Result<(), PipeError> {
        scratch.clear();
        scratch.extend_from_slice(&[0u8; 4]); // 占位，稍后回填长度
        let mut payload = Vec::new();
        frame.encode_into(&mut payload);
        scratch[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        scratch.extend_from_slice(&payload);
        write_all(self.handle, scratch)
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
}

impl PipeClient {
    /// 带超时地连接。助手还没起来时返回错误，调用方负责重试。
    pub fn connect(timeout_ms: u32) -> Result<Self, PipeError> {
        let name = wide(crate::PIPE_NAME);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);

        loop {
            // SAFETY: 名字有效；只读打开，不共享
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    windows_sys::Win32::Foundation::GENERIC_READ,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                return Ok(Self { handle });
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
            unsafe { WaitNamedPipeW(name.as_ptr(), 500) };
        }
    }

    /// 阻塞读一帧。
    pub fn recv_frame(&mut self) -> Result<Frame, PipeError> {
        let mut len_buf = [0u8; 4];
        read_exact(self.handle, &mut len_buf)?;
        let len = u32::from_le_bytes(len_buf) as usize;
        if len > MAX_FRAME_BYTES {
            return Err(PipeError::FrameTooLarge(len));
        }

        let mut payload = vec![0u8; len];
        read_exact(self.handle, &mut payload)?;
        Frame::decode(&payload).map_err(PipeError::Proto)
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
