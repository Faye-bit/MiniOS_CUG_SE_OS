//! 核心数据类型定义。

/// 对象全局唯一标识符（16 字节 UUID 原始字节）
pub type ObjectId = [u8; 16];

/// 数据块索引
pub type BlockIndex = u64;

/// 共享内存页号
pub type PageIndex = u32;

/// 对象基本信息摘要，用于 List 操作返回。
#[derive(Debug, Clone)]
pub struct ObjectSummary {
    /// 对象 UUID
    pub uuid: ObjectId,
    /// 对象名称
    pub name: String,
    /// 对象数据大小（字节）
    pub size: u64,
    /// 内容类型（MIME 类型）
    pub content_type: String,
    /// 创建时间（Unix 时间戳）
    pub created_at: i64,
    /// 自定义标签
    pub tags: String,
    /// 占用的数据块数量
    pub block_count: u32,
}

/// 完整对象数据，用于 Get 操作返回。
#[derive(Debug, Clone)]
pub struct ObjectData {
    /// 对象元数据摘要
    pub summary: ObjectSummary,
    /// 对象实际数据内容
    pub data: Vec<u8>,
}

/// 存储引擎统计信息。
#[derive(Debug, Clone)]
pub struct StoreStats {
    /// 当前存储的对象总数
    pub total_objects: u64,
    /// 数据块总数
    pub total_blocks: u64,
    /// 空闲数据块数
    pub free_blocks: u64,
    /// 已用数据块数
    pub used_blocks: u64,
    /// 文件总大小（字节）
    pub file_size: u64,
    /// 创建时间（Unix 时间戳）
    pub created_at: i64,
    /// 最后修改时间（Unix 时间戳）
    pub last_modified: i64,
}

/// 缓存统计信息。
#[derive(Debug, Clone)]
pub struct CacheStats {
    /// 最大条目容量
    pub capacity: usize,
    /// 当前条目数
    pub size: usize,
    /// 当前内存占用（字节）
    pub memory_used: u64,
    /// 最大内存限制（字节）
    pub memory_max: u64,
    /// 命中次数
    pub hits: u64,
    /// 未命中次数
    pub misses: u64,
    /// 淘汰次数
    pub evictions: u64,
    /// 命中率（0.0 ~ 1.0）
    pub hit_rate: f64,
}

/// 共享内存统计信息。
#[derive(Debug, Clone)]
pub struct ShmStats {
    /// 数据页总数
    pub total_pages: u32,
    /// 空闲页数
    pub free_pages: u32,
    /// 已用页数
    pub used_pages: u32,
    /// 碎片率（0.0 ~ 1.0，越高越碎片化）
    pub fragmentation_ratio: f64,
}
