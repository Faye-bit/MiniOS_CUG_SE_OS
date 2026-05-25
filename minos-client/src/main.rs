//! MinOS 客户端 — 命令行接口。
//!
//! 命令：
//! - put <file>       上传文件（自动分块传输）
//! - get <id>         下载对象
//! - delete <uuid>    删除对象
//! - list             列出所有对象
//! - status           查看服务端状态

use clap::{Parser, Subcommand};
use minos_lib::common::consts;
use minos_lib::shm::page::PageAllocator;
use minos_lib::shm::region::ShmRegion;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "minos", version, about = "MinOS object storage client")]
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
    },
    Get {
        id: String,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    Delete {
        uuid: String,
    },
    List,
    Status,
}

fn main() {
    let cli = Cli::parse();
    match &cli.command {
        Command::Status => cmd_status(&cli),
        Command::List => cmd_list(&cli),
        Command::Put { file, name, content_type, tags } => cmd_put(&cli, file, name, content_type, tags),
        Command::Get { id, output } => cmd_get(&cli, id, output),
        Command::Delete { uuid } => cmd_delete(&cli, uuid),
    }
}

// ─── 基础设施 ───

fn socket_cmd(socket_path: &str, command: &str) -> String {
    let mut stream = UnixStream::connect(socket_path).unwrap_or_else(|e| {
        eprintln!("ERROR: cannot connect to server at '{socket_path}': {e}");
        eprintln!("Is the minos-server running?");
        std::process::exit(1);
    });
    stream.write_all(command.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn open_shm(name: &str) -> ShmRegion {
    ShmRegion::open(name).unwrap_or_else(|e| {
        eprintln!("ERROR: cannot open shared memory '{name}': {e}");
        eprintln!("Is the minos-server running?");
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
    let resp = socket_cmd(&cli.socket, "STATUS\n");
    print!("{resp}");
}

fn cmd_list(cli: &Cli) {
    let resp = socket_cmd(&cli.socket, "LIST\n");
    print!("{resp}");
}

fn cmd_put(cli: &Cli, file: &PathBuf, name: &Option<String>, content_type: &str, tags: &str) {
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

    if data.is_empty() {
        eprintln!("ERROR: empty file");
        std::process::exit(1);
    }

    let total_size = data.len();
    let tags_safe = tags.replace(' ', "_");

    let region = open_shm(&cli.shm_name);
    let total_pages = region.header().total_pages;

    // 计算可用 chunk 大小（保留少量页避免碎片问题）
    let shm_capacity = total_pages as usize * consts::SHM_PAGE_SIZE as usize;

    // 小文件：单次 PUT
    if total_size <= shm_capacity {
        let page_alloc = make_page_alloc(&region);
        let pages_needed = ((total_size as u64 + 4095) / 4096) as u32;
        let start_page = page_alloc.alloc_pages(pages_needed).unwrap_or_else(|| {
            eprintln!("ERROR: not enough shared memory pages (need {pages_needed})");
            std::process::exit(1);
        });

        region.write_to_pages(start_page, &data);
        let cmd = format!(
            "PUT {obj_name} {total_size} {content_type} {tags_safe} {start_page} {pages_needed}\n"
        );
        let resp = socket_cmd(&cli.socket, &cmd);
        print!("{resp}");
        page_alloc.free_pages(start_page, pages_needed);
    } else {
        // 大文件：分块传输
        log_chunked_upload(&cli.socket, &region, &obj_name, &data, content_type, &tags_safe);
    }

    drop(region);
}

/// 分块上传大文件。
fn log_chunked_upload(
    socket_path: &str,
    region: &ShmRegion,
    name: &str,
    data: &[u8],
    content_type: &str,
    tags: &str,
) {
    let total_size = data.len();
    let total_pages = region.header().total_pages as usize;
    let page_size = consts::SHM_PAGE_SIZE as usize;
    // 每次传输使用最多 90% 的共享内存页
    let max_chunk_bytes = (total_pages * 9 / 10) * page_size;

    // 1. PUT_BEGIN
    let resp = socket_cmd(socket_path, &format!("PUT_BEGIN {name} {total_size} {content_type} {tags}\n"));
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

        let page_alloc = make_page_alloc(region);
        let pages_needed = ((chunk_size as u64 + 4095) / 4096) as u32;
        let start_page = page_alloc.alloc_pages(pages_needed).unwrap_or_else(|| {
            eprintln!("ERROR: not enough shm pages for chunk (need {pages_needed})");
            std::process::exit(1);
        });

        region.write_to_pages(start_page, chunk);
        let resp = socket_cmd(
            socket_path,
            &format!("PUT_CHUNK {name} {chunk_size} {start_page} {pages_needed}\n"),
        );
        if !resp.starts_with("OK") {
            eprint!("{resp}");
            page_alloc.free_pages(start_page, pages_needed);
            return;
        }

        // 服务端已读取并释放页，客户端也更新本地跟踪
        page_alloc.free_pages(start_page, pages_needed);
        offset = chunk_end;

        eprint!("\rUploading {name}: {}/{} bytes ({:.0}%)", offset, total_size, offset as f64 / total_size as f64 * 100.0);
    }
    eprintln!();

    // 3. PUT_END
    let resp = socket_cmd(socket_path, &format!("PUT_END {name}\n"));
    print!("{resp}");
}

fn cmd_get(cli: &Cli, id: &str, output: &Option<PathBuf>) {
    let cmd = format!("GET {id}\n");
    let resp = socket_cmd(&cli.socket, &cmd);
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
    let region = open_shm(&cli.shm_name);
    let data = region.read_from_pages(start_page, size);
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

fn cmd_delete(cli: &Cli, uuid: &str) {
    let cmd = format!("DELETE {uuid}\n");
    let resp = socket_cmd(&cli.socket, &cmd);
    print!("{resp}");
}
