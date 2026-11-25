//! ETW 实时会话：订阅读 `Microsoft-Windows-Kernel-Network` 的 TCP/UDP 事件。
//!
//! # 为什么非得走 ETW
//!
//! Windows 没有普通权限的「按进程字节数」API。`GetExtendedTcpTable` 只给连接表，
//! 不给字节；`GetPerTcpConnectionEStats` 既要管理员又只能逐连接轮询。
//! 唯一能拿到 PID 级字节流的就是 ETW 的内核网络 provider，代价是必须提权。
//!
//! # 事件布局
//!
//! 内核网络事件的前 8 字节是 `PID:u32, size:u32`，之后是 v4/v6 的地址与端口。
//! 地址解析和回环过滤见 [`crate::network`]，避免把代理在本机的转发算成网络流量。
//!
//! 只订阅计费用的 Send/Receive 事件；连接、断开、重传等事件不计字节。
//! 事件 ID 对照 Microsoft-Windows-Kernel-Network manifest。

use std::ffi::c_void;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_BAD_LENGTH, ERROR_SUCCESS};
use windows_sys::Win32::System::Diagnostics::Etw::{
    CloseTrace, ControlTraceW, EnableTraceEx2, OpenTraceW, ProcessTrace, CONTROLTRACE_HANDLE,
    EVENT_CONTROL_CODE_ENABLE_PROVIDER, EVENT_RECORD, EVENT_TRACE_CONTROL_STOP,
    EVENT_TRACE_LOGFILEW, EVENT_TRACE_PROPERTIES, PROCESSTRACE_HANDLE,
    PROCESS_TRACE_MODE_EVENT_RECORD, PROCESS_TRACE_MODE_REAL_TIME, WNODE_FLAG_TRACED_GUID,
};

use crate::agg::Aggregator;
use crate::network::decode_event;

/// `Microsoft-Windows-Kernel-Network`
const PROVIDER: windows_sys::core::GUID =
    windows_sys::core::GUID::from_u128(0x7dd42a49_5329_4832_8dfd_43d979153a88);

/// `EVENT_TRACE_REAL_TIME_MODE`
const REAL_TIME_MODE: u32 = 0x0000_0100;

/// 诊断开关：把每个事件的原始内容写进日志文件，用于校准事件表。
static DUMP_EVENTS: AtomicBool = AtomicBool::new(false);

/// StartTrace/ControlTrace write session metadata back into this allocation.
/// Reserve both UTF-16 name slots, including their terminators, even for a
/// real-time session with no log file. A header followed by only the exact
/// session name leaves no room for additional metadata returned by Windows.
#[repr(C)]
struct TraceProperties {
    properties: EVENT_TRACE_PROPERTIES,
    logger_name: [u16; 1025],
    log_file_name: [u16; 1025],
}

impl TraceProperties {
    fn new() -> Self {
        // SAFETY: the header and UTF-16 arrays have valid all-zero values.
        let mut value: Self = unsafe { std::mem::zeroed() };
        value.properties.Wnode.BufferSize = size_of::<Self>() as u32;
        value.properties.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        value.properties.LogFileMode = REAL_TIME_MODE;
        value.properties.FlushTimer = 1;
        value.properties.LoggerNameOffset = std::mem::offset_of!(Self, logger_name) as u32;
        // Zero disables file output; the reserved slot is only output capacity.
        value.properties.LogFileNameOffset = 0;
        value
    }

    fn as_mut_ptr(&mut self) -> *mut EVENT_TRACE_PROPERTIES {
        // Derive from the whole allocation: the API also writes the tail slots.
        (self as *mut Self).cast()
    }
}

fn start_trace(
    handle: &mut CONTROLTRACE_HANDLE,
    logger_name: &[u16],
    properties: &mut TraceProperties,
) -> u32 {
    // Both StartTraceW and ControlTraceW are in/out APIs. Never reuse their
    // output (e.g. offsets, flags or sizes) as the input of a subsequent start.
    *properties = TraceProperties::new();
    // SAFETY: the name is NUL-terminated and the aligned properties allocation
    // includes the writable name slots declared by Wnode.BufferSize.
    unsafe {
        windows_sys::Win32::System::Diagnostics::Etw::StartTraceW(
            handle,
            logger_name.as_ptr(),
            properties.as_mut_ptr(),
        )
    }
}

/// 安装时移除改名前遗留的采集会话；旧助手仍存活时不抢占它的会话。
pub(crate) fn stop_legacy_session() -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let _owner = loop {
        match crate::instance::InstanceGuard::acquire("Global\\TrafficMonitor-Etw-Owner") {
            Ok(Some(owner)) => break owner,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Ok(None) => return Err("旧采集助手仍在运行，请退出后重新安装".into()),
            Err(code) => return Err(format!("检查旧采集助手失败（Win32 错误 {code}）")),
        }
    };
    let name: Vec<u16> = "TrafficMonitor-Etw".encode_utf16().chain(Some(0)).collect();
    let mut properties = TraceProperties::new();
    // SAFETY: 属性结构正确对齐，预留完整名称空间；名称由独立 NUL 字符串持有。
    let code = unsafe {
        ControlTraceW(
            CONTROLTRACE_HANDLE { Value: 0 },
            name.as_ptr(),
            properties.as_mut_ptr(),
            EVENT_TRACE_CONTROL_STOP,
        )
    };
    match code {
        // ERROR_WMI_INSTANCE_NOT_FOUND：没有旧会话需要清理。
        ERROR_SUCCESS | 4201 => Ok(()),
        code => Err(format!("清理旧采集会话失败（Win32 错误 {code}）")),
    }
}

/// 诊断日志路径。助手是 `windows_subsystem = "windows"`，没有控制台，
/// 事件只能落到文件里，否则 `--dump-events` 什么都看不到。
fn dump_path() -> &'static PathBuf {
    static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join(tm_core::identity::APP_NAME).join("etw-dump.log")
    })
}

pub fn set_dump_events(on: bool) {
    if on {
        if let Some(dir) = dump_path().parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        eprintln!("[etw] 事件诊断日志：{}", dump_path().display());
    }
    DUMP_EVENTS.store(on, Ordering::Relaxed);
}

/// 写一行事件诊断。ETW 回调跑在实时线程上，这里只做一次 append，
/// 且只在 `--dump-events` 打开时调用，不会影响正常运行的性能。
fn dump_event(event_id: u16, pid: u32, size: u64, len: usize, hex: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dump_path())
    {
        let _ = writeln!(
            f,
            "id={event_id} pid={pid} size={size} len={len} bytes={hex}"
        );
    }
}

#[derive(Debug)]
pub enum EtwError {
    /// 启动会话失败，带 Win32 错误码
    Start(u32),
    /// 启用 provider 失败
    Enable(u32),
    /// OpenTrace 拿到无效句柄
    OpenTrace,
    /// 需要管理员权限
    NeedAdmin,
    /// 已经有别的助手在跑（同名 ETW 会话被占用，且管道是通的）
    AlreadyRunning,
}

impl std::fmt::Display for EtwError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start(ERROR_BAD_LENGTH) => write!(
                f,
                "启动 ETW 会话失败（Win32 错误 24 / ERROR_BAD_LENGTH）：会话属性缓冲区长度不兼容，请更新采集助手并导出诊断日志"
            ),
            Self::Start(c) => write!(f, "启动 ETW 会话失败（Win32 错误 {c}）"),
            Self::Enable(c) => write!(f, "启用内核网络 provider 失败（Win32 错误 {c}）"),
            Self::OpenTrace => write!(f, "OpenTrace 返回了无效句柄"),
            Self::NeedAdmin => write!(
                f,
                "需要管理员权限：Windows 只允许提权进程读取「每进程流量」"
            ),
            Self::AlreadyRunning => write!(f, "已有采集助手在运行（ETW 会话与管道都被占用）"),
        }
    }
}

impl std::error::Error for EtwError {}

/// 一个正在运行的 ETW 会话。Drop 时自动停止并回收线程。
pub struct EtwSession {
    _instance: crate::instance::InstanceGuard,
    handle: CONTROLTRACE_HANDLE,
    /// 会话名必须活到 STOP 调用之后
    logger_name: Vec<u16>,
    consumer: Option<JoinHandle<()>>,
}

impl EtwSession {
    /// 启动会话并开始把字节累加进 `sink`。会阻塞到消费者线程就绪。
    pub fn start(sink: Arc<Mutex<Aggregator>>) -> Result<Self, EtwError> {
        // 必须在探测/清理同名会话之前持有跨进程锁。管道忙只表示 UI 已连接，
        // 不能作为助手死亡的依据。Global 与 ETW 会话的机器级作用域一致。
        let instance = crate::instance::InstanceGuard::acquire("Global\\Tramoing-Etw-Owner")
            .map_err(EtwError::Start)?
            .ok_or(EtwError::AlreadyRunning)?;
        let mut logger_name: Vec<u16> = crate::SESSION_NAME.encode_utf16().collect();
        logger_name.push(0);

        let mut props = TraceProperties::new();
        let mut handle = CONTROLTRACE_HANDLE { Value: 0 };
        let mut rc = start_trace(&mut handle, &logger_name, &mut props);

        // 183 = ERROR_ALREADY_EXISTS：同名会话已经存在。两种可能，处理方式相反：
        //   - 管道能连上 → 真有一个助手在跑，本进程没有存在的意义；
        //   - 管道连不上 → 上一个助手被强杀留下的僵尸会话，按名字停掉再重试一次。
        // 以前这里一律当成「启动失败」，日志还提示「请确认管理员权限」，
        // 于是残留会话会让助手永远起不来，用户只能手动 --uninstall。
        const ERROR_ALREADY_EXISTS: u32 = 183;
        if rc == ERROR_ALREADY_EXISTS {
            // 兼容尚未使用互斥锁的旧助手：仅管道明确不存在时才清理残留会话。
            if !matches!(
                crate::pipe::PipeClient::connect(0),
                Err(crate::pipe::PipeError::Connect(2))
            ) {
                return Err(EtwError::AlreadyRunning);
            }
            let mut stop_props = TraceProperties::new();
            // SAFETY: 按名字停会话时 ControlHandle 传 0，名字以 NUL 结尾
            unsafe {
                ControlTraceW(
                    CONTROLTRACE_HANDLE { Value: 0 },
                    logger_name.as_ptr(),
                    stop_props.as_mut_ptr(),
                    EVENT_TRACE_CONTROL_STOP,
                );
            }
            rc = start_trace(&mut handle, &logger_name, &mut props);
        }

        if rc != ERROR_SUCCESS {
            return Err(if rc == ERROR_ACCESS_DENIED {
                EtwError::NeedAdmin
            } else {
                EtwError::Start(rc)
            });
        }

        // 订阅内核网络 provider。level=4(Informational)，keyword=0 表示全收。
        // SAFETY: handle 来自 StartTraceW，参数均为只读常量。
        let rc = unsafe {
            EnableTraceEx2(
                handle,
                &PROVIDER,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER,
                4,
                0,
                0,
                0,
                std::ptr::null(),
            )
        };
        if rc != ERROR_SUCCESS {
            // 会话已经建起来了，失败也要收掉，否则下次启动会撞名
            let mut stop_props = TraceProperties::new();
            unsafe {
                ControlTraceW(
                    handle,
                    std::ptr::null(),
                    stop_props.as_mut_ptr(),
                    EVENT_TRACE_CONTROL_STOP,
                );
            }
            return Err(EtwError::Enable(rc));
        }

        let (ready_tx, ready_rx) = mpsc::channel();

        let mut session = Self {
            _instance: instance,
            handle,
            logger_name: crate::SESSION_NAME
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect(),
            consumer: None,
        };
        session.consumer = Some(
            std::thread::Builder::new()
                .name("etw-consumer".to_owned())
                .spawn(move || consumer_loop(logger_name, sink, ready_tx))
                .map_err(|_| EtwError::Start(8))?,
        );
        match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(session),
            Ok(Err(e)) => Err(e), // session 的 Drop 停止会话并回收线程
            Err(_) => Err(EtwError::OpenTrace),
        }
    }
}

impl Drop for EtwSession {
    fn drop(&mut self) {
        // 先停会话，ProcessTrace 才会返回，消费线程才可能 join
        let mut properties = TraceProperties::new();
        // SAFETY: properties 与 handle 都还在有效期内
        unsafe {
            ControlTraceW(
                self.handle,
                std::ptr::null(),
                properties.as_mut_ptr(),
                EVENT_TRACE_CONTROL_STOP,
            );
        }
        if let Some(t) = self.consumer.take() {
            let _ = t.join();
        }
        let _ = &self.logger_name;
    }
}

/// 消费线程：打开实时会话并阻塞在 ProcessTrace。
fn consumer_loop(
    logger_name: Vec<u16>,
    sink: Arc<Mutex<Aggregator>>,
    ready: Sender<Result<(), EtwError>>,
) {
    // SAFETY: 全零对 EVENT_TRACE_LOGFILEW 是合法的初始状态
    let mut logfile: EVENT_TRACE_LOGFILEW = unsafe { std::mem::zeroed() };
    logfile.LoggerName = logger_name.as_ptr() as *mut u16;
    // 写入 Copy 类型的 union 字段是安全操作
    logfile.Anonymous1.ProcessTraceMode =
        PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
    logfile.Anonymous2.EventRecordCallback = Some(on_event);
    // Arc 活到 ProcessTrace 返回，所有回调结束后才释放。
    logfile.Context = Arc::as_ptr(&sink) as *mut c_void;

    let handle: PROCESSTRACE_HANDLE = unsafe { OpenTraceW(&mut logfile) };
    if handle.Value == u64::MAX {
        let _ = ready.send(Err(EtwError::OpenTrace));
        return;
    }
    let _ = ready.send(Ok(()));

    // 阻塞，直到会话被 STOP
    unsafe {
        ProcessTrace(&handle, 1, std::ptr::null(), std::ptr::null());
        CloseTrace(handle);
    }
}

/// ETW 回调。跑在系统的实时线程上，做的事越少越好。
unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    if record.is_null() {
        return;
    }
    let rec = &*record;
    let event_id = rec.EventHeader.EventDescriptor.Id;
    let len = rec.UserDataLength as usize;

    if rec.UserData.is_null() || len < 8 {
        return;
    }
    let data = std::slice::from_raw_parts(rec.UserData as *const u8, len);
    let pid = u32::from_ne_bytes([data[0], data[1], data[2], data[3]]);
    let size = u32::from_ne_bytes([data[4], data[5], data[6], data[7]]) as u64;

    if DUMP_EVENTS.load(Ordering::Relaxed) {
        let hex: String = data
            .iter()
            .take(24)
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!("[etw] id={event_id} pid={pid} size={size} len={len} bytes={hex}");
        dump_event(event_id, pid, size, len, &hex);
    }

    let Some(event) = decode_event(event_id, rec.EventHeader.EventDescriptor.Version, data) else {
        return;
    };
    if let Some(sink) = (rec.UserContext as *const Mutex<Aggregator>).as_ref() {
        if let Ok(mut agg) = sink.lock() {
            agg.add(event.pid, event.dir, event.bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Diagnostics::Etw::{
        EventRegister, EventUnregister, StartTraceW, EVENT_TRACE_CONTROL_QUERY,
        EVENT_TRACE_FILE_MODE_SEQUENTIAL, EVENT_TRACE_PRIVATE_IN_PROC,
        EVENT_TRACE_PRIVATE_LOGGER_MODE,
    };

    #[test]
    fn native_properties_support_private_session_query_and_stop() {
        // Private in-process sessions exercise the actual Windows property
        // marshalling without elevation or touching the installed collector.
        let provider = windows_sys::core::GUID::from_u128(0x9a2daa45_f5cf_4308_839c_3110b87b9435);
        let name: Vec<u16> = "Tramoing-Properties-Test"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut registration = 0;
        assert_eq!(
            unsafe { EventRegister(&provider, None, std::ptr::null(), &mut registration) },
            ERROR_SUCCESS
        );
        struct Registration(i64);
        impl Drop for Registration {
            fn drop(&mut self) {
                unsafe { EventUnregister(self.0) };
            }
        }
        let _registration = Registration(registration as i64);

        let mut properties = TraceProperties::new();
        properties.properties.Wnode.Guid = provider;
        properties.properties.LogFileMode = EVENT_TRACE_PRIVATE_LOGGER_MODE
            | EVENT_TRACE_PRIVATE_IN_PROC
            | EVENT_TRACE_FILE_MODE_SEQUENTIAL;
        let directory = tempfile::tempdir().unwrap();
        let file_name: Vec<u16> = directory
            .path()
            .join("private.etl")
            .to_str()
            .unwrap()
            .encode_utf16()
            .chain(Some(0))
            .collect();
        properties.log_file_name[..file_name.len()].copy_from_slice(&file_name);
        properties.properties.LogFileNameOffset =
            std::mem::offset_of!(TraceProperties, log_file_name) as u32;
        let mut handle = CONTROLTRACE_HANDLE { Value: 0 };
        let rc = unsafe { StartTraceW(&mut handle, name.as_ptr(), properties.as_mut_ptr()) };
        assert_eq!(rc, ERROR_SUCCESS, "private StartTraceW: {rc}");

        struct Session(CONTROLTRACE_HANDLE);
        impl Drop for Session {
            fn drop(&mut self) {
                let mut properties = TraceProperties::new();
                unsafe {
                    ControlTraceW(
                        self.0,
                        std::ptr::null(),
                        properties.as_mut_ptr(),
                        EVENT_TRACE_CONTROL_STOP,
                    );
                }
            }
        }
        let session = Session(handle);
        let mut queried = TraceProperties::new();
        let rc = unsafe {
            ControlTraceW(
                session.0,
                std::ptr::null(),
                queried.as_mut_ptr(),
                EVENT_TRACE_CONTROL_QUERY,
            )
        };
        assert_eq!(rc, ERROR_SUCCESS, "private ControlTraceW query: {rc}");
        assert_eq!(&queried.logger_name[..name.len()], name.as_slice());
        // Stop must also have enough space for the system's output properties.
        let rc = unsafe {
            ControlTraceW(
                session.0,
                std::ptr::null(),
                queried.as_mut_ptr(),
                EVENT_TRACE_CONTROL_STOP,
            )
        };
        assert_eq!(rc, ERROR_SUCCESS, "private ControlTraceW stop: {rc}");
        std::mem::forget(session);
    }

    #[test]
    fn bad_length_identifies_a_buffer_failure_instead_of_missing_privileges() {
        let message = EtwError::Start(ERROR_BAD_LENGTH).to_string();
        assert!(message.contains("24 / ERROR_BAD_LENGTH"));
        assert!(message.contains("缓冲区"));
        assert!(!message.contains("管理员"));
        assert!(EtwError::NeedAdmin.to_string().contains("管理员"));
    }

    #[test]
    #[ignore = "requires administrator; starts only the isolated Tramoing-Etw-Verification session"]
    fn native_realtime_session_restarts_after_control_overwrites_properties() {
        let name: Vec<u16> = "Tramoing-Etw-Verification"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut properties = TraceProperties::new();
        for _ in 0..2 {
            let mut handle = CONTROLTRACE_HANDLE { Value: 0 };
            let rc = start_trace(&mut handle, &name, &mut properties);
            assert_eq!(rc, ERROR_SUCCESS, "real-time StartTraceW: {rc}");
            let enabled = unsafe {
                EnableTraceEx2(
                    handle,
                    &PROVIDER,
                    EVENT_CONTROL_CODE_ENABLE_PROVIDER,
                    4,
                    0,
                    0,
                    0,
                    std::ptr::null(),
                )
            };
            // Always stop our session, including on provider-enabling failure.
            let stopped = unsafe {
                ControlTraceW(
                    handle,
                    std::ptr::null(),
                    properties.as_mut_ptr(),
                    EVENT_TRACE_CONTROL_STOP,
                )
            };
            assert_eq!(enabled, ERROR_SUCCESS, "EnableTraceEx2: {enabled}");
            assert_eq!(stopped, ERROR_SUCCESS, "ControlTraceW stop: {stopped}");
        }
    }
}
