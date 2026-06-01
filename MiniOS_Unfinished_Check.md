# MiniOS 未完成事项检查报告

> 检查依据：`Design_Need.md` 中的课程设计需求，以及当前项目源码、`README.md`、`design.md`、`outputs/output.txt`。
>
> 检查时间：2026-05-31
>
> 说明：本报告只整理未完成或存在风险的地方，未对源码进行修改。
>
> 修复进展：2026-06-01 已处理高优先级问题，并补齐了 checksum 校验、客户端 start、空文件上传、Rust 命名 warning、测试数量文档同步等中低优先级事项。文本协议复杂字段支持、大文件分块直接落盘、协议槽位/信号量接入属于可选增强，当前通过文档说明保留为后续优化。

## 1. 当前总体完成情况

从当前代码结构看，MiniOS 已经完成了主要框架：

- 自定义单文件对象存储 `store.odb`；
- 超级块、元数据区、自由块位图、数据块链表；
- `Put / Get / Delete / List` 基本对象操作；
- POSIX 共享内存区域管理；
- 共享内存页位图和连续页分配；
- Unix Domain Socket 控制通道；
- 服务端守护进程基本框架；
- 命令行客户端；
- LRU 缓存模块；
- 多线程处理客户端连接；
- 基础单元测试和设计文档。

本机执行 `cargo test` 的结果为：

```text
57 passed; 0 failed
```

当前主要问题不是“项目无法运行”，而是部分需求还没有完全落地，或者已有代码与设计文档描述存在差距。

## 2. 高优先级问题

### 2.1 共享内存页不足时没有等待机制

#### 需求描述

`Design_Need.md` 中要求：

> 当共享内存数据区无足够连续空闲页时，新请求必须等待。

#### 当前实现

当前共享内存页分配器 `PageAllocator::alloc_pages()` 在找不到连续空闲页时直接返回 `None`。

相关位置：

- `minios-lib/src/shm/page.rs`
- `minios-client/src/main.rs`
- `minios-server/src/main.rs`

客户端上传时，如果共享内存页不足，会直接报错退出：

```rust
let start_page = page_alloc.alloc_pages(pages_needed).unwrap_or_else(|| {
    region.unlock_page_mutex().unwrap();
    eprintln!("ERROR: not enough shared memory pages (need {pages_needed})");
    std::process::exit(1);
});
```

服务端 GET 时，如果无法为返回数据分配页，会直接返回错误：

```rust
None => "ERROR no free shm pages\n".into(),
```

#### 存在问题

这与“无足够连续空闲页时等待”的需求不一致。当前实现是失败返回，不是阻塞等待。

#### 建议补齐方向

可以增加以下机制之一：

- 使用 POSIX 条件变量或信号量，在页释放后通知等待者；
- 在客户端和服务端封装 `alloc_pages_wait()`，循环尝试分配并短暂 sleep；
- 为等待增加超时机制，避免永久阻塞；
- 在设计文档中补充等待策略、超时策略和错误处理策略。

推荐较简单的课程设计实现：

```text
lock page mutex
while alloc_pages(count) == None:
    unlock page mutex
    sleep 10ms
    lock page mutex
unlock page mutex
```

如果想更规范，可以使用条件变量或信号量，在 `free_pages()` 后唤醒等待客户端。

### 2.2 缓存预热功能没有真正执行

#### 需求描述

`Design_Need.md` 要求：

> 支持缓存预热功能。

#### 当前实现

`LruCache` 中已经实现了 `warmup()` 方法：

- `minios-lib/src/cache/lru.rs`

但服务端启动时只调用了 `store.list()` 并打印日志，没有真正把对象数据读入缓存：

```rust
let cache = LruCache::new(args.cache_capacity, cache_memory);
{
    let obj_list = store.list();
    log::info!(
        "Cache warmup: {} objects in store, cache ready (capacity={})",
        obj_list.len(),
        args.cache_capacity
    );
}
```

#### 存在问题

日志中写了 `Cache warmup`，但实际缓存仍为空。严格来说，当前只完成了缓存模块和统计功能，没有完成“预热功能”的实际接入。

#### 建议补齐方向

服务端启动后可以：

1. 调用 `store.list()` 获取对象 UUID；
2. 取前 `cache_capacity` 个对象；
3. 调用 `store.get_by_id()` 读取对象数据；
4. 写入 `LruCache`。

需要注意：当前 `store` 在创建 `cache` 后仍要被移动进 `ServerState`，所以预热代码要在移动之前完成。

### 2.3 `--max-clients` 参数未生效

#### 需求描述

需求中要求服务端支持多客户端并发访问。当前服务端参数中也提供了：

```rust
#[arg(long, default_value_t = consts::DEFAULT_MAX_CLIENTS as u32)]
max_clients: u32,
```

#### 当前实现

服务端在接受连接时直接为每个连接创建一个线程：

```rust
match listener.accept() {
    Ok((stream, addr)) => {
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            handle_client(stream, state);
        });
    }
    ...
}
```

但 `args.max_clients` 没有参与任何逻辑。

#### 存在问题

命令行参数存在但没有效果。大量客户端同时连接时，服务端会不受限制地创建线程。

#### 建议补齐方向

可以选择一种实现：

- 使用计数器限制活跃客户端线程数量；
- 超过限制时立即返回 `ERROR server busy`；
- 使用固定大小线程池；
- 使用信号量控制并发数量。

课程设计中较容易实现的是 `Arc<AtomicU32>`：

```text
accept connection
if active_clients >= max_clients:
    reply "ERROR server busy"
else:
    active_clients += 1
    spawn thread
    thread exit 时 active_clients -= 1
```

### 2.4 异常退出时可能出现存储文件不一致

#### 需求描述

`Design_Need.md` 要求：

> 保证数据的一致性和可靠性，所有对象数据最终持久化到一个自行设计的单一复合文档文件中。

#### 当前实现

在 `ObjectStore::put()` 中：

1. 分配内存中的位图；
2. 写入数据块；
3. 写入元数据；
4. 更新超级块；
5. 但没有立即调用 `flush_bitmap()`。

在 `ObjectStore::delete()` 中：

1. 释放内存中的位图；
2. 标记元数据 tombstone；
3. 更新超级块；
4. 也没有立即调用 `flush_bitmap()`。

位图持久化主要发生在：

```rust
pub fn flush(&mut self) -> miniosResult<()> {
    self.flush_superblock()?;
    self.flush_bitmap()?;
    self.file.flush()?;
    Ok(())
}
```

服务端正常退出时会调用 `store.flush()`。

#### 存在问题

如果服务端在 `put/delete` 后、正常退出前异常崩溃，可能出现：

- 元数据已经写入；
- 超级块已经更新；
- 但自由块位图仍是旧状态。

这样重启后可能导致：

- 已占用块被认为仍然空闲；
- 已删除对象释放的块仍被认为占用；
- 后续写入覆盖已有对象数据。

#### 建议补齐方向

简单补法：

- 在 `put()` 成功写入元数据后，立即 `flush_bitmap()`；
- 在 `delete()` 释放块后，立即 `flush_bitmap()`；
- 最后统一 `file.flush()`。

更稳妥的做法：

- 引入简单事务状态或操作日志；
- 写入顺序采用“数据块 -> 位图 -> 元数据 -> 超级块”；
- 启动时检查元数据和位图是否一致。

对课程设计而言，立即持久化位图和 `file.flush()` 已经能明显提高可靠性。

## 3. 中优先级问题

### 3.1 元数据 checksum 写了但打开时没有校验

#### 当前实现

`MetadataEntry` 中有：

```rust
pub fn verify_checksum(&self) -> bool
```

但 `ObjectStore::open()` 加载元数据时只是反序列化，没有调用校验：

```rust
metadata_cache.push(MetadataEntry::from_bytes(&entry_buf));
```

#### 存在问题

如果 `store.odb` 的元数据区损坏，当前系统不会发现，仍可能把损坏条目当成正常对象处理。

#### 建议补齐方向

打开存储文件时：

- 对 `ACTIVE` 条目调用 `verify_checksum()`；
- 校验失败时返回 `InvalidStore`；
- 或跳过损坏条目并在日志中报警。

更适合课程设计展示的是：校验失败直接报错，说明系统具有损坏检测能力。

### 3.2 服务端并发粒度较粗

#### 当前实现

服务端确实是“每连接一线程”：

```rust
std::thread::spawn(move || {
    handle_client(stream, state);
});
```

但所有线程共享一个：

```rust
Arc<Mutex<ServerState>>
```

每个请求进入后都会锁住整个 `ServerState`，包括：

- `ObjectStore`;
- `LruCache`;
- `ShmRegion`;
- `PageAllocator`;
- `pending_uploads`。

#### 存在问题

功能上可以保证一致性，但并发处理能力有限。多个客户端连接虽然有多个线程，但真正处理核心逻辑时基本是串行的。

#### 建议补齐方向

如果希望增强“多客户端并发访问”的说服力，可以考虑：

- 将 `cache`、`store`、`pending_uploads` 拆成独立锁；
- 读操作和写操作分离；
- `GET` 缓存命中时不必持有存储引擎锁；
- 或在文档中说明采用粗粒度锁是为了课程设计中的一致性和实现简洁。

### 3.3 客户端缺少 start 启动接口

#### 需求描述

`Design_Need.md` 中要求：

> 提供基本的启动、停止、状态查询接口。

#### 当前实现

客户端有：

- `status`;
- `stop`;
- `put`;
- `get`;
- `delete`;
- `list`。

但没有 `start` 命令。

服务端启动依赖用户手动执行：

```bash
./target/release/minios-server ...
```

#### 存在问题

如果验收标准要求“客户端能够控制服务启停”，当前只完成了停止和状态查询，没有完成启动。

#### 建议补齐方向

可选方案：

- 在客户端增加 `start` 子命令，通过 `std::process::Command` 拉起 `minios-server`；
- 或在文档中明确：启动接口由 shell/systemd 提供，客户端只负责停止和状态查询。

如果实现 `start`，需要考虑：

- server 可执行文件路径；
- store/socket/shm 参数；
- 是否后台运行；
- PID 文件位置。

### 3.4 文本协议对空格和复杂 tags 支持不好

#### 当前实现

服务端解析命令使用：

```rust
let parts: Vec<&str> = msg.splitn(7, ' ').collect();
```

客户端会把 tags 中的空格替换为下划线：

```rust
let tags_safe = tags.replace(' ', "_");
```

#### 存在问题

以下输入可能无法正确表达：

- 对象名称带空格；
- tags JSON 中包含空格；
- content-type 或 tags 中包含特殊字符；
- 未来扩展字段时协议会越来越脆弱。

#### 建议补齐方向

简单方案：

- 文档中明确对象名和 tags 不支持空格；
- 对 tags 使用 URL encode 或 base64。

更规范方案：

- 控制通道改为 JSON line；
- 或使用长度前缀二进制协议。

例如：

```json
{"op":"PUT","name":"hello world.txt","size":123,"content_type":"text/plain","tags":{"author":"Alice"},"start_page":0,"num_pages":1}
```

### 3.5 大文件分块上传服务端会完整累积到内存

#### 当前实现

大文件上传时，服务端用 `PendingUpload` 保存完整数据：

```rust
struct PendingUpload {
    data: Vec<u8>,
    content_type: String,
    tags: String,
}
```

每个 `PUT_CHUNK` 都追加到 `Vec<u8>`：

```rust
upload.data.extend_from_slice(&chunk);
```

`PUT_END` 时再一次性写入 `store.odb`。

#### 存在问题

虽然实现了“分块通过共享内存传输”，但服务端仍然会把大文件完整放入内存。对于较大对象，可能造成内存占用过高。

#### 建议补齐方向

可以在文档中说明这是课程设计简化实现。

如果要增强实现：

- `PUT_BEGIN` 时预分配存储块；
- `PUT_CHUNK` 到达后直接写入 `store.odb`；
- `PUT_END` 只提交元数据；
- 失败时回滚已分配块。

这个改动较大，不建议在短时间内作为第一优先级。

## 4. 低优先级问题

### 4.1 protocol 模块和命名信号量目前没有接入主流程

#### 当前实现

项目中存在：

- `minios-lib/src/protocol/request.rs`;
- `minios-lib/src/protocol/response.rs`;
- `ShmSemaphore`。

这些定义了共享内存请求槽位、响应槽位和信号量。

但实际 server/client 主流程使用的是：

- Unix Domain Socket 文本协议传控制命令；
- POSIX 共享内存传对象数据；
- pthread mutex 保护页位图。

请求/响应槽位和信号量没有成为实际通信路径的一部分。

#### 存在问题

这不一定影响功能，但会造成“代码里有一套协议结构，实际运行又是另一套协议”的观感。

#### 建议补齐方向

二选一：

- 删除或弱化文档中对共享内存请求/响应槽位的描述，明确当前采用 Socket 文本控制协议；
- 或把 `ShmRequest / ShmResponse / ShmSemaphore` 真正接入主流程。

考虑到当前项目已经稳定运行，建议优先统一文档描述，不急着重写通信协议。

### 4.2 `design.md` 中测试数量和实际不一致

#### 当前情况

`design.md` 中写的是：

```text
60 个单元测试
```

但当前 `cargo test` 实际输出：

```text
57 passed
```

#### 存在问题

功能上影响不大，但提交或答辩时可能被老师注意到。

#### 建议补齐方向

二选一：

- 修改 `design.md` 中的测试数量为 57；
- 或补足缺少的测试，让数量和文档一致。

### 4.3 Rust 命名风格 warning

#### 当前情况

`cargo test` 输出 warning：

```text
type `miniosError` should have an upper camel case name
type `miniosResult` should have an upper camel case name
```

位置：

- `minios-lib/src/common/error.rs`

当前命名：

```rust
pub enum miniosError
pub type miniosResult<T>
```

#### 存在问题

不影响运行，但不符合 Rust 命名规范。

#### 建议补齐方向

改为：

```rust
pub enum MiniosError
pub type MiniosResult<T> = Result<T, MiniosError>;
```

由于涉及全项目引用替换，建议单独作为一次小提交完成。

### 4.4 CLI 对空文件上传有限制

#### 当前实现

客户端上传空文件时直接报错：

```rust
if data.is_empty() {
    eprintln!("ERROR: empty file");
    std::process::exit(1);
}
```

但底层 `ObjectStore::put()` 是支持空对象的，并且有单元测试。

#### 存在问题

对象存储一般可以允许 0 字节对象。当前 CLI 限制和底层能力不一致。

#### 建议补齐方向

如果课程需求没有禁止空对象，建议允许 CLI 上传空文件。

实现时要注意：

- 0 字节对象无需共享内存页；
- 可以直接走一个特殊 `PUT_EMPTY`；
- 或让 `PUT` 支持 `num_pages = 0`。

## 5. 已经完成且表现较好的部分

以下部分与需求匹配度较高：

### 5.1 自定义 `store.odb` 文件格式

已实现：

- 超级块；
- 元数据区；
- 自由块位图；
- 数据块区；
- 数据块 next 指针链表；
- `Put / Get / Delete / List`；
- 基础统计信息。

相关模块：

- `minios-lib/src/storage/superblock.rs`;
- `minios-lib/src/storage/metadata.rs`;
- `minios-lib/src/storage/bitmap.rs`;
- `minios-lib/src/storage/engine.rs`。

### 5.2 共享内存分页管理

已实现：

- POSIX `shm_open`;
- `mmap`;
- 控制页；
- 页位图；
- 连续页 First-Fit 分配；
- 跨进程 `pthread_mutex_t`；
- 页读写接口。

相关模块：

- `minios-lib/src/shm/region.rs`;
- `minios-lib/src/shm/page.rs`;
- `minios-lib/src/shm/sync.rs`。

### 5.3 LRU 缓存模块

已实现：

- 容量限制；
- 内存限制；
- 命中率统计；
- LRU 淘汰；
- 删除后失效；
- warmup 方法。

主要缺口是服务端启动时没有真正调用 warmup。

### 5.4 服务端与客户端基本功能

已实现：

- `put`;
- `get`;
- `delete`;
- `list`;
- `status`;
- `stop`;
- 大文件分块上传；
- 并发上传基本可用。

`outputs/output.txt` 中的 Ubuntu 测试显示 10 个并发上传对象均成功写入并可列出。

## 6. 建议后续修改顺序

如果后续要继续完善，建议按以下顺序处理：

1. 实现共享内存页不足等待机制；
2. 接入真实缓存预热；
3. 让 `--max-clients` 生效；
4. `put/delete` 后及时持久化位图，提高异常退出可靠性；
5. 打开 `store.odb` 时校验元数据 checksum；
6. 统一 `design.md` 中测试数量和实际结果；
7. 修复 Rust 命名 warning；
8. 明确或优化文本协议对空格、JSON tags 的支持；
9. 视时间决定是否支持客户端 `start` 命令；
10. 视时间决定是否优化大文件分块上传的内存占用。

## 7. 推荐提交拆分

如果按 Git 工作流逐项提交，建议拆成以下独立提交：

```text
docs: add unfinished feature checklist
fix(shm): wait for free pages during shared memory allocation
feat(cache): warm up LRU cache on server startup
feat(server): enforce max client connection limit
fix(storage): persist bitmap after put and delete
fix(storage): validate metadata checksum on open
docs: sync test count and protocol notes
style(common): rename error/result types to Rust style
```

每个提交只完成一个明确目标，符合项目 `AGENTS.md` 中的 Git Workflow 要求。
