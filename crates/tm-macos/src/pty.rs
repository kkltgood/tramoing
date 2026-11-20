//! 用伪终端（pty）承载子进程的 stdout。
//!
//! # 为什么非要 pty
//!
//! `nettop` 的 stdout 接到管道时走的是 stdio 的**全缓冲**：缓冲区 4 KB，
//! 而一帧进程列表约 1 KB，于是它要攒够三四帧才 flush 一次。
//! 实测（`nettop -x -l 0 -s 1` 重定向到管道）帧间隔是 3.0 s ——
//! 数据一帧没丢，只是全被憋在 ntop 自己的用户态缓冲里。
//!
//! 接到 tty 上时 stdio 是**行缓冲**，每帧立刻出来，实测间隔 1.00 s。
//! macOS 没有 GNU `stdbuf`，SIP 也不允许对系统二进制注入
//! `DYLD_INSERT_LIBRARIES`，所以只能自己开一对 pty 把子进程的 stdout 接上去。
//!
//! # 顺带解决的两件事
//!
//! - 关闭 `OPOST`：行规程默认会把 `\n` 改写成 `\r\n`，开着会让每行多一个回车；
//! - 关闭 `ECHO`：子进程若去读 stdin，回显会把读到的内容混进输出。
//!
//! 窗口大小也不是摆设：`nettop` 会按终端宽度排版，宽度为 0 时列会挤在一起。

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};

/// 打开一对伪终端，返回 `(master, slave)`。
///
/// `slave` 交给子进程当 stdout（`Stdio::from` 即可），`master` 留在父进程读。
/// 子进程退出后 `master` 读到 EOF，读线程自然结束。
pub fn open(cols: u16, rows: u16) -> io::Result<(File, File)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    // SAFETY: 两个 fd 由 openpty 填充；name 传 null 表示不需要终端名，
    // termp 传 null 表示用系统默认的 termios（下面再按需微调）。
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    // 先交给 File 管理，后续配置失败时也会关闭两个描述符。
    // SAFETY: openpty 成功后返回两个独立且由调用方拥有的 fd。
    let master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        // SAFETY: File 在整个调用期间持有合法 fd。
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }

    // SAFETY: 上面刚拿到的两个合法 fd
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(slave.as_raw_fd(), &mut t) != 0 {
            return Err(io::Error::last_os_error());
        }
        t.c_oflag &= !libc::OPOST;
        t.c_lflag &= !libc::ECHO;
        if libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &t) != 0 {
            return Err(io::Error::last_os_error());
        }
    }

    Ok((master, slave))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn pty_descriptors_are_not_inherited_by_unrelated_children() {
        use std::os::fd::AsRawFd;
        let (master, slave) = open(120, 40).unwrap();
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            // SAFETY: the Files above own both descriptors throughout this check.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert!(flags >= 0);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
    }

    #[test]
    fn pty_roundtrip_has_no_carriage_return() {
        let (mut master, mut slave) = open(120, 40).expect("应能打开 pty");
        // 模拟子进程往 stdout 写一行：从端写、主端读
        slave.write_all(b"hello\n").expect("写入从端");

        let mut buf = [0u8; 32];
        use std::io::Read;
        let n = master.read(&mut buf).expect("从主端读取");
        // OPOST 已关闭，换行不应被改写成 "\r\n"
        assert_eq!(&buf[..n], b"hello\n");
    }
}
