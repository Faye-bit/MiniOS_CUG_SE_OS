//! minios 客户端 — 命令行接口。
//!
//! 命令：
//! - put <file>       上传文件（自动分块传输）
//! - get <id>         下载对象
//! - info <id>        查看对象元数据
//! - delete <uuid>    删除对象
//! - list             列出所有对象
//! - status           查看服务端状态

use clap::{Parser, Subcommand};
use minios_lib::common::consts;
use minios_lib::shm::page::PageAllocator;
use minios_lib::shm::queue::ShmQueue;
use minios_lib::shm::region::ShmRegion;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Stdio;

#[derive(Parser, Debug)]
#[command(name = "minios", version, about = "minios object storage client")]
struct Cli {
    #[arg(long, default_value = consts::DEFAULT_SOCKET_PATH)]
    socket: String,
    #[arg(long, default_value = consts::DEFAULT_SHM_NAME)]
    shm_name: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Put {
        file: PathBuf,
        #[arg(short, long)]
        name: Option<String>,
        #[arg(long, default_value = "application/octet-stream")]
        content_type: String,
        #[arg(long, default_value = "{}")]
        tags: String,
        /// 强制覆盖已存在的同名对象
        #[arg(short, long, default_value_t = false)]
        force: bool,
    },
    Get {
        id: String,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// 查看对象元数据（不下载数据）
    Info {
        id: String,
    },
    Delete {
        uuid: String,
    },
    List,
    Status,
    /// 启动服务端
    Start {
        #[arg(long)]
        server: Option<PathBuf>,
        #[arg(long, default_value = consts::DEFAULT_STORE_PATH)]
        store_path: String,
        #[arg(long, default_value_t = false)]
        daemon: bool,
        #[arg(long, default_value = "/tmp/minios.pid")]
        pidfile: String,
        #[arg(long, default_value = "/tmp/minios.log")]
        log_file: PathBuf,
    },
    /// 停止服务端
    Stop,
}

fn main() {
    let cli = Cli::parse();
    match &cli.command {
        Command::Status => cmd_status(&cli),
        Command::List => cmd_list(&cli),
        Command::Put {
            file,
            name,
            content_type,
            tags,
            force,
        } => cmd_put(&cli, file, name, content_type, tags, *force),
        Command::Get { id, output } => cmd_get(&cli, id, output),
        Command::Info { id } => cmd_info(&cli, id),
        Command::Delete { uuid } => cmd_delete(&cli, uuid),
        Command::Start {
            server,
            store_path,
            daemon,
            pidfile,
            log_file,
        } => {
            cmd_start(&cli, server, store_path, *daemon, pidfile, log_file)
        }
        Command::Stop => cmd_stop(&cli),
    }
}

fn cmd_start(
    cli: &Cli,
    server: &Option<PathBuf>,
    store_path: &str,
    daemon: bool,
    pidfile: &str,
    log_file: &PathBuf,
) {
    let server_path = server.clone().unwrap_or_else(default_server_path);
    let mut cmd = std::process::Command::new(&server_path);
    cmd.arg("--store-path")
        .arg(store_path)
        .arg("--socket-path")
        .arg(&cli.socket)
        .arg("--shm-name")
        .arg(&cli.shm_name)
        .arg("--pidfile")
        .arg(pidfile);

    if daemon {
        cmd.arg("--daemon");
    } else {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file)
            .unwrap_or_else(|e| {
                eprintln!("ERROR: cannot open log file '{}': {e}", log_file.display());
                std::process::exit(1);
            });
        let log_err = log.try_clone().unwrap_or_else(|e| {
            eprintln!("ERROR: cannot clone log file handle: {e}");
            std::process::exit(1);
        });
        cmd.stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err));
    }

    let child = cmd.spawn().unwrap_or_else(|e| {
        eprintln!("ERROR: cannot start server '{}': {e}", server_path.display());
        std::process::exit(1);
    });
    println!("OK server started pid={}", child.id());
}

fn default_server_path() -> PathBuf {
    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            return dir.join("minios-server");
        }
    }
    PathBuf::from("minios-server")
}

fn cmd_stop(cli: &Cli) {
    let resp = queue_cmd(&cli.socket, &cli.shm_name, "STOP\n");
    print!("{resp}");
}

// ─── 基础设施 ───

fn socket_cmd(socket_path: &str, command: &str) -> String {
    let mut stream = UnixStream::connect(socket_path).unwrap_or_else(|e| {
        eprintln!("ERROR: cannot connect to server at '{socket_path}': {e}");
        eprintln!("Is the minios-server running?");
        std::process::exit(1);
    });
    stream.write_all(command.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

/// 通过共享内存请求/响应队列发送命令（基于信号量的有界缓冲区）。
///
/// 如果服务端启用了共享内存队列（--shm-queue-slots > 0），则使用信号量
/// 同步的请求/响应环形缓冲区进行通信；否则自动回退到 Unix Socket。
///
/// ## 流程
/// 1. 打开共享内存 → 检查队列魔数
/// 2. 如果队列可用：push_request → pop_response
/// 3. 如果队列不可用或出错：fallback 到 socket_cmd
fn queue_cmd(socket_path: &str, shm_name: &str, command: &str) -> String {
    // 尝试打开共享内存
    let region = match ShmRegion::open(shm_name) {
        Ok(r) => r,
        Err(_) => return queue_cmd(socket_path, shm_name, command),
    };

    // 检查队列是否可用（魔数 "MOSQ"）
    let ctrl_page = unsafe { region.data_area().sub(consts::SHM_PAGE_SIZE as usize) };
    if !ShmQueue::is_available(ctrl_page, region.header()) {
        drop(region);
        return queue_cmd(socket_path, shm_name, command);
    }

    // 打开队列（复用已有的信号量和互斥锁）
    let queue = match ShmQueue::open(ctrl_page, region.header(), shm_name) {
        Ok(q) => q,
        Err(_) => {
            drop(region);
            return queue_cmd(socket_path, shm_name, command);
        }
    };

    // 发送请求（P(req_empty) → lock → write → unlock → V(req_full)）
    let client_id = std::process::id();
    if let Err(_e) = queue.push_request(client_id, command) {
        let _ = queue.close();
        drop(region);
        return queue_cmd(socket_path, shm_name, command);
    }

    // 接收响应（P(resp_full) → lock → read → unlock → V(resp_empty)）
    let response = match queue.pop_response() {
        Ok(resp) => resp.response_str().to_string(),
        Err(_e) => {
            let _ = queue.close();
            drop(region);
            return queue_cmd(socket_path, shm_name, command);
        }
    };

    // 清理
    let _ = queue.close();
    drop(region);
    response
}

fn open_shm(name: &str) -> ShmRegion {
    ShmRegion::open(name).unwrap_or_else(|e| {
        eprintln!("ERROR: cannot open shared memory '{name}': {e}");
        eprintln!("Is the minios-server running?");
        std::process::exit(1);
    })
}

fn make_page_alloc(region: &ShmRegion) -> PageAllocator {
    let total_pages = region.header().total_pages;
    let free_pages_offset = 16usize;
    let free_pages_ptr = unsafe {
        (region.data_area() as *mut u8)
            .sub(consts::SHM_PAGE_SIZE as usize)
            .add(free_pages_offset) as *mut u32
    };
    unsafe { PageAllocator::new(region.bitmap_ptr(), total_pages, free_pages_ptr) }
}

// ─── 命令实现 ───

fn cmd_status(cli: &Cli) {
    let resp = queue_cmd(&cli.socket, &cli.shm_name, "STATUS\n");
    print!("{resp}");
}

fn cmd_list(cli: &Cli) {
    let resp = queue_cmd(&cli.socket, &cli.shm_name, "LIST\n");
    print!("{resp}");
}

fn cmd_put(cli: &Cli, file: &PathBuf, name: &Option<String>, content_type: &str, tags: &str, force: bool) {
    let obj_name = name
        .clone()
        .unwrap_or_else(|| file.file_name().unwrap().to_string_lossy().to_string());

    let data = match std::fs::read(file) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ERROR: cannot read file '{}': {e}", file.display());
            std::process::exit(1);
        }
    };

    let total_size = data.len();
    let tags_safe = tags.replace(' ', "_");

    if total_size == 0 {
        let cmd = format!("PUT {obj_name} 0 {content_type} {tags_safe} 0 0\n");
        let resp = queue_cmd(&cli.socket, &cli.shm_name, &cmd);
        print!("{resp}");
        return;
    }

    let region = open_shm(&cli.shm_name);
    let total_pages = region.header().total_pages;

    // 计算可用 chunk 大小（保留少量页避免碎片问题）
    let shm_capacity = total_pages as usize * consts::SHM_PAGE_SIZE as usize;

    // 小文件：单次 PUT
    if total_size <= shm_capacity {
        // 锁定页互斥锁，保护分配和写入的原子性
        region.lock_page_mutex().unwrap();
        let page_alloc = make_page_alloc(&region);
        let pages_needed = ((total_size as u64 + 4095) / 4096) as u32;
        let start_page = page_alloc
            .alloc_pages_wait(
                pages_needed,
                || region.unlock_page_mutex().unwrap(),
                || region.lock_page_mutex().unwrap(),
            )
            .unwrap_or_else(|| {
                region.unlock_page_mutex().unwrap();
                eprintln!("ERROR: invalid shared memory request (need {pages_needed} pages)");
                std::process::exit(1);
            });

        region.write_to_pages(start_page, &data);
        region.unlock_page_mutex().unwrap();

        let put_cmd = if force { "PUT_FORCE" } else { "PUT" };
        let cmd = format!(
            "{put_cmd} {obj_name} {total_size} {content_type} {tags_safe} {start_page} {pages_needed}\n"
        );
        let resp = queue_cmd(&cli.socket, &cli.shm_name, &cmd);
        print!("{resp}");
        // 注意：页由服务端在 cmd_put 中释放，客户端不重复释放，
        // 否则并发场景下其他客户端可能已重新分配该页，导致竞态损坏。
    } else {
        // 大文件：分块传输
        log_chunked_upload(&cli.socket, &cli.shm_name, &region, &obj_name, &data, content_type, &tags_safe, force);
    }

    drop(region);
}

/// 分块上传大文件。
fn log_chunked_upload(
    socket_path: &str,
    shm_name: &str,
    region: &ShmRegion,
    name: &str,
    data: &[u8],
    content_type: &str,
    tags: &str,
    force: bool,
) {
    let total_size = data.len();
    let total_pages = region.header().total_pages as usize;
    let page_size = consts::SHM_PAGE_SIZE as usize;
    // 每次传输使用最多 90% 的共享内存页
    let max_chunk_bytes = (total_pages * 9 / 10) * page_size;

    // 1. PUT_BEGIN
    let resp = socket_cmd(
        socket_path,
        &format!("PUT_BEGIN {name} {total_size} {content_type} {tags}\n"),
    );
    if !resp.starts_with("OK") {
        eprint!("{resp}");
        return;
    }

    // 2. 循环 PUT_CHUNK
    let mut offset = 0;
    while offset < total_size {
        let chunk_end = (offset + max_chunk_bytes).min(total_size);
        let chunk = &data[offset..chunk_end];
        let chunk_size = chunk.len();

        // 锁定页互斥锁，分配页并写入数据
        region.lock_page_mutex().unwrap();
        let page_alloc = make_page_alloc(region);
        let pages_needed = ((chunk_size as u64 + 4095) / 4096) as u32;
        let start_page = page_alloc
            .alloc_pages_wait(
                pages_needed,
                || region.unlock_page_mutex().unwrap(),
                || region.lock_page_mutex().unwrap(),
            )
            .unwrap_or_else(|| {
                region.unlock_page_mutex().unwrap();
                eprintln!("ERROR: invalid shared memory request (need {pages_needed} pages)");
                std::process::exit(1);
            });

        region.write_to_pages(start_page, chunk);
        region.unlock_page_mutex().unwrap();

        let resp = socket_cmd(
            socket_path,
            &format!("PUT_CHUNK {name} {chunk_size} {start_page} {pages_needed}\n"),
        );
        if !resp.starts_with("OK") {
            eprint!("{resp}");
            // 服务端报错时未释放页，客户端自行清理
            region.lock_page_mutex().unwrap();
            page_alloc.free_pages(start_page, pages_needed);
            region.unlock_page_mutex().unwrap();
            return;
        }

        // 正常路径：页已由服务端在 cmd_put_chunk 中释放，客户端不重复释放
        offset = chunk_end;

        eprint!(
            "\rUploading {name}: {}/{} bytes ({:.0}%)",
            offset,
            total_size,
            offset as f64 / total_size as f64 * 100.0
        );
    }
    eprintln!();

    // 3. PUT_END（或 PUT_END_FORCE）
    let end_cmd = if force { "PUT_END_FORCE" } else { "PUT_END" };
    let resp = queue_cmd(socket_path, shm_name, &format!("{end_cmd} {name}\n"));
    print!("{resp}");
}

fn cmd_get(cli: &Cli, id: &str, output: &Option<PathBuf>) {
    let cmd = format!("GET {id}\n");
    let resp = queue_cmd(&cli.socket, &cli.shm_name, &cmd);
    let resp = resp.trim();

    if resp.starts_with("ERROR") {
        eprintln!("{resp}");
        std::process::exit(1);
    }

    // 格式: OK <size> <start_page> <num_pages>
    let parts: Vec<&str> = resp.split_whitespace().collect();
    if parts.len() < 4 || parts[0] != "OK" {
        eprintln!("ERROR: unexpected response: {resp}");
        std::process::exit(1);
    }

    let size: u64 = parts[1].parse().unwrap_or(0);
    if size == 0 {
        eprintln!("OK (empty object)");
        return;
    }

    let start_page: u32 = parts[2].parse().unwrap_or(0);
    let num_pages: u32 = parts[3].parse().unwrap_or(0);

    let region = open_shm(&cli.shm_name);

    // 锁定页互斥锁，保护读取和释放的原子性
    region.lock_page_mutex().unwrap();
    let data = region.read_from_pages(start_page, size);
    let page_alloc = make_page_alloc(&region);
    page_alloc.free_pages(start_page, num_pages);
    region.unlock_page_mutex().unwrap();

    drop(region);

    if let Some(out_path) = output {
        std::fs::write(out_path, &data).unwrap_or_else(|e| {
            eprintln!("ERROR writing output file: {e}");
            std::process::exit(1);
        });
        eprintln!("OK: {} bytes written to {}", data.len(), out_path.display());
    } else {
        std::io::stdout().write_all(&data).unwrap();
    }
}

fn cmd_info(cli: &Cli, id: &str) {
    let cmd = format!("INFO {id}\n");
    let resp = queue_cmd(&cli.socket, &cli.shm_name, &cmd);
    print!("{resp}");
}

fn cmd_delete(cli: &Cli, uuid: &str) {
    let cmd = format!("DELETE {uuid}\n");
    let resp = queue_cmd(&cli.socket, &cli.shm_name, &cmd);
    print!("{resp}");
}
