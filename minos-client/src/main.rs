//! MinOS 客户端 — 命令行接口。
//!
//! 支持以下命令：
//! - put <file>       上传本地文件到对象存储
//! - get <id>         下载对象到标准输出或文件
//! - delete <id>      删除对象（通过 UUID）
//! - list             列出所有对象
//! - status           查看服务端状态

use clap::{Parser, Subcommand};
use minos_lib::common::consts;
use minos_lib::shm::page::PageAllocator;
use minos_lib::shm::region::ShmRegion;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// MinOS 对象存储客户端。
#[derive(Parser, Debug)]
#[command(name = "minos", version, about = "MinOS object storage client")]
struct Cli {
    /// Unix Socket 路径
    #[arg(long, default_value = consts::DEFAULT_SOCKET_PATH)]
    socket: String,

    /// 共享内存名称
    #[arg(long, default_value = consts::DEFAULT_SHM_NAME)]
    shm_name: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 上传文件
    Put {
        /// 本地文件路径
        file: PathBuf,
        /// 对象名称（默认使用文件名）
        #[arg(short, long)]
        name: Option<String>,
        /// 内容类型（MIME）
        #[arg(long, default_value = "application/octet-stream")]
        content_type: String,
        /// 自定义标签（JSON 字符串）
        #[arg(long, default_value = "")]
        tags: String,
    },
    /// 下载对象
    Get {
        /// 对象 UUID 或名称
        id: String,
        /// 输出文件路径（默认输出到 stdout）
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// 删除对象
    Delete {
        /// 对象 UUID
        uuid: String,
    },
    /// 列出所有对象
    List,
    /// 查看服务状态
    Status,
}

fn main() {
    let cli = Cli::parse();

    match &cli.command {
        Command::Status => cmd_status(&cli),
        Command::List => cmd_list(&cli),
        Command::Put { file, name, content_type, tags } => {
            cmd_put(&cli, file, name, content_type, tags)
        }
        Command::Get { id, output } => cmd_get(&cli, id, output),
        Command::Delete { uuid } => cmd_delete(&cli, uuid),
    }
}

/// 建立到服务端的 Unix Socket 连接。
fn connect_socket(socket_path: &str) -> UnixStream {
    UnixStream::connect(socket_path).unwrap_or_else(|e| {
        eprintln!("ERROR: cannot connect to server at '{socket_path}': {e}");
        eprintln!("Is the minos-server running?");
        std::process::exit(1);
    })
}

/// 打开共享内存区域。
fn open_shm(name: &str) -> ShmRegion {
    ShmRegion::open(name).unwrap_or_else(|e| {
        eprintln!("ERROR: cannot open shared memory '{name}': {e}");
        std::process::exit(1);
    })
}

/// 通过 socket 发送文本命令并读取响应。
fn socket_command(socket_path: &str, command: &str) -> String {
    let mut stream = connect_socket(socket_path);
    stream.write_all(command.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

// ─── 命令实现 ───

fn cmd_status(cli: &Cli) {
    let resp = socket_command(&cli.socket, "STATUS\n");
    println!("{resp}");
}

fn cmd_list(cli: &Cli) {
    let resp = socket_command(&cli.socket, "LIST\n");
    println!("{resp}");
}

fn cmd_put(cli: &Cli, file: &PathBuf, name: &Option<String>, content_type: &str, tags: &str) {
    let obj_name = name
        .clone()
        .unwrap_or_else(|| file.file_name().unwrap().to_string_lossy().to_string());

    // 读取文件
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

    // 打开共享内存
    let region = open_shm(&cli.shm_name);
    let total_pages = region.header().total_pages;
    let free_pages_offset = 16usize;
    let free_pages_ptr = unsafe {
        (region.data_area() as *mut u8)
            .sub(consts::SHM_PAGE_SIZE as usize)
            .add(free_pages_offset) as *mut u32
    };
    let page_alloc = unsafe { PageAllocator::new(region.bitmap_ptr(), total_pages, free_pages_ptr) };

    // 计算需要的页数
    let pages_needed = ((data.len() as u64 + 4095) / 4096) as u32;
    let start_page = page_alloc.alloc_pages(pages_needed).unwrap_or_else(|| {
        eprintln!("ERROR: not enough shared memory pages (need {pages_needed})");
        std::process::exit(1);
    });

    // 写入数据到共享内存
    region.write_to_pages(start_page, &data);

    // 通过 socket 发送 Put 命令
    let put_cmd = format!(
        "PUT {name} {size} {content_type} {tags} {start_page} {pages_needed}\n",
        name = obj_name,
        size = data.len(),
    );
    let resp = socket_command(&cli.socket, &put_cmd);
    println!("{resp}");

    // 释放共享内存页
    page_alloc.free_pages(start_page, pages_needed);
    drop(region);
}

fn cmd_get(cli: &Cli, id: &str, _output: &Option<PathBuf>) {
    // 先尝试通过 socket 查询对象元数据
    let resp = socket_command(&cli.socket, &format!("GET {id}\n"));
    println!("{resp}");
}

fn cmd_delete(cli: &Cli, uuid: &str) {
    let resp = socket_command(&cli.socket, &format!("DELETE {uuid}\n"));
    println!("{resp}");
}
