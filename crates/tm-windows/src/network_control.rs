//! 独立于统计推流的控制管道。仅当前账户可连接，并校验两端的完整程序路径。
//! 请求只有 List / Set，助手不接受命令行或任意文件写入。
//! 非阻塞字节管道 + 有界消息 + 截止时间，异常客户端不能卡死控制线程。

use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Pipes::*;
use windows_sys::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::firewall::Firewall;
use crate::network_policy::{same_path, Reply, Request, MAX_MESSAGE};

const TIMEOUT: Duration = Duration::from_secs(12);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn pipe_name() -> Result<String, String> {
    Ok(format!(
        r"\\.\pipe\Tramoing-Network-Control-{}-v1",
        crate::scheduled_task::current_user_sid()?
    ))
}

fn error(action: &str) -> String {
    format!("{action}：{}", std::io::Error::last_os_error())
}

fn tick(deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        return Err("网络控制请求超时，请刷新状态后重试".into());
    }
    std::thread::sleep(Duration::from_millis(10));
    Ok(())
}

pub(crate) fn read(handle: HANDLE, buffer: &mut [u8], deadline: Instant) -> Result<(), String> {
    let mut offset = 0;
    while offset < buffer.len() {
        if Instant::now() >= deadline {
            return Err("读取网络控制消息超时".into());
        }
        let mut count = 0;
        let ok = unsafe {
            ReadFile(
                handle,
                buffer[offset..].as_mut_ptr(),
                (buffer.len() - offset).min(16384) as u32,
                &mut count,
                null_mut(),
            )
        };
        if ok == 0 && unsafe { GetLastError() } != ERROR_NO_DATA {
            return Err(error("读取网络控制消息失败"));
        }
        if count == 0 {
            tick(deadline)?;
        }
        offset += count as usize;
    }
    Ok(())
}

fn write(handle: HANDLE, buffer: &[u8], deadline: Instant) -> Result<(), String> {
    let mut offset = 0;
    while offset < buffer.len() {
        if Instant::now() >= deadline {
            return Err("发送网络控制消息超时".into());
        }
        let mut count = 0;
        let ok = unsafe {
            WriteFile(
                handle,
                buffer[offset..].as_ptr(),
                (buffer.len() - offset).min(16384) as u32,
                &mut count,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(error("发送网络控制消息失败"));
        }
        if count == 0 {
            tick(deadline)?;
        }
        offset += count as usize;
    }
    Ok(())
}

pub(crate) fn receive<T: serde::de::DeserializeOwned>(
    handle: HANDLE,
    deadline: Instant,
) -> Result<T, String> {
    let mut header = [0u8; 4];
    read(handle, &mut header, deadline)?;
    let len = u32::from_le_bytes(header) as usize;
    if len == 0 || len > MAX_MESSAGE {
        return Err("网络控制消息长度异常".into());
    }
    let mut body = vec![0; len];
    read(handle, &mut body, deadline)?;
    serde_json::from_slice(&body).map_err(|e| format!("网络控制消息格式错误：{e}"))
}

pub(crate) fn send<T: serde::Serialize>(
    handle: HANDLE,
    value: &T,
    deadline: Instant,
) -> Result<(), String> {
    let body = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if body.len() > MAX_MESSAGE {
        return Err("网络控制规则过多".into());
    }
    write(handle, &(body.len() as u32).to_le_bytes(), deadline)?;
    write(handle, &body, deadline)
}

pub(crate) fn verify_peer(handle: HANDLE, server: bool) -> Result<(), String> {
    let mut pid = 0;
    let ok = unsafe {
        if server {
            GetNamedPipeServerProcessId(handle, &mut pid)
        } else {
            GetNamedPipeClientProcessId(handle, &mut pid)
        }
    };
    if ok == 0 {
        return Err(error("无法核验网络控制进程"));
    }
    let name = if server {
        "tramoing-helper.exe"
    } else {
        "tramoing.exe"
    };
    let expected = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .with_file_name(name);
    let actual = crate::proc::executable_path(pid).ok_or("无法读取网络控制进程身份")?;
    if !same_path(&actual, &expected.to_string_lossy()) {
        return Err("网络控制进程身份不匹配，请重新安装完整版本".into());
    }
    if server {
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if raw.is_null() {
            return Err(error("无法核验助手权限"));
        }
        let process = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut token = null_mut();
        if unsafe { OpenProcessToken(process.as_raw_handle(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(error("无法读取助手权限"));
        }
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut size = 0;
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                std::mem::size_of_val(&elevation) as u32,
                &mut size,
            )
        } == 0
            || elevation.TokenIsElevated == 0
        {
            return Err("网络控制助手没有管理员权限，请重新安装采集助手".into());
        }
    }
    Ok(())
}

/// 从后台工作线程调用；UI 线程不等待管道。
pub fn request(request: &Request) -> Result<Reply, String> {
    request_named(&pipe_name()?, request, TIMEOUT)
}

pub(crate) fn request_named<R: serde::Serialize, T: serde::de::DeserializeOwned>(
    name: &str,
    request: &R,
    timeout: Duration,
) -> Result<T, String> {
    let name = wide(name);
    let deadline = Instant::now() + timeout;
    let raw = loop {
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            break handle;
        }
        if unsafe { GetLastError() } != ERROR_PIPE_BUSY {
            return Err("未连接到网络控制助手，请安装当前版本的 Windows 采集助手".into());
        }
        tick(deadline)?;
    };
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    verify_peer(handle.as_raw_handle(), true)?;
    if unsafe { SetNamedPipeHandleState(raw, &PIPE_NOWAIT, null(), null()) } == 0 {
        return Err(error("配置网络控制连接失败"));
    }
    send(raw, request, deadline)?;
    let reply = receive(raw, deadline)?;
    let _ = write(raw, &[1], deadline);
    Ok(reply)
}

/// Shared by statistics and control: only SYSTEM and the current account,
/// medium-integrity clients, no remote clients, and no pre-existing server.
pub(crate) fn create_restricted_pipe(name: &str, access: u32) -> Result<OwnedHandle, String> {
    let sid = crate::scheduled_task::current_user_sid()?;
    // 不使用 Everyone/Authenticated Users，也不允许远程管道访问。
    // Medium 标签允许普通权限界面写入由提权助手创建的管道。
    let sddl = wide(&format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})S:(ML;;NW;;;ME)"));
    let mut descriptor: *mut c_void = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(error("设置网络控制访问权限失败"));
    }
    let security = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let name = wide(name);
    let raw = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            access | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            65536,
            65536,
            0,
            &security,
        )
    };
    let creation_error = (raw == INVALID_HANDLE_VALUE).then(|| error("创建网络控制通道失败"));
    unsafe { LocalFree(descriptor) };
    if let Some(error) = creation_error {
        return Err(error);
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

pub struct Server(OwnedHandle);

impl Server {
    pub fn create() -> Result<Self, String> {
        Self::create_named(&pipe_name()?)
    }

    pub(crate) fn create_named(name: &str) -> Result<Self, String> {
        create_restricted_pipe(name, PIPE_ACCESS_DUPLEX).map(Self)
    }

    pub(crate) fn raw_handle(&self) -> HANDLE {
        self.0.as_raw_handle()
    }

    pub fn run(self, log: impl Fn(&str)) -> ! {
        let handle = self.0.as_raw_handle();
        loop {
            let ok = unsafe { ConnectNamedPipe(handle, null_mut()) };
            let code = unsafe { GetLastError() };
            if ok != 0 || code != ERROR_PIPE_CONNECTED {
                if ok == 0 && code == ERROR_NO_DATA {
                    unsafe { DisconnectNamedPipe(handle) };
                }
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            if let Err(e) = self.serve_one() {
                log(&e);
            }
            unsafe { DisconnectNamedPipe(handle) };
        }
    }

    fn serve_one(&self) -> Result<(), String> {
        let handle = self.0.as_raw_handle();
        let deadline = Instant::now() + TIMEOUT;
        verify_peer(handle, false)?;
        let request: Request = receive(handle, deadline)?;
        let result = Firewall::open().and_then(|firewall| match request {
            Request::List {} => firewall.list(),
            Request::Set { path, blocked } => firewall.set(&path, blocked),
        });
        let reply = match result {
            Ok(rules) => Reply { rules, error: None },
            Err(error) => Reply {
                rules: Vec::new(),
                error: Some(error),
            },
        };
        send(handle, &reply, deadline)?;
        // 等待客户端完整读完，防止 DisconnectNamedPipe 丢弃尚未读取的应答。
        read(handle, &mut [0u8; 1], deadline)
    }
}

/// 选择代理流量列表中尚未出现的客户端 exe。
pub fn is_program(path: &Path) -> bool {
    crate::network_policy::validate_path(&path.to_string_lossy()).is_ok() && path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_transport_is_bounded_and_rejects_an_untrusted_process() {
        let name = format!(r"\\.\pipe\Tramoing-control-test-{}", std::process::id());
        let server = Server::create_named(&name).unwrap();
        let name = wide(&name);
        let raw = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                null_mut(),
            )
        };
        assert_ne!(raw, INVALID_HANDLE_VALUE);
        let client = unsafe { OwnedHandle::from_raw_handle(raw) };
        unsafe { ConnectNamedPipe(server.0.as_raw_handle(), null_mut()) };
        assert_ne!(
            unsafe { SetNamedPipeHandleState(raw, &PIPE_NOWAIT, null(), null()) },
            0
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        send(
            raw,
            &Request::Set {
                path: r"C:\测试\app.exe".into(),
                blocked: true,
            },
            deadline,
        )
        .unwrap();
        let request: Request = receive(server.0.as_raw_handle(), deadline).unwrap();
        assert!(matches!(request, Request::Set { blocked: true, .. }));
        assert!(verify_peer(server.0.as_raw_handle(), false)
            .unwrap_err()
            .contains("身份不匹配"));
        write(raw, &((MAX_MESSAGE + 1) as u32).to_le_bytes(), deadline).unwrap();
        assert!(receive::<Request>(server.0.as_raw_handle(), deadline)
            .unwrap_err()
            .contains("长度"));
        assert!(receive::<Reply>(raw, Instant::now() + Duration::from_millis(20)).is_err());
        drop(client);
        unsafe { DisconnectNamedPipe(server.0.as_raw_handle()) };
    }
}
