//! 助手进程 ↔ 界面进程之间的帧协议。
//!
//! 为什么自己写编码不用 serde：一帧就三个数字加一个进程名，
//! 手写小端编码 20 行就够，还能省掉 serde/bincode 两坨依赖和它们的编译时间。
//!
//! 线上格式（全部小端）：
//!
//! ```text
//! 帧头   magic:u32(4)  version:u16(2)  count:u16(2)  seq:u64(8)      = 16 字节
//! v1 每项 pid:u32(4) rx:u64(8) tx:u64(8) name_len:u16(2) = 22 + name_len
//! v2 每项 pid:u32(4) rx:u64(8) tx:u64(8) name_len:u32(4) = 24 + name_len
//!        name:[u8; name_len]（UTF-8，完整可执行文件路径，不截断）
//! 可选帧尾 `TMT1:[u8;4] sampled_at_us:u64`：助手单调时钟上的采集时间。
//! ```
//! 短路径保持 v1 编码；出现超过 512 字节的路径时使用 v2。新版读取 v1/v2，
//! 旧界面不支持 v2，必须与助手一起升级。全帧限制 4 MiB，聚合器提前控制容量。
//!
//! 这个模块不碰任何 Windows API，所以能在 macOS 上跑单测 —— 协议错了
//! 是最难排查的一类问题，必须在这里就锁死。

/// 魔数 `TMN1`
pub const MAGIC: u32 = 0x544D_4E31;
pub const VERSION: u16 = 2;
const LEGACY_VERSION: u16 = 1;
const LEGACY_MAX_NAME: usize = 512;
/// 一帧最多多少个进程。Windows 上常年有 200~400 个进程，
/// 4096 留足余量；超过说明数据错乱，直接拒收而不是无限分配内存。
pub const MAX_PROCS: u16 = 4096;
/// Windows 路径最多 32768 个 UTF-16 单元，每单元最多对应 3 个 UTF-8 字节。
pub const MAX_NAME: usize = 32768 * 3;
/// 编码与传输共享上限，拒绝异常帧，不能因长路径而无限分配。
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

const HEADER_LEN: usize = 16;
const ENTRY_FIXED_V1: usize = 22;
pub(crate) const ENTRY_FIXED: usize = 24;
pub(crate) const FRAME_OVERHEAD: usize = HEADER_LEN + 12;
const TIME_TAG: &[u8; 4] = b"TMT1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcEntry {
    pub pid: u32,
    /// 累计接收字节
    pub rx: u64,
    /// 累计发送字节
    pub tx: u64,
    /// 进程可执行文件路径（服务端已解析好，界面端不用再查一次）
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// 帧序号，界面端可用来发现丢帧
    pub seq: u64,
    pub procs: Vec<ProcEntry>,
    /// 同一次助手运行中的单调采集时间（微秒），不受系统校时和传输延迟影响。
    pub sampled_at_us: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    BadMagic(u32),
    BadVersion(u16),
    TooManyProcs(usize),
    Truncated { need: usize, got: usize },
    NameTooLong(usize),
    FrameTooLarge(usize),
    NotUtf8,
}

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMagic(m) => write!(f, "帧魔数不对：{m:#010x}"),
            Self::BadVersion(v) => write!(f, "帧版本不支持：{v}"),
            Self::TooManyProcs(n) => write!(f, "帧内进程数异常：{n}"),
            Self::Truncated { need, got } => write!(f, "数据截断：需要 {need} 字节，只有 {got}"),
            Self::NameTooLong(n) => write!(f, "进程名过长：{n} 字节"),
            Self::FrameTooLarge(n) => write!(f, "帧长度异常：{n} 字节"),
            Self::NotUtf8 => write!(f, "进程名不是合法 UTF-8"),
        }
    }
}

impl std::error::Error for ProtoError {}

impl Frame {
    pub fn new(seq: u64, procs: Vec<ProcEntry>) -> Self {
        Self {
            seq,
            procs,
            sampled_at_us: None,
        }
    }

    /// 编码到已有缓冲区（复用分配，避免每秒新建 Vec）。
    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        out.clear();
        if self.procs.len() > MAX_PROCS as usize {
            return Err(ProtoError::TooManyProcs(self.procs.len()));
        }
        let version = if self.procs.iter().any(|p| p.name.len() > LEGACY_MAX_NAME) {
            VERSION
        } else {
            LEGACY_VERSION
        };
        let fixed = if version == LEGACY_VERSION {
            ENTRY_FIXED_V1
        } else {
            ENTRY_FIXED
        };
        let mut encoded_len = HEADER_LEN + self.sampled_at_us.map_or(0, |_| 12);
        for entry in &self.procs {
            if entry.name.len() > MAX_NAME {
                return Err(ProtoError::NameTooLong(entry.name.len()));
            }
            encoded_len += fixed + entry.name.len();
        }
        if encoded_len > MAX_FRAME_BYTES {
            return Err(ProtoError::FrameTooLarge(encoded_len));
        }
        out.reserve(encoded_len);
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&version.to_le_bytes());
        out.extend_from_slice(&(self.procs.len() as u16).to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());

        for p in &self.procs {
            let name = p.name.as_bytes();
            out.extend_from_slice(&p.pid.to_le_bytes());
            out.extend_from_slice(&p.rx.to_le_bytes());
            out.extend_from_slice(&p.tx.to_le_bytes());
            if version == LEGACY_VERSION {
                out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            } else {
                out.extend_from_slice(&(name.len() as u32).to_le_bytes());
            }
            out.extend_from_slice(name);
        }
        if let Some(at) = self.sampled_at_us {
            out.extend_from_slice(TIME_TAG);
            out.extend_from_slice(&at.to_le_bytes());
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut v = Vec::new();
        self.encode_into(&mut v)?;
        Ok(v)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() > MAX_FRAME_BYTES {
            return Err(ProtoError::FrameTooLarge(buf.len()));
        }
        let mut cur = Cursor::new(buf);

        let magic = cur.u32()?;
        if magic != MAGIC {
            return Err(ProtoError::BadMagic(magic));
        }
        let version = cur.u16()?;
        if !matches!(version, LEGACY_VERSION | VERSION) {
            return Err(ProtoError::BadVersion(version));
        }
        let count = cur.u16()?;
        if count > MAX_PROCS {
            return Err(ProtoError::TooManyProcs(count as usize));
        }
        let seq = cur.u64()?;

        let mut procs = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let pid = cur.u32()?;
            let rx = cur.u64()?;
            let tx = cur.u64()?;
            let (name_len, maximum) = if version == LEGACY_VERSION {
                (cur.u16()? as usize, LEGACY_MAX_NAME)
            } else {
                (cur.u32()? as usize, MAX_NAME)
            };
            if name_len > maximum {
                return Err(ProtoError::NameTooLong(name_len));
            }
            let raw = cur.bytes(name_len)?;
            let name = std::str::from_utf8(raw)
                .map_err(|_| ProtoError::NotUtf8)?
                .to_owned();
            procs.push(ProcEntry { pid, rx, tx, name });
        }

        let sampled_at_us = if buf[cur.pos..].starts_with(TIME_TAG) {
            cur.bytes(TIME_TAG.len())?;
            Some(cur.u64()?)
        } else {
            None
        };
        Ok(Self {
            seq,
            procs,
            sampled_at_us,
        })
    }
}

/// 极简读游标，越界就返回 Truncated，绝不越界读。
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], ProtoError> {
        let end = self.pos + n;
        if end > self.buf.len() {
            return Err(ProtoError::Truncated {
                need: end,
                got: self.buf.len(),
            });
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u16(&mut self) -> Result<u16, ProtoError> {
        Ok(u16::from_le_bytes(self.bytes(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, ProtoError> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, ProtoError> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_rejects_oversize_without_a_wrapped_count() {
        let frame = Frame::new(
            1,
            vec![
                ProcEntry {
                    pid: 1,
                    rx: 1,
                    tx: 1,
                    name: "x".into()
                };
                MAX_PROCS as usize + 1
            ],
        );
        assert_eq!(
            frame.encode(),
            Err(ProtoError::TooManyProcs(MAX_PROCS as usize + 1))
        );
    }

    fn sample() -> Frame {
        Frame::new(
            42,
            vec![
                ProcEntry {
                    pid: 1234,
                    rx: 1_000_000,
                    tx: 500,
                    name: r"C:\Program Files\Google\Chrome\Application\chrome.exe".to_owned(),
                },
                ProcEntry {
                    pid: 5678,
                    rx: 0,
                    tx: 9_999_999_999,
                    name: "C:\\Windows\\System32\\svchost.exe".to_owned(),
                },
            ],
        )
    }

    #[test]
    fn roundtrip() {
        let f = sample();
        let bytes = f.encode().unwrap();
        assert_eq!(Frame::decode(&bytes).unwrap(), f);
    }

    #[test]
    fn timestamp_extension_preserves_the_legacy_frame_prefix() {
        let legacy = sample();
        let original = legacy.encode().unwrap();
        let mut timed = legacy.clone();
        timed.sampled_at_us = Some(12_345_678);
        let encoded = timed.encode().unwrap();
        assert_eq!(&encoded[..original.len()], &original);
        assert_eq!(Frame::decode(&original).unwrap(), legacy);
        assert_eq!(Frame::decode(&encoded).unwrap(), timed);
        assert!(matches!(
            Frame::decode(&encoded[..encoded.len() - 1]),
            Err(ProtoError::Truncated { .. })
        ));
    }

    #[test]
    fn roundtrip_with_unicode_name() {
        let f = Frame::new(
            1,
            vec![ProcEntry {
                pid: 7,
                rx: 1,
                tx: 2,
                name: r"C:\用户\微信\WeChat.exe".to_owned(),
            }],
        );
        let bytes = f.encode().unwrap();
        assert_eq!(Frame::decode(&bytes).unwrap(), f);
    }

    #[test]
    fn empty_frame_roundtrips() {
        let f = Frame::new(0, Vec::new());
        let bytes = f.encode().unwrap();
        assert_eq!(bytes.len(), HEADER_LEN);
        assert_eq!(Frame::decode(&bytes).unwrap(), f);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = sample().encode().unwrap();
        bytes[0] = 0xFF;
        assert!(matches!(
            Frame::decode(&bytes),
            Err(ProtoError::BadMagic(_))
        ));
    }

    #[test]
    fn rejects_truncated_body() {
        let bytes = sample().encode().unwrap();
        let cut = &bytes[..bytes.len() - 5];
        assert!(matches!(
            Frame::decode(cut),
            Err(ProtoError::Truncated { .. })
        ));
    }

    #[test]
    fn rejects_header_only_garbage() {
        assert!(Frame::decode(&[]).is_err());
        assert!(Frame::decode(&[0u8; 8]).is_err());
    }

    #[test]
    fn reports_remaining_length_after_decode() {
        // 一帧后面跟着下一帧的开头，解码必须只吃自己那一段
        let a = sample();
        let mut stream = a.encode().unwrap();
        let first_len = stream.len();
        stream.extend_from_slice(&sample().encode().unwrap());
        let decoded = Frame::decode(&stream[..first_len]).unwrap();
        assert_eq!(decoded.procs.len(), 2);
    }

    #[test]
    fn distinct_long_unicode_paths_keep_complete_identities() {
        let prefix = format!("C:\\{}", "测".repeat(170));
        let names = [format!("{prefix}a.exe"), format!("{prefix}b.exe")];
        assert!(names[0].encode_utf16().count() < 260);
        let frame = Frame::new(
            1,
            names
                .into_iter()
                .enumerate()
                .map(|(pid, name)| ProcEntry {
                    pid: pid as u32,
                    rx: 1,
                    tx: 2,
                    name,
                })
                .collect(),
        );
        let bytes = frame.encode().unwrap();
        assert_eq!(u16::from_le_bytes(bytes[4..6].try_into().unwrap()), VERSION);
        assert_eq!(Frame::decode(&bytes).unwrap(), frame);
        assert_ne!(frame.procs[0].name, frame.procs[1].name);
    }

    #[test]
    fn full_windows_unicode_path_exceeds_u16_byte_length() {
        let mut frame = sample();
        frame.procs[0].name = "微".repeat(32760);
        let bytes = frame.encode().unwrap();
        assert_eq!(Frame::decode(&bytes).unwrap(), frame);
    }

    #[test]
    fn unsupported_names_and_oversized_frames_are_rejected_without_truncation() {
        let mut frame = sample();
        frame.procs[0].name = "a".repeat(MAX_NAME + 1);
        assert_eq!(frame.encode(), Err(ProtoError::NameTooLong(MAX_NAME + 1)));
        frame.procs = vec![
            ProcEntry {
                pid: 1,
                rx: 0,
                tx: 0,
                name: "a".repeat(MAX_NAME)
            };
            43
        ];
        assert!(matches!(frame.encode(), Err(ProtoError::FrameTooLarge(_))));
        assert_eq!(
            Frame::decode(&vec![0; MAX_FRAME_BYTES + 1]),
            Err(ProtoError::FrameTooLarge(MAX_FRAME_BYTES + 1))
        );
    }

    #[test]
    fn short_paths_keep_the_legacy_wire_format() {
        let frame = sample();
        let bytes = frame.encode().unwrap();
        assert_eq!(
            u16::from_le_bytes(bytes[4..6].try_into().unwrap()),
            LEGACY_VERSION
        );
        assert_eq!(
            bytes.len(),
            HEADER_LEN
                + frame
                    .procs
                    .iter()
                    .map(|p| ENTRY_FIXED_V1 + p.name.len())
                    .sum::<usize>()
        );
        assert_eq!(Frame::decode(&bytes).unwrap(), frame);
    }

    #[test]
    fn malformed_v2_path_length_is_bounded() {
        let mut bytes = Frame::new(
            1,
            vec![ProcEntry {
                pid: 1,
                rx: 0,
                tx: 0,
                name: "微".repeat(200),
            }],
        )
        .encode()
        .unwrap();
        bytes[HEADER_LEN + 20..HEADER_LEN + 24].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            Frame::decode(&bytes),
            Err(ProtoError::NameTooLong(u32::MAX as usize))
        );
    }
    #[test]
    fn exact_frame_limit_roundtrips_but_one_extra_byte_is_rejected() {
        let count = 43;
        let mut remaining = MAX_FRAME_BYTES - FRAME_OVERHEAD - count * ENTRY_FIXED;
        let mut entries = Vec::new();
        for pid in 0..count {
            let len = remaining.min(MAX_NAME);
            remaining -= len;
            entries.push(ProcEntry {
                pid: pid as u32,
                rx: 1,
                tx: 0,
                name: "x".repeat(len),
            });
        }
        assert_eq!(remaining, 0);
        let mut frame = Frame::new(1, entries);
        frame.sampled_at_us = Some(123);
        let encoded = frame.encode().unwrap();
        assert_eq!(encoded.len(), MAX_FRAME_BYTES);
        assert_eq!(Frame::decode(&encoded).unwrap(), frame);
        frame.procs.last_mut().unwrap().name.push('x');
        assert_eq!(
            frame.encode(),
            Err(ProtoError::FrameTooLarge(MAX_FRAME_BYTES + 1))
        );
    }
}
