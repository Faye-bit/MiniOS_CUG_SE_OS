# Minios — Mini Object Storage

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
- `minios-server` — 服务端守护进程
- `minios-client` (或 `minios`) — 命令行客户端

---

## 运行服务端

### 前台模式（调试用）

```bash
./target/release/minios-server \
    --store-path ./store.odb \
    --socket-path /tmp/minios.sock \
    --shm-name /minios_shm \
    --shm-pages 256 \
    --cache-capacity 128 \
    --cache-memory-mb 64 \
    --max-objects 1024 \
    --total-blocks 4096
```

### 后台模式（集成测试用）

```bash
# 重定向输出，避免干扰终端
./target/release/minios-server --store-path /tmp/test_store.odb > /tmp/minios.log 2>&1 &
SERVER_PID=$!
sleep 1
```

### 守护进程模式

```bash
./target/release/minios-server \
    --daemon \
    --pidfile /tmp/minios.pid \
    --store-path /var/lib/minios/store.odb
```

### 停止服务

```bash
# 方式1：客户端 stop 命令（推荐，同步返回结果）
./target/release/minios-client stop

# 方式2：发送 SIGTERM 后等待进程退出
kill $SERVER_PID && wait $SERVER_PID

# 方式3：通过 PID 文件
kill $(cat /tmp/minios.pid)
```

### 命令行参数

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--store-path` | `./store.odb` | 存储文件路径 |
| `--socket-path` | `/tmp/minios.sock` | Unix Socket 路径 |
| `--shm-name` | `/minios_shm` | 共享内存名称 |
| `--shm-pages` | `256` | 共享内存数据页数 |
| `--cache-capacity` | `128` | LRU 缓存条目容量 |
| `--cache-memory-mb` | `64` | LRU 缓存最大内存 (MB) |
| `--max-objects` | `1024` | 最大对象数 |
| `--total-blocks` | `4096` | 数据块总数 (每个 4KB) |
| `--max-clients` | `16` | 最大并发客户端数 |
| `--daemon` | `false` | 以守护进程方式运行 |
| `--pidfile` | `/tmp/minios.pid` | PID 文件路径 |

---

## 客户端命令

```bash
# 设置别名
alias minios="./target/release/minios-client"
```

### 上传文件

```bash
# 基本用法（对象名默认为文件名）
minios put ./hello.txt

# 指定名称和类型
minios put ./data.bin --name my-data --content-type application/octet-stream

# 添加自定义标签
minios put ./photo.jpg --name avatar --content-type image/jpeg --tags '{"user":"alice","size":"large"}'

# 连接远程服务端
minios --socket /tmp/minios.sock put ./file.bin
```

### 启动服务

```bash
# 默认启动同目录下的 minios-server，并将日志写入 /tmp/minios.log
minios start --store-path /tmp/test_store.odb

# 指定服务端程序路径
minios start --server ./target/release/minios-server --store-path /tmp/test_store.odb
```

### 下载对象

```bash
# 通过 UUID 下载
minios get 550e8400-e29b-41d4-a716-446655440000

# 通过名称下载
minios get hello.txt

# 保存到文件
minios get hello.txt --output ./downloaded.txt
```

### 删除对象

```bash
minios delete 550e8400-e29b-41d4-a716-446655440000
```

### 列出所有对象

```bash
minios list
```

输出示例：
```
LIST 2
550e8400e29b41d4a716446655440000 hello.txt 1024 text/plain 1716567890
660e8400e29b41d4a716446655440001 data.bin 4096 application/octet-stream 1716567900
```

### 查看服务状态

```bash
minios status
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

## 功能演示

以下是一个完整的演示流程，按顺序展示系统所有主要功能。建议在 Ubuntu Linux 上直接复制粘贴运行。

### 0. 环境清理

```bash
# 清理上次测试的残留文件
rm -f /tmp/test_store.odb /tmp/test_file.txt /tmp/large.bin /tmp/minios.log /tmp/minios.sock /tmp/minios.pid
```

### 1. 启动服务端

```bash
./target/release/minios-server \
    --store-path /tmp/test_store.odb \
    --socket-path /tmp/minios.sock \
    > /tmp/minios.log 2>&1 &

SERVER_PID=$!
sleep 1
echo "Server PID: $SERVER_PID"
```

### 2. 上传文件

```bash
# 创建一个测试文件
echo "Hello, MiniOS! This is a test file." > /tmp/test_file.txt

# 上传文件（对象名默认为文件名）
./target/release/minios-client put /tmp/test_file.txt
# => OK <32位十六进制 UUID>

# 指定对象名称和 MIME 类型
./target/release/minios-client put /tmp/test_file.txt \
    --name hello-world \
    --content-type text/plain
# => OK <UUID>
```

### 3. 查看对象列表

```bash
./target/release/minios-client list
# =>
# OK 2
# dffb681b... test_file.txt 37 text/plain 1779982957
# a64e240a... hello-world    37 text/plain 1779982957
```

### 4. 下载对象

```bash
# 按名称下载（输出到 stdout）
./target/release/minios-client get hello-world
# => Hello, MiniOS! This is a test file.

# 按 UUID 下载并保存到文件
UUID=$(./target/release/minios-client list | grep hello-world | awk '{print $1}')
./target/release/minios-client get "$UUID" --output /tmp/downloaded.txt
cat /tmp/downloaded.txt
# => Hello, MiniOS! This is a test file.
```

### 5. 带标签上传

```bash
# 上传带自定义标签的文件
echo '{"author":"Alice","project":"demo"}' > /tmp/meta.json
./target/release/minios-client put /tmp/meta.json \
    --name config \
    --content-type application/json \
    --tags '{"author":"Alice","project":"demo"}'
# => OK <UUID>
```

### 6. 查看服务状态

```bash
./target/release/minios-client status
# =>
# OK
# store_objects: 3
# store_blocks_free: 4093/4096
# store_file_size: 16781312
# cache_entries: 3/128
# cache_hit_rate: 0.00
# shm_pages_free: 256/256
```

**状态解读**: `store_objects` 显示当前存储的对象数量；`store_blocks_free` 显示剩余数据块；`cache_hit_rate` 表示 LRU 缓存命中率；`shm_pages_free` 显示共享内存空闲页数（256 页全空闲，因为所有请求已完成）。

### 7. 大文件分块上传

```bash
# 生成 10MB 随机数据文件
dd if=/dev/urandom of=/tmp/large.bin bs=1M count=10 2>/dev/null

# 上传大文件（自动触发分块传输协议）
./target/release/minios-client put /tmp/large.bin --name large-test
# => Uploading large-test: .../10485760 bytes (100%)
# => OK <UUID>
```

**工作原理**: 共享内存默认 256 页 x 4KB = 1MB，而文件有 10MB。客户端自动检测文件超过共享内存容量，切换为分块上传协议：`PUT_BEGIN → PUT_CHUNK × N → PUT_END`。每块最多使用 90% 的共享内存页，服务端将块数据累积到 `PendingUpload` 缓冲区，最后一块传输完成后统一写入 `store.odb`。

### 8. 数据持久化验证

```bash
# 停止服务端
./target/release/minios-client stop
wait $SERVER_PID 2>/dev/null
echo "Server stopped."

# 重新启动服务端（使用同一个 store.odb）
./target/release/minios-server \
    --store-path /tmp/test_store.odb \
    --socket-path /tmp/minios.sock \
    > /tmp/minios.log 2>&1 &
SERVER_PID=$!
sleep 1

# 验证数据仍然存在
./target/release/minios-client list
# => OK 4  （hello-world, test_file.txt, config, large-test 均在）

./target/release/minios-client get hello-world
# => Hello, MiniOS! This is a test file.
```

### 9. 并发上传

```bash
# 创建 10 个测试文件
pids=()
for i in $(seq 1 10); do
    echo "concurrent-data-$i" > "/tmp/concurrent_$i.txt" &
    pids+=($!)
done
for pid in "${pids[@]}"; do
    wait "$pid"
done

# 同时启动 10 个客户端上传
echo "=== 并发上传开始 ==="
pids=()
for i in $(seq 1 10); do
    ./target/release/minios-client put "/tmp/concurrent_$i.txt" --name "concurrent-$i" &
    pids+=($!)
done
for pid in "${pids[@]}"; do
    wait "$pid"
done
echo "=== 并发上传完成 ==="

# 验证：所有 10 个对象均已成功持久化
./target/release/minios-client list | grep concurrent | wc -l
# => 10
```

**并发安全机制**: 多个客户端进程通过共享内存传输数据，使用 `pthread_mutex_t`（`PTHREAD_PROCESS_SHARED`）保护页分配位图的并发访问。客户端在持有锁期间完成页分配和数据写入，释放锁后再发送 socket 命令，避免死锁。页由服务端处理请求时释放，客户端不重复释放，防止并发竞态。

### 10. 删除对象

```bash
# 删除指定对象
UUID=$(./target/release/minios-client list | grep config | awk '{print $1}')
./target/release/minios-client delete "$UUID"
# => OK deleted

# 确认已删除
./target/release/minios-client list | grep config
# => (无输出)
```

### 11. 停止服务

```bash
# 方式 1：客户端 stop 命令（推荐）
./target/release/minios-client stop
# => OK shutting down

# 方式 2：发送信号（stop 命令不可用时）
kill $SERVER_PID 2>/dev/null
```

### 12. 守护进程模式（可选）

```bash
# 以守护进程方式启动
./target/release/minios-server \
    --daemon \
    --pidfile /tmp/minios.pid \
    --store-path /tmp/test_store.odb

# 也可以通过客户端启动服务端（默认查找同目录下的 minios-server）
./target/release/minios-client start \
    --store-path /tmp/test_store.odb \
    --log-file /tmp/minios.log

# 通过 PID 文件管理
cat /tmp/minios.pid       # 查看 PID
kill $(cat /tmp/minios.pid)  # 停止守护进程
```

### 清理

```bash
rm -f /tmp/test_store.odb /tmp/test_file.txt /tmp/large.bin /tmp/downloaded.txt \
      /tmp/meta.json /tmp/concurrent_*.txt /tmp/minios.log /tmp/minios.sock /tmp/minios.pid
```

---

## 运行测试

### 单元测试

```bash
# 运行所有测试
cargo test

# 运行特定模块
cargo test -p minios-lib --lib storage::       # 存储引擎 (38 个)
cargo test -p minios-lib --lib shm::           # 共享内存 (11 个，部分需 Linux)
cargo test -p minios-lib --lib cache::         # LRU 缓存 (9 个)

# 显示测试输出
cargo test -- --nocapture

# 运行单个测试
cargo test -p minios-lib --lib storage::engine::tests::test_put_large_object_spans_blocks
```

### 集成测试（手动）

```bash
# 0. 清理残留
rm -f /tmp/test_store.odb /tmp/test_file.txt /tmp/large.bin /tmp/minios.sock

# 1. 启动服务端（后台 + 重定向日志）
./target/release/minios-server --store-path /tmp/test_store.odb > /tmp/minios.log 2>&1 &
SERVER_PID=$!
sleep 1

# 2. 基本操作流程
echo "Hello, MiniOS!" > /tmp/test_file.txt
./target/release/minios-client put /tmp/test_file.txt --name hello
./target/release/minios-client list
./target/release/minios-client get hello
./target/release/minios-client status

# 3. 大对象测试（10MB，自动分块传输）
dd if=/dev/urandom of=/tmp/large.bin bs=1M count=10 2>/dev/null
./target/release/minios-client put /tmp/large.bin --name large-test

# 4. 重启持久化测试
kill $SERVER_PID && wait $SERVER_PID
./target/release/minios-server --store-path /tmp/test_store.odb > /tmp/minios.log 2>&1 &
SERVER_PID=$!
sleep 1
./target/release/minios-client list   # 应显示之前的对象
./target/release/minios-client get hello

# 5. 清理
./target/release/minios-client stop 2>/dev/null || kill $SERVER_PID 2>/dev/null
rm -f /tmp/test_store.odb /tmp/test_file.txt /tmp/large.bin /tmp/minios.log /tmp/minios.sock
```

### 并发测试

```bash
# 启动服务端后，并发上传多个文件
pids=()
for i in $(seq 1 10); do
    echo "data-$i" > "/tmp/concurrent_$i.txt" &
    pids+=($!)
done
for pid in "${pids[@]}"; do
    wait "$pid"
done

pids=()
for i in $(seq 1 10); do
    ./target/release/minios-client put "/tmp/concurrent_$i.txt" --name "concurrent-$i" &
    pids+=($!)
done
for pid in "${pids[@]}"; do
    wait "$pid"
done

# 验证
./target/release/minios-client list | grep concurrent
```

注意：如果服务端通过 `&` 在当前 shell 后台运行，不要在并发测试中直接使用无参数`wait`。无参数 `wait` 会等待当前 shell 的所有后台任务，包括仍在运行的`minios-server`，因此命令会一直阻塞到服务端退出。上面的写法只等待本轮测试创建的客户端/文件生成进程。

---

## 项目结构

```
MiniOS/
├── Cargo.toml              # Cargo workspace 定义
├── design.md               # 详细设计文档（含 UML 图）
├── README.md               # 本文件
├── .gitignore
│
├── minios-lib/              # 核心库
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
├── minios-server/           # 服务端入口
│   ├── Cargo.toml
│   └── src/main.rs
│
└── minios-client/           # 客户端入口
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
