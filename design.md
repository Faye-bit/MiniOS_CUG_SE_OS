# MinOS 对象存储服务 — 设计文档

## 1. 系统概述

MinOS（Mini Object Storage）是一个简单的对象存储服务，采用扁平化命名空间管理数据，
所有对象持久化到单一复合文档文件 `store.odb` 中。系统由服务端守护进程和命令行客户端组成，
通过 Unix Domain Socket + POSIX 共享内存双通道进行进程间通信。

### 1.1 架构图

```
+─────────────+     Unix Socket (控制消息)     +──────────────+
|  minos-client| ──────────────────────────> |  minos-server |
|    (CLI)     | <────── 共享内存 (数据) ────> |   (Daemon)    |
+─────────────+                              +──────┬───────+
                                                    │
                                         +──────────┴──────────+
                                         |   ObjectStore       |
                                         |   (store.odb)       |
                                         +─────────────────────+
```

### 1.2 工作流程

1. 客户端通过 `shm_open` + `mmap` 打开共享内存区域
2. 客户端将对象数据写入共享内存的数据页
3. 客户端通过 Unix Socket 发送控制命令（含页号、大小等元信息）
4. 服务端从共享内存读取数据，写入 `store.odb`
5. 服务端通过 Socket 返回操作结果

---

## 2. store.odb 文件格式

### 2.1 布局

```
Offset 0
+──────────────────────────────────────────────+
│ Superblock (4096 bytes)                      │
│ magic("MOSB"), version, 各区偏移量/大小/计数  │
+──────────────────────────────────────────────+
│ Metadata Area (N × 256 bytes)               │
│ 每条目: uuid(16) + name(64) + size(8)       │
│ + content_type(32) + created_at(8) + tags(64)│
│ + block_ptr_head(8) + block_count(4)         │
│ + flags(1) + checksum(1) + _reserved(46)    │
+──────────────────────────────────────────────+
│ Free Block Bitmap                            │
│ ceil(total_blocks/8) bytes, 4KB 对齐         │
│ 位值: 1 = 空闲, 0 = 占用                     │
+──────────────────────────────────────────────+
│ Data Block Area (total_blocks × 4096 bytes) │
│ 每个块: [4088 bytes payload | 8 bytes next]  │
│ next = u64::MAX 表示链表末尾                  │
+──────────────────────────────────────────────+
```

### 2.2 块分配算法

- **位图存储**: 使用 `Vec<u64>` 数组，每个 u64 管理 64 个数据块
- **单块分配**: 遍历 u64 字，使用 `trailing_zeros()` 指令在单个周期内定位空闲位
- **多块分配**: 循环调用单块分配，块不需要连续（通过链表指针串联）
- **释放**: 幂等操作（重复释放不改变计数），防止 double-free 导致计数错误

### 2.3 元数据条目

- 固定 256 字节，`#[repr(C)]` 布局
- 字段顺序经优化消除 padding
- 校验和: bytes 0..205 的 XOR（覆盖 uuid 到 flags 的所有字段）
- flags: 0x00=空闲, 0x01=活跃, 0x02=tombstone（已删除待复用）

---

## 3. 共享内存缓冲区管理

### 3.1 区域布局

```
Page 0 (控制页, 4096 bytes):
+──────────────────────────────────────────────+
│ ShmControlHeader (48 bytes 头部)             │
│   magic("MOSM"), total_pages, free_pages...  │
├──────────────────────────────────────────────┤
│ Page Bitmap (ceil(total_pages/8) bytes)      │
├──────────────────────────────────────────────┤
│ Request Slots (max_requests × 256 bytes)     │
├──────────────────────────────────────────────┤
│ Response Slots (max_requests × 256 bytes)    │
├──────────────────────────────────────────────┤
│ pthread_mutex_t (process-shared)             │
+──────────────────────────────────────────────+

Pages 1 .. N (数据页, 每页 4096 bytes):
  对象数据传输区域
```

### 3.2 页分配算法（First-Fit）

```
alloc_pages(count):
  从头开始逐位扫描位图
  遇到空闲位(1)时开始计数
  连续计数达到 count 时，将这些位标记为 0（占用）
  返回起始页号
  如果没有足够连续空闲位，返回 None

free_pages(start, count):
  将 [start, start+count) 范围内的位设置为 1（空闲）
  幂等: 仅当位当前为 0 时才递增 free_pages
```

### 3.3 碎片度量

碎片率 = 1 - (最大连续空闲块大小 / 总空闲页数)

值越接近 1.0 表示碎片化越严重。

### 3.4 同步机制

- **互斥锁**: `pthread_mutex_t` 位于共享内存中，使用 `PTHREAD_PROCESS_SHARED` 属性
- **信号量**: POSIX 命名信号量 (`sem_open`/`sem_wait`/`sem_post`) 用于客户端-服务端通知
  - `minos_server_sem`: 服务端等待客户端请求
  - `minos_client_sem`: 客户端等待服务端响应

---

## 4. LRU 缓存

- 数据结构: `HashMap<ObjectId, CacheEntry>` + `VecDeque<ObjectId>`（访问顺序）
- 限制策略: 条目数限制 + 内存占用限制（双阈值）
- 淘汰: 从 VecDeque 头部弹出最久未使用的条目
- Touch: 访问时将条目移到 VecDeque 尾部
- 预热: 从存储引擎预加载对象元数据

---

## 5. 通信协议

### 5.1 请求槽位（256 bytes）

| 字段 | 偏移 | 大小 | 说明 |
|------|------|------|------|
| size | 0 | 8 | 对象数据大小 |
| timestamp | 8 | 8 | 请求时间戳 |
| client_id | 16 | 4 | 客户端 PID |
| num_pages | 20 | 4 | 占用共享内存页数 |
| start_page | 24 | 4 | 起始页号 |
| request_type | 28 | 1 | 0=Put,1=Get,2=Delete,3=List,4=Status,5=Shutdown |
| status | 29 | 1 | 0=空闲,1=待处理,2=处理中,3=已完成 |
| object_id | 30 | 16 | UUID |
| name | 46 | 64 | 对象名称 |
| content_type | 110 | 32 | MIME 类型 |
| tags | 142 | 64 | 标签 JSON |

### 5.2 响应槽位（256 bytes）

| 字段 | 偏移 | 大小 | 说明 |
|------|------|------|------|
| size | 0 | 8 | 返回数据大小 |
| client_id | 8 | 4 | 客户端 ID |
| num_pages | 12 | 4 | 数据占用页数 |
| start_page | 16 | 4 | 数据起始页号 |
| list_count | 20 | 4 | List 返回数量 |
| status_code | 24 | 1 | 0=Ok,1=NotFound,2=NoSpace,3=Error |
| slot_status | 25 | 1 | 0=空闲,1=已填充 |
| object_id | 26 | 16 | UUID |
| message | 42 | 128 | 状态消息 |

---

## 6. 模块组织

```
MiniOS/
  minos-lib/          核心库
    common/           常量、类型、错误
    storage/          ★ 对象存储引擎 (superblock, bitmap, metadata, engine)
    shm/              ★ 共享内存管理 (sync, region, page)
    cache/            LRU 缓存
    protocol/         通信协议 (request, response)
    daemon/           守护进程管理
  minos-server/       服务端入口
  minos-client/       客户端入口
```

---

## 7. 测试策略

- **单元测试** (57 个): 每个模块独立测试，覆盖边界条件和错误路径
  - storage: 37 个（superblock 9 + bitmap 10 + metadata 7 + engine 11）
  - shm: 11 个（sync 3 + page 8）
  - cache: 9 个
- **集成测试** (Phase 7): 端到端流程、并发访问、持久化重启
