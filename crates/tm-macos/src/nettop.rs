//! `nettop` 输出解析。
//!
//! # 实测格式（macOS 27.0.1，`nettop -P -x -l 0 -J bytes_in,bytes_out`）
//!
//! ```text
//! launchd.1                                                                                    147705            3300
//! mDNSResponder.506                                                                          78765772        28543065
//! Google Chrome H..81838                                                                     37566388         4545245
//! Codex (Aperitif.79108                                                                         17119           14881
//! ```
//!
//! 三个必须绕开的坑：
//!
//! 1. **不是 CSV**。`-x` 的意思是「输出原始数值（不缩写成 1.2M）」，输出仍是按列对齐的纯文本，
//!    所以只能用空白切分，不能按逗号切。
//! 2. **没有帧分隔符**。不会为每次采样打印 `time,...` 表头，因此不能用表头判断帧边界。
//!    这里改用「同一个 pid 在本块内重复出现」来判定新一帧 —— 每次采样进程列表只出现一遍，
//!    第二次见到同一 pid 必然意味着进入了下一轮采样。
//! 3. **进程名被截断到约 20 字符**，且可能含空格（`Codex (Aperitif`）。
//!    所以名字只能当展示用的脏数据，唯一标识必须靠 pid；M1 会用 `libproc` 按 pid 重新取全名和 bundle id。
//!
//! 另外：数值列过宽时会出现两列粘连成 `0252` 这种脏行，以及整行少一列的情况。
//! 这类行一律丢弃并计数（[`NettopParser::unparsed`]），绝不去猜 —— 猜错会污染速率。

use std::collections::HashSet;

/// 一帧里的一行：某个进程的累计字节。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcRow {
    /// nettop 给的（可能被截断的）进程名
    pub name: String,
    pub pid: u32,
    pub rx: u64,
    pub tx: u64,
}

#[derive(Debug, Default)]
pub struct NettopParser {
    block: Vec<ProcRow>,
    seen: HashSet<u32>,
    /// 吃进来的非空行数
    pub lines_seen: usize,
    /// 解析失败被丢掉的行数
    pub unparsed: usize,
}

impl NettopParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一行输出。返回 `Some(rows)` 表示一帧结束（检测到 pid 重复）。
    pub fn feed(&mut self, line: &str) -> Option<Vec<ProcRow>> {
        let t = line.trim();
        if t.is_empty() {
            return None;
        }
        self.lines_seen += 1;

        if is_header(t) {
            return None;
        }

        let Some(row) = parse_record(t) else {
            self.unparsed += 1;
            return None;
        };

        if self.seen.contains(&row.pid) {
            // 同一个 pid 又在同一块里出现 → 上一轮采样结束
            let finished = std::mem::take(&mut self.block);
            self.seen.clear();
            self.seen.insert(row.pid);
            self.block.push(row);
            return if finished.is_empty() {
                None
            } else {
                Some(finished)
            };
        }

        self.seen.insert(row.pid);
        self.block.push(row);
        None
    }

    /// 取走还没凑满一帧的残余数据。采样结束时调用，避免丢掉最后一批。
    pub fn flush(&mut self) -> Vec<ProcRow> {
        self.seen.clear();
        std::mem::take(&mut self.block)
    }
}

/// 表头行（部分模式会打印，跳过但不算解析失败）。
fn is_header(line: &str) -> bool {
    let first = line.split_whitespace().next().unwrap_or("");
    let lower = first.to_ascii_lowercase();
    lower == "time" || lower == "interface" || lower == "proc" || line.contains("bytes_in")
}

/// 解析一条记录：`进程名.进程ID  rx  tx`。
///
/// 从右往左取两个数字，剩下全部当进程名 —— 因为进程名里可能带空格（`Codex (Aperitif`）。
fn parse_record(line: &str) -> Option<ProcRow> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    if tokens.len() >= 3 {
        let n = tokens.len();
        let tx = tokens[n - 1].parse::<u64>().ok()?;
        let rx = tokens[n - 2].parse::<u64>().ok()?;
        let proc_token = tokens[..n - 2].join(" ");
        let (name, pid) = parse_proc(&proc_token)?;
        return Some(ProcRow { name, pid, rx, tx });
    }

    // 兜底：万一将来版本真的输出 CSV
    let fields: Vec<&str> = line
        .split(',')
        .map(|f| f.trim().trim_matches('"'))
        .filter(|f| !f.is_empty())
        .collect();
    if fields.len() < 3 {
        return None;
    }
    let n = fields.len();
    let tx = fields[n - 1].parse::<u64>().ok()?;
    let rx = fields[n - 2].parse::<u64>().ok()?;
    let (name, pid) = parse_proc(&fields[..n - 2].join(" "))?;
    Some(ProcRow { name, pid, rx, tx })
}

/// 解析 `"Google Chrome H..81838"` → `("Google Chrome H", 81838)`。
/// 进程名本身可能带点（`com.apple.WebKit.WebContent`），所以从右侧切。
fn parse_proc(token: &str) -> Option<(String, u32)> {
    let token = token.trim().trim_matches('"').trim();
    if token.is_empty() {
        return None;
    }
    let (name, pid) = token.rsplit_once('.')?;
    let pid: u32 = pid.trim().parse().ok()?;
    let name = name.trim().trim_end_matches('.').trim();
    if name.is_empty() {
        return None;
    }
    Some((name.to_owned(), pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实采样片段（来自 macOS 27.0.1 实测输出），含两个被列宽破坏的脏行。
    const REAL_SAMPLE: &str = "\
launchd.1                                                                                    147705            3300
syslogd.397                                                                                       0          101707
mDNSResponder.506                                                                          78765772        28543065
netbiosd.78936                                                                                4452243          356846
natpmpd.79040                                                                                     0252
dhcp6d.79042                                                                                    432672
Codex (Aperitif.79108                                                                         17119           14881
Google Chrome H..81838                                                                     37566388         4545245
verge-mihomo.9306                                                                         328457601       331704672
";

    fn all_rows(sample: &str) -> (Vec<ProcRow>, usize) {
        let mut p = NettopParser::new();
        let mut rows = Vec::new();
        for line in sample.lines() {
            if let Some(mut frame) = p.feed(line) {
                rows.append(&mut frame);
            }
        }
        rows.append(&mut p.flush());
        (rows, p.unparsed)
    }

    #[test]
    fn parses_real_aligned_output() {
        let (rows, unparsed) = all_rows(REAL_SAMPLE);
        let find = |name: &str| rows.iter().find(|r| r.name == name).cloned();

        let launchd = find("launchd").expect("launchd 应被解析");
        assert_eq!(launchd.pid, 1);
        assert_eq!(launchd.rx, 147705);
        assert_eq!(launchd.tx, 3300);

        let chrome = find("Google Chrome H").expect("Chrome 应被解析");
        assert_eq!(chrome.pid, 81838);
        assert_eq!(chrome.rx, 37566388);

        // 进程名带空格的情况
        let codex = find("Codex (Aperitif").expect("Codex 应被解析");
        assert_eq!(codex.pid, 79108);
        assert_eq!(codex.tx, 14881);

        // 脏行应被丢弃并计数，而不是猜一个值出来
        assert_eq!(unparsed, 2, "natpmpd / dhcp6d 两行缺列，应被丢弃");
        assert!(find("natpmpd").is_none());
    }

    #[test]
    fn detects_frame_boundary_by_repeated_pid() {
        let two_frames = format!("{REAL_SAMPLE}{REAL_SAMPLE}");
        let mut p = NettopParser::new();
        let mut frames = Vec::new();
        for line in two_frames.lines() {
            if let Some(rows) = p.feed(line) {
                frames.push(rows);
            }
        }
        assert_eq!(frames.len(), 1, "第二轮的首行触发一次切帧");
        assert!(frames[0].iter().any(|r| r.name == "launchd"));
    }

    #[test]
    fn proc_token_parsing() {
        assert_eq!(
            parse_proc("Google Chrome H..81838"),
            Some(("Google Chrome H".to_owned(), 81838))
        );
        assert_eq!(
            parse_proc("com.apple.WebKit.WebContent.987"),
            Some(("com.apple.WebKit.WebContent".to_owned(), 987))
        );
        assert_eq!(parse_proc("bytes_in"), None);
        assert_eq!(parse_proc(""), None);
    }

    #[test]
    fn header_lines_are_ignored_not_counted_as_errors() {
        let mut p = NettopParser::new();
        assert!(p
            .feed("time    interface   state   bytes_in   bytes_out")
            .is_none());
        assert_eq!(p.unparsed, 0);
    }

    #[test]
    fn csv_fallback_works() {
        let row = parse_record("Safari.567,2048,1024").expect("CSV 兜底");
        assert_eq!(row.name, "Safari");
        assert_eq!(row.pid, 567);
        assert_eq!(row.rx, 2048);
        assert_eq!(row.tx, 1024);
    }
}
