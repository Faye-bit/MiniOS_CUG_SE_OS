# Minios 对象存储服务 — 设计文档

## 1. 系统概述

MiniOS（Mini Object Storage）是一个简单的对象存储服务，采用扁平化命名空间管理数据，
所有对象持久化到单一复合文档文件 `store.odb` 中。系统由服务端守护进程和命令行客户端组成，
通过 Unix Domain Socket + POSIX 共享内存双通道进行进程间通信。

### 1.1 系统架构

```mermaid
graph TB
    subgraph "用户空间"
        CLI["minios-client<br/>(CLI 命令行)"]
    end

    subgraph "服务端进程"
        SERVER["minios-server<br/>(Daemon 守护进程)"]
        CACHE["LruCache<br/>HashMap + VecDeque"]
        SHM_MGR["共享内存管理器<br/>ShmRegion + PageAllocator"]
    end

    subgraph "持久化存储"
        STORE["store.odb<br/>单一复合文档文件"]
    end

    subgraph "进程间通信"
        SOCKET["Unix Domain Socket<br/>控制消息通道"]
        SHM["POSIX 共享内存<br/>shm_open / mmap<br/>大数据传输通道"]
    end

    CLI -->|"PUT/GET/DELETE/LIST/STATUS"| SOCKET
    SOCKET --> SERVER
    CLI -->|"写入/读取对象数据"| SHM
    SHM --> SERVER

    SERVER --> CACHE
    CACHE -->|"cache miss"| STORE
    CACHE -->|"cache hit"| SERVER
    SERVER --> STORE
    SERVER --> SHM_MGR
```

### 1.2 请求处理流程

```mermaid
sequenceDiagram
    actor User as 用户
    participant CLI as minios-client
    participant SHM as 共享内存
    participant MUTEX as pthread_mutex_t
    participant SOCK as Unix Socket
    participant SRV as minios-server
    participant ENG as ObjectStore
    participant ODB as store.odb

    Note over User,ODB: === PUT 操作 (小文件单次传输) ===

    User->>CLI: minios put ./data.bin --name obj1
    CLI->>SHM: shm_open + mmap 打开共享内存
    CLI->>MUTEX: lock_page_mutex()
    CLI->>SHM: PageAllocator::alloc_pages(N) 申请数据页
    CLI->>SHM: write_to_pages(start, data) 写入对象数据
    CLI->>MUTEX: unlock_page_mutex()
    CLI->>SOCK: 发送 "PUT name size type tags start_page num_pages\n"
    SOCK->>SRV: 接收请求 (每连接一线程)
    SRV->>MUTEX: lock_page_mutex()
    SRV->>SHM: read_from_pages(start, size) 读取数据
    SRV->>SHM: PageAllocator::free_pages() 释放页
    SRV->>MUTEX: unlock_page_mutex()
    SRV->>ENG: store.put(name, data, type, tags)
    ENG->>ODB: Bitmap::allocate_multi(N) 分配数据块
    ENG->>ODB: write_data_block() × N (含 next 指针链表)
    ENG->>ODB: MetadataEntry 写入元数据条目
    ENG->>ODB: Superblock::write_to() 更新超级块
    SRV->>SOCK: 返回 "OK <uuid>\n"
    SOCK->>CLI: 显示结果

    Note over User,ODB: === GET 操作 ===

    User->>CLI: minios get <uuid>
    CLI->>SOCK: 发送 "GET <uuid>\n"
    SOCK->>SRV: 接收请求
    SRV->>CACHE: LruCache::get(uuid) 查缓存
    alt 缓存命中
        CACHE-->>SRV: 返回数据
    else 缓存未命中
        SRV->>ENG: store.get_by_id(uuid)
        ENG->>ODB: read metadata → 解析块链表
        ENG->>ODB: read_data_block() × N 读取拼接
        ENG-->>SRV: 返回完整对象
        SRV->>CACHE: LruCache::put(uuid, data) 更新缓存
    end
    SRV->>MUTEX: lock_page_mutex()
    SRV->>SHM: alloc_pages + write_to_pages
    SRV->>MUTEX: unlock_page_mutex()
    SRV->>SOCK: 返回 "OK <size> <start_page> <num_pages>\n"
    SOCK->>CLI: 从共享内存读取数据
    CLI->>MUTEX: lock_page_mutex()
    CLI->>SHM: read_from_pages() + free_pages()
    CLI->>MUTEX: unlock_page_mutex()
    CLI-->>User: 输出到 stdout / 文件
```

### 1.3 模块依赖关系

```mermaid
graph TD
    subgraph minios-client
        CLI_MAIN["main.rs<br/>clap 命令解析"]
    end

    subgraph minios-server
        SRV_MAIN["main.rs<br/>守护进程 + socket 监听 + 线程池"]
    end

    subgraph minios-lib
        COMMON["common<br/>consts / types / error"]
        STORAGE["storage ★<br/>superblock / bitmap<br/>metadata / engine"]
        SHM_MOD["shm ★<br/>sync / region / page"]
        CACHE_MOD["cache<br/>lru"]
        PROTOCOL["protocol<br/>request / response"]
        DAEMON["daemon<br/>mod"]
    end

    CLI_MAIN --> SHM_MOD
    SRV_MAIN --> STORAGE
    SRV_MAIN --> SHM_MOD
    SRV_MAIN --> CACHE_MOD
    SRV_MAIN --> DAEMON
    STORAGE --> COMMON
    SHM_MOD --> COMMON
    CACHE_MOD --> COMMON
    PROTOCOL --> COMMON
    DAEMON --> COMMON
```

---

## 2. store.odb 文件格式

### 2.1 整体布局

```mermaid
block-beta
    columns 1

    block:superblock
        columns 1
        s1["Superblock (4 KB)"]:1
        s2["magic MOSB | version | total_objects"]:1
        s3["各区偏移量 | 数据块计数 | 空闲块计数"]:1
        s4["created_at | last_modified | _reserved"]:1
    end

    space

    block:metadata
        columns 1
        m1["Metadata Area (N × 256 bytes)"]:1
        m2["Entry 0"]:1
        m3["Entry 1"]:1
        m4["..."]:1
        m5["Entry N-1"]:1
    end

    space

    block:bitmap
        columns 1
        b1["Free Block Bitmap"]:1
        b2["ceil(total_blocks/8) bytes | 4KB 对齐"]:1
        b3["1 = 空闲 | 0 = 占用"]:1
    end

    space

    block:data
        columns 1
        d1["Data Block Area (M × 4 KB)"]:1
        d2["Block 0: 4088 B payload | 8 B next_ptr"]:1
        d3["Block 1: 4088 B payload | 8 B next_ptr"]:1
        d4["..."]:1
        d5["Block M-1: 4088 B payload | 8 B next_ptr"]:1
    end
```

### 2.2 元数据条目布局（256 bytes）

```mermaid
block-beta
    columns 17
    uuid["uuid<br/>16 B"]:4
    name["name<br/>64 B"]:8
    size["size<br/>8 B"]:2
    ctype["content_type<br/>32 B"]:3

    space:17

    cat["created_at<br/>8 B"]:2
    tags["tags<br/>64 B"]:8
    bph["block_ptr_head<br/>8 B"]:2
    bc["block_count<br/>4 B"]:2
    fl["fl<br/>1"]:1
    ck["ck<br/>1"]:1
    res["_reserved<br/>46 B"]:1
```

### 2.3 数据块链表结构

```mermaid
flowchart LR
    subgraph Metadata
        M["MetadataEntry<br/>block_ptr_head = 3<br/>block_count = 3<br/>size = 10,000 B"]
    end

    M -->|"block_ptr_head"| B3

    subgraph "Block 3"
        B3["payload: 4088 B<br/>───────────────<br/>next_ptr: 7"]
    end

    B3 -->|"next"| B7

    subgraph "Block 7"
        B7["payload: 4088 B<br/>───────────────<br/>next_ptr: 12"]
    end

    B7 -->|"next"| B12

    subgraph "Block 12"
        B12["payload: 1824 B<br/>(末尾填充零)<br/>───────────────<br/>next_ptr: MAX<br/>(链表结束)"]
    end
```

### 2.4 位图分配算法

```mermaid
flowchart TD
    START["Bitmap::allocate_one()"] --> LOOP["遍历 bits\[word_idx\]"]
    LOOP --> CHECK{"word != 0 ?"}
    CHECK -->|"是"| TZ["bit_idx = word.trailing_zeros()"]
    TZ --> CLEAR["word &amp;= ~(1 &lt;&lt; bit_idx)<br/>free_blocks--"]
    CLEAR --> RET_OK["返回 block_idx"]
    CHECK -->|"否 (word 全 0)"| NEXT["word_idx++"]
    NEXT --> HAS_MORE{"word_idx &lt; len ?"}
    HAS_MORE -->|"是"| LOOP
    HAS_MORE -->|"否"| RET_NONE["返回 None (空间耗尽)"]
```

---

## 3. 共享内存缓冲区管理

### 3.1 区域布局

控制页（Page 0，4096 bytes）布局：

| 区域 | 偏移 | 大小 | 说明 |
|------|------|------|------|
| `ShmControlHeader` | 0 | ~32 B | 魔数、版本、页大小、总页数、空闲页数、位图偏移/大小 |
| Page Bitmap | `page_bitmap_offset` | `ceil(total/8)` B (8 字节对齐) | 页分配位图，1=空闲，0=占用 |
| `pthread_mutex_t` | 位图之后 (按 `pthread_mutex_t` 对齐) | ~40 B | 跨进程页分配互斥锁 (`PTHREAD_PROCESS_SHARED`) |
| (保留) | 互斥锁之后 | 剩余空间 | 未使用 |

数据页从 Page 1 开始，共 `total_pages` 页，每页 4096 bytes。

```mermaid
block-beta
    columns 1

    block:page0
        columns 1
        p0title["Page 0 — 控制页 (4096 B)"]:1

        block:ctrl
            columns 3
            header["ShmControlHeader<br/>~32 B"]:3
            pbitmap["Page Bitmap<br/>ceil(total/8) B (8B 对齐)"]:3
            mutex["pthread_mutex_t<br/>(PTHREAD_PROCESS_SHARED)"]:3
        end
    end

    space

    block:pages
        columns 1
        p1["Page 1 — 数据页 (4096 B)"]:1
        p2["Page 2 — 数据页 (4096 B)"]:1
        p3["..."]:1
        pn["Page N — 数据页 (4096 B)"]:1
    end
```

### 3.2 页分配 First-Fit 算法

```mermaid
flowchart TD
    START["PageAllocator::alloc_pages(count)"] --> INIT["consecutive = 0<br/>start = 0"]
    INIT --> SCAN["遍历 bit_idx 0..total_pages"]
    SCAN --> IS_FREE{"is_free(bit_idx) ?"}
    IS_FREE -->|"是"| INC["consecutive++"]
    INC --> FIRST{"consecutive == 1 ?"}
    FIRST -->|"是"| SET_START["start = bit_idx"]
    FIRST -->|"否"| CHECK_CNT
    SET_START --> CHECK_CNT{"consecutive == count ?"}
    CHECK_CNT -->|"是"| MARK["循环 i = 0..count:<br/>set_bit(start+i, false)<br/>*free_pages_ptr -= count"]
    MARK --> RET_OK["返回 Some(start)"]
    CHECK_CNT -->|"否"| NEXT
    IS_FREE -->|"否"| RESET["consecutive = 0"]
    RESET --> NEXT["bit_idx++"]
    NEXT --> HAS_MORE{"bit_idx &lt; total_pages ?"}
    HAS_MORE -->|"是"| IS_FREE
    HAS_MORE -->|"否"| RET_NONE["返回 None<br/>(外部碎片导致分配失败)"]
```

### 3.3 跨进程页分配同步

页分配位图位于共享内存中，服务端和多个客户端可能同时访问。为保证分配/释放的原子性，
使用位于控制页中的 `pthread_mutex_t`（`PTHREAD_PROCESS_SHARED` 属性）进行跨进程互斥。

**锁的获取模式**：

| 操作 | 客户端 | 服务端 |
|------|--------|--------|
| PUT (小文件) | lock → alloc → write → unlock → socket_cmd | lock(state) → lock(page) → read → free → unlock(page) → store.put |
| PUT (分块) | lock → alloc → write → unlock → socket_cmd (每块) | lock(state) → lock(page) → read → free → unlock(page) → 追加缓冲区 |
| GET | socket_cmd → lock → read → free → unlock | lock(state) → lock(page) → alloc → write → unlock(page) |

**关键设计：客户端在发送 socket 命令前释放锁。**

客户端不在持有页锁期间等待服务端响应，避免死锁。
服务端处理请求时先获取 `ServerState` 锁（`Arc<Mutex<>>`），
再获取页锁——两把锁始终按相同顺序获取：

```
ServerState 锁 (内部锁) → 页互斥锁 (外部锁)
```

客户端只持有页锁，不持有 `ServerState` 锁，因此不会发生锁序反转。

**PUT 操作生命周期**：
1. 客户端：lock → alloc → write → unlock → 发送 socket 命令
2. 服务端：收到命令 → lock(state) → lock(page) → read → free → unlock(page) → store.put → unlock(state)
3. 页由服务端释放，客户端不重复释放（避免并发竞态）

**GET 操作生命周期**：
1. 客户端：发送 socket 命令，收到响应
2. 服务端：lock(state) → 查缓存/store → lock(page) → alloc → write → unlock(page) → unlock(state) → 返回页号
3. 客户端：lock → read → free → unlock（客户端释放页）

```mermaid
sequenceDiagram
    participant C1 as 客户端 A
    participant C2 as 客户端 B
    participant MUTEX as pthread_mutex_t
    participant SHM as 共享内存位图
    participant S as 服务端

    Note over C1,S: 两个客户端并发 PUT

    C1->>MUTEX: lock() ✓
    C2->>MUTEX: lock() 阻塞...

    C1->>SHM: alloc_pages(2) → pages 0,1
    C1->>SHM: write_to_pages(0, data_A)
    C1->>MUTEX: unlock()

    C2->>MUTEX: lock() ✓
    C1->>S: socket: PUT name_A ... 0 2

    C2->>SHM: alloc_pages(2) → pages 2,3
    C2->>SHM: write_to_pages(2, data_B)
    C2->>MUTEX: unlock()

    C2->>S: socket: PUT name_B ... 2 2
    S->>S: 处理 A: lock(state)→lock(page)→read→free(0,2)→unlock(page)→put→unlock(state)
    S->>S: 处理 B: lock(state)→lock(page)→read→free(2,2)→unlock(page)→put→unlock(state)
```

---

## 4. LRU 缓存

### 4.1 数据结构

```mermaid
classDiagram
    class LruCache {
        -capacity: usize
        -max_memory: u64
        -current_memory: u64
        -map: HashMap~ObjectId, CacheEntry~
        -order: VecDeque~ObjectId~
        -hits: u64
        -misses: u64
        -evictions: u64
        +new(capacity, max_memory) Self
        +get(id) Option~&[u8]~
        +put(id, data, name, size)
        +invalidate(id)
        +hit_rate() f64
        +stats() CacheStats
        +warmup(ids, loader, limit) usize
        -touch(id)
        -remove_entry(id)
        -evict_one() bool
    }

    class CacheEntry {
        -data: Vec~u8~
        -size: u64
    }

    class CacheStats {
        +capacity: usize
        +size: usize
        +memory_used: u64
        +memory_max: u64
        +hits: u64
        +misses: u64
        +evictions: u64
        +hit_rate: f64
    }

    LruCache *-- CacheEntry : map 存储
    LruCache --> CacheStats : stats() 返回
```

### 4.2 淘汰流程

```mermaid
flowchart TD
    PUT["LruCache::put(id, data, name, size)"] --> CHECK_SIZE{"size > max_memory ?"}
    CHECK_SIZE -->|"是"| SKIP["直接返回 (不缓存超大对象)"]
    CHECK_SIZE -->|"否"| EXISTS{"map.contains(id) ?"}
    EXISTS -->|"是"| REMOVE["remove_entry(id) 先移除旧条目"]
    EXISTS -->|"否"| EVICT_LOOP
    REMOVE --> EVICT_LOOP{"current_memory + size > max_memory<br/>OR map.len() >= capacity ?"}
    EVICT_LOOP -->|"是"| EVICT["evict_one()<br/>从 VecDeque 头部弹出<br/>从 HashMap 移除<br/>current_memory -= entry.size<br/>evictions++"]
    EVICT --> EVICT_LOOP
    EVICT_LOOP -->|"否"| INSERT["map.insert(id, entry)<br/>order.push_back(id)<br/>current_memory += size"]
    INSERT --> DONE["完成"]

    subgraph "Touch (get 命中时)"
        TOUCH["get(id) 命中"] --> FIND["在 order 中查找 id 位置"]
        FIND --> REM_POS["order.remove(pos)"]
        REM_POS --> PUSH_BACK["order.push_back(id)"]
    end
```

---

## 5. 通信协议

系统采用 **双通道** 架构：

- **控制通道**：Unix Domain Socket，文本协议，每次请求一个连接
- **数据通道**：POSIX 共享内存 (`shm_open`/`mmap`)，传输对象数据

### 5.1 命令格式

所有命令以 `\n` 结尾，服务端返回以 `\n` 结尾的文本响应。

**基础命令**：

| 命令 | 格式 | 响应 | 说明 |
|------|------|------|------|
| `PUT` | `PUT <name> <size> <content_type> <tags> <start_page> <num_pages>\n` | `OK <uuid>\n` | 小文件单次上传 |
| `GET` | `GET <uuid_or_name>\n` | `OK <size> <start_page> <num_pages>\n` | 下载对象 |
| `DELETE` | `DELETE <uuid>\n` | `OK deleted\n` | 删除对象 |
| `LIST` | `LIST\n` | `OK <count>\n` + 每行一个对象 | 列出所有对象 |
| `STATUS` | `STATUS\n` | `OK\n` + 多行统计信息 | 查看服务端状态 |
| `STOP` | `STOP\n` | `OK shutting down\n` | 停止服务端 |

**错误响应**：以 `ERROR` 开头，后跟描述信息。

### 5.2 分块上传协议

大文件（超过共享内存容量）通过三步协议分块上传：

| 步骤 | 命令 | 说明 |
|------|------|------|
| 1. 开始 | `PUT_BEGIN <name> <total_size> <content_type> <tags>\n` | 服务端创建上传缓冲区 (`PendingUpload`) |
| 2. 循环 | `PUT_CHUNK <name> <chunk_size> <start_page> <num_pages>\n` | 服务端从共享内存读取块，追加到缓冲区，释放页 |
| 3. 结束 | `PUT_END <name>\n` | 服务端将完整数据写入 `store.odb`，清理缓冲区 |

```mermaid
sequenceDiagram
    participant CLI as 客户端
    participant SRV as 服务端
    participant BUF as PendingUpload 缓冲区
    participant STORE as ObjectStore

    Note over CLI,STORE: 大文件分块上传

    CLI->>SRV: PUT_BEGIN myfile 50000 text/plain {}
    SRV->>BUF: 创建缓冲区 (capacity=50000)

    loop 每块 ≤ 90% 共享内存容量
        CLI->>CLI: lock→alloc→write→unlock
        CLI->>SRV: PUT_CHUNK myfile 20000 0 5
        SRV->>SRV: lock(page)→read→free→unlock(page)
        SRV->>BUF: 追加 20000 bytes
        SRV-->>CLI: OK
    end

    CLI->>SRV: PUT_END myfile
    SRV->>STORE: store.put(name, full_data, type, tags)
    SRV->>BUF: 移除缓冲区
    SRV-->>CLI: OK <uuid>
```

**设计要点**：
- 每块最多使用 90% 的共享内存页，保留少量页避免碎片导致的分配失败
- 正常路径：页由服务端在 `PUT_CHUNK` 中释放，客户端不重复释放
- 错误路径：服务端返回 ERROR 时未释放页，客户端需自行清理
- `PUT_END` 之后数据才真正持久化到 `store.odb`

### 5.3 通信模式

```mermaid
flowchart LR
    subgraph 客户端
        CMD[构造命令字符串]
        SEND[UnixStream::connect]
        WRITE[write_all + flush]
        READ[read_to_string]
    end

    subgraph 服务端
        ACCEPT[accept 连接]
        SPAWN[thread::spawn]
        PARSE[解析命令字符串]
        DISPATCH[dispatch_command]
        EXEC[执行操作]
        REPLY[write_all 响应]
    end

    CMD --> SEND --> WRITE --> READ
    ACCEPT --> SPAWN --> PARSE --> DISPATCH --> EXEC --> REPLY
    WRITE -.->|Unix Socket| PARSE
    REPLY -.->|Unix Socket| READ
```

服务端采用 **每连接一线程** 模型：`UnixListener` 设为非阻塞模式，
主循环以 50ms 间隔轮询 `accept()`，每次 accept 成功后 `thread::spawn` 处理。
这种模型足够简单，适合课程设计场景。

---

## 6. 模块组织

```mermaid
graph TD
    subgraph "Cargo Workspace"
        LIB["minios-lib<br/>(核心库)"]
        SERVER_BIN["minios-server<br/>(服务端二进制)"]
        CLIENT_BIN["minios-client<br/>(客户端二进制)"]
    end

    SERVER_BIN --> LIB
    CLIENT_BIN --> LIB

    subgraph "minios-lib 内部模块"
        COMMON_LIB["common"]
        STORAGE_LIB["storage ★"]
        SHM_LIB["shm ★"]
        CACHE_LIB["cache"]
        PROTO_LIB["protocol"]
        DAEMON_LIB["daemon"]
    end

    LIB --> COMMON_LIB
    LIB --> STORAGE_LIB
    LIB --> SHM_LIB
    LIB --> CACHE_LIB
    LIB --> PROTO_LIB
    LIB --> DAEMON_LIB

    STORAGE_LIB -->|"superblock, bitmap, metadata, engine"| COMMON_LIB
    SHM_LIB -->|"sync, region, page"| COMMON_LIB
    CACHE_LIB -->|"lru"| COMMON_LIB
    PROTO_LIB -->|"request, response"| COMMON_LIB
    DAEMON_LIB -->|"mod"| COMMON_LIB
```

---

## 7. 测试策略

```mermaid
pie title 60 个单元测试分布
    "storage/superblock" : 9
    "storage/bitmap" : 10
    "storage/metadata" : 7
    "storage/engine" : 11
    "shm/sync" : 3
    "shm/page" : 8
    "shm/region" : 3
    "cache/lru" : 9
```

| 模块 | 测试数 | 覆盖要点 |
|------|--------|----------|
| superblock | 9 | 创建、序列化往返、魔数/版本校验、文件读写、时间戳 |
| bitmap | 10 | 单块分配、多块分配、耗尽、释放、幂等释放、序列化 |
| metadata | 7 | 空闲条目、活跃条目、校验和、序列化、中文名、截断 |
| engine | 11 | 创建/打开、Put/Get/Delete/List、大对象跨块、持久化、统计 |
| shm/sync | 3 | 互斥锁加解锁、信号量 wait/post、try_wait |
| shm/page | 8 | 单页分配、多页连续、耗尽、碎片、碎片率、边界 |
| shm/region | 3 | 创建/销毁、写入/读取、打开已存在区域 |
| cache/lru | 9 | 存/取、未命中、命中率、条目淘汰、内存淘汰、LRU 顺序、失效、预热 |

### 7.1 手动并发测试注意事项

服务端在集成测试中通常以 `./target/release/minios-server ... &` 的形式作为当前
shell 的后台任务运行。编写并发上传测试时，需要记录每个测试子进程的 PID，并逐个
`wait "$pid"`；不能直接使用无参数 `wait`，否则 shell 会同时等待仍在运行的
`minios-server`，造成测试脚本看起来卡住。

该现象属于测试脚本等待范围错误，不是共享内存页锁或服务端请求处理线程死锁。客户端
并发 `put` 返回 `OK <uuid>` 后已经完成上传，后续阻塞发生在 shell 等待后台任务阶段。
