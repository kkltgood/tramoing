//! `nettop -L`（CSV、按连接）输出解析，以及把流量归属回「发起方」。
//!
//! # 为什么要按连接
//!
//! `nettop -P` 只给每个进程的汇总，走系统代理时一条下载会出现在三个 socket 上：
//!
//! ```text
//! Chrome  → 127.0.0.1:7890   rx +n     （回环，客户端侧）
//! 代理    → 127.0.0.1:52344  tx +n     （回环，服务端侧）
//! 代理    → 远端:443         rx +n     （外网）
//! ```
//!
//! 汇总口径下要么把 n 算两遍（总量虚高），要么整条记到代理名下（看不到 App）。
//! 按连接解析后可以精确配对：回环两端是同一个五元组的镜像，服务端那一侧是代理，
//! 客户端那一侧才是发起方。于是：
//!
//! - **总量**只取外网接口的连接；
//! - **归属**取「外网连接 + 指向代理的回环客户端侧」，代理自己的外网连接里
//!   已经被回填过的那部分减掉，剩下的才是它自身的流量（DNS、自更新等）。
//!
//! 本机互访（浏览器 ↔ 本地 dev server）两端都不是代理，直接不计 —— 那部分
//! 从来没上过网卡，不该出现在流量统计里。

use std::collections::{HashMap, HashSet};

use tm_core::AppKey;

/// 一条连接（或一个监听 socket）的累计计数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowRow {
    pub pid: u32,
    pub proc_name: String,
    /// tcp4 / tcp6 / udp4 / udp6
    pub proto: String,
    pub local_ip: String,
    pub local_port: u16,
    pub remote_ip: String,
    pub remote_port: u16,
    pub iface: String,
    pub state: String,
    pub rx: u64,
    pub tx: u64,
}

impl FlowRow {
    fn transport(&self) -> &str {
        match self.proto.as_str() {
            "tcp4" | "tcp6" => "tcp",
            "udp4" | "udp6" => "udp",
            proto => proto,
        }
    }

    fn is_listen(&self) -> bool {
        self.state == "Listen"
    }

    /// TUN 模式的隧道接口（Clash 增强模式、WireGuard 等）。
    /// 原生 IPsec / PPP 不走这里：它们的报文由内核封装，没有对应的用户态转发
    /// 进程，把接口排除掉反而会把流量算丢。
    fn is_tunnel(&self) -> bool {
        self.iface.starts_with("utun")
    }

    fn is_loopback(&self) -> bool {
        self.iface == "lo0" || (is_loopback_ip(&self.local_ip) && is_loopback_ip(&self.remote_ip))
    }
}

fn is_loopback_ip(ip: &str) -> bool {
    if ip == "localhost" {
        return true;
    }
    // 通配地址表示未指定端点，不代表回环；UDP 在物理接口上也会显示 *:*。
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => ip.is_loopback(),
        Ok(std::net::IpAddr::V6(ip)) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// 解析
// ---------------------------------------------------------------------------

/// 一帧里的解析结果。进程汇总行本身不带速率，只是给后面的连接行提供 pid。
#[derive(Debug, Default)]
pub struct FlowParser {
    block: Vec<FlowRow>,
    cur_pid: Option<u32>,
    cur_name: String,
    /// 吃进来的非空行数
    pub lines_seen: usize,
    /// 解析失败的连接行数
    pub unparsed: usize,
    header_seen: bool,
}

impl FlowParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一行 CSV。返回 `Some(rows)` 表示一帧结束。
    ///
    /// `nettop -L` 每次采样都会重打表头（`time,,interface,...`），用它切帧最可靠。
    pub fn feed(&mut self, line: &str) -> Option<Vec<FlowRow>> {
        let t = line.trim_end_matches(['\r', '\n']);
        if t.trim().is_empty() {
            return None;
        }
        self.lines_seen += 1;

        if is_header(t) {
            self.cur_pid = None;
            if !std::mem::replace(&mut self.header_seen, true) {
                return None; // 第一帧的表头
            }
            return Some(std::mem::take(&mut self.block));
        }

        match parse_row(t) {
            Row::Flow(mut row) => match self.cur_pid {
                Some(pid) => {
                    row.pid = pid;
                    row.proc_name = self.cur_name.clone();
                    self.block.push(row);
                }
                // 连接行前面没有进程行：这一行没法归属，丢掉并计数
                None => self.unparsed += 1,
            },
            Row::Process { pid, name } => {
                self.cur_pid = Some(pid);
                self.cur_name = name;
            }
            Row::Junk => self.unparsed += 1,
        }
        None
    }

    /// 取走还没凑满一帧的残余数据。
    pub fn flush(&mut self) -> Vec<FlowRow> {
        std::mem::take(&mut self.block)
    }
}

fn is_header(line: &str) -> bool {
    line.split(',').next().map(str::trim) == Some("time")
}

enum Row {
    Flow(FlowRow),
    Process { pid: u32, name: String },
    Junk,
}

fn parse_row(line: &str) -> Row {
    let f: Vec<&str> = line.split(',').collect();
    if f.len() < 6 {
        return Row::Junk;
    }
    let name = f[1].trim();
    if name.is_empty() {
        return Row::Junk;
    }

    // 连接行：`tcp4 127.0.0.1:8021<->*:*` / `udp4 *:67<->*:*`
    if let Some((proto, rest)) = name.split_once(' ').filter(|(proto, rest)| {
        matches!(*proto, "tcp4" | "tcp6" | "udp4" | "udp6") && rest.contains("<->")
    }) {
        let Some((left, right)) = rest.split_once("<->") else {
            return Row::Junk;
        };
        let (local_ip, local_port) = parse_endpoint(left);
        let (remote_ip, remote_port) = parse_endpoint(right);
        return Row::Flow(FlowRow {
            pid: 0,
            proc_name: String::new(),
            proto: proto.to_owned(),
            local_ip,
            local_port,
            remote_ip,
            remote_port,
            iface: f[2].trim().to_owned(),
            state: f[3].trim().to_owned(),
            rx: num(f.get(4).copied()),
            tx: num(f.get(5).copied()),
        });
    }

    // 进程汇总行：`launchd.1`，后面的连接行继承这个 pid 和名字
    let Some((base, pid)) = name.rsplit_once('.') else {
        return Row::Junk;
    };
    let Ok(pid) = pid.trim().parse::<u32>() else {
        return Row::Junk;
    };
    let base = base.trim();
    if base.is_empty() {
        return Row::Junk;
    }
    Row::Process {
        pid,
        name: base.to_owned(),
    }
}

fn num(field: Option<&str>) -> u64 {
    field
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// `127.0.0.1:8021` / `::1.8021` / `*:67` / `public1.alidns.com:443` → (ip, port)
fn parse_endpoint(s: &str) -> (String, u16) {
    let s = s.trim();
    if matches!(s, "*" | "*:*" | "*.*") {
        return (String::new(), 0);
    }
    // Bracketed IPv6 is unambiguous, including a zone identifier.
    if let Some((ip, port)) = s.strip_prefix('[').and_then(|s| s.split_once("]:")) {
        if let Some(port) = endpoint_port(port) {
            return (norm_ip(ip), port);
        }
    }
    // nettop uses a dot for IPv6 ports: ::1.8021, fe80::1%en0.443, *.5353.
    if let Some((ip, port)) = s.rsplit_once('.') {
        if ip == "*" || (ip.contains(':') && numeric_ip(ip)) {
            if let Some(port) = endpoint_port(port) {
                return (norm_ip(ip), port);
            }
        }
    }
    // A bare IP has no port. In particular ::1 must not become (":", 1),
    // nor may 192.0.2.1 or ::ffff:192.0.2.1 lose their last address octet.
    if numeric_ip(s) {
        return (norm_ip(s), 0);
    }
    // IPv4 (or a hostname in nonnumeric fixtures) uses a colon. An unbracketed
    // IPv6 address never uses this separator, even when its last group is decimal.
    if let Some((ip, port)) = s.rsplit_once(':') {
        if !ip.contains(':') {
            if let Some(port) = endpoint_port(port) {
                return (norm_ip(ip), port);
            }
        }
    }
    (norm_ip(s), 0)
}

fn endpoint_port(port: &str) -> Option<u16> {
    if port.trim() == "*" {
        Some(0)
    } else {
        port.trim().parse().ok()
    }
}

fn numeric_ip(ip: &str) -> bool {
    ip.trim_start_matches('[')
        .trim_end_matches(']')
        .split('%')
        .next()
        .unwrap_or_default()
        .parse::<std::net::IpAddr>()
        .is_ok()
}

fn norm_ip(s: &str) -> String {
    let s = s.trim().trim_start_matches('[').trim_end_matches(']');
    if s == "*" {
        String::new()
    } else {
        s.to_owned()
    }
}

// ---------------------------------------------------------------------------
// 归属
// ---------------------------------------------------------------------------

/// 一次归属结果：某个应用这一帧新增的字节。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bytes {
    pub rx: u64,
    pub tx: u64,
}

impl Bytes {
    fn add(&mut self, other: Bytes) {
        self.rx = self.rx.saturating_add(other.rx);
        self.tx = self.tx.saturating_add(other.tx);
    }

    fn is_zero(&self) -> bool {
        self.rx == 0 && self.tx == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FlowKey {
    pid: u32,
    /// IPv4 / IPv6 两套 socket 可能落在同一个四元组上，协议要进 key
    proto: String,
    /// 通配 socket（`udp4 *:5353<->*:*`）会在每个接口各出一条记录，
    /// 计数器是分开的，所以接口必须进 key，否则几条记录会互相「差分」，
    /// 小值那条会被当成计数器回退，凭空多出一大块流量。
    iface: String,
    local: (String, u16),
    remote: (String, u16),
}

/// 一帧里某个进程的字节分类。
///
/// - `ext`：物理网卡上的外网流量（真正上网卡的）
/// - `srv`：回环上作为服务端收到的（代理转发出去的，回填时要减掉）
/// - `tun`：隧道侧（`utunN`）的，等识别出转发进程后再回填给 App
#[derive(Default)]
struct PidAcc {
    ext: Bytes,
    srv: Bytes,
    tun: Bytes,
}

/// 逐帧把连接增量归属到发起方。
///
/// 规则见模块注释。关键状态只有一个：每条连接上一次的累计值 ——
/// 连接关闭后它的字节不会「倒退」，所以必须在连接级别做差分，
/// 不能直接对进程汇总做差分（关掉一条连接会让汇总变小）。
#[derive(Debug)]
pub struct FlowAccounting {
    frame: u64,
    /// FlowKey -> (上次 rx, 上次 tx, 最后出现的帧)
    last: std::collections::HashMap<FlowKey, (u64, u64, u64)>,
    /// 连接消失后还保留几帧，防止同一连接短暂缺席导致重复计基线
    keep_frames: u64,
    ledgers: HashMap<u32, RelayLedger>,
}

/// 已先记到代理的外网字节用于抵消迟到的回环样本；先到的客户端样本等外网预算。
#[derive(Debug, Default)]
struct RelayLedger {
    credited: Bytes,
    pending: std::collections::BTreeMap<AppKey, Bytes>,
    seen: u64,
}

impl Default for FlowAccounting {
    fn default() -> Self {
        Self::new()
    }
}

impl FlowAccounting {
    pub fn new() -> Self {
        Self {
            frame: 0,
            last: std::collections::HashMap::new(),
            keep_frames: 8,
            ledgers: HashMap::new(),
        }
    }

    /// 处理一帧。`resolve(pid, 进程名)` 用来把 pid 归并成应用。
    pub fn update<F>(&mut self, rows: &[FlowRow], mut resolve: F) -> Vec<(AppKey, Bytes)>
    where
        F: FnMut(u32, &str) -> AppKey,
    {
        self.frame += 1;

        // TCP/UDP 可独立绑定同一个端口；双栈 TCP 监听仍可接受 IPv4 连接。
        let listeners: HashSet<(&str, u32, u16)> = rows
            .iter()
            .filter(|r| r.is_listen() && r.local_port != 0)
            .map(|r| (r.transport(), r.pid, r.local_port))
            .collect();

        // 本机端点 -> pid，用来找回环另一端的属主
        let mut owner: HashMap<(&str, &str, u16), u32> = HashMap::new();
        let mut names: HashMap<u32, &str> = HashMap::new();
        for r in rows {
            names.entry(r.pid).or_insert(r.proc_name.as_str());
            if r.local_port != 0 {
                owner.insert((r.transport(), r.local_ip.as_str(), r.local_port), r.pid);
            }
        }

        let mut acc: HashMap<u32, PidAcc> = HashMap::new();
        // (客户端 pid, 服务端 pid, 增量)
        let mut client_side: Vec<(u32, Option<u32>, Bytes)> = Vec::new();
        let mut cli_sum: HashMap<u32, Bytes> = HashMap::new();

        // 同一个四元组可能出现多行：mDNS 这类服务会开多个 socket 共用通配端口，
        // 每行计数器独立。逐行差分会让小值那行被当成「计数器回退」，每帧凭空
        // 多出几 MB。这里按 key 合并，取各字段最大值。
        let mut collapsed: HashMap<FlowKey, (u64, u64, usize)> = HashMap::new();
        for (idx, r) in rows.iter().enumerate() {
            if r.is_listen() || r.local_port == 0 {
                continue;
            }
            let key = FlowKey {
                pid: r.pid,
                proto: r.proto.clone(),
                iface: r.iface.clone(),
                local: (r.local_ip.clone(), r.local_port),
                remote: (r.remote_ip.clone(), r.remote_port),
            };
            let e = collapsed.entry(key).or_insert((0, 0, idx));
            e.0 = e.0.max(r.rx);
            e.1 = e.1.max(r.tx);
        }

        for (key, (cur_rx, cur_tx, idx)) in collapsed {
            let r = &rows[idx];
            let (drx, dtx) = match self.last.get(&key) {
                // 已有基线：正常差分；计数器回退说明端口被复用，按「从 0 重计」处理
                Some(&(prx, ptx, _)) => (counter_delta(cur_rx, prx), counter_delta(cur_tx, ptx)),
                // 新连接：这一帧只建基线，避免把连接建立前的历史字节算进来
                None if self.frame == 1 => (0, 0),
                None => (cur_rx, cur_tx),
            };
            self.last.insert(key, (cur_rx, cur_tx, self.frame));

            let d = Bytes { rx: drx, tx: dtx };
            if d.is_zero() {
                continue;
            }

            if r.is_loopback() {
                let is_server = listeners.contains(&(r.transport(), r.pid, r.local_port));
                if is_server {
                    // 服务端侧，和客户端侧是同一份数据，先记下来用于抵扣
                    acc.entry(r.pid).or_default().srv.add(d);
                } else {
                    let server = owner
                        .get(&(r.transport(), r.remote_ip.as_str(), r.remote_port))
                        .copied()
                        .filter(|p| listeners.contains(&(r.transport(), *p, r.remote_port)));
                    client_side.push((r.pid, server, d));
                    cli_sum.entry(r.pid).or_default().add(d);
                }
            } else if r.is_tunnel() && !r.remote_ip.is_empty() && r.remote_port != 0 {
                // 隧道侧：这是 App 自己的连接，真正发出去的是转发进程
                acc.entry(r.pid).or_default().tun.add(d);
            } else {
                acc.entry(r.pid).or_default().ext.add(d);
            }
        }

        // 从连接拓扑判断代理，不能依赖两边恰好在同一秒都有增量。
        let external: HashSet<u32> = rows
            .iter()
            .filter(|r| !r.is_listen() && !r.is_loopback() && !r.is_tunnel())
            .map(|r| r.pid)
            .collect();
        let mut upstream = HashMap::new();
        for r in rows.iter().filter(|r| r.is_loopback() && !r.is_listen()) {
            if !listeners.contains(&(r.transport(), r.pid, r.local_port)) {
                if let Some(server) =
                    owner.get(&(r.transport(), r.remote_ip.as_str(), r.remote_port))
                {
                    if listeners.contains(&(r.transport(), *server, r.remote_port)) {
                        upstream.insert(r.pid, *server);
                    }
                }
            }
        }
        let root_of = |mut pid: u32| -> Option<u32> {
            let mut seen = HashSet::new();
            while seen.insert(pid) {
                if external.contains(&pid) {
                    return Some(pid);
                }
                pid = *upstream.get(&pid)?;
            }
            None
        };
        let mut relays: HashSet<u32> = listeners
            .iter()
            .map(|(_, pid, _)| *pid)
            .filter(|pid| root_of(*pid).is_some())
            .collect();

        let tunnel = acc.values().fold(Bytes::default(), |mut sum, a| {
            sum.add(a.tun);
            sum
        });
        let tunnel_owner = pick_tunnel_owner(&acc, tunnel);
        if let Some(pid) = tunnel_owner {
            relays.insert(pid);
        }

        let mut out: HashMap<AppKey, Bytes> = HashMap::new();
        let mut app_of = |pid: u32| resolve(pid, names.get(&pid).copied().unwrap_or(""));
        let mut claims: Vec<(u32, AppKey, Bytes)> = Vec::new();
        if let Some(root) = tunnel_owner {
            for (pid, a) in &acc {
                if !a.tun.is_zero() {
                    claims.push((root, app_of(*pid), a.tun));
                }
            }
        }
        for (pid, server, d) in &client_side {
            if !relays.contains(pid) {
                if let Some(root) = server.and_then(root_of) {
                    claims.push((root, app_of(*pid), *d));
                }
            }
        }

        // 任何回填都必须消耗同一代理的外网预算，总和始终等于物理接口增量。
        // 外网先于回环到达时已经记到代理；后到的副本抵消 credit，不再新增总量。
        // 无法仅凭累计采样可靠地撤回历史归属，这部分保留在代理名下。
        claims.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        for (root, app, mut d) in claims {
            let ledger = self.ledgers.entry(root).or_default();
            ledger.seen = self.frame;
            let old_rx = d.rx.min(ledger.credited.rx);
            let old_tx = d.tx.min(ledger.credited.tx);
            ledger.credited.rx -= old_rx;
            ledger.credited.tx -= old_tx;
            d.rx -= old_rx;
            d.tx -= old_tx;
            if !d.is_zero() {
                ledger.pending.entry(app).or_default().add(d);
            }
        }
        for (pid, a) in &acc {
            let mut budget = a.ext;
            if relays.contains(pid) || self.ledgers.contains_key(pid) {
                let ledger = self.ledgers.entry(*pid).or_default();
                ledger.seen = self.frame;
                for (app, pending) in &mut ledger.pending {
                    let take = Bytes {
                        rx: budget.rx.min(pending.rx),
                        tx: budget.tx.min(pending.tx),
                    };
                    budget.rx -= take.rx;
                    budget.tx -= take.tx;
                    pending.rx -= take.rx;
                    pending.tx -= take.tx;
                    if !take.is_zero() {
                        out.entry(app.clone()).or_default().add(take);
                    }
                }
                ledger.pending.retain(|_, d| !d.is_zero());
                ledger.credited.add(budget);
            }
            if !budget.is_zero() {
                out.entry(app_of(*pid)).or_default().add(budget);
            }
        }

        // 清理消失很久的连接
        let keep = self.keep_frames;
        let frame = self.frame;
        self.last
            .retain(|_, (_, _, seen)| frame.saturating_sub(*seen) <= keep);
        self.ledgers
            .retain(|_, ledger| frame.saturating_sub(ledger.seen) <= keep);

        out.into_iter().collect()
    }
}

/// 每帧隧道侧至少这么多字节才尝试识别转发进程，避免 mDNS 这类隧道上的
/// 多播噪声把判断带偏。
const MIN_TUNNEL_BYTES: u64 = 4096;

/// 从「隧道侧字节」反推谁在转发它。
///
/// TUN 模式没有端口可以配对：App 的 socket 落在 `utunN`，转发进程的 socket
/// 落在物理网卡上，两者四元组毫无关系。唯一可靠的守恒关系是**量** ——
/// 转发进程的物理流量 ≈ 隧道里所有 App 的流量之和，只会略多（加密/协议开销）。
/// 于是挑「没有隧道侧流量、物理流量又最接近隧道总量」的那个进程。
fn pick_tunnel_owner(acc: &HashMap<u32, PidAcc>, tunnel: Bytes) -> Option<u32> {
    let total = tunnel.rx.saturating_add(tunnel.tx);
    if total < MIN_TUNNEL_BYTES {
        return None;
    }
    let t = total as f64;
    let mut best: Option<(u32, f64)> = None;
    for (pid, a) in acc {
        // 转发进程自己不产生隧道侧流量；有隧道侧流量的是被转发的 App
        if !a.tun.is_zero() || a.ext.is_zero() {
            continue;
        }
        let p = a.ext.rx.saturating_add(a.ext.tx) as f64;
        let ratio = p / t;
        if !(0.7..=3.0).contains(&ratio) {
            continue;
        }
        let score = (ratio - 1.0).abs();
        if best.is_none_or(|(_, s)| score < s) {
            best = Some((*pid, score));
        }
    }
    best.map(|(pid, _)| pid)
}

fn counter_delta(cur: u64, prev: u64) -> u64 {
    if cur >= prev {
        cur - prev
    } else {
        cur
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_distinguish_ipv6_ports_wildcards_and_bare_addresses() {
        for (text, ip, port) in [
            ("127.0.0.1:8021", "127.0.0.1", 8021),
            ("::1.8021", "::1", 8021),
            ("fe80::1%en0.443", "fe80::1%en0", 443),
            ("[fe80::1%en0]:443", "fe80::1%en0", 443),
            ("::ffff:192.0.2.1.443", "::ffff:192.0.2.1", 443),
            ("*:67", "", 67),
            ("*.5353", "", 5353),
            ("*:*", "", 0),
            ("*.*", "", 0),
            ("[::]:*", "::", 0),
            ("::", "::", 0),
            ("::1", "::1", 0),
            ("192.0.2.1", "192.0.2.1", 0),
            ("::ffff:192.0.2.1", "::ffff:192.0.2.1", 0),
            ("public1.alidns.com:443", "public1.alidns.com", 443),
        ] {
            assert_eq!(parse_endpoint(text), (ip.into(), port), "{text}");
        }
    }

    const HEADER: &str = "time,,interface,state,bytes_in,bytes_out,rx_dupe,rx_ooo,re-tx,rtt_avg,rcvsize,tx_win,tc_class,tc_mgt,cc_algo,P,C,R,W,arch,";

    fn flow(line: &str) -> FlowRow {
        match parse_row(line) {
            Row::Flow(r) => r,
            _ => panic!("不是连接行：{line}"),
        }
    }

    fn parse_frame(sample: &str) -> Vec<FlowRow> {
        let mut p = FlowParser::new();
        let mut rows = Vec::new();
        for line in sample.lines() {
            if let Some(mut frame) = p.feed(line) {
                rows.append(&mut frame);
            }
        }
        rows.append(&mut p.flush());
        rows
    }

    #[test]
    fn parses_ipv4_flow() {
        let row = flow("14:22:45.601558,tcp4 156.11.42.110:53998<->17.57.145.25:5223,en7,Established,23687,181549,0,0,0,50.78 ms,131072,31872,RD,-,cubic,-,-,-,-,ch,");
        assert_eq!(row.local_ip, "156.11.42.110");
        assert_eq!(row.local_port, 53998);
        assert_eq!(row.remote_ip, "17.57.145.25");
        assert_eq!(row.remote_port, 5223);
        assert_eq!(row.iface, "en7");
        assert_eq!(row.rx, 23687);
        assert_eq!(row.tx, 181549);
    }

    #[test]
    fn parses_ipv6_dotted_port() {
        let row =
            flow("14:22:45.601593,tcp6 ::1.8021<->*.*,lo0,Listen,,,,,,,,,,-,cubic,-,-,-,-,so,");
        assert_eq!(row.local_ip, "::1");
        assert_eq!(row.local_port, 8021);
        assert_eq!(row.remote_port, 0);
        assert!(row.is_listen());
        assert!(row.is_loopback());
    }

    #[test]
    fn parses_udp_wildcards() {
        let row =
            flow("14:22:45.600351,udp4 *:67<->*:*,bridge100,,166841,900,,,,,786896,,CTL,,,,,,,so,");
        assert_eq!(row.local_ip, "");
        assert_eq!(row.local_port, 67);
        assert_eq!(row.rx, 166841);
        assert_eq!(row.tx, 900);
    }

    #[test]
    fn parses_hostname_remote() {
        let row = flow("t,tcp4 127.0.0.1:52344<->localhost:7890,lo0,Established,100,200,");
        assert_eq!(row.remote_ip, "localhost");
        assert_eq!(row.remote_port, 7890);
    }

    #[test]
    fn connection_rows_inherit_pid_from_process_row() {
        let sample = format!(
            "{HEADER}\n14:00:00.000001,Google Chrome H.81838,,,1,2,\n\
             14:00:00.000002,tcp4 127.0.0.1:52344<->127.0.0.1:7890,lo0,Established,5,6,"
        );
        let rows = parse_frame(&sample);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pid, 81838);
        assert_eq!(rows[0].proc_name, "Google Chrome H");
    }

    #[test]
    fn process_names_starting_with_protocol_prefix_keep_their_own_pid() {
        for name in ["tcpdump", "udp-client", "tcp helper", "tcp4 helper"] {
            let rows = parse_frame(&format!(
                "{HEADER}\nt,previous.1,,,0,0,\nt,{name}.42,,,0,0,\n\
                 t,tcp4 192.0.2.1:50000<->198.51.100.1:443,en0,Established,5,6,"
            ));
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].pid, 42);
            assert_eq!(rows[0].proc_name, name);
        }
    }

    #[test]
    fn header_splits_frames() {
        let sample = format!(
            "{HEADER}\n14:00:00.1,launchd.1,,,1,2,\n\
             14:00:00.2,tcp4 127.0.0.1:1<->127.0.0.1:2,lo0,Established,5,6,\n\
             {HEADER}\n14:00:01.1,launchd.1,,,11,12,\n\
             14:00:01.2,tcp4 127.0.0.1:1<->127.0.0.1:2,lo0,Established,7,8,\n"
        );
        let mut p = FlowParser::new();
        let mut frames = Vec::new();
        for line in sample.lines() {
            if let Some(rows) = p.feed(line) {
                frames.push(rows);
            }
        }
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), 1);
        assert_eq!(frames[0][0].rx, 5);
    }
}

#[cfg(test)]
mod accounting_tests {
    use super::*;

    #[allow(clippy::too_many_arguments)] // CSV fixture mirrors the input columns.
    fn row(
        pid: u32,
        name: &str,
        iface: &str,
        state: &str,
        lip: &str,
        lp: u16,
        rip: &str,
        rp: u16,
        rx: u64,
        tx: u64,
    ) -> FlowRow {
        FlowRow {
            pid,
            proc_name: name.to_owned(),
            proto: "tcp4".to_owned(),
            local_ip: lip.to_owned(),
            local_port: lp,
            remote_ip: rip.to_owned(),
            remote_port: rp,
            iface: iface.to_owned(),
            state: state.to_owned(),
            rx,
            tx,
        }
    }

    fn resolve(pid: u32, name: &str) -> AppKey {
        AppKey::new(format!("{name}#{pid}"))
    }

    fn of(out: &[(AppKey, Bytes)], name: &str) -> Bytes {
        out.iter()
            .find(|(k, _)| k.as_str() == name)
            .map(|(_, b)| *b)
            .unwrap_or_default()
    }

    fn listen(pid: u32, name: &str, port: u16) -> FlowRow {
        row(pid, name, "lo0", "Listen", "127.0.0.1", port, "", 0, 0, 0)
    }

    #[test]
    fn wildcard_udp_on_a_physical_interface_is_not_loopback() {
        let mut acc = FlowAccounting::new();
        let frame = |bytes| {
            let mut r = row(42, "udp-app", "en0", "", "", 5353, "", 0, bytes, 0);
            r.proto = "udp4".into();
            vec![r]
        };
        acc.update(&frame(100), resolve);
        assert_eq!(of(&acc.update(&frame(600), resolve), "udp-app#42").rx, 500);
    }

    /// 系统代理下的一次下载：客户端回环 rx = 代理回环 tx = 代理外网 rx。
    /// 归属必须给客户端，代理不能重复计。
    #[test]
    fn proxied_download_is_credited_to_the_client() {
        let mut acc = FlowAccounting::new();
        let frame = |n: u64| {
            vec![
                listen(9, "verge-mihomo", 7890),
                row(
                    100,
                    "Chrome",
                    "lo0",
                    "Established",
                    "127.0.0.1",
                    52344,
                    "127.0.0.1",
                    7890,
                    n,
                    0,
                ),
                row(
                    9,
                    "verge-mihomo",
                    "lo0",
                    "Established",
                    "127.0.0.1",
                    7890,
                    "127.0.0.1",
                    52344,
                    0,
                    n,
                ),
                row(
                    9,
                    "verge-mihomo",
                    "en7",
                    "Established",
                    "192.168.1.10",
                    40000,
                    "93.184.216.34",
                    443,
                    n,
                    0,
                ),
            ]
        };

        assert!(acc.update(&frame(0), resolve).is_empty(), "首帧只建基线");
        let out = acc.update(&frame(1_000_000), resolve);

        assert_eq!(of(&out, "Chrome#100").rx, 1_000_000);
        assert_eq!(of(&out, "verge-mihomo#9"), Bytes::default());
    }

    #[test]
    fn udp_endpoint_does_not_replace_a_tcp_proxy_owner() {
        for (udp_first, listener_proto) in [
            (false, "tcp4"),
            (true, "tcp4"),
            (false, "tcp6"),
            (true, "tcp6"),
        ] {
            let mut acc = FlowAccounting::new();
            let frame = |n| {
                let mut listener = listen(9, "proxy", 7890);
                listener.proto = listener_proto.into();
                if listener_proto == "tcp6" {
                    listener.local_ip = "::".into();
                }
                let mut rows = vec![
                    listener,
                    row(
                        100,
                        "browser",
                        "lo0",
                        "Established",
                        "127.0.0.1",
                        52344,
                        "127.0.0.1",
                        7890,
                        n,
                        0,
                    ),
                    row(
                        9,
                        "proxy",
                        "lo0",
                        "Established",
                        "127.0.0.1",
                        7890,
                        "127.0.0.1",
                        52344,
                        0,
                        n,
                    ),
                    row(
                        9,
                        "proxy",
                        "en0",
                        "Established",
                        "192.0.2.1",
                        40000,
                        "198.51.100.1",
                        443,
                        n,
                        0,
                    ),
                ];
                // TCP and UDP may bind the same address and port independently.
                let mut udp = row(
                    200,
                    "udp-service",
                    "lo0",
                    "",
                    "127.0.0.1",
                    7890,
                    "",
                    0,
                    0,
                    0,
                );
                udp.proto = "udp4".into();
                rows.insert(if udp_first { 0 } else { rows.len() }, udp);
                rows
            };
            acc.update(&frame(0), resolve);
            let out = acc.update(&frame(1000), resolve);
            assert_eq!(of(&out, "browser#100").rx, 1000);
            assert_eq!(of(&out, "proxy#9"), Bytes::default());
            assert_eq!(of(&out, "udp-service#200"), Bytes::default());
        }
    }

    #[test]
    fn direct_app_counts_its_external_traffic() {
        let mut acc = FlowAccounting::new();
        let frame = |n: u64| {
            vec![row(
                7,
                "Safari",
                "en0",
                "Established",
                "192.168.1.10",
                50000,
                "1.1.1.1",
                443,
                n,
                n / 10,
            )]
        };
        acc.update(&frame(0), resolve);
        let out = acc.update(&frame(4096), resolve);
        assert_eq!(of(&out, "Safari#7"), Bytes { rx: 4096, tx: 409 });
    }

    /// 本机互访（浏览器 → 本地 dev server）没上过网卡，不该计入。
    #[test]
    fn local_only_traffic_is_ignored() {
        let mut acc = FlowAccounting::new();
        let frame = |n: u64| {
            vec![
                listen(200, "node", 3000),
                row(
                    100,
                    "Chrome",
                    "lo0",
                    "Established",
                    "127.0.0.1",
                    52344,
                    "127.0.0.1",
                    3000,
                    n,
                    0,
                ),
                row(
                    200,
                    "node",
                    "lo0",
                    "Established",
                    "127.0.0.1",
                    3000,
                    "127.0.0.1",
                    52344,
                    0,
                    n,
                ),
            ]
        };
        acc.update(&frame(0), resolve);
        let out = acc.update(&frame(800), resolve);
        assert!(out.is_empty(), "本机互访不应出现在流量列表里：{out:?}");
    }

    /// 链式代理：App → 代理1 → 代理2 → 外网。
    /// 只应记到 App 头上，两级代理都不能重复计。
    #[test]
    fn chained_proxies_are_not_double_counted() {
        let mut acc = FlowAccounting::new();
        let frame = |n: u64| {
            vec![
                listen(9, "mihomo", 7890),
                listen(10, "surge", 7891),
                row(
                    100,
                    "Chrome",
                    "lo0",
                    "Established",
                    "127.0.0.1",
                    52344,
                    "127.0.0.1",
                    7890,
                    n,
                    0,
                ),
                row(
                    9,
                    "mihomo",
                    "lo0",
                    "Established",
                    "127.0.0.1",
                    7890,
                    "127.0.0.1",
                    52344,
                    0,
                    n,
                ),
                row(
                    9,
                    "mihomo",
                    "lo0",
                    "Established",
                    "127.0.0.1",
                    53000,
                    "127.0.0.1",
                    7891,
                    n,
                    0,
                ),
                row(
                    10,
                    "surge",
                    "lo0",
                    "Established",
                    "127.0.0.1",
                    7891,
                    "127.0.0.1",
                    53000,
                    0,
                    n,
                ),
                row(
                    10,
                    "surge",
                    "en7",
                    "Established",
                    "192.168.1.10",
                    40000,
                    "93.184.216.34",
                    443,
                    n,
                    0,
                ),
            ]
        };
        acc.update(&frame(0), resolve);
        let out = acc.update(&frame(1_000_000), resolve);

        assert_eq!(of(&out, "Chrome#100").rx, 1_000_000);
        assert_eq!(of(&out, "mihomo#9"), Bytes::default());
        assert_eq!(of(&out, "surge#10"), Bytes::default());
    }

    /// 代理自己的流量（自更新、DNS）不该被减没。
    #[test]
    fn proxy_keeps_its_own_traffic() {
        let mut acc = FlowAccounting::new();
        let frame = |srv: u64, ext: u64| {
            vec![
                listen(9, "clash", 7890),
                row(
                    100,
                    "Chrome",
                    "lo0",
                    "Established",
                    "127.0.0.1",
                    52344,
                    "127.0.0.1",
                    7890,
                    srv,
                    0,
                ),
                row(
                    9,
                    "clash",
                    "lo0",
                    "Established",
                    "127.0.0.1",
                    7890,
                    "127.0.0.1",
                    52344,
                    0,
                    srv,
                ),
                row(
                    9,
                    "clash",
                    "en7",
                    "Established",
                    "192.168.1.10",
                    40000,
                    "1.1.1.1",
                    443,
                    ext,
                    0,
                ),
            ]
        };
        acc.update(&frame(0, 0), resolve);
        // 客户端下了 1 MB，代理自己下了 0.5 MB
        let out = acc.update(&frame(1_000_000, 1_500_000), resolve);
        assert_eq!(of(&out, "Chrome#100").rx, 1_000_000);
        assert_eq!(of(&out, "clash#9").rx, 500_000);
    }

    /// 通配 socket（mDNS）会按接口各出一条记录，计数器互相独立。
    /// key 里漏掉接口时，小值那条会被当成「计数器回退」，凭空多出流量。
    #[test]
    fn wildcard_socket_on_multiple_interfaces_is_not_phantom_traffic() {
        let mut acc = FlowAccounting::new();
        let frame = |bridge: u64, ap1: u64| {
            vec![
                row(
                    491,
                    "mDNSResponder",
                    "bridge100",
                    "",
                    "*",
                    5353,
                    "*",
                    0,
                    bridge,
                    0,
                ),
                row(491, "mDNSResponder", "ap1", "", "*", 5353, "*", 0, ap1, 0),
            ]
        };
        acc.update(&frame(1000, 400), resolve);
        let out = acc.update(&frame(2000, 800), resolve);
        assert_eq!(of(&out, "mDNSResponder#491").rx, 1400);
    }

    /// 同一个四元组出现多行（mDNS 多个 socket 共用通配端口），
    /// 计数器独立，不能互相差分 —— 否则小值那行会被当成回退，每帧凭空多出流量。
    #[test]
    fn duplicate_rows_for_one_tuple_do_not_phantom() {
        let mut acc = FlowAccounting::new();
        let frame = |a: u64, b: u64| {
            vec![
                row(3006, "Chrome", "en7", "", "*", 5353, "*", 0, a, 1002),
                row(3006, "Chrome", "en7", "", "*", 5353, "*", 0, b, 1002),
            ]
        };
        acc.update(&frame(2_000_000, 3_000_000), resolve);
        let out = acc.update(&frame(2_000_100, 3_000_200), resolve);
        assert_eq!(of(&out, "Chrome#3006").rx, 200);
    }

    /// 启动基线之后出现的新连接必须保留第一批字节。
    #[test]
    fn new_connection_keeps_initial_bytes() {
        let mut acc = FlowAccounting::new();
        acc.update(&[listen(9, "clash", 7890)], resolve);
        let out = acc.update(
            &[
                listen(9, "clash", 7890),
                row(
                    100,
                    "Chrome",
                    "lo0",
                    "Established",
                    "127.0.0.1",
                    52344,
                    "127.0.0.1",
                    7890,
                    9_000_000,
                    0,
                ),
                row(
                    9,
                    "clash",
                    "lo0",
                    "Established",
                    "127.0.0.1",
                    7890,
                    "127.0.0.1",
                    52344,
                    0,
                    9_000_000,
                ),
                row(
                    9,
                    "clash",
                    "en7",
                    "Established",
                    "192.168.1.10",
                    40000,
                    "1.1.1.1",
                    443,
                    9_000_000,
                    0,
                ),
            ],
            resolve,
        );
        assert_eq!(of(&out, "Chrome#100").rx, 9_000_000);
    }

    #[test]
    fn delayed_proxy_forwarding_conserves_external_total() {
        let mut acc = FlowAccounting::new();
        let frame = |ext, forwarded| {
            vec![
                listen(9, "proxy", 7890),
                row(
                    100,
                    "browser",
                    "lo0",
                    "Established",
                    "127.0.0.1",
                    52344,
                    "127.0.0.1",
                    7890,
                    forwarded,
                    0,
                ),
                row(
                    9,
                    "proxy",
                    "lo0",
                    "Established",
                    "127.0.0.1",
                    7890,
                    "127.0.0.1",
                    52344,
                    0,
                    forwarded,
                ),
                row(
                    9,
                    "proxy",
                    "en0",
                    "Established",
                    "192.168.1.1",
                    40000,
                    "1.1.1.1",
                    443,
                    ext,
                    0,
                ),
            ]
        };
        acc.update(&frame(0, 0), resolve);
        let a = acc.update(&frame(10_000, 0), resolve);
        let b = acc.update(&frame(11_000, 10_000), resolve);
        assert_eq!(a.iter().chain(&b).map(|(_, d)| d.rx).sum::<u64>(), 11_000);
    }

    #[test]
    fn parser_delivers_empty_baseline_and_idle_frames() {
        const HEADER: &str = "time,,interface,state,bytes_in,bytes_out";
        let mut parser = FlowParser::new();
        assert!(parser.feed(HEADER).is_none());
        assert_eq!(parser.feed(HEADER), Some(vec![]));
        assert_eq!(parser.feed(HEADER), Some(vec![]));
    }
}

#[cfg(test)]
mod tun_tests {
    use super::*;

    #[allow(clippy::too_many_arguments)] // CSV fixture mirrors the input columns.
    fn row(
        pid: u32,
        name: &str,
        iface: &str,
        lip: &str,
        lp: u16,
        rip: &str,
        rp: u16,
        rx: u64,
        tx: u64,
    ) -> FlowRow {
        FlowRow {
            pid,
            proc_name: name.to_owned(),
            proto: "tcp4".to_owned(),
            local_ip: lip.to_owned(),
            local_port: lp,
            remote_ip: rip.to_owned(),
            remote_port: rp,
            iface: iface.to_owned(),
            state: "Established".to_owned(),
            rx,
            tx,
        }
    }

    fn resolve(pid: u32, name: &str) -> AppKey {
        AppKey::new(format!("{name}#{pid}"))
    }

    fn of(out: &[(AppKey, Bytes)], name: &str) -> Bytes {
        out.iter()
            .find(|(k, _)| k.as_str() == name)
            .map(|(_, b)| *b)
            .unwrap_or_default()
    }

    /// TUN 模式的一次下载：App 的 socket 在 utun3 上，代理的 socket 在 en7 上。
    /// 归属给 App，代理不能重复计，总量仍然只有一份。
    #[test]
    fn tun_download_is_credited_to_the_app() {
        let mut acc = FlowAccounting::new();
        let frame = |n: u64| {
            vec![
                row(
                    100,
                    "Safari",
                    "utun3",
                    "192.168.1.10",
                    52344,
                    "93.184.216.34",
                    443,
                    n,
                    0,
                ),
                row(
                    9,
                    "mihomo",
                    "en7",
                    "192.168.1.10",
                    53000,
                    "93.184.216.34",
                    443,
                    n,
                    0,
                ),
            ]
        };
        acc.update(&frame(0), resolve);
        let out = acc.update(&frame(1_000_000), resolve);

        assert_eq!(of(&out, "Safari#100").rx, 1_000_000);
        assert_eq!(of(&out, "mihomo#9"), Bytes::default());
    }

    #[test]
    fn tun_upload_is_credited_to_the_app() {
        let mut acc = FlowAccounting::new();
        let frame = |n: u64| {
            vec![
                row(
                    100,
                    "Safari",
                    "utun3",
                    "192.168.1.10",
                    52344,
                    "93.184.216.34",
                    443,
                    0,
                    n,
                ),
                row(
                    9,
                    "mihomo",
                    "en7",
                    "192.168.1.10",
                    53000,
                    "93.184.216.34",
                    443,
                    0,
                    n,
                ),
            ]
        };
        acc.update(&frame(0), resolve);
        let out = acc.update(&frame(400_000), resolve);
        assert_eq!(of(&out, "Safari#100").tx, 400_000);
        assert_eq!(of(&out, "mihomo#9").tx, 0);
    }

    /// 加密/协议开销是代理真实付出的流量，应留在代理名下。
    #[test]
    fn tun_overhead_stays_with_the_tunnel() {
        let mut acc = FlowAccounting::new();
        let frame = |n: u64, overhead: u64| {
            vec![
                row(
                    100,
                    "Safari",
                    "utun3",
                    "192.168.1.10",
                    52344,
                    "93.184.216.34",
                    443,
                    n,
                    0,
                ),
                row(
                    9,
                    "mihomo",
                    "en7",
                    "192.168.1.10",
                    53000,
                    "93.184.216.34",
                    443,
                    n + overhead,
                    0,
                ),
            ]
        };
        acc.update(&frame(0, 0), resolve);
        let out = acc.update(&frame(1_000_000, 2048), resolve);
        assert_eq!(of(&out, "Safari#100").rx, 1_000_000);
        assert_eq!(of(&out, "mihomo#9").rx, 2048);
    }

    /// 认不出转发进程时，隧道侧一律不算 —— 宁可少算，也不能让总量翻倍。
    #[test]
    fn tun_without_identifiable_owner_does_not_double_count() {
        let mut acc = FlowAccounting::new();
        let frame = |tun: u64, other: u64| {
            vec![
                row(
                    100,
                    "Safari",
                    "utun3",
                    "192.168.1.10",
                    52344,
                    "93.184.216.34",
                    443,
                    tun,
                    0,
                ),
                row(
                    7,
                    "Other",
                    "en7",
                    "192.168.1.10",
                    50000,
                    "1.1.1.1",
                    443,
                    other,
                    0,
                ),
            ]
        };
        acc.update(&frame(0, 0), resolve);
        let out = acc.update(&frame(1_000_000, 500), resolve);
        assert_eq!(of(&out, "Safari#100"), Bytes::default());
        assert_eq!(of(&out, "Other#7").rx, 500);
    }

    /// 隧道上只有几十字节的多播噪声时不要乱认属主。
    #[test]
    fn tun_noise_below_threshold_is_ignored() {
        let mut acc = FlowAccounting::new();
        let frame = |n: u64| {
            vec![
                row(
                    100,
                    "Safari",
                    "utun3",
                    "0.0.0.0",
                    5353,
                    "224.0.0.251",
                    5353,
                    n,
                    0,
                ),
                row(
                    9,
                    "mihomo",
                    "en7",
                    "192.168.1.10",
                    53000,
                    "1.1.1.1",
                    443,
                    n,
                    0,
                ),
            ]
        };
        acc.update(&frame(0), resolve);
        let out = acc.update(&frame(100), resolve);
        assert!(out.iter().all(|(_, b)| b.rx <= 100), "{out:?}");
    }
}
