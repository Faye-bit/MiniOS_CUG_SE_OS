//! MinOS 对象存储服务端 — 守护进程入口。
//!
//! 负责：
//! 1. 解析命令行参数
//! 2. 创建/打开 store.odb 存储引擎
//! 3. 初始化共享内存区域和同步原语
//! 4. 监听 Unix Domain Socket，处理客户端请求
//! 5. 优雅关闭

use clap::Parser;
use minos_lib::cache::lru::LruCache;
use minos_lib::common::consts;
use minos_lib::daemon;
use minos_lib::shm::page::PageAllocator;
use minos_lib::shm::region::ShmRegion;
use minos_lib::storage::engine::ObjectStore;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// MinOS 对象存储服务端。
#[derive(Parser, Debug)]
#[command(name = "minos-server", version, about = "MinOS object storage daemon")]
struct Args {
    /// 存储文件路径
    #[arg(long, default_value = consts::DEFAULT_STORE_PATH)]
    store_path: String,

    /// Unix Domain Socket 路径
    #[arg(long, default_value = consts::DEFAULT_SOCKET_PATH)]
    socket_path: String,

    /// 共享内存名称
    #[arg(long, default_value = consts::DEFAULT_SHM_NAME)]
    shm_name: String,

    /// 共享内存数据页数
    #[arg(long, default_value_t = consts::DEFAULT_SHM_PAGES)]
    shm_pages: u32,

    /// LRU 缓存容量（条目数）
    #[arg(long, default_value_t = consts::DEFAULT_CACHE_CAPACITY)]
    cache_capacity: usize,

    /// LRU 缓存最大内存（MB）
    #[arg(long, default_value_t = 64)]
    cache_memory_mb: u64,

    /// 最大对象数
    #[arg(long, default_value_t = consts::DEFAULT_MAX_OBJECTS)]
    max_objects: u64,

    /// 数据块总数
    #[arg(long, default_value_t = consts::DEFAULT_TOTAL_BLOCKS)]
    total_blocks: u64,

    /// 最大并发请求数
    #[arg(long, default_value_t = consts::DEFAULT_MAX_CLIENTS as u32)]
    max_clients: u32,

    /// 以守护进程方式运行
    #[arg(long, default_value_t = false)]
    daemon: bool,

    /// PID 文件路径
    #[arg(long, default_value = "/tmp/minos.pid")]
    pidfile: String,
}

/// 共享的服务端状态，各工作线程共享。
struct ServerState {
    store: ObjectStore,
    cache: LruCache,
    region: ShmRegion,
    page_alloc: PageAllocator,
}

/// 线程安全引用包装。
type SharedState = Arc<Mutex<ServerState>>;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    daemon::setup_signal_handlers().expect("setup signal handlers");

    if args.daemon {
        daemon::daemonize().expect("daemonize");
        daemon::write_pidfile(&args.pidfile).expect("write pidfile");
    }

    log::info!("MinOS server starting...");

    // 打开或创建存储文件
    let store = if Path::new(&args.store_path).exists() {
        log::info!("Opening existing store: {}", args.store_path);
        ObjectStore::open(&args.store_path).unwrap_or_else(|e| {
            log::error!("Cannot open store: {e}");
            std::process::exit(1);
        })
    } else {
        log::info!(
            "Creating new store: {} (max_objects={}, total_blocks={})",
            args.store_path, args.max_objects, args.total_blocks
        );
        ObjectStore::create(&args.store_path, args.max_objects, args.total_blocks)
            .unwrap_or_else(|e| {
                log::error!("Cannot create store: {e}");
                std::process::exit(1);
            })
    };

    // 初始化 LRU 缓存
    let cache_memory = args.cache_memory_mb * 1024 * 1024;
    let cache = LruCache::new(args.cache_capacity, cache_memory);
    {
        let obj_list = store.list();
        log::info!(
            "Cache warmup: {} objects in store, cache ready (capacity={})",
            obj_list.len(),
            args.cache_capacity
        );
    }

    // 创建共享内存区域（先 unlink 清理残留）
    {
        use std::ffi::CString;
        let cname = CString::new(args.shm_name.as_str()).unwrap();
        unsafe { libc::shm_unlink(cname.as_ptr()) };
    }
    let slot_size = 256u32;
    let region = ShmRegion::create(&args.shm_name, args.shm_pages, args.max_clients, slot_size)
        .unwrap_or_else(|e| {
            log::error!("Cannot create shared memory: {e}");
            std::process::exit(1);
        });
    log::info!(
        "Shared memory '{}' created: {} data pages",
        args.shm_name, args.shm_pages
    );

    // 初始化页分配器
    let total_pages = region.header().total_pages;
    let free_pages_offset = 16usize;
    let free_pages_ptr = unsafe {
        (region.data_area() as *mut u8)
            .sub(consts::SHM_PAGE_SIZE as usize)
            .add(free_pages_offset) as *mut u32
    };
    unsafe { *free_pages_ptr = total_pages; }

    let page_alloc = unsafe { PageAllocator::new(region.bitmap_ptr(), total_pages, free_pages_ptr) };

    let bitmap_size = region.header().page_bitmap_size as usize;
    unsafe {
        std::ptr::write_bytes(region.bitmap_ptr(), 0xFF, bitmap_size);
        let valid_in_last = total_pages % 8;
        if valid_in_last != 0 {
            *region.bitmap_ptr().add(bitmap_size - 1) = (1u8 << valid_in_last) - 1;
        }
    }

    let state: SharedState = Arc::new(Mutex::new(ServerState {
        store,
        cache,
        region,
        page_alloc,
    }));

    // 清理旧的 socket
    let _ = std::fs::remove_file(&args.socket_path);
    let listener = UnixListener::bind(&args.socket_path).unwrap_or_else(|e| {
        log::error!("Cannot bind to {}: {}", args.socket_path, e);
        std::process::exit(1);
    });
    log::info!("Listening on {}", args.socket_path);

    listener.set_nonblocking(true).expect("set nonblocking");

    // 主事件循环（纯轮询，无阻塞调用）
    loop {
        if daemon::is_shutdown_requested() {
            log::info!("Shutdown signal received, exiting...");
            break;
        }

        // 接收新连接
        match listener.accept() {
            Ok((stream, addr)) => {
                log::debug!("New connection: {:?}", addr);
                let state = Arc::clone(&state);
                std::thread::spawn(move || {
                    handle_client(stream, state);
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                log::error!("Accept error: {e}");
                break;
            }
        }

        std::thread::sleep(Duration::from_millis(50));
    }

    // 优雅关闭
    log::info!("Shutting down...");
    if let Ok(state) = Arc::try_unwrap(state) {
        let mut state = state.into_inner().unwrap();
        state.store.flush().ok();
        let _ = state.region.destroy();
    }
    let _ = std::fs::remove_file(&args.socket_path);
    daemon::remove_pidfile(&args.pidfile);
    log::info!("MinOS server stopped.");
}

// ─── Socket 客户端处理 ───

/// 处理 Unix Socket 客户端连接。
///
/// 协议（空格分隔的文本行）：
/// - PUT <name> <size> <content_type> <tags> <start_page> <num_pages>
/// - GET <uuid_or_name>
/// - DELETE <uuid>
/// - LIST
/// - STATUS
/// - STOP
fn handle_client(mut stream: UnixStream, state: SharedState) {
    let mut buf = [0u8; 4096];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return,
    };

    let msg = String::from_utf8_lossy(&buf[..n]);
    let response = dispatch_command(msg.trim(), &state);
    let _ = stream.write_all(response.as_bytes());
}

/// 命令分发。
fn dispatch_command(msg: &str, state: &SharedState) -> String {
    let parts: Vec<&str> = msg.splitn(7, ' ').collect();
    if parts.is_empty() {
        return "ERROR empty command\n".into();
    }

    match parts[0].to_uppercase().as_str() {
        "PUT" => cmd_put_socket(&parts, state),
        "GET" => cmd_get_socket(&parts, state),
        "DELETE" => cmd_delete_socket(&parts, state),
        "LIST" => cmd_list_socket(state),
        "STATUS" => cmd_status_socket(state),
        "STOP" => {
            use std::sync::atomic::AtomicBool;
            // 通过 daemon 模块的原子标志设置关闭信号
            unsafe {
                let ptr = &daemon::is_shutdown_requested as *const _ as *mut AtomicBool;
                (*ptr).store(true, std::sync::atomic::Ordering::SeqCst);
            }
            "OK shutting down\n".into()
        }
        _ => format!("ERROR unknown command '{}'\n", parts[0]),
    }
}

/// PUT: 从共享内存读取数据，写入 store.odb。
/// 格式: PUT <name> <size> <content_type> <tags> <start_page> <num_pages>
fn cmd_put_socket(parts: &[&str], state: &SharedState) -> String {
    if parts.len() < 7 {
        return "ERROR PUT requires: name size content_type tags start_page num_pages\n".into();
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

    // 从共享内存读取数据
    let data = server_state.region.read_from_pages(start_page, size);

    // 写入存储引擎
    match server_state.store.put(name, &data, content_type, tags) {
        Ok(uuid) => {
            // 释放共享内存页
            server_state.page_alloc.free_pages(start_page, num_pages);
            // 更新缓存
            server_state.cache.put(uuid, data, name.into(), size);
            format!("OK {}\n", uuid_fmt(&uuid))
        }
        Err(e) => {
            server_state.page_alloc.free_pages(start_page, num_pages);
            format!("ERROR {e}\n")
        }
    }
}

/// GET: 从 store.odb 读取数据，写入共享内存返回。
/// 格式: GET <uuid_or_name>
fn cmd_get_socket(parts: &[&str], state: &SharedState) -> String {
    if parts.len() < 2 {
        return "ERROR GET requires uuid or name\n".into();
    }
    let id = parts[1];
    let mut server_state = state.lock().unwrap();

    // 尝试解析为 UUID
    let result = if let Some(uuid) = parse_uuid(id) {
        // 先查缓存，再查存储
        let cached = server_state.cache.get(&uuid).map(|d| d.to_vec());
        if let Some(data) = cached {
            write_get_result(&mut server_state, data)
        } else {
            match server_state.store.get_by_id(&uuid) {
                Ok(Some(obj)) => {
                    let sz = obj.summary.size;
                    server_state
                        .cache
                        .put(obj.summary.uuid, obj.data.clone(), obj.summary.name, sz);
                    write_get_result(&mut server_state, obj.data)
                }
                Ok(None) => "ERROR object not found\n".into(),
                Err(e) => format!("ERROR {e}\n"),
            }
        }
    } else {
        // 按名称查找
        match server_state.store.get_by_name(id) {
            Ok(Some(obj)) => {
                let sz = obj.summary.size;
                server_state
                    .cache
                    .put(obj.summary.uuid, obj.data.clone(), obj.summary.name, sz);
                write_get_result(&mut server_state, obj.data)
            }
            Ok(None) => "ERROR object not found\n".into(),
            Err(e) => format!("ERROR {e}\n"),
        }
    };
    result
}

/// DELETE: 删除对象。
/// 格式: DELETE <uuid>
fn cmd_delete_socket(parts: &[&str], state: &SharedState) -> String {
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
            server_state.cache.invalidate(&uuid);
            "OK deleted\n".into()
        }
        Ok(false) => "ERROR object not found\n".into(),
        Err(e) => format!("ERROR {e}\n"),
    }
}

/// LIST: 列出所有对象。
fn cmd_list_socket(state: &SharedState) -> String {
    let server_state = state.lock().unwrap();
    let objects = server_state.store.list();
    let mut resp = format!("OK {}\n", objects.len());
    for obj in &objects {
        resp.push_str(&format!(
            "{} {} {} {} {}\n",
            uuid_fmt(&obj.uuid),
            obj.name,
            obj.size,
            obj.content_type,
            obj.created_at,
        ));
    }
    resp
}

/// STATUS: 查询服务状态。
fn cmd_status_socket(state: &SharedState) -> String {
    let server_state = state.lock().unwrap();
    let stats = server_state.store.stats();
    let cache_stats = server_state.cache.stats();
    let header = server_state.region.header();
    format!(
        "OK\nstore_objects: {}\nstore_blocks_free: {}/{}\nstore_file_size: {}\n\
         cache_entries: {}/{}\ncache_hit_rate: {:.2}\nshm_pages_free: {}/{}\n",
        stats.total_objects,
        stats.free_blocks,
        stats.total_blocks,
        stats.file_size,
        cache_stats.size,
        cache_stats.capacity,
        cache_stats.hit_rate,
        header.free_pages,
        header.total_pages,
    )
}

// ─── 辅助函数 ───

/// 将对象数据写入共享内存并返回响应字符串。
fn write_get_result(state: &mut ServerState, data: Vec<u8>) -> String {
    if data.is_empty() {
        return "OK 0 0 0\n".into();
    }
    let pages_needed = ((data.len() as u64 + 4095) / 4096) as u32;
    match state.page_alloc.alloc_pages(pages_needed) {
        Some(start_page) => {
            state.region.write_to_pages(start_page, &data);
            format!("OK {} {} {}\n", data.len(), start_page, pages_needed)
        }
        None => "ERROR no free shm pages\n".into(),
    }
}

/// 格式化 UUID。
fn uuid_fmt(uuid: &[u8; 16]) -> String {
    uuid.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

/// 尝试将十六进制字符串解析为 UUID 字节数组。
fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 32 {
        return None;
    }
    let mut uuid = [0u8; 16];
    for i in 0..16 {
        uuid[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(uuid)
}
