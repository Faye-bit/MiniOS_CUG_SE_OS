//! 全局常量定义。

/// store.odb 文件魔数
pub const STORE_MAGIC: [u8; 4] = *b"MOSB";

/// 共享内存区域魔数
pub const SHM_MAGIC: [u8; 4] = *b"MOSM";

/// 当前文件格式版本
pub const STORE_VERSION: u32 = 1;

/// 数据块大小（字节）
pub const BLOCK_SIZE: u64 = 4096;

/// 每个数据块中实际有效载荷（为 next 指针预留 8 字节）
pub const BLOCK_PAYLOAD: usize = (BLOCK_SIZE as usize) - 8; // 4088

/// 块链结束标记
pub const BLOCK_CHAIN_END: u64 = u64::MAX;

/// 元数据条目大小（字节）
pub const METADATA_ENTRY_SIZE: u64 = 256;

/// 对象名称最大长度（含结尾 null）
pub const MAX_NAME_LEN: usize = 63;

/// 内容类型字符串最大长度（含结尾 null）
pub const MAX_CONTENT_TYPE_LEN: usize = 31;

/// 标签 JSON 字符串最大长度（含结尾 null）
pub const MAX_TAGS_LEN: usize = 63;

/// 共享内存页面大小（字节）
pub const SHM_PAGE_SIZE: u32 = 4096;

/// 默认 Unix Domain Socket 路径
pub const DEFAULT_SOCKET_PATH: &str = "/tmp/minos.sock";

/// 默认共享内存名称
pub const DEFAULT_SHM_NAME: &str = "/minos_shm";

/// 默认存储文件路径
pub const DEFAULT_STORE_PATH: &str = "./store.odb";

/// 默认最大对象数量
pub const DEFAULT_MAX_OBJECTS: u64 = 1024;

/// 默认数据块总数
pub const DEFAULT_TOTAL_BLOCKS: u64 = 4096;

/// 默认缓存容量（条目数）
pub const DEFAULT_CACHE_CAPACITY: usize = 128;

/// 默认缓存最大内存占用（字节，64MB）
pub const DEFAULT_CACHE_MAX_MEMORY: u64 = 64 * 1024 * 1024;

/// 默认共享内存数据页数
pub const DEFAULT_SHM_PAGES: u32 = 256;

/// 默认服务端最大并发连接数
pub const DEFAULT_MAX_CLIENTS: usize = 16;
