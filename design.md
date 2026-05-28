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
    participant SOCK as Unix Socket
    participant SRV as minios-server
    participant ENG as ObjectStore
    participant ODB as store.odb

    Note over User,ODB: === PUT 操作 ===

    User->>CLI: minios put ./data.bin --name obj1
    CLI->>SHM: shm_open + mmap 打开共享内存
    CLI->>SHM: PageAllocator::alloc_pages(N) 申请数据页
    CLI->>SHM: write_to_pages(start, data) 写入对象数据
    CLI->>SOCK: 发送 PUT 命令 (name, size, pages...)
    SOCK->>SRV: 接收请求
    SRV->>SHM: read_from_pages(start, size) 读取数据
    SRV->>ENG: store.put(name, data, type, tags)
    ENG->>ODB: Bitmap::allocate_multi(N) 分配数据块
    ENG->>ODB: write_data_block() × N (含 next 指针链表)
    ENG->>ODB: MetadataEntry::new() + write_metadata_entry()
    ENG->>ODB: Superblock::write_to() 更新超级块
    SRV->>SHM: PageAllocator::free_pages() 释放页
    SRV->>SOCK: 返回 UUID
    SOCK->>CLI: 显示结果

    Note over User,ODB: === GET 操作 ===

    User->>CLI: minios get <uuid>
    CLI->>SOCK: 发送 GET 命令 (uuid)
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
    SRV->>SHM: alloc_pages + write_to_pages
    SRV->>SOCK: 返回响应 (pages, size)
    SOCK->>CLI: 从共享内存读取数据
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
    CLI_MAIN --> PROTOCOL
    SRV_MAIN --> STORAGE
    SRV_MAIN --> SHM_MOD
    SRV_MAIN --> CACHE_MOD
    SRV_MAIN --> PROTOCOL
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

```mermaid
block-beta
    columns 1

    block:page0
        columns 1
        p0title["Page 0 — 控制页 (4096 B)"]:1

        block:ctrl
            columns 5
            header["ShmControlHeader<br/>48 B"]:5
            pbitmap["Page Bitmap<br/>ceil(total/8) B"]:5
            req["Request Slots<br/>max_req × 256 B"]:5
            resp["Response Slots<br/>max_req × 256 B"]:5
            mutex["pthread_mutex_t<br/>(PTHREAD_PROCESS_SHARED)"]:5
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

### 3.3 客户端-服务端同步机制

```mermaid
sequenceDiagram
    participant C as Client
    participant SHM as 共享内存
    participant MUTEX as pthread_mutex_t
    participant SEM_SRV as sem_server
    participant SEM_CLI as sem_client
    participant S as Server

    Note over C,S: === 请求阶段 ===

    C->>MUTEX: pthread_mutex_lock()
    C->>SHM: 查找空闲 Request Slot
    C->>SHM: 填充请求 (type, uuid, name, pages...)
    C->>SHM: slot.status = PENDING
    C->>MUTEX: pthread_mutex_unlock()
    C->>SEM_SRV: sem_post() 通知服务端

    Note over C,S: === 处理阶段 ===

    S->>SEM_SRV: sem_wait() 等待请求
    S->>MUTEX: pthread_mutex_lock()
    S->>SHM: 扫描 Request Slots
    S->>SHM: slot.status = PROCESSING
    S->>MUTEX: pthread_mutex_unlock()
    S->>S: 处理请求 (Put/Get/Delete...)

    Note over C,S: === 响应阶段 ===

    S->>MUTEX: pthread_mutex_lock()
    S->>SHM: 填充 Response Slot
    S->>SHM: req.status = DONE
    S->>MUTEX: pthread_mutex_unlock()
    S->>SEM_CLI: sem_post() 通知客户端

    C->>SEM_CLI: sem_wait() 等待响应
    C->>MUTEX: pthread_mutex_lock()
    C->>SHM: 读取 Response Slot
    C->>MUTEX: pthread_mutex_unlock()
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

### 5.1 消息类型

```mermaid
classDiagram
    class RequestType {
        <<enumeration>>
        Put = 0
        Get = 1
        Delete = 2
        List = 3
        Status = 4
        Shutdown = 5
    }

    class ShmRequest {
        <<256 bytes, #[repr(C)]>>
        +size: u64
        +timestamp: i64
        +client_id: u32
        +num_pages: u32
        +start_page: u32
        +request_type: u8
        +status: u8
        +object_id: [u8; 16]
        +name: [u8; 64]
        +content_type: [u8; 32]
        +tags: [u8; 64]
    }

    class ResponseStatus {
        <<enumeration>>
        Ok = 0
        NotFound = 1
        NoSpace = 2
        Error = 3
        InvalidRequest = 4
    }

    class ShmResponse {
        <<256 bytes, #[repr(C)]>>
        +size: u64
        +client_id: u32
        +num_pages: u32
        +start_page: u32
        +list_count: u32
        +status_code: u8
        +slot_status: u8
        +object_id: [u8; 16]
        +message: [u8; 128]
    }

    ShmRequest --> RequestType
    ShmResponse --> ResponseStatus
```

### 5.2 槽位状态机

```mermaid
stateDiagram-v2
    [*] --> FREE : 初始化
    FREE --> PENDING : 客户端填充请求
    PENDING --> PROCESSING : 服务端开始处理
    PROCESSING --> DONE : 服务端处理完成
    DONE --> FREE : 客户端读取响应后重置
```

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
pie title 57 个单元测试分布
    "storage/superblock" : 9
    "storage/bitmap" : 10
    "storage/metadata" : 7
    "storage/engine" : 11
    "shm/sync" : 3
    "shm/page" : 8
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
| cache/lru | 9 | 存/取、未命中、命中率、条目淘汰、内存淘汰、LRU 顺序、失效、预热 |

### 7.1 手动并发测试注意事项

服务端在集成测试中通常以 `./target/release/minios-server ... &` 的形式作为当前
shell 的后台任务运行。编写并发上传测试时，需要记录每个测试子进程的 PID，并逐个
`wait "$pid"`；不能直接使用无参数 `wait`，否则 shell 会同时等待仍在运行的
`minios-server`，造成测试脚本看起来卡住。

该现象属于测试脚本等待范围错误，不是共享内存页锁或服务端请求处理线程死锁。客户端
并发 `put` 返回 `OK <uuid>` 后已经完成上传，后续阻塞发生在 shell 等待后台任务阶段。
