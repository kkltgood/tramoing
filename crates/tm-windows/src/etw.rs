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
//! 内核网络事件的前 8 字节是 `PID:u32, size:u32`，后面的字段随 v4/v6 不同，
//! 我们不关心地址和端口，所以只读前 8 字节。
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

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Diagnostics::Etw::{
    CloseTrace, ControlTraceW, EnableTraceEx2, OpenTraceW, ProcessTrace, CONTROLTRACE_HANDLE,
    EVENT_CONTROL_CODE_ENABLE_PROVIDER, EVENT_RECORD, EVENT_TRACE_CONTROL_STOP,
    EVENT_TRACE_LOGFILEW, EVENT_TRACE_PROPERTIES, PROCESSTRACE_HANDLE,
    PROCESS_TRACE_MODE_EVENT_RECORD, PROCESS_TRACE_MODE_REAL_TIME, WNODE_FLAG_TRACED_GUID,
};

use crate::agg::{Aggregator, Dir};

/// `Microsoft-Windows-Kernel-Network`
const PROVIDER: windows_sys::core::GUID =
    windows_sys::core::GUID::from_u128(0x7dd42a49_5329_4832_8dfd_43d979153a88);

/// `EVENT_TRACE_REAL_TIME_MODE`
const REAL_TIME_MODE: u32 = 0x0000_0100;

/// 诊断开关：把每个事件的原始内容写进日志文件，用于校准事件表。
static DUMP_EVENTS: AtomicBool = AtomicBool::new(false);

/// 诊断日志路径。助手是 `windows_subsystem = "windows"`，没有控制台，
/// 事件只能落到文件里，否则 `--dump-events` 什么都看不到。
fn dump_path() -> &'static PathBuf {
    static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("TrafficMonitor").join("etw-dump.log")
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
    /// 属性缓冲区，STOP 时还要用一次。
    /// 用 `Vec<u64>` 而不是 `Vec<u8>` 作底层存储：`EVENT_TRACE_PROPERTIES` 含 u64 字段，
    /// 必须 8 字节对齐，`Vec<u8>` 只保证 1 字节对齐。
    props: Vec<u64>,
    consumer: Option<JoinHandle<()>>,
}

impl EtwSession {
    /// 启动会话并开始把字节累加进 `sink`。会阻塞到消费者线程就绪。
    pub fn start(sink: Arc<Mutex<Aggregator>>) -> Result<Self, EtwError> {
        // 必须在探测/清理同名会话之前持有跨进程锁。管道忙只表示 UI 已连接，
        // 不能作为助手死亡的依据。Global 与 ETW 会话的机器级作用域一致。
        let instance = crate::instance::InstanceGuard::acquire("Global\\TrafficMonitor-Etw-Owner")
            .map_err(EtwError::Start)?
            .ok_or(EtwError::AlreadyRunning)?;
        let mut logger_name: Vec<u16> = crate::SESSION_NAME.encode_utf16().collect();
        logger_name.push(0);

        // EVENT_TRACE_PROPERTIES 后面紧跟会话名，用一块连续内存装下。
        // 底层用 u64 数组保证 8 字节对齐（结构体里有 u64 字段）。
        let props_size =
            std::mem::size_of::<EVENT_TRACE_PROPERTIES>() + logger_name.len() * size_of::<u16>();
        let mut props: Vec<u64> = vec![0; props_size.div_ceil(8)];
        let base = props.as_mut_ptr() as *mut u8;
        let name_offset = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();

        // SAFETY: props 的字节数 >= props_size，且按 8 字节对齐；
        // name_offset 之后的 name.len()*2 字节都在缓冲区内。
        unsafe {
            let p = base as *mut EVENT_TRACE_PROPERTIES;
            (*p).Wnode.BufferSize = props_size as u32;
            (*p).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
            (*p).LogFileMode = REAL_TIME_MODE;
            (*p).FlushTimer = 1;
            (*p).LoggerNameOffset = name_offset as u32;
            std::ptr::copy_nonoverlapping(
                logger_name.as_ptr() as *const u8,
                base.add(name_offset),
                logger_name.len() * size_of::<u16>(),
            );
        }

        let mut handle = CONTROLTRACE_HANDLE { Value: 0 };
        // SAFETY: 指针指向我们持有到会话结束的缓冲区
        let mut rc = unsafe {
            windows_sys::Win32::System::Diagnostics::Etw::StartTraceW(
                &mut handle,
                logger_name.as_ptr(),
                base as *mut EVENT_TRACE_PROPERTIES,
            )
        };

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
            // SAFETY: 按名字停会话时 ControlHandle 传 0，名字以 NUL 结尾
            unsafe {
                ControlTraceW(
                    CONTROLTRACE_HANDLE { Value: 0 },
                    logger_name.as_ptr(),
                    props.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES,
                    EVENT_TRACE_CONTROL_STOP,
                );
            }
            // SAFETY: 同上面的 StartTraceW
            rc = unsafe {
                windows_sys::Win32::System::Diagnostics::Etw::StartTraceW(
                    &mut handle,
                    logger_name.as_ptr(),
                    base as *mut EVENT_TRACE_PROPERTIES,
                )
            };
        }

        if rc != ERROR_SUCCESS {
            return Err(if rc == 5 {
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
            unsafe {
                ControlTraceW(
                    handle,
                    std::ptr::null(),
                    props.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES,
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
            props,
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
        // SAFETY: props 与 handle 都还在有效期内
        unsafe {
            ControlTraceW(
                self.handle,
                std::ptr::null(),
                self.props.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES,
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

/// manifest: TCPv4 10/11、TCPv6 26/27、UDPv4 42/43、UDPv6 58/59。
fn direction(event_id: u16) -> Option<Dir> {
    match event_id {
        10 | 26 | 42 | 58 => Some(Dir::Tx),
        11 | 27 | 43 | 59 => Some(Dir::Rx),
        _ => None,
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

    let Some(dir) = direction(event_id) else {
        return;
    };
    if let Some(sink) = (rec.UserContext as *const Mutex<Aggregator>).as_ref() {
        if let Ok(mut agg) = sink.lock() {
            agg.add(pid, dir, size);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_tcp_and_udp_for_both_address_families() {
        for id in [10u16, 26, 42, 58] {
            assert_eq!(direction(id), Some(Dir::Tx));
            assert_eq!(direction(id + 1), Some(Dir::Rx));
        }
        // Connect/Disconnect/Retransmit/Accept/Reconnect/Fail 不是新增流量。
        for id in [12, 13, 14, 15, 16, 17, 28, 29, 30, 31, 32, 33] {
            assert_eq!(direction(id), None);
        }
        assert_eq!(direction(1), None, "非流量事件应被忽略");
        assert_eq!(direction(999), None);
    }
}
