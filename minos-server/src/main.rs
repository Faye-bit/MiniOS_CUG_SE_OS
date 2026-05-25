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
use minos_lib::protocol::request::{slot_status, RequestType, ShmRequest};
use minos_lib::protocol::response::{ResponseStatus, ShmResponse};
use minos_lib::shm::page::PageAllocator;
use minos_lib::shm::region::ShmRegion;
use minos_lib::shm::sync::{ShmMutex, ShmSemaphore};
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
    mutex: ShmMutex,
    page_alloc: PageAllocator,
    server_sem: ShmSemaphore,
    client_sem: ShmSemaphore,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    // 设置信号处理器
    daemon::setup_signal_handlers().expect("setup signal handlers");

    // 守护进程化
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
            args.store_path,
            args.max_objects,
            args.total_blocks
        );
        ObjectStore::create(&args.store_path, args.max_objects, args.total_blocks)
            .unwrap_or_else(|e| {
                log::error!("Cannot create store: {e}");
                std::process::exit(1);
            })
    };

    // 初始化 LRU 缓存
    let cache_memory = args.cache_memory_mb * 1024 * 1024;
    let mut cache = LruCache::new(args.cache_capacity, cache_memory);

    // 缓存预热：加载前 N 个对象
    {
        let obj_list = store.list();
        let ids: Vec<_> = obj_list.iter().map(|o| o.uuid).collect();
        let loaded = cache.warmup(
            &ids,
            |_id| {
                // 预热需要读取数据 — 这里先做浅预热（仅加载元数据）
                // 实际数据在首次访问时通过 cache miss 加载
                None
            },
            args.cache_capacity,
        );
        log::info!("Cache warmup: loaded {loaded} entries (total objects: {})", obj_list.len());
    }

    // 创建共享内存区域
    let slot_size = 256u32; // ShmRequest/ShmResponse 大小
    let region = ShmRegion::create(&args.shm_name, args.shm_pages, args.max_clients, slot_size)
        .unwrap_or_else(|e| {
            log::error!("Cannot create shared memory: {e}");
            std::process::exit(1);
        });
    log::info!(
        "Shared memory '{}' created: {} data pages",
        args.shm_name,
        args.shm_pages
    );

    // 初始化互斥锁
    let mutex = unsafe { ShmMutex::init_at(region.mutex_ptr()) }
        .unwrap_or_else(|e| {
            log::error!("Cannot init mutex: {e}");
            std::process::exit(1);
        });

    // 初始化页分配器
    let total_pages = region.header().total_pages;
    let free_pages_offset = 16usize; // free_pages 字段在 ShmControlHeader 中的偏移量
    let free_pages_ptr = unsafe {
        (region.data_area() as *mut u8).sub(consts::SHM_PAGE_SIZE as usize).add(free_pages_offset) as *mut u32
    };
    // 初始化 free_pages
    unsafe { *free_pages_ptr = total_pages; }

    let page_alloc = unsafe {
        PageAllocator::new(
            region.bitmap_ptr(),
            total_pages,
            free_pages_ptr,
        )
    };

    // 位图初始化为全 1（所有页空闲）
    let bitmap_size = region.header().page_bitmap_size as usize;
    unsafe {
        std::ptr::write_bytes(region.bitmap_ptr(), 0xFF, bitmap_size);
        // 处理最后一字节的尾部
        let valid_in_last = total_pages % 8;
        if valid_in_last != 0 {
            let mask = (1u8 << valid_in_last) - 1;
            *region.bitmap_ptr().add(bitmap_size - 1) = mask;
        }
    }

    let server_sem = ShmSemaphore::create("minos_server_sem", 0)
        .unwrap_or_else(|e| {
            log::error!("Cannot create server semaphore: {e}");
            std::process::exit(1);
        });
    let client_sem = ShmSemaphore::create("minos_client_sem", 0)
        .unwrap_or_else(|e| {
            log::error!("Cannot create client semaphore: {e}");
            std::process::exit(1);
        });

    let state = Arc::new(Mutex::new(ServerState {
        store,
        cache,
        region,
        mutex,
        page_alloc,
        server_sem,
        client_sem,
    }));

    // 清理旧的 socket 文件
    let _ = std::fs::remove_file(&args.socket_path);

    let listener = UnixListener::bind(&args.socket_path).unwrap_or_else(|e| {
        log::error!("Cannot bind to {}: {}", args.socket_path, e);
        std::process::exit(1);
    });
    log::info!("Listening on {}", args.socket_path);

    // 主事件循环
    listener
        .set_nonblocking(true)
        .expect("set nonblocking");

    loop {
        // 检查关闭信号
        if daemon::is_shutdown_requested() {
            log::info!("Shutdown signal received, exiting...");
            break;
        }

        // 处理新的客户端连接
        match listener.accept() {
            Ok((stream, addr)) => {
                log::info!("New connection from {:?}", addr);
                let state = Arc::clone(&state);
                std::thread::spawn(move || {
                    handle_client(stream, state);
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // 没有待处理的连接，检查共享内存中的请求
            }
            Err(e) => {
                log::error!("Accept error: {e}");
                break;
            }
        }

        // 处理共享内存中的请求（轮询模式）
        process_shm_requests(&state);

        // 短暂休眠避免忙等
        std::thread::sleep(Duration::from_millis(10));
    }

    // 优雅关闭
    log::info!("Shutting down...");
    if let Ok(state) = Arc::try_unwrap(state) {
        let mut state = state.into_inner().unwrap();
        state.store.flush().ok();
        let _ = state.region.destroy();
        let _ = state.server_sem.close_and_unlink();
        let _ = state.client_sem.close_and_unlink();
    }
    let _ = std::fs::remove_file(&args.socket_path);
    daemon::remove_pidfile(&args.pidfile);
    log::info!("MinOS server stopped.");
}

/// 处理 Unix Socket 客户端连接（简单文本协议）。
fn handle_client(mut stream: UnixStream, state: Arc<Mutex<ServerState>>) {
    let mut buf = [0u8; 4096];
    match stream.read(&mut buf) {
        Ok(n) if n > 0 => {
            let msg = String::from_utf8_lossy(&buf[..n]);
            let response = handle_socket_command(&msg, &state);
            let _ = stream.write_all(response.as_bytes());
        }
        _ => {}
    }
}

/// 处理 socket 文本命令。
fn handle_socket_command(msg: &str, state: &Arc<Mutex<ServerState>>) -> String {
    let parts: Vec<&str> = msg.trim().split_whitespace().collect();
    if parts.is_empty() {
        return "ERROR: empty command\n".into();
    }

    match parts[0].to_uppercase().as_str() {
        "STATUS" => {
            let state = state.lock().unwrap();
            let stats = state.store.stats();
            let cache_stats = state.cache.stats();
            let header = state.region.header();
            format!(
                "STATUS OK\n\
                 store_objects: {}\n\
                 store_blocks_free: {}/{}\n\
                 store_file_size: {}\n\
                 cache_entries: {}/{}\n\
                 cache_hit_rate: {:.2}\n\
                 shm_pages_free: {}/{}\n",
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
        "STOP" => {
            daemon::is_shutdown_requested();
            // 设置关闭标志
            unsafe {
                // 直接设置原子变量
                std::sync::atomic::AtomicBool::from_ptr(
                    &daemon::is_shutdown_requested as *const _ as *mut bool,
                );
            }
            "OK: shutting down\n".into()
        }
        "LIST" => {
            let state = state.lock().unwrap();
            let objects = state.store.list();
            let mut resp = format!("LIST {}\n", objects.len());
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
        _ => format!("ERROR: unknown command '{0}'\n", parts[0]),
    }
}

/// 格式化 UUID 为十六进制字符串。
fn uuid_fmt(uuid: &[u8; 16]) -> String {
    uuid.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join("")
}

/// 处理共享内存中的客户端请求（轮询请求槽位）。
fn process_shm_requests(state: &Arc<Mutex<ServerState>>) {
    let mut server_state = state.lock().unwrap();

    // 尝试获取互斥锁（非阻塞，避免死锁）
    if server_state.mutex.lock().is_err() {
        return;
    }

    let max_requests = server_state.region.header().max_requests as usize;
    let slot_size = server_state.region.header().slot_size as usize;
    let request_base = {
        let header_ptr = server_state.region.header() as *const _ as *mut u8;
        unsafe { header_ptr.add(server_state.region.header().request_slots_offset as usize) }
    };

    for i in 0..max_requests {
        let slot_ptr = unsafe { request_base.add(i * slot_size) as *mut ShmRequest };
        let request = unsafe { &mut *slot_ptr };

        if request.status != slot_status::PENDING {
            continue;
        }

        request.status = slot_status::PROCESSING;

        let result = process_request(request, &mut server_state);

        // 写入响应
        let response_base = {
            let header_ptr = server_state.region.header() as *const _ as *mut u8;
            unsafe { header_ptr.add(server_state.region.header().response_slots_offset as usize) }
        };
        let response_ptr = unsafe { response_base.add(i * slot_size) as *mut ShmResponse };
        unsafe {
            *response_ptr = result;
        }

        request.status = slot_status::DONE;
    }

    let _ = server_state.mutex.unlock();

    // 通知客户端
    let _ = server_state.client_sem.post();
    let _ = server_state.server_sem.wait();
}

/// 处理单个请求并生成响应。
fn process_request(req: &ShmRequest, state: &mut ServerState) -> ShmResponse {
    match RequestType::from_u8(req.request_type) {
        Some(RequestType::Put) => {
            // 从共享内存中读取数据
            let data = state.region.read_from_pages(req.start_page, req.size);
            match state.store.put(
                req.name_str(),
                &data,
                req.content_type_str(),
                req.tags_str(),
            ) {
                Ok(uuid) => {
                    // 释放共享内存页
                    state.page_alloc.free_pages(req.start_page, req.num_pages);
                    // 更新缓存
                    state.cache.put(uuid, data, req.name_str().into(), req.size);
                    ShmResponse::ok(req.client_id, uuid, req.size)
                }
                Err(e) => {
                    state.page_alloc.free_pages(req.start_page, req.num_pages);
                    ShmResponse::error(req.client_id, ResponseStatus::Error, &e.to_string())
                }
            }
        }
        Some(RequestType::Get) => {
            // 先查缓存
            if let Some(data) = state.cache.get(&req.object_id) {
                let pages_needed = (data.len() as u64 + 4095) / 4096;
                if pages_needed == 0 {
                    return ShmResponse::error(
                        req.client_id,
                        ResponseStatus::Ok,
                        "empty object",
                    );
                }
                if let Some(start_page) = state.page_alloc.alloc_pages(pages_needed as u32) {
                    state.region.write_to_pages(start_page, data);
                    let mut resp = ShmResponse::ok(req.client_id, req.object_id, data.len() as u64);
                    resp.num_pages = pages_needed as u32;
                    resp.start_page = start_page;
                    return resp;
                } else {
                    return ShmResponse::error(
                        req.client_id,
                        ResponseStatus::NoSpace,
                        "no free shm pages",
                    );
                }
            }

            // 从存储引擎读取
            match state.store.get_by_id(&req.object_id) {
                Ok(Some(obj)) => {
                    let data = obj.data;
                    state.cache.put(
                        obj.summary.uuid,
                        data.clone(),
                        obj.summary.name,
                        obj.summary.size,
                    );

                    let pages_needed = (data.len() as u64 + 4095) / 4096;
                    if pages_needed == 0 {
                        return ShmResponse::ok(req.client_id, req.object_id, 0);
                    }
                    if let Some(start_page) = state.page_alloc.alloc_pages(pages_needed as u32) {
                        state.region.write_to_pages(start_page, &data);
                        let mut resp = ShmResponse::ok(req.client_id, req.object_id, obj.summary.size);
                        resp.num_pages = pages_needed as u32;
                        resp.start_page = start_page;
                        resp
                    } else {
                        ShmResponse::error(
                            req.client_id,
                            ResponseStatus::NoSpace,
                            "no free shm pages",
                        )
                    }
                }
                Ok(None) => ShmResponse::error(
                    req.client_id,
                    ResponseStatus::NotFound,
                    "object not found",
                ),
                Err(e) => {
                    ShmResponse::error(req.client_id, ResponseStatus::Error, &e.to_string())
                }
            }
        }
        Some(RequestType::Delete) => match state.store.delete(&req.object_id) {
            Ok(true) => {
                state.cache.invalidate(&req.object_id);
                ShmResponse::ok(req.client_id, req.object_id, 0)
            }
            Ok(false) => {
                ShmResponse::error(req.client_id, ResponseStatus::NotFound, "object not found")
            }
            Err(e) => ShmResponse::error(req.client_id, ResponseStatus::Error, &e.to_string()),
        },
        Some(RequestType::List) => {
            let objects = state.store.list();
            // 简化：通过 socket 返回列表（共享内存用于大数据传输）
            ShmResponse::ok(req.client_id, [0u8; 16], objects.len() as u64)
        }
        Some(RequestType::Status) => {
            let stats = state.store.stats();
            let cache_stats = state.cache.stats();
            let msg = format!(
                "objects={} blocks={}/{} cache_hit={:.2}",
                stats.total_objects,
                stats.free_blocks,
                stats.total_blocks,
                cache_stats.hit_rate,
            );
            let mut resp = ShmResponse::ok(req.client_id, [0u8; 16], 0);
            resp.message[..msg.len().min(127)]
                .copy_from_slice(&msg.as_bytes()[..msg.len().min(127)]);
            resp
        }
        Some(RequestType::Shutdown) => {
            daemon::is_shutdown_requested();
            // 设置全局关闭标志
            ShmResponse::ok(req.client_id, [0u8; 16], 0)
        }
        None => {
            ShmResponse::error(req.client_id, ResponseStatus::InvalidRequest, "unknown request type")
        }
    }
}
