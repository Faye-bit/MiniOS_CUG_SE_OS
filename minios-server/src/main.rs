//! minios 对象存储服务端 — 守护进程入口。

use clap::Parser;
use minios_lib::cache::lru::LruCache;
use minios_lib::common::consts;
use minios_lib::daemon;
use minios_lib::shm::page::PageAllocator;
use minios_lib::shm::region::ShmRegion;
use minios_lib::storage::engine::ObjectStore;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "minios-server", version, about = "minios object storage daemon")]
struct Args {
    #[arg(long, default_value = consts::DEFAULT_STORE_PATH)]
    store_path: String,
    #[arg(long, default_value = consts::DEFAULT_SOCKET_PATH)]
    socket_path: String,
    #[arg(long, default_value = consts::DEFAULT_SHM_NAME)]
    shm_name: String,
    #[arg(long, default_value_t = consts::DEFAULT_SHM_PAGES)]
    shm_pages: u32,
    #[arg(long, default_value_t = consts::DEFAULT_CACHE_CAPACITY)]
    cache_capacity: usize,
    #[arg(long, default_value_t = 64)]
    cache_memory_mb: u64,
    #[arg(long, default_value_t = consts::DEFAULT_MAX_OBJECTS)]
    max_objects: u64,
    #[arg(long, default_value_t = consts::DEFAULT_TOTAL_BLOCKS)]
    total_blocks: u64,
    #[arg(long, default_value_t = consts::DEFAULT_MAX_CLIENTS as u32)]
    max_clients: u32,
    #[arg(long, default_value_t = false)]
    daemon: bool,
    #[arg(long, default_value = "/tmp/minios.pid")]
    pidfile: String,
}

/// 待完成的分块上传。
struct PendingUpload {
    data: Vec<u8>,
    content_type: String,
    tags: String,
}

struct ServerState {
    store: ObjectStore,
    cache: LruCache,
    region: ShmRegion,
    page_alloc: PageAllocator,
    /// 按名称索引的分块上传缓冲区
    pending_uploads: HashMap<String, PendingUpload>,
}

type SharedState = Arc<Mutex<ServerState>>;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    daemon::setup_signal_handlers().expect("setup signal handlers");

    if args.daemon {
        daemon::daemonize().expect("daemonize");
        daemon::write_pidfile(&args.pidfile).expect("write pidfile");
    }

    log::info!("minios server starting...");

    let mut store = if Path::new(&args.store_path).exists() {
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

    let cache_memory = args.cache_memory_mb * 1024 * 1024;
    let mut cache = LruCache::new(args.cache_capacity, cache_memory);
    {
        let obj_list = store.list();
        let ids: Vec<[u8; 16]> = obj_list.iter().map(|o| o.uuid).collect();
        let limit = args.cache_capacity.min(ids.len());
        let mut loaded = 0;
        for id in ids.iter().take(limit) {
            match store.get_by_id(id) {
                Ok(Some(obj)) => {
                    cache.put(obj.summary.uuid, obj.data, obj.summary.name, obj.summary.size);
                    loaded += 1;
                }
                Ok(None) => {}
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

    // 创建共享内存区域
    {
        use std::ffi::CString;
        let cname = CString::new(args.shm_name.as_str()).unwrap();
        unsafe { libc::shm_unlink(cname.as_ptr()) };
    }
    let region = ShmRegion::create(&args.shm_name, args.shm_pages)
        .unwrap_or_else(|e| {
            log::error!("Cannot create shared memory: {e}");
            std::process::exit(1);
        });
    log::info!(
        "Shared memory '{}' created: {} data pages",
        args.shm_name, args.shm_pages
    );

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
        pending_uploads: HashMap::new(),
    }));

    let _ = std::fs::remove_file(&args.socket_path);
    let listener = UnixListener::bind(&args.socket_path).unwrap_or_else(|e| {
        log::error!("Cannot bind to {}: {}", args.socket_path, e);
        std::process::exit(1);
    });
    log::info!("Listening on {}", args.socket_path);

    let active_clients = Arc::new(AtomicU32::new(0));
    let max_clients = args.max_clients;

    listener.set_nonblocking(true).expect("set nonblocking");

    loop {
        if daemon::is_shutdown_requested() {
            log::info!("Shutdown signal received, exiting...");
            break;
        }

        match listener.accept() {
            Ok((stream, addr)) => {
                let current = active_clients.load(Ordering::Relaxed);
                if current >= max_clients {
                    log::warn!("Rejecting connection from {:?}: server busy ({}/{})", addr, current, max_clients);
                    let mut stream = stream;
                    let _ = stream.write_all(b"ERROR server busy\n");
                    // stream dropped, closing connection
                } else {
                    active_clients.fetch_add(1, Ordering::Relaxed);
                    let state = Arc::clone(&state);
                    let active_clients = Arc::clone(&active_clients);
                    std::thread::spawn(move || {
                        handle_client(stream, state);
                        active_clients.fetch_sub(1, Ordering::Relaxed);
                    });
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                log::error!("Accept error: {e}");
                break;
            }
        }

        std::thread::sleep(Duration::from_millis(50));
    }

    log::info!("Shutting down...");

    // 首先关闭 listener，停止接受新连接
    drop(listener);
    // 短暂等待活动的工作线程完成
    std::thread::sleep(Duration::from_millis(200));

    // 尝试解锁 state 并清理
    match Arc::try_unwrap(state) {
        Ok(mutex) => {
            let mut inner = mutex.into_inner().unwrap();
            inner.store.flush().ok();
            let _ = inner.region.destroy();
            log::info!("Store flushed and shared memory destroyed.");
        }
        Err(arc) => {
            // 仍有活跃引用，通过 lock 强制清理
            log::warn!("Some client threads still active, forcing cleanup...");
            if let Ok(mut inner) = arc.lock() {
                inner.store.flush().ok();
            }
            // 注意：此时无法安全 destroy region（其他线程可能还在使用），
            // 但进程退出后内核会自动回收共享内存
        }
    }

    let _ = std::fs::remove_file(&args.socket_path);
    daemon::remove_pidfile(&args.pidfile);
    log::info!("minios server stopped.");
}

// ─── Socket 客户端处理 ───

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
        "PUT" => cmd_put(&parts, state),
        "PUT_BEGIN" => cmd_put_begin(&parts, state),
        "PUT_CHUNK" => cmd_put_chunk(&parts, state),
        "PUT_END" => cmd_put_end(&parts, state),
        "GET" => cmd_get(&parts, state),
        "DELETE" => cmd_delete(&parts, state),
        "LIST" => cmd_list(state),
        "STATUS" => cmd_status(state),
        "STOP" => {
            daemon::request_shutdown();
            "OK shutting down\n".into()
        }
        _ => format!("ERROR unknown command '{}'\n", parts[0]),
    }
}

// ─── 分块上传协议 ───

/// PUT_BEGIN <name> <total_size> <content_type> <tags>
/// 服务端为指定名称创建上传缓冲区。
fn cmd_put_begin(parts: &[&str], state: &SharedState) -> String {
    if parts.len() < 5 {
        return "ERROR PUT_BEGIN requires: name total_size content_type tags\n".into();
    }
    let name = parts[1].to_string();
    let total_size: usize = match parts[2].parse() {
        Ok(v) => v,
        Err(_) => return "ERROR invalid total_size\n".into(),
    };
    let content_type = parts[3].to_string();
    let tags = parts[4].to_string();

    let mut server_state = state.lock().unwrap();
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

/// PUT_CHUNK <name> <chunk_size> <start_page> <num_pages>
/// 从共享内存读取一块数据，追加到上传缓冲区。
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

    if !server_state.pending_uploads.contains_key(&name) {
        return "ERROR no pending upload for this name, send PUT_BEGIN first\n".into();
    }

    // 锁定页互斥锁后从共享内存读取块数据
    let chunk = {
        server_state.region.lock_page_mutex().unwrap();
        let c = server_state.region.read_from_pages(start_page, chunk_size);
        server_state.page_alloc.free_pages(start_page, num_pages);
        server_state.region.unlock_page_mutex().unwrap();
        c
    };

    // 追加到上传缓冲区
    let upload = server_state.pending_uploads.get_mut(&name).unwrap();
    upload.data.extend_from_slice(&chunk);

    log::info!(
        "PUT_CHUNK: name={name}, chunk_size={}, total_accumulated={}",
        chunk_size,
        upload.data.len()
    );
    "OK\n".into()
}

/// PUT_END <name>
/// 将累积的数据写入 store.odb，清理上传缓冲区。
fn cmd_put_end(parts: &[&str], state: &SharedState) -> String {
    if parts.len() < 2 {
        return "ERROR PUT_END requires: name\n".into();
    }
    let name = parts[1].to_string();

    let mut server_state = state.lock().unwrap();
    let upload = match server_state.pending_uploads.remove(&name) {
        Some(u) => u,
        None => return "ERROR no pending upload for this name\n".into(),
    };

    let size = upload.data.len() as u64;
    match server_state.store.put(&name, &upload.data, &upload.content_type, &upload.tags) {
        Ok(uuid) => {
            server_state
                .cache
                .put(uuid, upload.data, name.clone(), size);
            log::info!("PUT_END: name={name}, uuid={:?}, size={size}", uuid_fmt(&uuid));
            format!("OK {}\n", uuid_fmt(&uuid))
        }
        Err(e) => format!("ERROR {e}\n"),
    }
}

/// 标准 PUT（小文件单次传输，保留兼容）。
/// 格式: PUT <name> <size> <content_type> <tags> <start_page> <num_pages>
fn cmd_put(parts: &[&str], state: &SharedState) -> String {
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

    // 锁定页互斥锁，保护读取和释放操作的原子性
    server_state.region.lock_page_mutex().unwrap();
    let data = server_state.region.read_from_pages(start_page, size);
    server_state.page_alloc.free_pages(start_page, num_pages);
    server_state.region.unlock_page_mutex().unwrap();

    match server_state.store.put(name, &data, content_type, tags) {
        Ok(uuid) => {
            server_state.cache.put(uuid, data, name.into(), size);
            format!("OK {}\n", uuid_fmt(&uuid))
        }
        Err(e) => {
            format!("ERROR {e}\n")
        }
    }
}

// ─── GET / DELETE / LIST / STATUS ───

fn cmd_get(parts: &[&str], state: &SharedState) -> String {
    if parts.len() < 2 {
        return "ERROR GET requires uuid or name\n".into();
    }
    let id = parts[1];
    let mut server_state = state.lock().unwrap();

    if let Some(uuid) = parse_uuid(id) {
        if let Some(data) = server_state.cache.get(&uuid).map(|d| d.to_vec()) {
            return write_get_result(&mut server_state, data);
        }
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
    } else {
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
    }
}

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
            server_state.cache.invalidate(&uuid);
            "OK deleted\n".into()
        }
        Ok(false) => "ERROR object not found\n".into(),
        Err(e) => format!("ERROR {e}\n"),
    }
}

fn cmd_list(state: &SharedState) -> String {
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

fn cmd_status(state: &SharedState) -> String {
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

fn write_get_result(state: &mut ServerState, data: Vec<u8>) -> String {
    if data.is_empty() {
        return "OK 0 0 0\n".into();
    }
    let pages_needed = ((data.len() as u64 + 4095) / 4096) as u32;
    // 锁定页互斥锁，保护分配和写入的原子性
    state.region.lock_page_mutex().unwrap();
    let region = &state.region;
    let start_page = state.page_alloc.alloc_pages_wait(
        pages_needed,
        || region.unlock_page_mutex().unwrap(),
        || region.lock_page_mutex().unwrap(),
    );
    state.region.write_to_pages(start_page, &data);
    let result = format!("OK {} {} {}\n", data.len(), start_page, pages_needed);
    state.region.unlock_page_mutex().unwrap();
    result
}

fn uuid_fmt(uuid: &[u8; 16]) -> String {
    uuid.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

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
