//! # MiniOS 对象存储服务端 — 守护进程入口
//!
//! 本文件是 minios-server 的主入口，实现了对象存储服务的核心功能：
//!
//! ## 处理流程
//!
//! 1. **启动阶段**：解析参数 → 守护进程化（可选）→ 打开/创建 store.odb →
//!    创建共享内存 → 初始化缓存 → 启动 MPMC 工作线程池 → 启动 Prometheus HTTP
//!    端点 → 绑定 Socket → 进入事件循环
//! 2. **请求处理**（MPMC 模型）：
//!    - 生产者（主 accept 循环）：accept 连接 → 读取命令 →
//!      打包为 `WorkItem::Command` → 推入 crossbeam bounded channel →
//!      继续 accept 下一连接
//!    - 消费者（固定数量工作线程）：从 channel 拉取 `WorkItem` →
//!      `dispatch_command` 处理 → 写回 socket 响应 → 循环等待下一任务
//! 3. **关闭阶段**：收到停止信号 → 停止 accept → 向每个工作线程发送
//!    `WorkItem::Shutdown` → Join 所有工作线程 → 刷新存储 → 清理共享内存 →
//!    删除 PID 文件

use clap::Parser;
use crossbeam::channel::{self, Sender};
use minios_lib::cache::lru::LruCache;
use minios_lib::common::consts;
use minios_lib::daemon;
use minios_lib::metrics::MetricsRegistry;
use minios_lib::shm::page::PageAllocator;
use minios_lib::shm::region::ShmRegion;
use minios_lib::storage::engine::ObjectStore;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ═══════════════════════════════════════════════════════════════════════
// 命令行参数定义
// ═══════════════════════════════════════════════════════════════════════

/// 服务端命令行参数
#[derive(Parser, Debug)]
#[command(name = "minios-server", version, about = "minios object storage daemon")]
struct Args {
    /// 存储文件路径（默认 ./store.odb）
    #[arg(long, default_value = consts::DEFAULT_STORE_PATH)]
    store_path: String,

    /// Unix Domain Socket 路径（默认 /tmp/minios.sock）
    #[arg(long, default_value = consts::DEFAULT_SOCKET_PATH)]
    socket_path: String,

    /// 共享内存名称（默认 minios_shm）
    #[arg(long, default_value = consts::DEFAULT_SHM_NAME)]
    shm_name: String,

    /// 共享内存数据页数量（默认 256，即 ~1MB）
    #[arg(long, default_value_t = consts::DEFAULT_SHM_PAGES)]
    shm_pages: u32,

    /// 缓存最大条目数（默认 128）
    #[arg(long, default_value_t = consts::DEFAULT_CACHE_CAPACITY)]
    cache_capacity: usize,

    /// 缓存最大内存占用，单位 MB（默认 64MB）
    #[arg(long, default_value_t = 64)]
    cache_memory_mb: u64,

    /// 最大对象数（默认 1024，决定元数据区大小）
    #[arg(long, default_value_t = consts::DEFAULT_MAX_OBJECTS)]
    max_objects: u64,

    /// 数据块总数（默认 4096，决定数据区大小）
    #[arg(long, default_value_t = consts::DEFAULT_TOTAL_BLOCKS)]
    total_blocks: u64,

    /// 最大并发客户端连接数（默认 16）
    #[arg(long, default_value_t = consts::DEFAULT_MAX_CLIENTS as u32)]
    max_clients: u32,

    /// 是否以守护进程模式运行
    #[arg(long, default_value_t = false)]
    daemon: bool,

    /// PID 文件路径（默认 /tmp/minios.pid）
    #[arg(long, default_value = "/tmp/minios.pid")]
    pidfile: String,

    /// Prometheus 指标 HTTP 端口（默认 9090，设为 0 禁用）
    #[arg(long, default_value_t = 9090u16)]
    metrics_port: u16,

    /// 工作线程池大小（默认 4）
    #[arg(long, default_value_t = 4usize)]
    worker_threads: usize,
}

// ═══════════════════════════════════════════════════════════════════════
// 内部数据结构
// ═══════════════════════════════════════════════════════════════════════

/// 正在进行的文件分块上传的缓冲区
///
/// 当客户端使用 PUT_BEGIN 开始分块上传时创建，
/// 累积 PUT_CHUNK 的数据直到 PUT_END 时写入存储。
struct PendingUpload {
    /// 已累积的数据（每次 PUT_CHUNK 追加到此 Vec 中）
    data: Vec<u8>,

    /// MIME 内容类型（如 "image/png"）
    content_type: String,

    /// 自定义标签（JSON 字符串）
    tags: String,
}

/// 服务端全局共享状态
///
/// 所有客户端处理线程共享同一个 `Arc<Mutex<ServerState>>`：
/// - `Arc`（原子引用计数）允许多线程共享所有权
/// - `Mutex`（互斥锁）保证同一时间只有一个线程访问状态
struct ServerState {
    /// 对象存储引擎 — 管理 store.odb 文件
    store: ObjectStore,

    /// LRU 缓存 — 加速常用对象的读取
    cache: LruCache,

    /// 共享内存区域 — 服务端端视图
    region: ShmRegion,

    /// 共享内存页分配器 — 管理页的分配/释放
    page_alloc: PageAllocator,

    /// 待完成的分块上传缓冲（按名称索引）
    ///
    /// Key：对象名称
    /// Value：累积的数据和元信息
    pending_uploads: HashMap<String, PendingUpload>,

    /// Prometheus 监控指标注册表
    metrics: MetricsRegistry,
}

/// 线程安全的状态共享类型
///
/// `Arc<Mutex<T>>` 是 Rust 中最常见的线程间共享模式：
/// - `Arc` = Atomic Reference Counting（原子引用计数）
/// - `Mutex` = Mutual Exclusion（互斥锁）
type SharedState = Arc<Mutex<ServerState>>;

// ═══════════════════════════════════════════════════════════════════════
// 多生产者-多消费者工作项
// ═══════════════════════════════════════════════════════════════════════

/// 工作队列中的任务项。
///
/// 每个客户端连接被打包为一个 `Command` 任务，
/// 包含 UnixSocket 流和已解析的命令文本。
/// `Shutdown` 标记用于在关闭时通知工作线程退出。
enum WorkItem {
    /// 来自客户端的命令请求
    Command {
        /// Unix Domain Socket 连接（用于写回响应）
        stream: UnixStream,
        /// 已解析的命令文本（例如 "PUT name size ..."）
        command: String,
    },
    /// 关闭信号——每个工作线程收到此信号后退出循环
    Shutdown,
}

// ═══════════════════════════════════════════════════════════════════════
// 主函数 — 服务端生命周期
// ═══════════════════════════════════════════════════════════════════════

fn main() {
    // 初始化日志系统（env_logger）
    // 默认日志级别为 "info"，可通过 RUST_LOG 环境变量覆盖
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    // ── 将相对路径转为绝对路径（在 daemonize 之前）──
    // daemonize() 会调用 chdir("/")，导致所有相对路径从根目录开始解析。
    // 如果用户在根目录没有写权限，store.odb 创建会静默失败。
    // 因此必须在守护进程化之前将 store_path 转为绝对路径。
    let store_path = if Path::new(&args.store_path).is_relative() {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(&args.store_path)
    } else {
        PathBuf::from(&args.store_path)
    };
    let store_path = store_path.display().to_string();

    // ── 设置信号处理器 ──
    // 注册 SIGTERM/SIGINT 处理器
    daemon::setup_signal_handlers().expect("setup signal handlers");

    // ── 守护进程化（如果指定了 --daemon）──
    if args.daemon {
        daemon::daemonize().expect("daemonize");
        daemon::write_pidfile(&args.pidfile).expect("write pidfile");
    }

    log::info!("minios server starting...");

    // ── 打开或创建存储文件 ──
    let mut store = if Path::new(&store_path).exists() {
        // 文件存在 → 打开已有存储
        log::info!("Opening existing store: {}", store_path);
        ObjectStore::open(&store_path).unwrap_or_else(|e| {
            log::error!("Cannot open store: {e}");
            std::process::exit(1);
        })
    } else {
        // 文件不存在 → 创建新存储
        log::info!(
            "Creating new store: {} (max_objects={}, total_blocks={})",
            store_path, args.max_objects, args.total_blocks
        );
        ObjectStore::create(&store_path, args.max_objects, args.total_blocks)
            .unwrap_or_else(|e| {
                log::error!("Cannot create store: {e}");
                std::process::exit(1);
            })
    };

    // ── 缓存预热 ──
    // 启动时预加载一部分对象到 LRU 缓存，加速前几次请求的响应
    let cache_memory = args.cache_memory_mb * 1024 * 1024; // MB → 字节
    let mut cache = LruCache::new(args.cache_capacity, cache_memory);
    {
        // 获取所有对象的列表
        let obj_list = store.list();
        let ids: Vec<[u8; 16]> = obj_list.iter().map(|o| o.uuid).collect();

        // 最多预热 cache_capacity 个对象
        let limit = args.cache_capacity.min(ids.len());
        let mut loaded = 0;

        for id in ids.iter().take(limit) {
            match store.get_by_id(id) {
                Ok(Some(obj)) => {
                    // 将对象放入缓存
                    cache.put(obj.summary.uuid, obj.data, obj.summary.name, obj.summary.size);
                    loaded += 1;
                }
                Ok(None) => {} // 对象可能已被删除（并发修改）
                Err(e) => {
                    log::warn!("Cache warmup: failed to load object {:x?}: {}", id, e);
                }
            }
        }
        log::info!(
            "Cache warmup: loaded {}/{} objects (capacity={})",
            loaded,
            obj_list.len(),
            args.cache_capacity
        );
    }

    // ── 创建共享内存区域 ──
    // 先清理可能残留的同名共享内存（前次异常退出留下）
    {
        use std::ffi::CString;
        let cname = CString::new(args.shm_name.as_str()).unwrap();
        // shm_unlink 不会影响已打开的共享内存，
        // 只是从 /dev/shm/ 中删除文件名
        unsafe { libc::shm_unlink(cname.as_ptr()) };
    }

    // 创建新的共享内存区域
    let region = ShmRegion::create(&args.shm_name, args.shm_pages)
        .unwrap_or_else(|e| {
            log::error!("Cannot create shared memory: {e}");
            std::process::exit(1);
        });
    log::info!(
        "Shared memory '{}' created: {} data pages",
        args.shm_name, args.shm_pages
    );

    // ── 初始化页分配器 ──
    let total_pages = region.header().total_pages;
    let free_pages_offset = 16usize; // 控制头中 free_pages 字段的偏移

    // 计算 free_pages 计数器的地址
    let free_pages_ptr = unsafe {
        (region.data_area() as *mut u8)         // 数据区起始
            .sub(consts::SHM_PAGE_SIZE as usize) // 回退到控制页开头
            .add(free_pages_offset)              // 前进到 free_pages 字段
            as *mut u32
    };

    // 初始化空闲页计数
    unsafe { *free_pages_ptr = total_pages; }

    // 创建页分配器
    let page_alloc = unsafe {
        PageAllocator::new(region.bitmap_ptr(), total_pages, free_pages_ptr)
    };

    // ── 初始化页位图 ──
    // 将所有位初始化为 1（所有页空闲）
    let bitmap_size = region.header().page_bitmap_size as usize;
    unsafe {
        // write_bytes 以字节为单位写入值
        // 0xFF = 0b11111111 → 一个字节中的 8 个位都是 1
        std::ptr::write_bytes(region.bitmap_ptr(), 0xFF, bitmap_size);

        // 处理尾部：如果 total_pages 不是 8 的整数倍
        // 最后一个字节中超出 total_pages 的位应设为 0
        let valid_in_last = total_pages % 8;
        if valid_in_last != 0 {
            // (1 << valid) - 1 生成掩码，只保留有效的低位
            // 例：valid=3 → mask=0b00000111
            *region.bitmap_ptr().add(bitmap_size - 1) = (1u8 << valid_in_last) - 1;
        }
    }

    // ── 创建 Prometheus 指标注册表 ──
    let start_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let metrics = MetricsRegistry::new(start_time);

    // ── 组装全局状态 ──
    let state: SharedState = Arc::new(Mutex::new(ServerState {
        store,
        cache,
        region,
        page_alloc,
        pending_uploads: HashMap::new(),
        metrics,
    }));

    // ── 绑定 Unix Socket ──
    // 先删除可能残留的 socket 文件（前次异常退出留下）
    let _ = std::fs::remove_file(&args.socket_path);

    let listener = UnixListener::bind(&args.socket_path).unwrap_or_else(|e| {
        log::error!("Cannot bind to {}: {}", args.socket_path, e);
        std::process::exit(1);
    });
    log::info!("Listening on {}", args.socket_path);

    // ── 启动 Prometheus HTTP 端点（如果端口不为 0）──
    if args.metrics_port > 0 {
        let metrics_state = Arc::clone(&state);
        let metrics_port = args.metrics_port;
        std::thread::spawn(move || {
            spawn_metrics_server(metrics_state, metrics_port);
        });
        log::info!(
            "Prometheus metrics endpoint listening on http://0.0.0.0:{}/metrics",
            args.metrics_port
        );
    }

    // ── 并发控制 ──
    // 使用原子计数器跟踪当前活跃的客户端连接数
    let active_clients = Arc::new(AtomicU32::new(0));
    let max_clients = args.max_clients;

    // ── 创建 MPMC 工作队列 ──
    // crossbeam::channel 是一个多生产者-多消费者无界/有界通道。
    // 主 accept 循环作为生产者，将客户端命令推入队列；
    // 固定数量的工作线程作为消费者，从队列拉取并处理命令。
    // 队列容量设为 max_clients × 2，提供适度缓冲。
    let (work_tx, work_rx) = channel::bounded::<WorkItem>(max_clients as usize * 2);
    let work_tx: Arc<Sender<WorkItem>> = Arc::new(work_tx);

    // ── 启动工作线程池 ──
    let num_workers = args.worker_threads.max(1);
    let mut worker_handles = Vec::with_capacity(num_workers);

    for worker_id in 0..num_workers {
        let rx = work_rx.clone();
        let state = Arc::clone(&state);
        let active_clients = Arc::clone(&active_clients);

        let handle = std::thread::Builder::new()
            .name(format!("minios-worker-{worker_id}"))
            .spawn(move || {
                worker_loop(rx, state, active_clients);
            })
            .expect("spawn worker thread");
        worker_handles.push(handle);
    }
    log::info!("Worker pool started: {num_workers} threads, queue capacity={}", max_clients * 2);

    // 设置 listener 为非阻塞模式
    // 这样 accept() 不会阻塞，我们可以定期检查 shutdown 信号
    listener.set_nonblocking(true).expect("set nonblocking");

    // ── 主事件循环（accept + push）──
    loop {
        // 检查是否收到关闭信号（SIGTERM / SIGINT / STOP 命令）
        if daemon::is_shutdown_requested() {
            log::info!("Shutdown signal received, exiting accept loop...");
            break;
        }

        // 尝试接受新连接（非阻塞）
        match listener.accept() {
            Ok((mut stream, addr)) => {
                // ── 连接数控制 ──
                let accepted = active_clients
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                        (current < max_clients).then_some(current + 1)
                    })
                    .is_ok();

                if !accepted {
                    let current = active_clients.load(Ordering::SeqCst);
                    log::warn!(
                        "Rejecting connection from {:?}: server busy ({}/{})",
                        addr, current, max_clients
                    );
                    let _ = stream.write_all(b"ERROR server busy\n");
                    continue;
                }

                // ── 从 socket 读取命令文本 ──
                let mut buf = [0u8; 4096];
                let n = match stream.read(&mut buf) {
                    Ok(n) if n > 0 => n,
                    _ => {
                        // 读取失败或连接关闭 → 回退计数
                        active_clients.fetch_sub(1, Ordering::SeqCst);
                        continue;
                    }
                };

                let msg = String::from_utf8_lossy(&buf[..n]);
                let command = msg.trim().to_string();

                // ── 更新指标 ──
                {
                    let s = state.lock().unwrap();
                    s.metrics.connections_total.inc();
                    s.metrics.active_connections.set(
                        active_clients.load(Ordering::SeqCst) as f64
                    );
                }

                // ── 推入 MPMC 工作队列 ──
                // try_send 非阻塞：如果队列已满，拒绝连接
                match work_tx.try_send(WorkItem::Command { stream, command }) {
                    Ok(()) => {} // 成功入队
                    Err(crossbeam::channel::TrySendError::Full(item)) => {
                        // 队列满 → 拒绝连接
                        let current = active_clients.load(Ordering::SeqCst);
                        log::warn!(
                            "Rejecting connection from {:?}: work queue full ({}/{})",
                            addr, current, max_clients
                        );
                        let mut s = match item {
                            WorkItem::Command { stream, .. } => stream,
                            WorkItem::Shutdown => unreachable!(),
                        };
                        let _ = s.write_all(b"ERROR server busy\n");
                        active_clients.fetch_sub(1, Ordering::SeqCst);
                    }
                    Err(crossbeam::channel::TrySendError::Disconnected(_)) => {
                        // 通道已关闭（所有接收端已丢弃）
                        log::error!("Work queue disconnected, exiting...");
                        active_clients.fetch_sub(1, Ordering::SeqCst);
                        break;
                    }
                }
            }
            // 非阻塞模式下没有连接时返回 WouldBlock，正常情况
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            // 真正的错误（如 socket 被删除）
            Err(e) => {
                log::error!("Accept error: {e}");
                break;
            }
        }

        // 休眠 50ms 避免忙等待
        std::thread::sleep(Duration::from_millis(50));
    }

    // ═════════════════════════════════════════════════════════════════
    // 优雅关闭流程
    // ═════════════════════════════════════════════════════════════════

    log::info!("Shutting down...");

    // 关闭 listener（不再接受新连接）
    drop(listener);

    // 向每个工作线程发送 Shutdown 信号
    for _ in 0..num_workers {
        // 忽略发送错误（可能已有 worker 崩溃导致通道关闭）
        let _ = work_tx.send(WorkItem::Shutdown);
    }

    // 等待所有工作线程退出
    log::info!("Waiting for {} worker threads to finish...", num_workers);
    for handle in worker_handles {
        if let Err(e) = handle.join() {
            log::error!("Worker thread panicked: {:?}", e);
        }
    }
    log::info!("All worker threads finished.");

    // 尝试获取独占所有权并清理资源
    match Arc::try_unwrap(state) {
        Ok(mutex) => {
            let mut inner = mutex.into_inner().unwrap();
            inner.store.flush().ok();
            let _ = inner.region.destroy();
            log::info!("Store flushed and shared memory destroyed.");
        }
        Err(arc) => {
            log::warn!("Some references still active, forcing cleanup...");
            if let Ok(mut inner) = arc.lock() {
                inner.store.flush().ok();
            }
        }
    }

    // 清理 socket 文件
    let _ = std::fs::remove_file(&args.socket_path);
    // 删除 PID 文件
    daemon::remove_pidfile(&args.pidfile);

    log::info!("minios server stopped.");
}

// ═══════════════════════════════════════════════════════════════════════
// 工作线程池 — 多生产者-多消费者模型
// ═══════════════════════════════════════════════════════════════════════

/// 工作线程主循环。
///
/// 从 MPMC 通道中拉取 `WorkItem`：
/// - `Command { stream, command }` → 调用 `dispatch_command` 处理 →
///   将响应写回 socket → 递减活跃连接计数
/// - `Shutdown` → 退出循环
///
/// 多个工作线程共享同一个 `Receiver`（通过 `crossbeam::channel` 的 MPMC 语义），
/// 主 accept 循环作为生产者，工作线程池作为消费者，构成了完整的多生产者-多消费者模型。
fn worker_loop(
    rx: crossbeam::channel::Receiver<WorkItem>,
    state: SharedState,
    active_clients: Arc<AtomicU32>,
) {
    loop {
        match rx.recv() {
            Ok(WorkItem::Command {
                mut stream,
                command,
            }) => {
                // 处理命令（在 State 锁内执行）
                let response = dispatch_command(&command, &state);

                // 请求计数 +1 并快照最新指标
                {
                    let server_state = state.lock().unwrap();
                    server_state.metrics.requests_total.inc();
                    snapshot_metrics(&server_state);
                }

                // 写回响应
                let _ = stream.write_all(response.as_bytes());

                // 递减活跃客户端计数并更新 gauge
                let remaining = active_clients.fetch_sub(1, Ordering::SeqCst) - 1;
                if let Ok(s) = state.lock() {
                    s.metrics.active_connections.set(remaining as f64);
                }
            }
            Ok(WorkItem::Shutdown) => {
                log::debug!(
                    "Worker {:?} received shutdown signal",
                    std::thread::current().name()
                );
                break;
            }
            Err(_) => {
                // 通道已关闭（所有生产者已释放 Sender）
                break;
            }
        }
    }
}

/// 处理单个客户端连接（保留接口兼容性，当前未使用）。
///
/// 迁移至 MPMC 模型后，命令读取（socket read）在 accept 循环中完成，
/// 命令执行在工作线程中完成。此函数保留以备将来直接调用（如测试）。
#[allow(dead_code)]
fn handle_client(mut stream: UnixStream, state: &SharedState) {
    let mut buf = [0u8; 4096];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return,
    };

    let msg = String::from_utf8_lossy(&buf[..n]);
    let response = dispatch_command(msg.trim(), state);

    {
        let server_state = state.lock().unwrap();
        server_state.metrics.requests_total.inc();
        snapshot_metrics(&server_state);
    }

    let _ = stream.write_all(response.as_bytes());
}

/// 命令分发器 — 根据命令文本的第一个词匹配处理函数
///
/// ## 支持的协议格式
///
/// ### 标准 PUT（小文件单次传输）
/// ```
/// PUT <name> <size> <content_type> <tags> <start_page> <num_pages>
/// ```
///
/// ### 分块上传协议（大文件）
/// ```
/// PUT_BEGIN <name> <total_size> <content_type> <tags>
/// PUT_CHUNK <name> <chunk_size> <start_page> <num_pages>
/// PUT_END <name>
/// ```
///
/// ### 强制覆盖上传
/// ```
/// PUT_FORCE <name> <size> <content_type> <tags> <start_page> <num_pages>
/// ```
///
/// ### 查询操作
/// ```
/// GET <uuid_or_name>
/// INFO <uuid_or_name>
/// DELETE <uuid>
/// LIST
/// STATUS
/// STOP
/// ```
fn dispatch_command(msg: &str, state: &SharedState) -> String {
    // 按空格分割命令，最多 7 个部分
    // 因为标准 PUT 有 7 个参数
    let parts: Vec<&str> = msg.splitn(7, ' ').collect();
    if parts.is_empty() {
        return "ERROR empty command\n".into();
    }

    // 命令名大小写不敏感
    match parts[0].to_uppercase().as_str() {
        "PUT" => cmd_put(&parts, state),
        "PUT_FORCE" => cmd_put_force(&parts, state),
        "PUT_BEGIN" => cmd_put_begin(&parts, state),
        "PUT_CHUNK" => cmd_put_chunk(&parts, state),
        "PUT_END" => cmd_put_end(&parts, state),
        "PUT_END_FORCE" => cmd_put_end_force(&parts, state),
        "GET" => cmd_get(&parts, state),
        "INFO" => cmd_info(&parts, state),
        "DELETE" => cmd_delete(&parts, state),
        "LIST" => cmd_list(state),
        "STATUS" => cmd_status(state),
        "STOP" => {
            // 设置全局关闭标志，主循环检测到后会执行优雅退出
            daemon::request_shutdown();
            "OK shutting down\n".into()
        }
        _ => format!("ERROR unknown command '{}'\n", parts[0]),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 分块上传协议处理
// ═══════════════════════════════════════════════════════════════════════

/// PUT_BEGIN — 初始化分块上传会话
///
/// 创建一个 PendingUpload 结构体，后续 PUT_CHUNK 的数据将追加到其中。
///
/// ## 协议格式
/// `PUT_BEGIN <name> <total_size> <content_type> <tags>`
///
/// ## 设计意图
/// 分块上传允许客户端传输超出共享内存容量的大文件。
/// 客户端将文件切割成多个块，逐块通过共享内存传送。
/// 服务端在内存中累积所有块，最后一次性写入存储。
fn cmd_put_begin(parts: &[&str], state: &SharedState) -> String {
    // 验证参数数量
    if parts.len() < 5 {
        return "ERROR PUT_BEGIN requires: name total_size content_type tags\n".into();
    }

    // 解析各参数
    let name = parts[1].to_string();
    let total_size: usize = match parts[2].parse() {
        Ok(v) => v,
        Err(_) => return "ERROR invalid total_size\n".into(),
    };
    let content_type = parts[3].to_string();
    let tags = parts[4].to_string();

    // 获取全局状态锁并创建上传缓冲区
    let mut server_state = state.lock().unwrap();

    // Vec::with_capacity 预分配空间，避免逐块扩容时的重复内存分配
    server_state.pending_uploads.insert(
        name.clone(),
        PendingUpload {
            data: Vec::with_capacity(total_size),
            content_type,
            tags,
        },
    );

    log::info!("PUT_BEGIN: name={name}, total_size={total_size}");
    "OK\n".into()
}

/// PUT_CHUNK — 接收一块数据并追加到上传缓冲区
///
/// ## 协议格式
/// `PUT_CHUNK <name> <chunk_size> <start_page> <num_pages>`
///
/// ## 流程
/// 1. 验证存在对应的上传会话
/// 2. 从共享内存读取该块的数据
/// 3. 释放共享内存页（归还给页池）
/// 4. 追加数据到 PendingUpload 缓冲区
fn cmd_put_chunk(parts: &[&str], state: &SharedState) -> String {
    if parts.len() < 5 {
        return "ERROR PUT_CHUNK requires: name chunk_size start_page num_pages\n".into();
    }

    let name = parts[1].to_string();
    let chunk_size: u64 = match parts[2].parse() {
        Ok(v) => v,
        Err(_) => return "ERROR invalid chunk_size\n".into(),
    };
    let start_page: u32 = match parts[3].parse() {
        Ok(v) => v,
        Err(_) => return "ERROR invalid start_page\n".into(),
    };
    let num_pages: u32 = match parts[4].parse() {
        Ok(v) => v,
        Err(_) => return "ERROR invalid num_pages\n".into(),
    };

    let mut server_state = state.lock().unwrap();

    // 必须先有 PUT_BEGIN 创建的上传会话
    if !server_state.pending_uploads.contains_key(&name) {
        return "ERROR no pending upload for this name, send PUT_BEGIN first\n".into();
    }

    // 在锁内锁定互斥锁 → 读取共享内存 → 释放页 → 解锁互斥锁
    // 操作顺序很重要：先获取 page_mutex 再操作 shared memory
    let chunk = {
        server_state.region.lock_page_mutex().unwrap();
        let c = server_state.region.read_from_pages(start_page, chunk_size);
        // 服务端读取完毕后立即释放页（客户端不释放，避免竞态）
        server_state.page_alloc.free_pages(start_page, num_pages);
        server_state.region.unlock_page_mutex().unwrap();
        c
    };

    // 追加块数据到上传缓冲区
    let upload = server_state.pending_uploads.get_mut(&name).unwrap();
    upload.data.extend_from_slice(&chunk);

    log::info!(
        "PUT_CHUNK: name={name}, chunk_size={}, total_accumulated={}",
        chunk_size,
        upload.data.len()
    );
    "OK\n".into()
}

/// PUT_END — 完成分块上传，将累积数据写入存储
///
/// ## 协议格式
/// `PUT_END <name>`
///
/// ## 流程
/// 1. 从 pending_uploads 中取出累积数据
/// 2. 调用 store.put() 写入磁盘
/// 3. 更新缓存
/// 4. 清理上传缓冲区
fn cmd_put_end(parts: &[&str], state: &SharedState) -> String {
    if parts.len() < 2 {
        return "ERROR PUT_END requires: name\n".into();
    }
    let name = parts[1].to_string();

    let mut server_state = state.lock().unwrap();

    // 从 HashMap 中取出并移除上传会话
    let upload = match server_state.pending_uploads.remove(&name) {
        Some(u) => u,
        None => return "ERROR no pending upload for this name\n".into(),
    };

    let size = upload.data.len() as u64;

    // 写入存储引擎
    match server_state.store.put(&name, &upload.data, &upload.content_type, &upload.tags) {
        Ok(uuid) => {
            // 将新对象加入缓存
            server_state
                .cache
                .put(uuid, upload.data, name.clone(), size);
            log::info!("PUT_END: name={name}, uuid={:?}, size={size}", uuid_fmt(&uuid));
            format!("OK {}\n", uuid_fmt(&uuid))
        }
        Err(e) => format!("ERROR {e}\n"),
    }
}

/// PUT_END_FORCE — 强制完成分块上传，若名称已存在则覆盖旧对象。
///
/// ## 协议格式
/// `PUT_END_FORCE <name>`
///
/// 与 PUT_END 相同，但使用 put_overwrite 替代 put，
/// 当目标名称已存在时自动删除旧对象后写入。
fn cmd_put_end_force(parts: &[&str], state: &SharedState) -> String {
    if parts.len() < 2 {
        return "ERROR PUT_END_FORCE requires: name\n".into();
    }
    let name = parts[1].to_string();

    let mut server_state = state.lock().unwrap();

    let upload = match server_state.pending_uploads.remove(&name) {
        Some(u) => u,
        None => return "ERROR no pending upload for this name\n".into(),
    };

    let size = upload.data.len() as u64;

    match server_state.store.put_overwrite(&name, &upload.data, &upload.content_type, &upload.tags) {
        Ok(uuid) => {
            server_state
                .cache
                .put(uuid, upload.data, name.clone(), size);
            log::info!("PUT_END_FORCE: name={name}, uuid={:?}, size={size}", uuid_fmt(&uuid));
            format!("OK {}\n", uuid_fmt(&uuid))
        }
        Err(e) => format!("ERROR {e}\n"),
    }
}

/// 标准 PUT（小文件单次传输，保留向后兼容）
///
/// ## 协议格式
/// `PUT <name> <size> <content_type> <tags> <start_page> <num_pages>`
///
/// ## 流程
/// 1. 从共享内存读取对象数据
/// 2. 释放共享内存页
/// 3. 写入存储引擎
/// 4. 更新缓存
///
/// 此接口保留用于小于共享内存容量的文件。
fn cmd_put(parts: &[&str], state: &SharedState) -> String {
    // 标准 PUT 需要 7 个参数
    if parts.len() < 7 {
        return "ERROR PUT requires: name size content_type tags start_page num_pages\n".into();
    }

    // 解析各参数
    let name = parts[1];
    let size: u64 = match parts[2].parse() {
        Ok(v) => v,
        Err(_) => return "ERROR invalid size\n".into(),
    };
    let content_type = parts[3];
    let tags = parts[4];
    let start_page: u32 = match parts[5].parse() {
        Ok(v) => v,
        Err(_) => return "ERROR invalid start_page\n".into(),
    };
    let num_pages: u32 = match parts[6].parse() {
        Ok(v) => v,
        Err(_) => return "ERROR invalid num_pages\n".into(),
    };

    let mut server_state = state.lock().unwrap();

    // 锁定互斥锁 → 从共享内存读取 → 释放页 → 解锁
    server_state.region.lock_page_mutex().unwrap();
    let data = server_state.region.read_from_pages(start_page, size);
    server_state.page_alloc.free_pages(start_page, num_pages);
    server_state.region.unlock_page_mutex().unwrap();

    // 写入存储
    match server_state.store.put(name, &data, content_type, tags) {
        Ok(uuid) => {
            // 加入缓存
            server_state.cache.put(uuid, data, name.into(), size);
            format!("OK {}\n", uuid_fmt(&uuid))
        }
        Err(e) => {
            format!("ERROR {e}\n")
        }
    }
}

/// PUT_FORCE — 强制上传，若名称已存在则覆盖旧对象。
///
/// ## 协议格式
/// `PUT_FORCE <name> <size> <content_type> <tags> <start_page> <num_pages>`
///
/// 与标准 PUT 相同，但当目标名称已存在时自动删除旧对象后写入。
/// 警告：此操作会彻底删除旧对象及其数据，不可恢复。
fn cmd_put_force(parts: &[&str], state: &SharedState) -> String {
    if parts.len() < 7 {
        return "ERROR PUT_FORCE requires: name size content_type tags start_page num_pages\n".into();
    }

    let name = parts[1];
    let size: u64 = match parts[2].parse() {
        Ok(v) => v,
        Err(_) => return "ERROR invalid size\n".into(),
    };
    let content_type = parts[3];
    let tags = parts[4];
    let start_page: u32 = match parts[5].parse() {
        Ok(v) => v,
        Err(_) => return "ERROR invalid start_page\n".into(),
    };
    let num_pages: u32 = match parts[6].parse() {
        Ok(v) => v,
        Err(_) => return "ERROR invalid num_pages\n".into(),
    };

    let mut server_state = state.lock().unwrap();

    server_state.region.lock_page_mutex().unwrap();
    let data = server_state.region.read_from_pages(start_page, size);
    server_state.page_alloc.free_pages(start_page, num_pages);
    server_state.region.unlock_page_mutex().unwrap();

    // 使用 put_overwrite：若名称已存在则先删除旧对象
    match server_state.store.put_overwrite(name, &data, content_type, tags) {
        Ok(uuid) => {
            server_state.cache.put(uuid, data, name.into(), size);
            format!("OK {}\n", uuid_fmt(&uuid))
        }
        Err(e) => {
            format!("ERROR {e}\n")
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// GET / DELETE / LIST / STATUS 命令处理
// ═══════════════════════════════════════════════════════════════════════

/// GET — 获取对象数据
///
/// ## 协议格式
/// `GET <uuid_or_name>`
///
/// ## 查找策略
/// 先尝试将参数解析为 UUID（32 位十六进制字符串），
/// 如果成功则按 UUID 查找，否则按名称查找。
///
/// ## 缓存策略
/// 1. 先查 LRU 缓存（内存命中 → 最快）
/// 2. 缓存未命中 → 从磁盘读取
/// 3. 读取成功后 → 放入缓存（加速后续访问）
fn cmd_get(parts: &[&str], state: &SharedState) -> String {
    if parts.len() < 2 {
        return "ERROR GET requires uuid or name\n".into();
    }
    let id = parts[1];
    let mut server_state = state.lock().unwrap();

    // 尝试按 UUID 查找
    if let Some(uuid) = parse_uuid(id) {
        // ── 第一步：查缓存 ──
        if let Some(data) = server_state.cache.get(&uuid).map(|d| d.to_vec()) {
            // 缓存命中！将数据写入共享内存返回给客户端
            return write_get_result(&mut server_state, data);
        }

        // ── 第二步：缓存未命中 → 从磁盘读取 ──
        match server_state.store.get_by_id(&uuid) {
            Ok(Some(obj)) => {
                let sz = obj.summary.size;
                // 放入缓存（利用刚读的数据，再次访问时命中缓存）
                server_state
                    .cache
                    .put(obj.summary.uuid, obj.data.clone(), obj.summary.name, sz);
                write_get_result(&mut server_state, obj.data)
            }
            Ok(None) => "ERROR object not found\n".into(),
            Err(e) => format!("ERROR {e}\n"),
        }
    } else {
        // 不是 UUID 格式 → 按名称查找
        // ── 第一步：按名称查缓存 ──
        if let Some(data) = server_state.cache.get_by_name(id).map(|d| d.to_vec()) {
            // 缓存命中！命中计数已由 get_by_name 内部更新
            return write_get_result(&mut server_state, data);
        }

        // ── 第二步：缓存未命中 → 从磁盘读取 ──
        match server_state.store.get_by_name(id) {
            Ok(Some(obj)) => {
                let sz = obj.summary.size;
                // 放入缓存（利用刚读的数据，再次访问时命中缓存）
                server_state
                    .cache
                    .put(obj.summary.uuid, obj.data.clone(), obj.summary.name, sz);
                write_get_result(&mut server_state, obj.data)
            }
            Ok(None) => "ERROR object not found\n".into(),
            Err(e) => format!("ERROR {e}\n"),
        }
    }
}

/// DELETE — 删除对象
///
/// ## 协议格式
/// `DELETE <uuid>`
///
/// ## 流程
/// 1. 验证 UUID 格式
/// 2. 从存储引擎删除对象（释放数据块，标记元数据为 tombstone）
/// 3. 使缓存中的副本失效
fn cmd_delete(parts: &[&str], state: &SharedState) -> String {
    if parts.len() < 2 {
        return "ERROR DELETE requires uuid\n".into();
    }
    let uuid = match parse_uuid(parts[1]) {
        Some(u) => u,
        None => return "ERROR invalid uuid format\n".into(),
    };
    let mut server_state = state.lock().unwrap();

    match server_state.store.delete(&uuid) {
        Ok(true) => {
            // 从缓存中移除（防止返回已删除的过期数据）
            server_state.cache.invalidate(&uuid);
            "OK deleted\n".into()
        }
        Ok(false) => "ERROR object not found\n".into(),
        Err(e) => format!("ERROR {e}\n"),
    }
}

/// LIST — 列出所有对象
///
/// ## 协议格式
/// `LIST`
///
/// ## 响应格式
/// ```text
/// OK <count>
/// <uuid> <name> <size> <content_type> <created_at> <tags>
/// ...
/// ```
fn cmd_list(state: &SharedState) -> String {
    let server_state = state.lock().unwrap();
    let objects = server_state.store.list();

    // 第一行：OK + 对象数量
    let mut resp = format!("OK {}\n", objects.len());

    // 逐行输出每个对象的摘要
    for obj in &objects {
        resp.push_str(&format!(
            "{} {} {} {} {} {}\n",
            uuid_fmt(&obj.uuid),    // UUID（32 字符十六进制）
            obj.name,                // 名称
            obj.size,                // 大小
            obj.content_type,        // MIME 类型
            obj.created_at,          // 创建时间戳
            obj.tags,                // 自定义标签
        ));
    }
    resp
}

/// INFO — 获取单个对象的元数据（不读取数据块）
///
/// ## 协议格式
/// `INFO <uuid_or_name>`
///
/// ## 查找策略
/// 先尝试将参数解析为 UUID，如果成功则按 UUID 查找，否则按名称查找。
///
/// ## 响应格式
/// ```text
/// OK
/// uuid: <32-char hex>
/// name: <name>
/// size: <bytes>
/// content_type: <mime>
/// created_at: <unix timestamp>
/// tags: <json>
/// block_count: <n>
/// ```
fn cmd_info(parts: &[&str], state: &SharedState) -> String {
    if parts.len() < 2 {
        return "ERROR INFO requires uuid or name\n".into();
    }
    let id = parts[1];
    let server_state = state.lock().unwrap();

    // 尝试按 UUID 查找，否则按名称查找
    let summary = if let Some(uuid) = parse_uuid(id) {
        server_state.store.get_summary_by_id(&uuid)
    } else {
        server_state.store.get_summary_by_name(id)
    };

    match summary {
        Some(obj) => {
            format!(
                "OK\n\
                 uuid: {}\n\
                 name: {}\n\
                 size: {}\n\
                 content_type: {}\n\
                 created_at: {}\n\
                 tags: {}\n\
                 block_count: {}\n",
                uuid_fmt(&obj.uuid),
                obj.name,
                obj.size,
                obj.content_type,
                obj.created_at,
                obj.tags,
                obj.block_count,
            )
        }
        None => format!("ERROR object not found: {id}\n"),
    }
}

/// STATUS — 查看服务端运行状态
///
/// ## 协议格式
/// `STATUS`
///
/// ## 响应内容
/// - 存储对象总数
/// - 数据块空闲/总量
/// - 缓存命中率
/// - 共享内存页使用情况
fn cmd_status(state: &SharedState) -> String {
    let server_state = state.lock().unwrap();
    let stats = server_state.store.stats();              // 存储统计
    let cache_stats = server_state.cache.stats();        // 缓存统计
    let header = server_state.region.header();            // 共享内存信息

    format!(
        "OK\n\
         store_objects: {}\n\
         store_blocks_free: {}/{}\n\
         store_file_size: {}\n\
         cache_entries: {}/{}\n\
         cache_hits: {}\n\
         cache_misses: {}\n\
         cache_evictions: {}\n\
         cache_hit_rate: {:.2}\n\
         shm_pages_free: {}/{}\n",
        stats.total_objects,
        stats.free_blocks,
        stats.total_blocks,
        stats.file_size,
        cache_stats.size,
        cache_stats.capacity,
        cache_stats.hits,
        cache_stats.misses,
        cache_stats.evictions,
        cache_stats.hit_rate,
        header.free_pages,
        header.total_pages,
    )
}

// ═══════════════════════════════════════════════════════════════════════
// Prometheus 指标快照与 HTTP 服务
// ═══════════════════════════════════════════════════════════════════════

/// 从 ServerState 中读取最新统计信息并更新 Prometheus 指标。
///
/// 每次请求处理完毕后调用此函数，将当前存储、缓存和共享内存的
/// 快照写入到 Prometheus Gauge / Counter 中。
fn snapshot_metrics(state: &ServerState) {
    let store_stats = state.store.stats();
    let cache_stats = state.cache.stats();
    let header = state.region.header();

    state.metrics.store_objects.set(store_stats.total_objects as f64);
    state.metrics.store_blocks_free.set(store_stats.free_blocks as f64);
    state.metrics
        .store_blocks_total
        .set(store_stats.total_blocks as f64);
    state.metrics
        .store_file_size_bytes
        .set(store_stats.file_size as f64);

    state.metrics.cache_entries.set(cache_stats.size as f64);
    // 注意：prometheus::Counter 只能递增，不能设置为绝对值。
    // 缓存的 hits/misses/evictions 通过 cache.get/put 时机更新 Counter，
    // 此处仅更新 Gauge 类型的快照值。

    // 更新共享内存页使用快照
    state.metrics.shm_pages_free.set(header.free_pages as f64);
    state.metrics.shm_pages_total.set(header.total_pages as f64);
}

/// 启动一个极简 HTTP 服务端，仅在 `/metrics` 路径上返回 Prometheus 文本格式的指标数据。
///
/// 此函数设计为在独立线程中运行，使用 `std::net::TcpListener` 避免引入异步依赖。
/// 对每个连接读取 HTTP 请求行，若为 `GET /metrics` 则返回指标文本，否则返回 404。
///
/// # 参数
/// - `state`: 服务端共享状态引用（只读获取指标快照）
/// - `port`: 监听的 TCP 端口
fn spawn_metrics_server(state: SharedState, port: u16) {
    let addr = format!("0.0.0.0:{port}");
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            log::error!("Failed to bind metrics HTTP server on {addr}: {e}");
            return;
        }
    };

    // 设置 accept 超时为 1 秒，以便定期检查 shutdown 信号
    let _ = listener.set_nonblocking(true);

    loop {
        // 检查关闭信号
        if daemon::is_shutdown_requested() {
            break;
        }

        match listener.accept() {
            Ok((mut stream, _addr)) => {
                // 读取请求行
                let mut req_buf = [0u8; 1024];
                let n = match stream.read(&mut req_buf) {
                    Ok(n) if n > 0 => n,
                    _ => continue,
                };

                let req = String::from_utf8_lossy(&req_buf[..n]);
                let first_line = req.lines().next().unwrap_or("");

                if first_line.starts_with("GET /metrics") {
                    // 获取当前时间戳，计算 uptime
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;

                    let body = {
                        let server_state = state.lock().unwrap();
                        // 在返回指标前，更新活跃连接数的 gauge
                        // （活跃连接数的更新在其他地方由 accept/spawn 回调负责）
                        server_state.metrics.encode(now)
                    };

                    let response = format!(
                        "HTTP/1.1 200 OK\r\n\
                         Content-Type: text/plain; version=0.0.4\r\n\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\
                         \r\n\
                         {}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes());
                } else {
                    let not_found = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(not_found.as_bytes());
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    log::info!("Metrics HTTP server stopped.");
}

// ═══════════════════════════════════════════════════════════════════════
// 辅助函数
// ═══════════════════════════════════════════════════════════════════════

/// 将对象数据写入共享内存并返回 GET 协议响应
///
/// ## 参数
/// - `state`：服务端全局状态（可变引用）
/// - `data`：要返回给客户端的对象数据
///
/// ## 返回值
/// 协议响应字符串：
/// - 成功：`OK <size> <start_page> <num_pages>\n`
/// - 失败：`ERROR object too large for shared memory response\n`
///
/// ## 流程
/// 1. 计算需要的共享内存页数
/// 2. 锁定互斥锁并分配连续页
/// 3. 将数据写入分配的页
/// 4. 返回页码位置信息给客户端
fn write_get_result(state: &mut ServerState, data: Vec<u8>) -> String {
    // 空对象特殊处理
    if data.is_empty() {
        return "OK 0 0 0\n".into();
    }

    // 计算需要的页数（向上取整）
    let pages_needed = ((data.len() as u64 + 4095) / 4096) as u32;

    // 锁定互斥锁 → 分配页 → 写入数据 → 解锁
    state.region.lock_page_mutex().unwrap();
    let region = &state.region;

    // 分配连续页（如果当前无足够空间则循环等待）
    let result = match state.page_alloc.alloc_pages_wait(
        pages_needed,
        || region.unlock_page_mutex().unwrap(), // 等待时释放锁
        || region.lock_page_mutex().unwrap(),    // 重试前获取锁
    ) {
        Some(start_page) => {
            // 分配成功 → 写入数据到共享内存
            state.region.write_to_pages(start_page, &data);
            // 返回数据位置信息给客户端
            format!("OK {} {} {}\n", data.len(), start_page, pages_needed)
        }
        None => "ERROR object too large for shared memory response\n".into(),
    };
    state.region.unlock_page_mutex().unwrap();
    result
}

/// 将 UUID 字节数组格式化为 32 字符十六进制字符串
///
/// ## 示例
/// ```
/// [0x55, 0x0e, 0x84, ...] → "550e8400e29b41d4a716446655440000"
/// ```
fn uuid_fmt(uuid: &[u8; 16]) -> String {
    uuid.iter()
        .map(|b| format!("{b:02x}")) // 每字节格式化为 2 位十六进制
        .collect::<Vec<_>>()          // 收集为 Vec<String>
        .join("")                      // 拼接为单一字符串
}

/// 将 32 字符十六进制字符串解析为 16 字节 UUID
///
/// ## 算法
/// 1. 过滤所有非十六进制字符（允许用户粘贴带 - 的 UUID）
/// 2. 检查过滤后是否恰好为 32 个字符
/// 3. 每 2 个字符解析为 1 个字节
///
/// ## 返回值
/// - `Some([u8; 16])`：解析成功
/// - `None`：格式不正确
fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    // 过滤只保留十六进制字符（0-9, a-f, A-F）
    // 这样用户可以使用带连字符或不带连字符的格式
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();

    // UUID 需要恰好 32 个十六进制字符（= 16 字节）
    if hex.len() != 32 {
        return None;
    }

    let mut uuid = [0u8; 16];
    for i in 0..16 {
        // 每 2 个字符为一组，解析为 1 字节
        uuid[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(uuid)
}
