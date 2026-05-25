//! 超级块（Superblock）— store.odb 文件的全局元信息。
//!
//! 超级块位于文件开头，占据一个完整块（4KB）。它记录了文件格式的
//! 魔数、版本、各区偏移量、对象计数等全局信息。类似于 ext2/ext4
//! 文件系统中的超级块设计。

use crate::common::consts;
use crate::common::error::{MinosError, MinosResult};
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::{SystemTime, UNIX_EPOCH};

/// 超级块结构体，占据 store.odb 文件的前 4096 字节。
///
/// 所有多字节整数以小端序存储。`#[repr(C)]` 保证字段顺序与内存布局一致。
#[repr(C)]
#[derive(Debug, Clone)]
pub struct Superblock {
    /// 魔数，固定为 `b"MOSB"`
    pub magic: [u8; 4],
    /// 文件格式版本号，当前为 1
    pub version: u32,
    /// 当前存储的对象总数（仅活跃对象）
    pub total_objects: u64,
    /// 元数据区起始偏移量（字节），始终为 4096
    pub metadata_area_offset: u64,
    /// 元数据区总大小（字节）= max_entries × entry_size，4KB 对齐
    pub metadata_area_size: u64,
    /// 单个元数据条目大小（字节），始终为 256
    pub metadata_entry_size: u64,
    /// 最大元数据条目数（决定最大对象数）
    pub max_metadata_entries: u64,
    /// 数据块区起始偏移量（字节）
    pub data_area_offset: u64,
    /// 数据块总数
    pub data_area_total_blocks: u64,
    /// 当前空闲数据块数
    pub data_area_free_blocks: u64,
    /// 单个数据块大小（字节），始终为 4096
    pub block_size: u64,
    /// 自由块位图起始偏移量（字节）
    pub free_bitmap_offset: u64,
    /// 自由块位图大小（字节）
    pub free_bitmap_size: u64,
    /// 文件创建时间（Unix 时间戳，秒）
    pub created_at: i64,
    /// 文件最后修改时间（Unix 时间戳，秒）
    pub last_modified: i64,
    /// 每个数据块中有效载荷大小（字节），始终为 4088
    pub data_block_payload: u64,
    /// 保留字段，填充至 4096 字节
    pub _reserved: [u8; 3976],
}

impl Superblock {
    /// 创建一个新的超级块实例，使用给定的最大元数据条目数和数据块总数。
    ///
    /// 根据这些参数自动计算各区偏移量和文件总大小。
    pub fn new(max_metadata_entries: u64, total_data_blocks: u64) -> Self {
        let metadata_entry_size = consts::METADATA_ENTRY_SIZE;
        let block_size = consts::BLOCK_SIZE;
        let block_payload = consts::BLOCK_PAYLOAD as u64;

        // 元数据区紧接超级块之后：offset = 4096
        let metadata_area_offset = block_size;
        // 元数据区大小 = 条目数 × 每条目大小，对齐到 4KB
        let raw_meta_size = max_metadata_entries * metadata_entry_size;
        let metadata_area_size = align_up(raw_meta_size, block_size);

        // 自由块位图紧接元数据区之后
        let free_bitmap_offset = metadata_area_offset + metadata_area_size;
        // 位图大小 = ceil(total_blocks / 8)，对齐到 4KB
        let raw_bitmap_size = (total_data_blocks + 7) / 8;
        let free_bitmap_size = align_up(raw_bitmap_size, block_size);

        // 数据块区位图之后
        let data_area_offset = free_bitmap_offset + free_bitmap_size;

        let now = current_timestamp();

        Self {
            magic: consts::STORE_MAGIC,
            version: consts::STORE_VERSION,
            total_objects: 0,
            metadata_area_offset,
            metadata_area_size,
            metadata_entry_size,
            max_metadata_entries,
            data_area_offset,
            data_area_total_blocks: total_data_blocks,
            data_area_free_blocks: total_data_blocks,
            block_size,
            free_bitmap_offset,
            free_bitmap_size,
            created_at: now,
            last_modified: now,
            data_block_payload: block_payload,
            _reserved: [0u8; 3976],
        }
    }

    /// 从文件读取超级块（文件偏移量 0 处 4096 字节）。
    pub fn read_from(file: &mut impl Read) -> MinosResult<Self> {
        let mut buf = [0u8; 4096];
        file.read_exact(&mut buf)?;

        // 验证魔数（快速失败，避免在无效文件上反序列化）
        if buf[0..4] != consts::STORE_MAGIC {
            return Err(MinosError::InvalidStore(
                "magic number mismatch: expected 'MOSB'".into(),
            ));
        }

        Ok(Self::deserialize(&buf))
    }

    /// 将超级块写入文件（文件偏移量 0 处 4096 字节）。
    pub fn write_to(&self, file: &mut (impl Write + Seek)) -> MinosResult<()> {
        file.seek(SeekFrom::Start(0))?;
        let bytes = self.serialize();
        file.write_all(&bytes)?;
        Ok(())
    }

    /// 验证超级块完整性：魔数、版本、区域一致性检查。
    pub fn validate(&self) -> MinosResult<()> {
        if self.magic != consts::STORE_MAGIC {
            return Err(MinosError::InvalidStore(format!(
                "bad magic: expected {:?}, got {:?}",
                consts::STORE_MAGIC,
                self.magic
            )));
        }
        if self.version != consts::STORE_VERSION {
            return Err(MinosError::InvalidStore(format!(
                "unsupported version: {} (expected {})",
                self.version,
                consts::STORE_VERSION
            )));
        }
        if self.block_size != consts::BLOCK_SIZE {
            return Err(MinosError::InvalidStore(format!(
                "unsupported block size: {} (expected {})",
                self.block_size,
                consts::BLOCK_SIZE
            )));
        }
        if self.metadata_entry_size != consts::METADATA_ENTRY_SIZE {
            return Err(MinosError::InvalidStore(format!(
                "unsupported entry size: {} (expected {})",
                self.metadata_entry_size,
                consts::METADATA_ENTRY_SIZE
            )));
        }

        // 验证区域偏移量的内部一致性
        let expected_bitmap_offset = self.metadata_area_offset + self.metadata_area_size;
        if self.free_bitmap_offset != expected_bitmap_offset {
            return Err(MinosError::InvalidStore(format!(
                "bitmap offset mismatch: {} vs expected {}",
                self.free_bitmap_offset, expected_bitmap_offset
            )));
        }

        let expected_data_offset = self.free_bitmap_offset + self.free_bitmap_size;
        if self.data_area_offset != expected_data_offset {
            return Err(MinosError::InvalidStore(format!(
                "data area offset mismatch: {} vs expected {}",
                self.data_area_offset, expected_data_offset
            )));
        }

        Ok(())
    }

    /// 计算 store.odb 文件的总大小。
    pub fn total_file_size(&self) -> u64 {
        self.data_area_offset + self.data_area_total_blocks * self.block_size
    }

    /// 更新最后修改时间戳。
    pub fn touch(&mut self) {
        self.last_modified = current_timestamp();
    }

    // ─── 序列化辅助方法 ───

    /// 将超级块序列化为 4096 字节数组（小端序）。
    fn serialize(&self) -> [u8; 4096] {
        let mut buf = [0u8; 4096];
        let mut offset = 0;

        // 魔数 (4 bytes)
        buf[offset..offset + 4].copy_from_slice(&self.magic);
        offset += 4;

        // version (4 bytes)
        buf[offset..offset + 4].copy_from_slice(&self.version.to_le_bytes());
        offset += 4;

        // total_objects (8 bytes)
        buf[offset..offset + 8].copy_from_slice(&self.total_objects.to_le_bytes());
        offset += 8;

        // metadata_area_offset (8 bytes)
        buf[offset..offset + 8].copy_from_slice(&self.metadata_area_offset.to_le_bytes());
        offset += 8;

        // metadata_area_size (8 bytes)
        buf[offset..offset + 8].copy_from_slice(&self.metadata_area_size.to_le_bytes());
        offset += 8;

        // metadata_entry_size (8 bytes)
        buf[offset..offset + 8].copy_from_slice(&self.metadata_entry_size.to_le_bytes());
        offset += 8;

        // max_metadata_entries (8 bytes)
        buf[offset..offset + 8].copy_from_slice(&self.max_metadata_entries.to_le_bytes());
        offset += 8;

        // data_area_offset (8 bytes)
        buf[offset..offset + 8].copy_from_slice(&self.data_area_offset.to_le_bytes());
        offset += 8;

        // data_area_total_blocks (8 bytes)
        buf[offset..offset + 8].copy_from_slice(&self.data_area_total_blocks.to_le_bytes());
        offset += 8;

        // data_area_free_blocks (8 bytes)
        buf[offset..offset + 8].copy_from_slice(&self.data_area_free_blocks.to_le_bytes());
        offset += 8;

        // block_size (8 bytes)
        buf[offset..offset + 8].copy_from_slice(&self.block_size.to_le_bytes());
        offset += 8;

        // free_bitmap_offset (8 bytes)
        buf[offset..offset + 8].copy_from_slice(&self.free_bitmap_offset.to_le_bytes());
        offset += 8;

        // free_bitmap_size (8 bytes)
        buf[offset..offset + 8].copy_from_slice(&self.free_bitmap_size.to_le_bytes());
        offset += 8;

        // created_at (8 bytes)
        buf[offset..offset + 8].copy_from_slice(&self.created_at.to_le_bytes());
        offset += 8;

        // last_modified (8 bytes)
        buf[offset..offset + 8].copy_from_slice(&self.last_modified.to_le_bytes());
        offset += 8;

        // data_block_payload (8 bytes)
        buf[offset..offset + 8].copy_from_slice(&self.data_block_payload.to_le_bytes());
        offset += 8;

        // _reserved (3976 bytes)
        buf[offset..offset + 3976].copy_from_slice(&self._reserved);

        buf
    }

    /// 从 4096 字节数组反序列化为超级块（小端序）。
    fn deserialize(buf: &[u8; 4096]) -> Self {
        let mut offset = 0;

        let magic = {
            let mut m = [0u8; 4];
            m.copy_from_slice(&buf[offset..offset + 4]);
            offset += 4;
            m
        };

        let version = u32::from_le_bytes([
            buf[offset],
            buf[offset + 1],
            buf[offset + 2],
            buf[offset + 3],
        ]);
        offset += 4;

        let total_objects = read_u64_le(buf, &mut offset);
        let metadata_area_offset = read_u64_le(buf, &mut offset);
        let metadata_area_size = read_u64_le(buf, &mut offset);
        let metadata_entry_size = read_u64_le(buf, &mut offset);
        let max_metadata_entries = read_u64_le(buf, &mut offset);
        let data_area_offset = read_u64_le(buf, &mut offset);
        let data_area_total_blocks = read_u64_le(buf, &mut offset);
        let data_area_free_blocks = read_u64_le(buf, &mut offset);
        let block_size = read_u64_le(buf, &mut offset);
        let free_bitmap_offset = read_u64_le(buf, &mut offset);
        let free_bitmap_size = read_u64_le(buf, &mut offset);
        let created_at = read_i64_le(buf, &mut offset);
        let last_modified = read_i64_le(buf, &mut offset);
        let data_block_payload = read_u64_le(buf, &mut offset);

        let mut _reserved = [0u8; 3976];
        _reserved.copy_from_slice(&buf[offset..offset + 3976]);

        Self {
            magic,
            version,
            total_objects,
            metadata_area_offset,
            metadata_area_size,
            metadata_entry_size,
            max_metadata_entries,
            data_area_offset,
            data_area_total_blocks,
            data_area_free_blocks,
            block_size,
            free_bitmap_offset,
            free_bitmap_size,
            created_at,
            last_modified,
            data_block_payload,
            _reserved,
        }
    }
}

// ─── 辅助函数 ───

/// 将 value 向上对齐到 alignment 的整数倍。
fn align_up(value: u64, alignment: u64) -> u64 {
    ((value + alignment - 1) / alignment) * alignment
}

/// 获取当前 Unix 时间戳（秒）。
fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// 从字节缓冲区读取 u64（小端序），并推进偏移量。
fn read_u64_le(buf: &[u8; 4096], offset: &mut usize) -> u64 {
    let bytes: [u8; 8] = buf[*offset..*offset + 8].try_into().unwrap();
    *offset += 8;
    u64::from_le_bytes(bytes)
}

/// 从字节缓冲区读取 i64（小端序），并推进偏移量。
fn read_i64_le(buf: &[u8; 4096], offset: &mut usize) -> i64 {
    let bytes: [u8; 8] = buf[*offset..*offset + 8].try_into().unwrap();
    *offset += 8;
    i64::from_le_bytes(bytes)
}

// ─── 单元测试 ───

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_new_superblock() {
        let sb = Superblock::new(1024, 4096);
        assert_eq!(sb.magic, *b"MOSB");
        assert_eq!(sb.version, consts::STORE_VERSION);
        assert_eq!(sb.total_objects, 0);
        assert_eq!(sb.metadata_area_offset, 4096);
        assert_eq!(sb.metadata_entry_size, 256);
        assert_eq!(sb.max_metadata_entries, 1024);
        assert_eq!(sb.data_area_total_blocks, 4096);
        assert_eq!(sb.data_area_free_blocks, 4096);
        assert_eq!(sb.block_size, 4096);
        assert_eq!(sb.data_block_payload, 4088);
        assert!(sb.created_at > 0);
    }

    #[test]
    fn test_serialize_roundtrip() {
        let sb = Superblock::new(512, 2048);
        let bytes = sb.serialize();
        let restored = Superblock::deserialize(&bytes);

        assert_eq!(restored.magic, sb.magic);
        assert_eq!(restored.version, sb.version);
        assert_eq!(restored.total_objects, sb.total_objects);
        assert_eq!(restored.metadata_area_offset, sb.metadata_area_offset);
        assert_eq!(restored.metadata_area_size, sb.metadata_area_size);
        assert_eq!(restored.data_area_total_blocks, sb.data_area_total_blocks);
        assert_eq!(restored.data_area_free_blocks, sb.data_area_free_blocks);
        assert_eq!(restored.free_bitmap_offset, sb.free_bitmap_offset);
        assert_eq!(restored.free_bitmap_size, sb.free_bitmap_size);
        assert_eq!(restored.data_area_offset, sb.data_area_offset);
        assert_eq!(restored.created_at, sb.created_at);
        assert_eq!(restored.last_modified, sb.last_modified);
    }

    #[test]
    fn test_validate_valid_superblock() {
        let sb = Superblock::new(256, 1024);
        assert!(sb.validate().is_ok());
    }

    #[test]
    fn test_validate_bad_magic() {
        let mut sb = Superblock::new(256, 1024);
        sb.magic = *b"BAD!";
        assert!(sb.validate().is_err());
    }

    #[test]
    fn test_validate_bad_version() {
        let mut sb = Superblock::new(256, 1024);
        sb.version = 99;
        assert!(sb.validate().is_err());
    }

    #[test]
    fn test_read_write_roundtrip() {
        let sb = Superblock::new(128, 512);
        let mut file = Cursor::new(vec![0u8; 4096]);

        sb.write_to(&mut file).unwrap();
        file.set_position(0);
        let restored = Superblock::read_from(&mut file).unwrap();

        assert_eq!(restored.magic, sb.magic);
        assert_eq!(restored.max_metadata_entries, sb.max_metadata_entries);
        assert_eq!(restored.data_area_total_blocks, sb.data_area_total_blocks);
    }

    #[test]
    fn test_read_bad_magic() {
        let mut file = Cursor::new([0u8; 4096]);
        let result = Superblock::read_from(&mut file);
        assert!(result.is_err());
    }

    #[test]
    fn test_total_file_size() {
        let sb = Superblock::new(1024, 4096);
        let expected = sb.data_area_offset + sb.data_area_total_blocks * sb.block_size;
        assert_eq!(sb.total_file_size(), expected);
        // 4096 (sb) + 262144 (meta = 1024*256) + 4096 (bitmap) + 16777216 (data)
        assert_eq!(sb.total_file_size(), 4096 + 262144 + 4096 + 16777216);
    }

    #[test]
    fn test_touch_updates_timestamp() {
        let mut sb = Superblock::new(128, 512);
        let old_ts = sb.last_modified;
        std::thread::sleep(std::time::Duration::from_secs(1));
        sb.touch();
        assert!(sb.last_modified > old_ts);
    }
}
