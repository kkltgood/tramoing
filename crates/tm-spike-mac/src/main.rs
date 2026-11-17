//! M0 探针（macOS）：直接用真实采样器打印按速率排序的应用。
//!
//! 用法：
//!   cargo run -p tm-spike-mac                 # 打印 8 帧
//!   cargo run -p tm-spike-mac -- --frames 30
//!   cargo run -p tm-spike-mac -- --dump       # 原样输出 nettop 前 60 行（换系统版本后排查格式用）
//!   cargo run -p tm-spike-mac -- --mock       # 不碰系统，用假数据验证管道
//!
//! 注意：必须在**真终端**里跑。若被沙箱/权限拦截，nettop 会被直接 kill。

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Instant;

use tm_core::{format_speed, AppRate, RateCalculator};
use tm_macos::{NettopParser, NettopSampler};
use tm_sampler::{MockSampler, Sampler, SamplerError};

const TOP_N: usize = 12;

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let args = Args::parse();

    if args.dump {
        return dump(args.lines);
    }

    let mut sampler: Box<dyn Sampler> = if args.mock {
        Box::new(MockSampler::new())
    } else {
        match NettopSampler::spawn() {
            Ok(s) => Box::new(s),
            Err(e) => {
                eprintln!("{e}");
                eprintln!("可以先用 --mock 验证管道，或在真终端里重试。");
                return 1;
            }
        }
    };

    let mut calc = RateCalculator::new();
    let mut last = Instant::now();

    for n in 1..=args.frames {
        match sampler.sample() {
            Ok(samples) => {
                let now = Instant::now();
                let dt = now.saturating_duration_since(last);
                last = now;
                let rates = calc.update(now, &samples);
                render(n, dt, &rates, &calc);
            }
            Err(SamplerError::Timeout) => {
                eprintln!("第 {n} 帧超时，跳过");
            }
            Err(e) => {
                eprintln!("采样失败：{e}");
                return 1;
            }
        }
    }

    0
}

fn dump(lines: usize) -> i32 {
    let bin = std::env::var("TM_NETTOP_BIN").unwrap_or_else(|_| "/usr/bin/nettop".to_owned());
    let mut child = match Command::new(&bin)
        .args(["-P", "-x", "-l", "0", "-J", "bytes_in,bytes_out"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("启动 nettop 失败：{e}");
            return 1;
        }
    };

    let stdout = child.stdout.take().expect("stdout");
    let mut parser = NettopParser::new();
    let mut frames = 0usize;
    let mut count = 0usize;

    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        println!("{line}");
        count += 1;
        if parser.feed(&line).is_some() {
            frames += 1;
        }
        if count >= lines {
            break;
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = parser.flush();

    eprintln!();
    eprintln!(
        "诊断：{count} 行，识别 {frames} 帧，丢弃 {} 行。",
        parser.unparsed
    );
    0
}

struct Args {
    frames: usize,
    dump: bool,
    lines: usize,
    mock: bool,
}

impl Args {
    fn parse() -> Self {
        let mut a = Self {
            frames: 8,
            dump: false,
            lines: 60,
            mock: false,
        };
        let mut it = std::env::args().skip(1);
        while let Some(x) = it.next() {
            match x.as_str() {
                "--frames" => {
                    if let Some(v) = it.next() {
                        a.frames = v.parse().unwrap_or(8).max(1);
                    }
                }
                "--dump" => a.dump = true,
                "--lines" => {
                    if let Some(v) = it.next() {
                        a.lines = v.parse().unwrap_or(60).max(1);
                    }
                }
                "--mock" => a.mock = true,
                "-h" | "--help" => {
                    println!("用法: tm-spike-mac [--frames N] [--dump [--lines N]] [--mock]");
                    std::process::exit(0);
                }
                _ => {}
            }
        }
        a
    }
}

fn render(n: usize, dt: std::time::Duration, rates: &[AppRate], calc: &RateCalculator) {
    let (rx, tx) = calc.total_bps();
    println!();
    println!(
        "帧 {n}   间隔 {:.3}s   合计 ↓{}  ↑{}",
        dt.as_secs_f64(),
        format_speed(rx),
        format_speed(tx)
    );
    println!("{:<3} {:<40} {:>11} {:>11}", "#", "应用", "下载", "上传");
    println!("{}", "-".repeat(68));

    if rates.is_empty() {
        println!("（本帧无速率，首帧只建立基线）");
        return;
    }

    for (i, r) in rates.iter().take(TOP_N).enumerate() {
        println!(
            "{:<3} {:<40} {:>11} {:>11}",
            i + 1,
            truncate(&r.app.to_string(), 38),
            format_speed(r.rx_bps),
            format_speed(r.tx_bps)
        );
    }
    if rates.len() > TOP_N {
        println!("… 另有 {} 个应用", rates.len() - TOP_N);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
