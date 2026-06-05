//! 对象元数据条目（MetadataEntry）— 存储单个对象的元信息。
//!
//! 每个条目固定 256 字节，位于 store.odb 的元数据区中。
//! 服务器启动时将整个元数据区加载到内存中进行线性扫描查找。
//!
//! 条目状态由 `flags` 字段控制：
//! - 0x00 = 空闲槽位
//! - 0x01 = 活跃对象
//! - 0x02 = 已删除（tombstone，等待复用）

use crate::common::consts;
use crate::common::types::ObjectId;

/// 元数据条目标志位。
pub mod flags {
    pub const FREE: u8 = 0x00;
    pub const ACTIVE: u8 = 0x01;
    pub const TOMBSTONE: u8 = 0x02;
}

/// 对象元数据条目。
///
/// 字段顺序经过精心排列以消除 #[repr(C)] 的 padding，
/// 使内存布局与磁盘序列化格式完全一致（均为 256 字节）。
///
/// 内存/磁盘布局（偏移量）：
/// ``` text
///   0..16  uuid           [u8; 16]
///  16..80  name           [u8; 64]
///  80..88  size           u64
///  88..120 content_type   [u8; 32]
/// 120..128 created_at     i64
/// 128..192 tags           [u8; 64]
/// 192..200 block_ptr_head u64
/// 200..204 block_count    u32
/// 204      flags          u8
/// 205      checksum       u8
/// 206..256 _reserved      [u8; 50]
/// ```
#[repr(C)]
#[derive(Debug, Clone)]
pub struct MetadataEntry {
    /// UUID v4 原始字节（16 bytes）
    pub uuid: [u8; 16],
    /// 对象名称，UTF-8 编码，null-terminated（64 bytes）
    pub name: [u8; 64],
    /// 对象数据大小（字节）
    pub size: u64,
    /// 内容类型（MIME 类型），null-terminated（32 bytes）
    pub content_type: [u8; 32],
    /// 创建时间（Unix 时间戳，秒）
    pub created_at: i64,
    /// 自定义标签（JSON 字符串），null-terminated（64 bytes）
    pub tags: [u8; 64],
    /// 数据块链表的头块索引（`BLOCK_CHAIN_END` 表示空链表）
    pub block_ptr_head: u64,
    /// 占用的数据块数量
    pub block_count: u32,
    /// 标志位：0 = 空闲, 1 = 活跃, 2 = tombstone
    pub flags: u8,
    /// 校验和（bytes 0..205 的 XOR）
    pub checksum: u8,
    /// 保留字段，填充至 256 字节
    pub _reserved: [u8; 50],
}

// 编译期大小检查
const _: () = assert!(std::mem::size_of::<MetadataEntry>() == 256);

impl MetadataEntry {
    /// 创建一个空闲槽位条目。
    pub const fn empty() -> Self {
        Self {
            uuid: [0u8; 16],
            name: [0u8; 64],
            size: 0,
            content_type: [0u8; 32],
            created_at: 0,
            tags: [0u8; 64],
            block_ptr_head: consts::BLOCK_CHAIN_END,
            block_count: 0,
            flags: flags::FREE,
            checksum: 0,
            _reserved: [0u8; 50],
        }
    }

    /// 创建一个活跃对象元数据条目。
    ///
    /// 自动截断超长的名称、类型和标签字符串。
    pub fn new(
        uuid: ObjectId,
        name: &str,
        size: u64,
        content_type: &str,
        tags: &str,
        block_ptr_head: u64,
        block_count: u32,
        created_at: i64,
    ) -> Self {
        let mut entry = Self {
            uuid,
            name: pack_str::<64>(name),
            size,
            content_type: pack_str::<32>(content_type),
            created_at,
            tags: pack_str::<64>(tags),
            block_ptr_head,
            block_count,
            flags: flags::ACTIVE,
            checksum: 0,
            _reserved: [0u8; 50],
        };
        entry.update_checksum();
        entry
    }

    /// 从 256 字节数组反序列化。
    pub fn from_bytes(bytes: &[u8; 256]) -> Self {
        let mut offset = 0;
        // 按照字段顺序逐个读取，更新 offset
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&bytes[offset..offset + 16]);
        offset += 16;
        
        let mut name = [0u8; 64];
        name.copy_from_slice(&bytes[offset..offset + 64]);
        offset += 64;

        let size = read_u64_le(bytes, &mut offset);

        let mut content_type = [0u8; 32];
        content_type.copy_from_slice(&bytes[offset..offset + 32]);
        offset += 32;

        let created_at = read_i64_le(bytes, &mut offset);

        let mut tags = [0u8; 64];
        tags.copy_from_slice(&bytes[offset..offset + 64]);
        offset += 64;

        let block_ptr_head = read_u64_le(bytes, &mut offset);
        let block_count = read_u32_le(bytes, &mut offset);
        let flags = bytes[offset];
        offset += 1;
        let checksum = bytes[offset];
        offset += 1;

        let mut _reserved = [0u8; 50];
        _reserved.copy_from_slice(&bytes[offset..offset + 50]);

        Self {
            uuid,
            name,
            size,
            content_type,
            created_at,
            tags,
            block_ptr_head,
            block_count,
            flags,
            checksum,
            _reserved,
        }
    }

    /// 序列化为 256 字节数组。
    pub fn to_bytes(&self) -> [u8; 256] {
        let mut buf = [0u8; 256];
        let mut offset = 0;

        buf[offset..offset + 16].copy_from_slice(&self.uuid);
        offset += 16;

        buf[offset..offset + 64].copy_from_slice(&self.name);
        offset += 64;

        buf[offset..offset + 8].copy_from_slice(&self.size.to_le_bytes());
        offset += 8;

        buf[offset..offset + 32].copy_from_slice(&self.content_type);
        offset += 32;

        buf[offset..offset + 8].copy_from_slice(&self.created_at.to_le_bytes());
        offset += 8;

        buf[offset..offset + 64].copy_from_slice(&self.tags);
        offset += 64;

        buf[offset..offset + 8].copy_from_slice(&self.block_ptr_head.to_le_bytes());
        offset += 8;

        buf[offset..offset + 4].copy_from_slice(&self.block_count.to_le_bytes());
        offset += 4;

        buf[offset] = self.flags;
        offset += 1;
        buf[offset] = self.checksum;
        offset += 1;

        buf[offset..offset + 50].copy_from_slice(&self._reserved);

        buf
    }

    /// 计算校验和并写入 checksum 字段。
    ///
    /// 校验和为 bytes 0..205 按位异或的结果。
    pub fn update_checksum(&mut self) {
        let bytes = self.to_bytes();
        let checksum = bytes[0..205].iter().fold(0u8, |acc, &b| acc ^ b);
        self.checksum = checksum;
    }

    /// 验证校验和是否正确。
    pub fn verify_checksum(&self) -> bool {
        let bytes = self.to_bytes();
        let computed = bytes[0..205].iter().fold(0u8, |acc, &b| acc ^ b);
        computed == self.checksum
    }

    /// 提取名称字符串（到第一个 null 字节为止）。
    pub fn name_str(&self) -> &str {
        fixed_bytes_to_str(&self.name)
    }

    /// 提取内容类型字符串。
    pub fn content_type_str(&self) -> &str {
        fixed_bytes_to_str(&self.content_type)
    }

    /// 提取标签字符串。
    pub fn tags_str(&self) -> &str {
        fixed_bytes_to_str(&self.tags)
    }

    /// 此槽位是否空闲。
    pub fn is_free(&self) -> bool {
        self.flags == flags::FREE
    }

    /// 此槽位是否为活跃对象。
    pub fn is_active(&self) -> bool {
        self.flags == flags::ACTIVE
    }
}

// ─── 辅助函数 ───

/// 将字符串写入定长字节数组，超出部分截断，剩余部分填 0（最后一个字节固定为 null terminator）。
fn pack_str<const N: usize>(s: &str) -> [u8; N] {
    let bytes = s.as_bytes();
    let cap = N - 1; // 为 null terminator 保留一个字节
    let len = bytes.len().min(cap);
    let mut buf = [0u8; N];
    buf[..len].copy_from_slice(&bytes[..len]);
    buf
}

/// 从定长字节数组提取字符串（到第一个 null 字节或末尾）。
fn fixed_bytes_to_str(bytes: &[u8]) -> &str {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).unwrap_or("")
}

fn read_u64_le(bytes: &[u8; 256], offset: &mut usize) -> u64 {
    let val = u64::from_le_bytes(bytes[*offset..*offset + 8].try_into().unwrap());
    *offset += 8;
    val
}

fn read_i64_le(bytes: &[u8; 256], offset: &mut usize) -> i64 {
    let val = i64::from_le_bytes(bytes[*offset..*offset + 8].try_into().unwrap());
    *offset += 8;
    val
}

fn read_u32_le(bytes: &[u8; 256], offset: &mut usize) -> u32 {
    let val = u32::from_le_bytes(bytes[*offset..*offset + 4].try_into().unwrap());
    *offset += 4;
    val
}

// ─── 单元测试 ───

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn make_test_uuid() -> ObjectId {
        *Uuid::new_v4().as_bytes()
    }

    #[test]
    fn test_empty_entry() {
        let entry = MetadataEntry::empty();
        assert!(entry.is_free());
        assert!(!entry.is_active());
        assert_eq!(entry.size, 0);
        assert_eq!(entry.block_count, 0);
        assert_eq!(entry.block_ptr_head, consts::BLOCK_CHAIN_END);
    }

    #[test]
    fn test_new_active_entry() {
        let uuid = make_test_uuid();
        let entry = MetadataEntry::new(
            uuid,
            "test.txt",
            1024,
            "text/plain",
            r#"{"author":"me"}"#,
            42,
            1,
            1700000000,
        );
        assert!(entry.is_active());
        assert!(!entry.is_free());
        assert_eq!(entry.uuid, uuid);
        assert_eq!(entry.name_str(), "test.txt");
        assert_eq!(entry.size, 1024);
        assert_eq!(entry.content_type_str(), "text/plain");
        assert_eq!(entry.tags_str(), r#"{"author":"me"}"#);
        assert_eq!(entry.block_ptr_head, 42);
        assert_eq!(entry.block_count, 1);
        assert_eq!(entry.created_at, 1700000000);
    }

    #[test]
    fn test_name_truncation() {
        let uuid = make_test_uuid();
        let long_name = "a".repeat(100);
        let entry = MetadataEntry::new(uuid, &long_name, 0, "", "", 0, 0, 0);
        assert_eq!(entry.name_str().len(), 63);
    }

    #[test]
    fn test_checksum() {
        let uuid = make_test_uuid();
        let mut entry = MetadataEntry::new(uuid, "check", 100, "app/octet", "", 0, 0, 0);
        assert!(entry.verify_checksum());

        // 篡改后校验和应失效
        entry.size = 999;
        assert!(!entry.verify_checksum());
    }

    #[test]
    fn test_serialize_roundtrip() {
        let uuid = make_test_uuid();
        let entry = MetadataEntry::new(
            uuid,
            "roundtrip.bin",
            4096,
            "application/octet-stream",
            r#"{"key":"value"}"#,
            10,
            2,
            1700000000,
        );
        let bytes = entry.to_bytes();
        let restored = MetadataEntry::from_bytes(&bytes);

        assert_eq!(restored.uuid, entry.uuid);
        assert_eq!(restored.name_str(), entry.name_str());
        assert_eq!(restored.size, entry.size);
        assert_eq!(restored.content_type_str(), entry.content_type_str());
        assert_eq!(restored.tags_str(), entry.tags_str());
        assert_eq!(restored.block_count, entry.block_count);
        assert_eq!(restored.block_ptr_head, entry.block_ptr_head);
        assert_eq!(restored.flags, entry.flags);
        assert_eq!(restored.created_at, entry.created_at);
        assert!(restored.verify_checksum());
    }

    #[test]
    fn test_empty_name() {
        let uuid = make_test_uuid();
        let entry = MetadataEntry::new(uuid, "", 0, "", "", 0, 0, 0);
        assert_eq!(entry.name_str(), "");
    }

    #[test]
    fn test_chinese_name() {
        let uuid = make_test_uuid();
        let entry = MetadataEntry::new(uuid, "测试文件.txt", 0, "", "", 0, 0, 0);
        assert_eq!(entry.name_str(), "测试文件.txt");
    }
}
