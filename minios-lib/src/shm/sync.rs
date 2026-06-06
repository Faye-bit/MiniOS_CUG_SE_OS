//! 跨进程同步原语封装。
//!
//! 封装 POSIX 线程库的跨进程互斥锁和命名信号量，
//! 用于协调服务端和客户端对共享内存区域的并发访问。
//!
//! ## 平台要求
//! 仅在 Linux 上使用，依赖 `PTHREAD_PROCESS_SHARED` 和 `sem_open`。

use crate::common::error::{MiniosError, MiniosResult};
use std::ffi::CString;

/// 跨进程互斥锁。
///
/// 位于共享内存区域内，可由多个进程同时锁定。
/// 初始化时设置 `PTHREAD_PROCESS_SHARED` 属性。
#[derive(Debug)]
pub struct ShmMutex {
    /// 指向共享内存中 pthread_mutex_t 的指针
    ptr: *mut libc::pthread_mutex_t,
}

// pthread_mutex_t 通过指针位于共享内存中，跨进程共享是安全的
unsafe impl Send for ShmMutex {}
unsafe impl Sync for ShmMutex {}

impl ShmMutex {
    /// 在指定内存地址处初始化跨进程共享的互斥锁。
    ///
    /// # Safety
    /// - `ptr` 必须指向共享内存区域中足够大的有效地址（至少 `sizeof(pthread_mutex_t)` 字节）
    /// - 每个互斥锁只能初始化一次
    pub unsafe fn init_at(ptr: *mut u8) -> MiniosResult<Self> {
        let mutex_ptr = ptr as *mut libc::pthread_mutex_t;

        let mut attr: libc::pthread_mutexattr_t = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::pthread_mutexattr_init(&mut attr) };
        if ret != 0 {
            return Err(MiniosError::ShmError(format!(
                "pthread_mutexattr_init failed: {ret}"
            )));
        }

        let ret = unsafe {
            libc::pthread_mutexattr_setpshared(&mut attr, libc::PTHREAD_PROCESS_SHARED)
        };
        if ret != 0 {
            unsafe { libc::pthread_mutexattr_destroy(&mut attr) };
            return Err(MiniosError::ShmError(format!(
                "pthread_mutexattr_setpshared failed: {ret}"
            )));
        }

        let ret = unsafe { libc::pthread_mutex_init(mutex_ptr, &attr) };
        unsafe { libc::pthread_mutexattr_destroy(&mut attr) };
        if ret != 0 {
            return Err(MiniosError::ShmError(format!(
                "pthread_mutex_init failed: {ret}"
            )));
        }

        Ok(Self { ptr: mutex_ptr })
    }

    /// 从已初始化的共享内存地址打开互斥锁（不重新初始化）。
    ///
    /// # Safety
    /// - `ptr` 必须指向已通过 `init_at` 初始化过的 `pthread_mutex_t`
    pub unsafe fn open_at(ptr: *mut u8) -> Self {
        Self {
            ptr: ptr as *mut libc::pthread_mutex_t,
        }
    }

    /// 锁定互斥锁。
    pub fn lock(&self) -> MiniosResult<()> {
        let ret = unsafe { libc::pthread_mutex_lock(self.ptr) };
        if ret != 0 {
            Err(MiniosError::ShmError(format!("pthread_mutex_lock failed: {ret}")))
        } else {
            Ok(())
        }
    }

    /// 解锁互斥锁。
    pub fn unlock(&self) -> MiniosResult<()> {
        let ret = unsafe { libc::pthread_mutex_unlock(self.ptr) };
        if ret != 0 {
            Err(MiniosError::ShmError(format!("pthread_mutex_unlock failed: {ret}")))
        } else {
            Ok(())
        }
    }

    /// 销毁互斥锁。
    ///
    /// # Safety
    /// 仅在互斥锁不再被任何进程使用时调用。
    pub unsafe fn destroy(&self) -> MiniosResult<()> {
        let ret = unsafe { libc::pthread_mutex_destroy(self.ptr) };
        if ret != 0 {
            Err(MiniosError::ShmError(format!("pthread_mutex_destroy failed: {ret}")))
        } else {
            Ok(())
        }
    }
}

/// 命名 POSIX 信号量。
///
/// 用于服务端与客户端之间的请求/响应通知。
/// 命名信号量在 `/dev/shm` 下有对应的文件节点，
/// 进程异常退出时可能残留，需要清理逻辑。
#[derive(Debug)]
pub struct ShmSemaphore {
    /// sem_open 返回的信号量指针
    sem: *mut libc::sem_t,
    /// 信号量名称（用于 sem_close 和 sem_unlink）
    name: CString,
}

// sem_t 内部有同步机制，可以跨线程/进程使用
unsafe impl Send for ShmSemaphore {}
unsafe impl Sync for ShmSemaphore {}

impl ShmSemaphore {
    /// 创建一个命名信号量（如果已存在则打开）。
    ///
    /// `name` 是信号量名称（不含 `/` 前缀），例如 `"minios_server_sem"`。
    /// `value` 是初始值。
    pub fn create(name: &str, value: u32) -> MiniosResult<Self> {
        let cname = CString::new(name).map_err(|e| {
            MiniosError::ShmError(format!("invalid semaphore name '{name}': {e}"))
        })?;
        // 尝试创建信号量，如果已存在则打开
        let sem = unsafe {
            libc::sem_open(
                cname.as_ptr(),
                libc::O_CREAT | libc::O_RDWR,
                0o600,
                value,
            )
        };

        if sem == libc::SEM_FAILED {
            let err = std::io::Error::last_os_error();
            // 如果 sem_open 失败，尝试先 unlink 再创建
            unsafe { libc::sem_unlink(cname.as_ptr()) };
            let sem = unsafe {
                libc::sem_open(
                    cname.as_ptr(),
                    libc::O_CREAT | libc::O_RDWR,
                    0o600,
                    value,
                )
            };
            if sem == libc::SEM_FAILED {
                return Err(MiniosError::ShmError(format!(
                    "sem_open('{name}') failed after unlink: {err}"
                )));
            }
            return Ok(Self { sem, name: cname });
        }

        Ok(Self { sem, name: cname })
    }

    /// 打开一个已存在的命名信号量。
    pub fn open(name: &str) -> MiniosResult<Self> {
        let cname = CString::new(name).map_err(|e| {
            MiniosError::ShmError(format!("invalid semaphore name '{name}': {e}"))
        })?;
        // 只打开已存在的信号量，不创建
        let sem = unsafe { libc::sem_open(cname.as_ptr(), 0) };
        if sem == libc::SEM_FAILED {
            let err = std::io::Error::last_os_error();
            return Err(MiniosError::ShmError(format!(
                "sem_open('{name}') failed: {err}"
            )));
        }

        Ok(Self { sem, name: cname })
    }

    /// 等待信号量（P 操作，减 1）。
    /// 如果当前值为 0 则阻塞。
    pub fn wait(&self) -> MiniosResult<()> {
        let ret = unsafe { libc::sem_wait(self.sem) }; // 阻塞等待直到 sem 的值 > 0，然后减 1
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            Err(MiniosError::ShmError(format!("sem_wait failed: {err}")))
        } else {
            Ok(())
        }
    }

    /// 非阻塞尝试等待信号量。
    /// 如果当前值为 0 则立即返回错误。
    pub fn try_wait(&self) -> MiniosResult<()> {
        let ret = unsafe { libc::sem_trywait(self.sem) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EAGAIN) {
                Err(MiniosError::ShmError("semaphore would block".into()))
            } else {
                Err(MiniosError::ShmError(format!("sem_trywait failed: {err}")))
            }
        } else {
            Ok(())
        }
    }

    /// 带超时的信号量等待（P 操作）。
    ///
    /// 在指定毫秒内等待信号量，超时返回 `ShmError("timeout")`。
    /// 用于队列监听线程定期检查 shutdown 标志。
    ///
    /// ## 平台实现
    /// - Linux：使用 `sem_timedwait`（高效，内核级超时）
    /// - macOS：使用 `try_wait` 轮询（10ms 步进），macOS 不完全支持 POSIX 信号量
    pub fn timed_wait_ms(&self, timeout_ms: u32) -> MiniosResult<()> {
        #[cfg(target_os = "linux")]
        {
            // Linux：使用真正的 sem_timedwait
            extern "C" {
                fn sem_timedwait(
                    sem: *mut libc::sem_t,
                    abs_timeout: *const libc::timespec,
                ) -> libc::c_int;
            }

            let mut now = unsafe { std::mem::zeroed::<libc::timespec>() };
            let ret = unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut now) };
            if ret != 0 {
                let err = std::io::Error::last_os_error();
                return Err(MiniosError::ShmError(format!(
                    "clock_gettime failed: {err}"
                )));
            }

            let total_nsec = now.tv_nsec as i64 + (timeout_ms as i64) * 1_000_000;
            let abs_sec = now.tv_sec as i64 + total_nsec / 1_000_000_000;
            let abs_nsec = (total_nsec % 1_000_000_000) as libc::c_long;

            let ts = libc::timespec {
                tv_sec: abs_sec as libc::time_t,
                tv_nsec: abs_nsec,
            };

            loop {
                let ret = unsafe { sem_timedwait(self.sem, &ts) };
                if ret == 0 {
                    return Ok(());
                }
                let err = std::io::Error::last_os_error();
                let code = err.raw_os_error();
                if code == Some(libc::ETIMEDOUT) {
                    return Err(MiniosError::ShmError("timeout".into()));
                } else if code == Some(libc::EINTR) {
                    continue;
                } else {
                    return Err(MiniosError::ShmError(format!(
                        "sem_timedwait failed: {err}"
                    )));
                }
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            // macOS 等：使用 try_wait 轮询
            let step_ms: u32 = 10;
            let mut elapsed: u32 = 0;
            while elapsed < timeout_ms {
                if self.try_wait().is_ok() {
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(step_ms as u64));
                elapsed += step_ms;
            }
            Err(MiniosError::ShmError("timeout".into()))
        }
    }

    /// 发送信号量（V 操作，加 1）。
    pub fn post(&self) -> MiniosResult<()> {
        let ret = unsafe { libc::sem_post(self.sem) }; // 将 sem 的值加 1，唤醒正在等待的线程/进程
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            Err(MiniosError::ShmError(format!("sem_post failed: {err}")))
        } else {
            Ok(())
        }
    }

    /// 关闭信号量并删除其名称（清理资源）。
    pub fn close_and_unlink(self) -> MiniosResult<()> {
        let ret = unsafe { libc::sem_close(self.sem) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            return Err(MiniosError::ShmError(format!("sem_close failed: {err}")));
        }
        let ret = unsafe { libc::sem_unlink(self.name.as_ptr()) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            // sem_unlink 失败不影响功能（信号量仍在使用时可能发生）
            log::warn!("sem_unlink('{}') failed: {err}", self.name.to_string_lossy());
        }
        Ok(())
    }

    /// 只关闭不删除名称（客户端使用）。
    pub fn close(self) -> MiniosResult<()> {
        let ret = unsafe { libc::sem_close(self.sem) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            return Err(MiniosError::ShmError(format!("sem_close failed: {err}")));
        }
        Ok(())
    }
}

/// 信号量名称常量
pub mod names {
    pub const SERVER_SEM: &str = "minios_server_sem";
    pub const CLIENT_SEM: &str = "minios_client_sem";
}

// ─── 单元测试 ───

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{alloc_zeroed, dealloc, Layout};

    fn alloc_mutex_mem() -> (*mut u8, Layout) {
        let layout = Layout::from_size_align(
            std::mem::size_of::<libc::pthread_mutex_t>(),
            8,
        )
        .unwrap();
        let ptr = unsafe { alloc_zeroed(layout) };
        (ptr, layout)
    }

    #[test]
    fn test_mutex_lock_unlock() {
        let (ptr, layout) = alloc_mutex_mem();
        let mutex = unsafe { ShmMutex::init_at(ptr).unwrap() };
        mutex.lock().unwrap();
        mutex.unlock().unwrap();
        unsafe {
            mutex.destroy().unwrap();
            dealloc(ptr, layout);
        }
    }

    #[test]
    fn test_semaphore_create_wait_post() {
        let sem = ShmSemaphore::create("minios_test_sem", 1).unwrap();
        sem.wait().unwrap(); // 消费掉初始值
        sem.post().unwrap(); // 加回来
        sem.wait().unwrap();
        sem.close_and_unlink().unwrap();
    }

    #[test]
    fn test_semaphore_try_wait() {
        let sem = ShmSemaphore::create("minios_test_try_sem", 0).unwrap();
        assert!(sem.try_wait().is_err()); // 值为 0，不应能拿到
        sem.post().unwrap();
        sem.try_wait().unwrap(); // 现在可以了
        sem.close_and_unlink().unwrap();
    }
}
