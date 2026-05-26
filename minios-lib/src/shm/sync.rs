//! 跨进程同步原语封装。
//!
//! 封装 POSIX 线程库的跨进程互斥锁和命名信号量，
//! 用于协调服务端和客户端对共享内存区域的并发访问。
//!
//! ## 平台要求
//! 仅在 Linux 上使用，依赖 `PTHREAD_PROCESS_SHARED` 和 `sem_open`。

use crate::common::error::{miniosError, miniosResult};
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
    pub unsafe fn init_at(ptr: *mut u8) -> miniosResult<Self> {
        let mutex_ptr = ptr as *mut libc::pthread_mutex_t;

        let mut attr: libc::pthread_mutexattr_t = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::pthread_mutexattr_init(&mut attr) };
        if ret != 0 {
            return Err(miniosError::ShmError(format!(
                "pthread_mutexattr_init failed: {ret}"
            )));
        }

        let ret = unsafe {
            libc::pthread_mutexattr_setpshared(&mut attr, libc::PTHREAD_PROCESS_SHARED)
        };
        if ret != 0 {
            unsafe { libc::pthread_mutexattr_destroy(&mut attr) };
            return Err(miniosError::ShmError(format!(
                "pthread_mutexattr_setpshared failed: {ret}"
            )));
        }

        let ret = unsafe { libc::pthread_mutex_init(mutex_ptr, &attr) };
        unsafe { libc::pthread_mutexattr_destroy(&mut attr) };
        if ret != 0 {
            return Err(miniosError::ShmError(format!(
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
    pub fn lock(&self) -> miniosResult<()> {
        let ret = unsafe { libc::pthread_mutex_lock(self.ptr) };
        if ret != 0 {
            Err(miniosError::ShmError(format!("pthread_mutex_lock failed: {ret}")))
        } else {
            Ok(())
        }
    }

    /// 解锁互斥锁。
    pub fn unlock(&self) -> miniosResult<()> {
        let ret = unsafe { libc::pthread_mutex_unlock(self.ptr) };
        if ret != 0 {
            Err(miniosError::ShmError(format!("pthread_mutex_unlock failed: {ret}")))
        } else {
            Ok(())
        }
    }

    /// 销毁互斥锁。
    ///
    /// # Safety
    /// 仅在互斥锁不再被任何进程使用时调用。
    pub unsafe fn destroy(&self) -> miniosResult<()> {
        let ret = unsafe { libc::pthread_mutex_destroy(self.ptr) };
        if ret != 0 {
            Err(miniosError::ShmError(format!("pthread_mutex_destroy failed: {ret}")))
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
    pub fn create(name: &str, value: u32) -> miniosResult<Self> {
        let cname = CString::new(name).map_err(|e| {
            miniosError::ShmError(format!("invalid semaphore name '{name}': {e}"))
        })?;

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
                return Err(miniosError::ShmError(format!(
                    "sem_open('{name}') failed after unlink: {err}"
                )));
            }
            return Ok(Self { sem, name: cname });
        }

        Ok(Self { sem, name: cname })
    }

    /// 打开一个已存在的命名信号量。
    pub fn open(name: &str) -> miniosResult<Self> {
        let cname = CString::new(name).map_err(|e| {
            miniosError::ShmError(format!("invalid semaphore name '{name}': {e}"))
        })?;

        let sem = unsafe { libc::sem_open(cname.as_ptr(), 0) };
        if sem == libc::SEM_FAILED {
            let err = std::io::Error::last_os_error();
            return Err(miniosError::ShmError(format!(
                "sem_open('{name}') failed: {err}"
            )));
        }

        Ok(Self { sem, name: cname })
    }

    /// 等待信号量（P 操作，减 1）。
    /// 如果当前值为 0 则阻塞。
    pub fn wait(&self) -> miniosResult<()> {
        let ret = unsafe { libc::sem_wait(self.sem) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            Err(miniosError::ShmError(format!("sem_wait failed: {err}")))
        } else {
            Ok(())
        }
    }

    /// 非阻塞尝试等待信号量。
    /// 如果当前值为 0 则立即返回错误。
    pub fn try_wait(&self) -> miniosResult<()> {
        let ret = unsafe { libc::sem_trywait(self.sem) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EAGAIN) {
                Err(miniosError::ShmError("semaphore would block".into()))
            } else {
                Err(miniosError::ShmError(format!("sem_trywait failed: {err}")))
            }
        } else {
            Ok(())
        }
    }

    /// 发送信号量（V 操作，加 1）。
    pub fn post(&self) -> miniosResult<()> {
        let ret = unsafe { libc::sem_post(self.sem) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            Err(miniosError::ShmError(format!("sem_post failed: {err}")))
        } else {
            Ok(())
        }
    }

    /// 关闭信号量并删除其名称（清理资源）。
    pub fn close_and_unlink(self) -> miniosResult<()> {
        let ret = unsafe { libc::sem_close(self.sem) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            return Err(miniosError::ShmError(format!("sem_close failed: {err}")));
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
    pub fn close(self) -> miniosResult<()> {
        let ret = unsafe { libc::sem_close(self.sem) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            return Err(miniosError::ShmError(format!("sem_close failed: {err}")));
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
