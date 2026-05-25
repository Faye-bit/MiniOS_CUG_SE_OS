//! 守护进程管理 — double-fork 守护进程化、PID 文件、信号处理。

use crate::common::error::{MinosError, MinosResult};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// 全局关闭标志，由 SIGTERM/SIGINT 信号处理器设置。
static SHUTDOWN_FLAG: AtomicBool = AtomicBool::new(false);

/// 检查是否收到关闭信号。
pub fn is_shutdown_requested() -> bool {
    SHUTDOWN_FLAG.load(Ordering::SeqCst)
}

/// 注册信号处理器（SIGTERM, SIGINT → 设置关闭标志）。
pub fn setup_signal_handlers() -> MinosResult<()> {
    unsafe {
        // SIGTERM
        let ret = libc::signal(libc::SIGTERM, handle_signal as *const () as libc::sighandler_t);
        if ret == libc::SIG_ERR {
            return Err(MinosError::DaemonError("failed to set SIGTERM handler".into()));
        }
        // SIGINT
        let ret = libc::signal(libc::SIGINT, handle_signal as *const () as libc::sighandler_t);
        if ret == libc::SIG_ERR {
            return Err(MinosError::DaemonError("failed to set SIGINT handler".into()));
        }
        // 忽略 SIGPIPE（防止写入已关闭的 socket 时崩溃）
        let ret = libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        if ret == libc::SIG_ERR {
            return Err(MinosError::DaemonError("failed to set SIGPIPE handler".into()));
        }
    }
    Ok(())
}

extern "C" fn handle_signal(_sig: i32) {
    SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
}

/// 将当前进程转为守护进程（double-fork 技术）。
///
/// 步骤：
/// 1. fork → 父进程退出
/// 2. 子进程 setsid → 成为新会话领导
/// 3. 再次 fork → 第一个子进程退出（确保进程不是会话领导，无法重新获取终端）
/// 4. chdir("/") → 避免占用挂载点
/// 5. 关闭标准输入输出
pub fn daemonize() -> MinosResult<()> {
    unsafe {
        // 第一次 fork
        match libc::fork() {
            -1 => {
                return Err(MinosError::DaemonError("first fork failed".into()));
            }
            0 => {
                // 子进程继续
            }
            _ => {
                // 父进程退出
                libc::_exit(0);
            }
        }

        // 创建新会话
        if libc::setsid() == -1 {
            return Err(MinosError::DaemonError("setsid failed".into()));
        }

        // 第二次 fork
        match libc::fork() {
            -1 => {
                return Err(MinosError::DaemonError("second fork failed".into()));
            }
            0 => {
                // 孙进程继续
            }
            _ => {
                libc::_exit(0);
            }
        }

        // 切换到根目录
        libc::chdir(b"/\0".as_ptr() as *const libc::c_char);

        // 重设文件创建掩码
        libc::umask(0o022);

        // 关闭标准文件描述符
        libc::close(0);
        libc::close(1);
        libc::close(2);

        // 重定向到 /dev/null
        let devnull = libc::open(
            b"/dev/null\0".as_ptr() as *const libc::c_char,
            libc::O_RDWR,
        );
        if devnull >= 0 {
            libc::dup2(devnull, 0);
            libc::dup2(devnull, 1);
            libc::dup2(devnull, 2);
            if devnull > 2 {
                libc::close(devnull);
            }
        }
    }

    Ok(())
}

/// 写入 PID 文件。
pub fn write_pidfile(path: impl AsRef<Path>) -> MinosResult<()> {
    let pid = std::process::id();
    let content = format!("{pid}\n");
    fs::write(path.as_ref(), content).map_err(|e| {
        MinosError::DaemonError(format!(
            "cannot write pidfile '{}': {}",
            path.as_ref().display(),
            e
        ))
    })?;
    Ok(())
}

/// 读取 PID 文件内容，返回 PID。
pub fn read_pidfile(path: impl AsRef<Path>) -> MinosResult<u32> {
    let mut content = String::new();
    fs::File::open(path.as_ref())
        .map_err(|e| {
            MinosError::DaemonError(format!(
                "cannot open pidfile '{}': {}",
                path.as_ref().display(),
                e
            ))
        })?
        .read_to_string(&mut content)
        .map_err(|e| MinosError::DaemonError(format!("cannot read pidfile: {e}")))?;

    content
        .trim()
        .parse()
        .map_err(|e| MinosError::DaemonError(format!("invalid pid in pidfile: {e}")))
}

/// 删除 PID 文件。
pub fn remove_pidfile(path: impl AsRef<Path>) {
    let _ = fs::remove_file(path);
}

/// 向进程发送信号。
pub fn send_signal(pid: u32, signal: i32) -> MinosResult<()> {
    let ret = unsafe { libc::kill(pid as libc::pid_t, signal) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        Err(MinosError::DaemonError(format!(
            "kill({pid}, {signal}) failed: {err}"
        )))
    } else {
        Ok(())
    }
}
