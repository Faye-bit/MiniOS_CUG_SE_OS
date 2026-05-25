# MinOS — Mini Object Storage

简单对象存储服务，采用扁平化命名空间管理数据。所有对象持久化到单一复合文档文件 `store.odb`，通过 Unix Domain Socket + POSIX 共享内存双通道进行进程间通信。

## 快速开始

### 环境要求

- **操作系统**: Ubuntu Linux (20.04+)
- **Rust 工具链**: 1.75+

```bash
# 安装 Rust（如未安装）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
```

### 克隆与编译

```bash
git clone https://github.com/Faye-bit/MiniOS_CUG_SE_OS.git
cd MiniOS_CUG_SE_OS
cargo build --release
```

编译产物位于 `target/release/` 目录：
- `minos-server` — 服务端守护进程
- `minos-client` (或 `minos`) — 命令行客户端

---

## 运行服务端

### 前台模式（调试用）

```bash
./target/release/minos-server \
    --store-path ./store.odb \
    --socket-path /tmp/minos.sock \
    --shm-name /minos_shm \
    --shm-pages 256 \
    --cache-capacity 128 \
    --cache-memory-mb 64 \
    --max-objects 1024 \
    --total-blocks 4096
```

### 后台模式（集成测试用）

```bash
# 重定向输出，避免干扰终端
./target/release/minos-server --store-path /tmp/test_store.odb > /tmp/minos.log 2>&1 &
SERVER_PID=$!
sleep 1
```

### 守护进程模式

```bash
./target/release/minos-server \
    --daemon \
    --pidfile /tmp/minos.pid \
    --store-path /var/lib/minos/store.odb
```

### 停止服务

```bash
# 方式1：客户端 stop 命令（推荐，同步返回结果）
./target/release/minos-client stop

# 方式2：发送 SIGTERM 后等待进程退出
kill $SERVER_PID && wait $SERVER_PID

# 方式3：通过 PID 文件
kill $(cat /tmp/minos.pid)
```

### 命令行参数

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--store-path` | `./store.odb` | 存储文件路径 |
| `--socket-path` | `/tmp/minos.sock` | Unix Socket 路径 |
| `--shm-name` | `/minos_shm` | 共享内存名称 |
| `--shm-pages` | `256` | 共享内存数据页数 |
| `--cache-capacity` | `128` | LRU 缓存条目容量 |
| `--cache-memory-mb` | `64` | LRU 缓存最大内存 (MB) |
| `--max-objects` | `1024` | 最大对象数 |
| `--total-blocks` | `4096` | 数据块总数 (每个 4KB) |
| `--max-clients` | `16` | 最大并发客户端数 |
| `--daemon` | `false` | 以守护进程方式运行 |
| `--pidfile` | `/tmp/minos.pid` | PID 文件路径 |

---

## 客户端命令

```bash
# 设置别名
alias minos="./target/release/minos-client"
```

### 上传文件

```bash
# 基本用法（对象名默认为文件名）
minos put ./hello.txt

# 指定名称和类型
minos put ./data.bin --name my-data --content-type application/octet-stream

# 添加自定义标签
minos put ./photo.jpg --name avatar --content-type image/jpeg --tags '{"user":"alice","size":"large"}'

# 连接远程服务端
minos --socket /tmp/minos.sock put ./file.bin
```

### 下载对象

```bash
# 通过 UUID 下载
minos get 550e8400-e29b-41d4-a716-446655440000

# 通过名称下载
minos get hello.txt

# 保存到文件
minos get hello.txt --output ./downloaded.txt
```

### 删除对象

```bash
minos delete 550e8400-e29b-41d4-a716-446655440000
```

### 列出所有对象

```bash
minos list
```

输出示例：
```
LIST 2
550e8400e29b41d4a716446655440000 hello.txt 1024 text/plain 1716567890
660e8400e29b41d4a716446655440001 data.bin 4096 application/octet-stream 1716567900
```

### 查看服务状态

```bash
minos status
```

输出示例：
```
STATUS OK
store_objects: 42
store_blocks_free: 3500/4096
store_file_size: 16777216
cache_entries: 42/128
cache_hit_rate: 0.87
shm_pages_free: 250/256
```

---

## 运行测试

### 单元测试

```bash
# 运行所有测试
cargo test

# 运行特定模块
cargo test -p minos-lib --lib storage::       # 存储引擎 (37 个)
cargo test -p minos-lib --lib shm::           # 共享内存 (11 个，部分需 Linux)
cargo test -p minos-lib --lib cache::         # LRU 缓存 (9 个)

# 显示测试输出
cargo test -- --nocapture

# 运行单个测试
cargo test -p minos-lib --lib storage::engine::tests::test_put_large_object_spans_blocks
```

### 集成测试（手动）

```bash
# 0. 清理残留
rm -f /tmp/test_store.odb /tmp/test_file.txt /tmp/large.bin /tmp/minos.sock

# 1. 启动服务端（后台 + 重定向日志）
./target/release/minos-server --store-path /tmp/test_store.odb > /tmp/minos.log 2>&1 &
SERVER_PID=$!
sleep 1

# 2. 基本操作流程
echo "Hello, MiniOS!" > /tmp/test_file.txt
./target/release/minos-client put /tmp/test_file.txt --name hello
./target/release/minos-client list
./target/release/minos-client get hello
./target/release/minos-client status

# 3. 大对象测试（10MB，自动分块传输）
dd if=/dev/urandom of=/tmp/large.bin bs=1M count=10 2>/dev/null
./target/release/minos-client put /tmp/large.bin --name large-test

# 4. 重启持久化测试
kill $SERVER_PID && wait $SERVER_PID
./target/release/minos-server --store-path /tmp/test_store.odb > /tmp/minos.log 2>&1 &
SERVER_PID=$!
sleep 1
./target/release/minos-client list   # 应显示之前的对象
./target/release/minos-client get hello

# 5. 清理
./target/release/minos-client stop 2>/dev/null || kill $SERVER_PID 2>/dev/null
rm -f /tmp/test_store.odb /tmp/test_file.txt /tmp/large.bin /tmp/minos.log /tmp/minos.sock
```

### 并发测试

```bash
# 启动服务端后，并发上传多个文件
for i in $(seq 1 10); do
    echo "data-$i" > "/tmp/concurrent_$i.txt" &
done
wait

for i in $(seq 1 10); do
    ./target/release/minos-client put "/tmp/concurrent_$i.txt" --name "concurrent-$i" &
done
wait

# 验证
./target/release/minos-client list | grep concurrent
```

---

## 项目结构

```
MiniOS/
├── Cargo.toml              # Cargo workspace 定义
├── design.md               # 详细设计文档（含 UML 图）
├── README.md               # 本文件
├── .gitignore
│
├── minos-lib/              # 核心库
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── common/         # 常量、类型定义、错误类型
│       │   ├── consts.rs
│       │   ├── types.rs
│       │   └── error.rs
│       ├── storage/        # ★ 对象存储引擎
│       │   ├── superblock.rs   # 超级块 (store.odb 头部 4KB)
│       │   ├── bitmap.rs       # 自由块位图
│       │   ├── metadata.rs     # 元数据条目 (256 bytes)
│       │   └── engine.rs       # ObjectStore (Put/Get/Delete/List)
│       ├── shm/            # ★ 共享内存管理
│       │   ├── sync.rs         # 跨进程互斥锁 + 命名信号量
│       │   ├── region.rs       # shm_open/mmap 生命周期
│       │   └── page.rs         # First-Fit 页分配器
│       ├── cache/          # LRU 缓存
│       │   └── lru.rs
│       ├── protocol/       # 通信协议
│       │   ├── request.rs      # 请求槽位 (256 bytes)
│       │   └── response.rs     # 响应槽位 (256 bytes)
│       └── daemon/         # 守护进程管理
│           └── mod.rs          # double-fork, 信号, PID 文件
│
├── minos-server/           # 服务端入口
│   ├── Cargo.toml
│   └── src/main.rs
│
└── minos-client/           # 客户端入口
    ├── Cargo.toml
    └── src/main.rs
```

---

## 技术要点

| 特性 | 实现 |
|------|------|
| 编程语言 | Rust (edition 2021) |
| 存储格式 | 自定义二进制文件 (store.odb)，超级块 + 位图 + 块链表 |
| 进程间通信 | Unix Domain Socket (控制) + POSIX 共享内存 shm_open/mmap (数据) |
| 同步机制 | pthread_mutex_t (PTHREAD_PROCESS_SHARED) + POSIX 命名信号量 |
| 内存管理 | 基于位图的 First-Fit 页分配器 |
| 缓存策略 | LRU (HashMap + VecDeque)，条目数 + 内存双阈值 |
| 守护进程 | double-fork + setsid + PID 文件 + SIGTERM/SIGINT 信号处理 |
| 目标平台 | Ubuntu Linux |
