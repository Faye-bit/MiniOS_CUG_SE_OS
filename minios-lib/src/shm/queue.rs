//! 共享内存请求/响应队列 — 基于信号量的有界缓冲区。
//!
//! ## 概述
//!
//! 本模块在共享内存控制页中实现了一个**有界缓冲区（Bounded Buffer）**，
//! 用于客户端和服务端之间的命令/响应的进程间通信。
//!
//! ## 设计
//!
//! 使用 **4 个 POSIX 命名信号量** 和 **2 个跨进程互斥锁** 实现经典的生产者-消费者模式：
//!
//! | 信号量       | 初值 | 语义               | 谁 P (wait)   | 谁 V (post)   |
//! |-------------|------|-------------------|---------------|---------------|
//! | req_empty   | N    | 请求队列中空闲槽位数 | 客户端（发送前）| 服务端（取出后）|
//! | req_full    | 0    | 请求队列中就绪槽位数 | 服务端（取出前）| 客户端（发送后）|
//! | resp_empty  | N    | 响应队列中空闲槽位数 | 服务端（发送前）| 客户端（取出后）|
//! | resp_full   | 0    | 响应队列中就绪槽位数 | 客户端（取出前）| 服务端（发送后）|
//!
//! 这是 Dijkstra (1965) 经典论文中描述的**生产者-消费者问题**的完整实现，
//! 直接对应操作系统教材中"用信号量实现进程间同步"的核心例题。
//!
//! ## 控制页中的布局
//!
//! 队列结构位于控制页中页互斥锁之后：
//! ```text
//! Page 0 (4096 B):
//!   ShmControlHeader (28 B)
//!   Page Bitmap       (variable)
//!   page_mutex        (pthread_mutex_t, ~40-64 B)
//!   -- queue area --
//!   ShmQueueHeader    (32 B)
//!   req_mutex         (pthread_mutex_t)
//!   resp_mutex        (pthread_mutex_t)
//!   QueueRequest[0..N-1]  (N * 256 B)
//!   QueueResponse[0..N-1] (N * 256 B)
//! ```

use crate::common::consts;
use crate::common::error::{MiniosError, MiniosResult};
use crate::shm::region::ShmControlHeader;
use crate::shm::sync::{ShmMutex, ShmSemaphore};
use std::sync::atomic::{AtomicBool, Ordering};

// ═══════════════════════════════════════════════════════════════════════
// 队列头结构（控制页内，32 bytes, #[repr(C)]）
// ═══════════════════════════════════════════════════════════════════════

/// 共享内存中的队列控制头。
///
/// 紧接在页分配互斥锁之后，记录请求和响应环形缓冲区的元数据。
/// `head` 是生产者写入的下一个位置，`tail` 是消费者读取的下一个位置。
/// 当 `head == tail` 时环形缓冲区为空。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ShmQueueHeader {
    /// 魔数 "MOSQ"
    pub magic: [u8; 4],
    /// 格式版本，当前为 1
    pub version: u32,
    /// 请求和响应方向的槽位数（总共 N 个请求槽 + N 个响应槽）
    pub num_slots: u32,
    /// 请求队列生产者索引（客户端写入后递增）
    pub req_head: u32,
    /// 请求队列消费者索引（服务端读取后递增）
    pub req_tail: u32,
    /// 响应队列生产者索引（服务端写入后递增）
    pub resp_head: u32,
    /// 响应队列消费者索引（客户端读取后递增）
    pub resp_tail: u32,
    /// 保留，填充至 32 字节
    pub _reserved: u32,
}

// 编译期大小检查
const _QH_SIZE: () = assert!(std::mem::size_of::<ShmQueueHeader>() == 32);

impl ShmQueueHeader {
    /// 创建新的队列头，初始时所有 head/tail 均为 0。
    pub const fn new(num_slots: u32) -> Self {
        Self {
            magic: consts::QUEUE_MAGIC,
            version: 1,
            num_slots,
            req_head: 0,
            req_tail: 0,
            resp_head: 0,
            resp_tail: 0,
            _reserved: 0,
        }
    }

    /// 验证队列头魔数和版本。
    pub fn validate(&self) -> MiniosResult<()> {
        if self.magic != consts::QUEUE_MAGIC {
            return Err(MiniosError::ShmError(format!(
                "bad queue magic: expected {:?}, got {:?}",
                consts::QUEUE_MAGIC,
                self.magic
            )));
        }
        if self.version != 1 {
            return Err(MiniosError::ShmError(format!(
                "unsupported queue version: {}",
                self.version
            )));
        }
        Ok(())
    }

}

// ═══════════════════════════════════════════════════════════════════════
// 队列槽位类型（各 256 bytes, #[repr(C)]）
// ═══════════════════════════════════════════════════════════════════════

/// 请求槽位（256 bytes）。
///
/// 客户端通过此结构将命令文本传递给服务端。
/// 与 socket 协议完全兼容——`command_text` 字段承载相同的空格分隔文本命令。
///
/// 内存布局：
/// ```text
///   0..4   client_id     u32         发送方 PID
///   4      status        u8          0=空闲, 1=就绪
///   5..7   _pad          [u8; 3]     对齐填充
///   8..255 command_text  [u8; 248]   null-terminated 命令文本
/// ```
#[repr(C)]
#[derive(Debug, Clone)]
pub struct QueueRequest {
    pub client_id: u32,
    pub status: u8,
    pub _pad: [u8; 3],
    pub command_text: [u8; 248],
}

const _QR_SIZE: () = assert!(std::mem::size_of::<QueueRequest>() == 256);

/// 槽位状态常量。
pub mod req_status {
    pub const FREE: u8 = 0;
    pub const READY: u8 = 1;
}

impl QueueRequest {
    /// 创建一个空闲槽位。
    pub const fn empty() -> Self {
        Self {
            client_id: 0,
            status: req_status::FREE,
            _pad: [0u8; 3],
            command_text: [0u8; 248],
        }
    }

    /// 打包命令文本到槽位中。自动截断超长命令（248 字节以内）。
    pub fn pack(client_id: u32, cmd: &str) -> Self {
        let mut command_text = [0u8; 248];
        let bytes = cmd.as_bytes();
        let len = bytes.len().min(247); // 保留 1 字节给 null terminator
        command_text[..len].copy_from_slice(&bytes[..len]);

        Self {
            client_id,
            status: req_status::READY,
            _pad: [0u8; 3],
            command_text,
        }
    }

    /// 提取命令文本（到第一个 null 字节或末尾）。
    pub fn command_str(&self) -> &str {
        fixed_str(&self.command_text)
    }
}

/// 响应槽位（256 bytes）。
///
/// 服务端通过此结构将响应文本返回给客户端。
///
/// 内存布局：
/// ```text
///   0..4   client_id      u32         目标客户端 PID（回显）
///   4      status         u8          0=空闲, 1=就绪
///   5..7   _pad           [u8; 3]     对齐填充
///   8..255 response_text  [u8; 248]   null-terminated 响应文本
/// ```
#[repr(C)]
#[derive(Debug, Clone)]
pub struct QueueResponse {
    pub client_id: u32,
    pub status: u8,
    pub _pad: [u8; 3],
    pub response_text: [u8; 248],
}

const _QRS_SIZE: () = assert!(std::mem::size_of::<QueueResponse>() == 256);

/// 响应槽位状态常量（复用 req_status 的值）。
pub mod resp_status {
    pub const FREE: u8 = 0;
    pub const READY: u8 = 1;
}

impl QueueResponse {
    /// 创建一个空闲槽位。
    pub const fn empty() -> Self {
        Self {
            client_id: 0,
            status: resp_status::FREE,
            _pad: [0u8; 3],
            response_text: [0u8; 248],
        }
    }

    /// 打包响应文本到槽位中。
    pub fn pack(client_id: u32, resp: &str) -> Self {
        let mut response_text = [0u8; 248];
        let bytes = resp.as_bytes();
        let len = bytes.len().min(247);
        response_text[..len].copy_from_slice(&bytes[..len]);

        Self {
            client_id,
            status: resp_status::READY,
            _pad: [0u8; 3],
            response_text,
        }
    }

    /// 提取响应文本。
    pub fn response_str(&self) -> &str {
        fixed_str(&self.response_text)
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 偏移量计算
// ═══════════════════════════════════════════════════════════════════════

/// 计算页互斥锁结束位置（从控制页起点算起的字节偏移）。
///
/// 队列结构从此位置开始放置。计算逻辑与 `ShmRegion::create()` 中一致。
pub fn page_mutex_end(header: &ShmControlHeader) -> usize {
    let raw = header.page_bitmap_offset as usize + header.page_bitmap_size as usize;
    let align = std::mem::align_of::<libc::pthread_mutex_t>();
    let mutex_off = (raw + align - 1) / align * align;
    mutex_off + std::mem::size_of::<libc::pthread_mutex_t>()
}

/// 计算队列头在控制页中的偏移量。
fn queue_header_offset(header: &ShmControlHeader) -> usize {
    let raw = page_mutex_end(header);
    (raw + 7) / 8 * 8 // 8 字节对齐
}

/// 计算请求互斥锁在控制页中的偏移量。
fn req_mutex_offset(header: &ShmControlHeader) -> usize {
    let raw = queue_header_offset(header) + std::mem::size_of::<ShmQueueHeader>();
    let align = std::mem::align_of::<libc::pthread_mutex_t>();
    (raw + align - 1) / align * align
}

/// 计算响应互斥锁在控制页中的偏移量。
fn resp_mutex_offset(header: &ShmControlHeader) -> usize {
    let raw = req_mutex_offset(header) + std::mem::size_of::<libc::pthread_mutex_t>();
    let align = std::mem::align_of::<libc::pthread_mutex_t>();
    (raw + align - 1) / align * align
}

/// 计算请求槽位数组在控制页中的偏移量。
fn req_slots_offset(header: &ShmControlHeader) -> usize {
    let raw = resp_mutex_offset(header) + std::mem::size_of::<libc::pthread_mutex_t>();
    (raw + 7) / 8 * 8 // 8 字节对齐
}

/// 计算响应槽位数组在控制页中的偏移量。
fn resp_slots_offset(header: &ShmControlHeader, num_slots: u32) -> usize {
    req_slots_offset(header) + num_slots as usize * consts::QUEUE_SLOT_SIZE as usize
}

/// 计算整个队列区占用的总字节数。
fn total_queue_size(header: &ShmControlHeader, num_slots: u32) -> usize {
    resp_slots_offset(header, num_slots) + num_slots as usize * consts::QUEUE_SLOT_SIZE as usize
        - queue_header_offset(header)
}

/// 返回可放置的最大槽位数（受控制页空闲空间限制，上限 7）。
pub fn max_slots_for(header: &ShmControlHeader) -> u32 {
    let available = consts::SHM_PAGE_SIZE as usize - page_mutex_end(header);
    let overhead = std::mem::size_of::<ShmQueueHeader>()
        + 2 * std::mem::size_of::<libc::pthread_mutex_t>();
    // 每个槽位 = 1 个 QueueRequest + 1 个 QueueResponse = 512 B
    let max_by_space = ((available.saturating_sub(overhead)) / 512) as u32;
    max_by_space.min(7)
}

// ═══════════════════════════════════════════════════════════════════════
// ShmQueue — 跨进程共享请求/响应队列
// ═══════════════════════════════════════════════════════════════════════

/// 跨进程共享的请求/响应队列。
///
/// ## 生命周期
///
/// - **服务端**：`ShmQueue::create()` → 使用 → `destroy()`（会 unlink 信号量）
/// - **客户端**：`ShmQueue::open()` → 使用 → `close()`（只 close 信号量，不 unlink）
///
/// ## 线程安全
///
/// 内部使用跨进程互斥锁保护队列指针，信号量保护槽位计数。
/// 同一 `ShmQueue` 实例不能在多个线程中同时使用（`!Sync`），
/// 但可通过外部 `Arc<Mutex<ShmQueue>>` 或各线程持有各自的 `ShmQueue` 实例来实现并发。
pub struct ShmQueue {
    /// 指向控制页中 ShmQueueHeader 的指针
    header: *mut ShmQueueHeader,
    /// 请求缓冲区互斥锁（保护 req_head 和槽位访问）
    req_mutex: ShmMutex,
    /// 响应缓冲区互斥锁（保护 resp_head 和槽位访问）
    resp_mutex: ShmMutex,
    /// 请求槽位数组指针
    req_slots: *mut QueueRequest,
    /// 响应槽位数组指针
    resp_slots: *mut QueueResponse,
    /// 请求空闲槽位信号量（初值 = N）
    req_empty: ShmSemaphore,
    /// 请求就绪槽位信号量（初值 = 0）
    req_full: ShmSemaphore,
    /// 响应空闲槽位信号量（初值 = N）
    resp_empty: ShmSemaphore,
    /// 响应就绪槽位信号量（初值 = 0）
    resp_full: ShmSemaphore,
    /// 槽位数
    num_slots: u32,
    /// 是否为创建者（决定 destroy 时是否 unlink 信号量）
    is_creator: bool,
}

// 内部指针指向共享内存，跨线程传递和共享安全
// SAFETY: 所有 *mut 指针指向共享内存区域（mmap'd SHM），
// 该内存在多线程/多进程间共享。信号量和互斥锁内部使用
// pthread 同步原语，保证线程安全。
unsafe impl Send for ShmQueue {}
unsafe impl Sync for ShmQueue {}

impl ShmQueue {
    // ── 生命周期 ──

    /// 服务端：在共享内存控制页中创建队列。
    ///
    /// 初始化 ShmQueueHeader、互斥锁、信号量和槽位内存。
    /// 控制页基址通过 `region.data_area()` 回退一个页得到。
    ///
    /// # 参数
    /// - `ctrl_page`: 控制页基址指针（`data_area() - PAGE_SIZE`）
    /// - `header_ref`: 共享内存控制头引用（用于计算偏移量）
    /// - `shm_name`: 共享内存名称（用于派生信号量名称）
    /// - `num_slots`: 请求/响应槽位数（1~7）
    pub fn create(
        ctrl_page: *mut u8,
        header_ref: &ShmControlHeader,
        shm_name: &str,
        num_slots: u32,
    ) -> MiniosResult<Self> {
        assert!(num_slots >= 1 && num_slots <= 7, "num_slots must be 1..7");

        let qh_off = queue_header_offset(header_ref);
        let rm_off = req_mutex_offset(header_ref);
        let rpm_off = resp_mutex_offset(header_ref);
        let rs_off = req_slots_offset(header_ref);
        let rps_off = resp_slots_offset(header_ref, num_slots);
        let total = total_queue_size(header_ref, num_slots);

        // 安全检查：队列区不能超出控制页
        assert!(
            qh_off + total <= consts::SHM_PAGE_SIZE as usize,
            "queue area overflow: {}+{} > 4096",
            qh_off,
            total
        );

        // ── 写入队列头 ──
        let header_ptr = unsafe { ctrl_page.add(qh_off) as *mut ShmQueueHeader };
        unsafe {
            std::ptr::write(header_ptr, ShmQueueHeader::new(num_slots));
        }

        // ── 初始化互斥锁 ──
        let req_mutex = unsafe { ShmMutex::init_at(ctrl_page.add(rm_off))? };
        let resp_mutex = unsafe { ShmMutex::init_at(ctrl_page.add(rpm_off))? };

        // ── 初始化槽位内存为零 ──
        let req_slots = unsafe { ctrl_page.add(rs_off) as *mut QueueRequest };
        let resp_slots = unsafe { ctrl_page.add(rps_off) as *mut QueueResponse };
        unsafe {
            std::ptr::write_bytes(req_slots, 0, num_slots as usize);
            std::ptr::write_bytes(resp_slots, 0, num_slots as usize);
        }

        // ── 创建信号量 ──
        let req_empty =
            ShmSemaphore::create(&sem_name(shm_name, "req_empty"), num_slots)?;
        let req_full =
            ShmSemaphore::create(&sem_name(shm_name, "req_full"), 0)?;
        let resp_empty =
            ShmSemaphore::create(&sem_name(shm_name, "resp_empty"), num_slots)?;
        let resp_full =
            ShmSemaphore::create(&sem_name(shm_name, "resp_full"), 0)?;

        Ok(Self {
            header: header_ptr,
            req_mutex,
            resp_mutex,
            req_slots,
            resp_slots,
            req_empty,
            req_full,
            resp_empty,
            resp_full,
            num_slots,
            is_creator: true,
        })
    }

    /// 客户端：打开已存在的队列。
    ///
    /// 验证 ShmQueueHeader 魔数并打开信号量和互斥锁（不重新初始化）。
    pub fn open(
        ctrl_page: *mut u8,
        header_ref: &ShmControlHeader,
        shm_name: &str,
    ) -> MiniosResult<Self> {
        let qh_off = queue_header_offset(header_ref);
        let rm_off = req_mutex_offset(header_ref);
        let rpm_off = resp_mutex_offset(header_ref);
        let rs_off = req_slots_offset(header_ref);

        // 读取并验证队列头
        let header_ptr = unsafe { ctrl_page.add(qh_off) as *const ShmQueueHeader };
        let header = unsafe { &*header_ptr };
        header.validate()?;

        let num_slots = header.num_slots;
        let rps_off = resp_slots_offset(header_ref, num_slots);

        // 打开互斥锁（不重新初始化）
        let req_mutex = unsafe { ShmMutex::open_at(ctrl_page.add(rm_off)) };
        let resp_mutex = unsafe { ShmMutex::open_at(ctrl_page.add(rpm_off)) };

        // 槽位指针
        let req_slots = unsafe { ctrl_page.add(rs_off) as *mut QueueRequest };
        let resp_slots = unsafe { ctrl_page.add(rps_off) as *mut QueueResponse };

        // 打开信号量（打开已存在的）
        let req_empty = ShmSemaphore::open(&sem_name(shm_name, "req_empty"))?;
        let req_full = ShmSemaphore::open(&sem_name(shm_name, "req_full"))?;
        let resp_empty = ShmSemaphore::open(&sem_name(shm_name, "resp_empty"))?;
        let resp_full = ShmSemaphore::open(&sem_name(shm_name, "resp_full"))?;

        Ok(Self {
            header: header_ptr as *mut ShmQueueHeader,
            req_mutex,
            resp_mutex,
            req_slots,
            resp_slots,
            req_empty,
            req_full,
            resp_empty,
            resp_full,
            num_slots,
            is_creator: false,
        })
    }

    /// 检查队列是否已初始化（通过魔数判断）。
    ///
    /// 客户端在尝试使用队列前调用此方法判断队列是否可用。
    pub fn is_available(ctrl_page: *mut u8, header_ref: &ShmControlHeader) -> bool {
        let qh_off = queue_header_offset(header_ref);
        let header_ptr = unsafe { ctrl_page.add(qh_off) as *const ShmQueueHeader };
        let header = unsafe { &*header_ptr };
        header.magic == consts::QUEUE_MAGIC
    }

    /// 销毁队列（仅创建者调用）。
    ///
    /// 销毁互斥锁、关闭并 unlink 信号量。
    /// 注意：共享内存本身由 ShmRegion 管理，此方法不释放 SHM。
    pub fn destroy(mut self) -> MiniosResult<()> {
        unsafe {
            self.req_mutex.destroy()?;
            self.resp_mutex.destroy()?;
        }
        // 清零队列头
        unsafe {
            std::ptr::write(self.header, ShmQueueHeader::new(0));
        }
        self.req_empty.close_and_unlink()?;
        self.req_full.close_and_unlink()?;
        self.resp_empty.close_and_unlink()?;
        self.resp_full.close_and_unlink()?;
        self.is_creator = false;
        Ok(())
    }

    /// 关闭队列（客户端调用，不 unlink 信号量）。
    pub fn close(self) -> MiniosResult<()> {
        self.req_empty.close()?;
        self.req_full.close()?;
        self.resp_empty.close()?;
        self.resp_full.close()?;
        Ok(())
    }

    // ── 生产者/消费者操作 ──

    /// 客户端：将命令文本推入请求队列（生产者）。
    ///
    /// 阻塞直到有空闲请求槽位。使用 `wait` → `lock` → `write` → `unlock` → `post` 模式。
    pub fn push_request(&self, client_id: u32, command: &str) -> MiniosResult<()> {
        // P(req_empty) —— 等待有空槽位
        self.req_empty.wait()?;

        // 加锁，写入槽位，推进 head
        self.req_mutex.lock()?;
        let idx = unsafe { (*self.header).req_head % self.num_slots };
        let slot = unsafe { self.req_slots.add(idx as usize) };
        unsafe { std::ptr::write(slot, QueueRequest::pack(client_id, command)) };
        unsafe { (*self.header).req_head = (*self.header).req_head.wrapping_add(1) };
        self.req_mutex.unlock()?;

        // V(req_full) —— 通知有新请求
        self.req_full.post()?;
        Ok(())
    }

    /// 服务端：从请求队列取出命令（消费者）。
    ///
    /// 使用 `timed_wait_ms` 以便定期检查 `shutdown_flag`。
    /// 返回 `Err("timeout")` 时调用方应检查 shutdown 标志后重试。
    ///
    /// 使用 `wait` → `lock` → `read` → `unlock` → `post` 模式。
    pub fn pop_request(&self, timeout_ms: u32, shutdown: &AtomicBool) -> MiniosResult<QueueRequest> {
        // P(req_full) —— 等待有就绪请求（带超时以检查 shutdown）
        loop {
            match self.req_full.timed_wait_ms(timeout_ms) {
                Ok(()) => break,
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("timeout") {
                        if shutdown.load(Ordering::SeqCst) {
                            return Err(MiniosError::ShmError("shutdown".into()));
                        }
                        // 超时但未 shutdown —— 继续等待
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        // 加锁，读取槽位，推进 tail
        self.req_mutex.lock()?;
        let idx = unsafe { (*self.header).req_tail % self.num_slots };
        let slot = unsafe { self.req_slots.add(idx as usize) };
        let request = unsafe { std::ptr::read(slot) };
        // 清空槽位
        unsafe { std::ptr::write(slot, QueueRequest::empty()) };
        unsafe { (*self.header).req_tail = (*self.header).req_tail.wrapping_add(1) };
        self.req_mutex.unlock()?;

        // V(req_empty) —— 释放一个空槽位
        self.req_empty.post()?;
        Ok(request)
    }

    /// 服务端：将响应推入响应队列（生产者）。
    ///
    /// 阻塞直到有空闲响应槽位。
    pub fn push_response(&self, client_id: u32, response: &str) -> MiniosResult<()> {
        // P(resp_empty)
        self.resp_empty.wait()?;

        self.resp_mutex.lock()?;
        let idx = unsafe { (*self.header).resp_head % self.num_slots };
        let slot = unsafe { self.resp_slots.add(idx as usize) };
        unsafe { std::ptr::write(slot, QueueResponse::pack(client_id, response)) };
        unsafe { (*self.header).resp_head = (*self.header).resp_head.wrapping_add(1) };
        self.resp_mutex.unlock()?;

        // V(resp_full)
        self.resp_full.post()?;
        Ok(())
    }

    /// 客户端：从响应队列取出响应（消费者）。
    ///
    /// 阻塞直到有就绪响应。FIFO 顺序——响应按请求的到达顺序返回。
    pub fn pop_response(&self) -> MiniosResult<QueueResponse> {
        // P(resp_full)
        self.resp_full.wait()?;

        self.resp_mutex.lock()?;
        let idx = unsafe { (*self.header).resp_tail % self.num_slots };
        let slot = unsafe { self.resp_slots.add(idx as usize) };
        let response = unsafe { std::ptr::read(slot) };
        // 清空槽位
        unsafe { std::ptr::write(slot, QueueResponse::empty()) };
        unsafe {
            (*self.header).resp_tail = (*self.header).resp_tail.wrapping_add(1);
        }
        self.resp_mutex.unlock()?;

        // V(resp_empty)
        self.resp_empty.post()?;
        Ok(response)
    }

    /// 向所有 4 个信号量各 post 一次，用于关闭时解除阻塞。
    ///
    /// 在服务端关闭流程中调用，唤醒在 `sem_wait` / `sem_timedwait` 上阻塞的线程。
    pub fn wake_all(&self) {
        let _ = self.req_empty.post();
        let _ = self.req_full.post();
        let _ = self.resp_empty.post();
        let _ = self.resp_full.post();
    }

    /// 返回槽位数。
    #[allow(dead_code)]
    pub fn slot_count(&self) -> u32 {
        self.num_slots
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 辅助函数
// ═══════════════════════════════════════════════════════════════════════

/// 从定长字节数组提取字符串（到第一个 null 字节）。
fn fixed_str(bytes: &[u8]) -> &str {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).unwrap_or("")
}

/// 根据共享内存名称生成信号量名称。
///
/// 例如 `shm_name = "/minios_shm"`, `suffix = "req_empty"` →
/// `"minios_shm_req_empty"`
fn sem_name(shm_name: &str, suffix: &str) -> String {
    let base = shm_name.trim_start_matches('/');
    format!("{base}_{suffix}")
}

// ═══════════════════════════════════════════════════════════════════════
// 单元测试
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shm::region::ShmControlHeader;

    fn make_test_header() -> ShmControlHeader {
        ShmControlHeader::new(256, 28, 32)
    }

    #[test]
    fn test_queue_header_new_and_validate() {
        let qh = ShmQueueHeader::new(4);
        assert_eq!(qh.magic, consts::QUEUE_MAGIC);
        assert_eq!(qh.version, 1);
        assert_eq!(qh.num_slots, 4);
        assert_eq!(qh.req_head, 0);
        assert_eq!(qh.req_tail, 0);
        assert!(qh.validate().is_ok());
    }

    #[test]
    fn test_queue_header_bad_magic() {
        let mut qh = ShmQueueHeader::new(4);
        qh.magic = *b"BAD!";
        assert!(qh.validate().is_err());
    }

    #[test]
    fn test_queue_request_pack_roundtrip() {
        let req = QueueRequest::pack(12345, "PUT hello.txt 100 text/plain {} 0 3");
        assert_eq!(req.client_id, 12345);
        assert_eq!(req.status, req_status::READY);
        assert_eq!(req.command_str(), "PUT hello.txt 100 text/plain {} 0 3");
    }

    #[test]
    fn test_queue_request_empty() {
        let req = QueueRequest::empty();
        assert_eq!(req.status, req_status::FREE);
        assert_eq!(req.client_id, 0);
        assert_eq!(req.command_str(), "");
    }

    #[test]
    fn test_queue_response_pack_roundtrip() {
        let resp = QueueResponse::pack(12345, "OK abcdef1234567890\n");
        assert_eq!(resp.client_id, 12345);
        assert_eq!(resp.status, resp_status::READY);
        assert_eq!(resp.response_str(), "OK abcdef1234567890\n");
    }

    #[test]
    fn test_queue_response_empty() {
        let resp = QueueResponse::empty();
        assert_eq!(resp.status, resp_status::FREE);
        assert_eq!(resp.response_str(), "");
    }

    #[test]
    fn test_queue_request_truncation() {
        let long_cmd = "X".repeat(300);
        let req = QueueRequest::pack(1, &long_cmd);
        assert!(req.command_str().len() <= 247);
    }

    #[test]
    fn test_offsets_fit_in_control_page() {
        let header = make_test_header();
        let max_slots = max_slots_for(&header);
        // 对于 256 页默认配置，应有至少 4 个槽位的空间
        assert!(max_slots >= 4, "max_slots={max_slots}, expected >= 4");
    }

    #[test]
    fn test_page_mutex_end() {
        let header = make_test_header();
        let end = page_mutex_end(&header);
        // pthread_mutex_t 大小因平台而异（Linux ~40B, macOS ~64B）
        // 验证返回值在合理范围内：> header+bitmap 且 < page_size
        assert!(end > 60, "mutex_end={end} should be > header+bitmap (60)");
        assert!(end < 4096, "mutex_end={end} should be < page_size (4096)");
    }

    #[test]
    fn test_queue_slot_sizes() {
        // QueueRequest 和 QueueResponse 各 256 bytes
        assert_eq!(std::mem::size_of::<QueueRequest>(), 256);
        assert_eq!(std::mem::size_of::<QueueResponse>(), 256);
        assert_eq!(std::mem::size_of::<ShmQueueHeader>(), 32);
    }

    #[test]
    fn test_sem_name_generation() {
        assert_eq!(sem_name("/minios_shm", "req_empty"), "minios_shm_req_empty");
        assert_eq!(sem_name("minios_shm", "req_full"), "minios_shm_req_full");
        assert_eq!(sem_name("/my_shm", "resp_empty"), "my_shm_resp_empty");
    }
}
