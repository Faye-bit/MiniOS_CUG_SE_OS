//! 对象存储引擎（ObjectStore）— store.odb 文件的核心管理接口。
//!
//! 负责创建/打开存储文件，并提供 Put、Get、Delete、List 四项原子操作。
//! 内部协调超级块、位图和元数据区的读写。

use crate::common::consts;
use crate::common::error::{MiniosError, MiniosResult};
use crate::common::types::{ObjectData, ObjectId, ObjectSummary, StoreStats};
use crate::storage::bitmap::BlockBitmap;
use crate::storage::metadata::{flags, MetadataEntry};
use crate::storage::superblock::Superblock;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// 对象存储引擎。
///
/// 管理 store.odb 文件的完整生命周期：
/// - 创建/打开存储文件
/// - 对象的增删查改
/// - 超级块、位图和元数据的持久化
pub struct ObjectStore {
    /// 存储文件句柄
    file: File,
    /// 超级块（全局元信息）
    superblock: Superblock,
    /// 自由块位图（数据块分配状态）
    bitmap: BlockBitmap,
    /// 元数据缓存（启动时全部加载到内存，关闭时持久化）
    metadata_cache: Vec<MetadataEntry>,
}

impl ObjectStore {
    // ─── 生命周期 ───

    /// 创建一个新的 store.odb 文件。
    ///
    /// # 参数
    /// - `path`: 文件路径
    /// - `max_objects`: 最大对象数（决定元数据区大小）
    /// - `total_blocks`: 数据块总数（决定文件总大小）
    pub fn create(
        path: impl AsRef<Path>,
        max_objects: u64,
        total_blocks: u64,
    ) -> MiniosResult<Self> {
        let superblock = Superblock::new(max_objects, total_blocks);
        let file_size = superblock.total_file_size();

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path.as_ref())
            .map_err(|e| {
                MiniosError::Io(std::io::Error::new(
                    e.kind(),
                    format!("cannot create store file '{}': {}", path.as_ref().display(), e),
                ))
            })?;

        // 预分配文件空间
        file.set_len(file_size)?;

        // 写入超级块
        superblock.write_to(&mut file)?;

        // 元数据区初始化为全零
        let meta_size = superblock.metadata_area_size;
        file.seek(SeekFrom::Start(superblock.metadata_area_offset))?;
        write_zeros(&mut file, meta_size)?;

        // 位图区初始化为全 1（所有块空闲）
        let bitmap = BlockBitmap::new(total_blocks);
        let bitmap_bytes = bitmap.to_bytes();
        file.seek(SeekFrom::Start(superblock.free_bitmap_offset))?;
        file.write_all(&bitmap_bytes)?;
        // 对齐填充
        let padded = align_up(bitmap_bytes.len() as u64, superblock.block_size);
        if padded > bitmap_bytes.len() as u64 {
            write_zeros(&mut file, padded - bitmap_bytes.len() as u64)?;
        }

        file.flush()?;

        let meta_entries = vec![MetadataEntry::empty(); max_objects as usize];

        Ok(Self {
            file,
            superblock,
            bitmap,
            metadata_cache: meta_entries,
        })
    }

    /// 打开一个已有的 store.odb 文件。
    pub fn open(path: impl AsRef<Path>) -> MiniosResult<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.as_ref())
            .map_err(|e| {
                MiniosError::Io(std::io::Error::new(
                    e.kind(),
                    format!("cannot open store file '{}': {}", path.as_ref().display(), e),
                ))
            })?;

        let superblock = Superblock::read_from(&mut file)?;
        superblock.validate()?;

        // 加载位图
        let bitmap_size = superblock.free_bitmap_size as usize;
        let mut bitmap_bytes = vec![0u8; bitmap_size];
        file.seek(SeekFrom::Start(superblock.free_bitmap_offset))?;
        file.read_exact(&mut bitmap_bytes)?;
        let bitmap = BlockBitmap::from_bytes(&bitmap_bytes, superblock.data_area_total_blocks);

        // 加载元数据区到内存
        let num_entries = superblock.max_metadata_entries as usize;
        let mut metadata_cache = Vec::with_capacity(num_entries);
        file.seek(SeekFrom::Start(superblock.metadata_area_offset))?;
        for slot in 0..num_entries {
            let mut entry_buf = [0u8; 256];
            file.read_exact(&mut entry_buf)?;
            let entry = MetadataEntry::from_bytes(&entry_buf);
            if entry.is_active() && !entry.verify_checksum() {
                return Err(MiniosError::InvalidStore(format!(
                    "metadata checksum mismatch at slot {slot}"
                )));
            }
            metadata_cache.push(entry);
        }

        Ok(Self {
            file,
            superblock,
            bitmap,
            metadata_cache,
        })
    }

    // ─── 对象操作 ───

    /// 存储一个对象，返回分配的 UUID。
    ///
    /// 流程：生成 UUID → 分配数据块 → 写入块数据（带链表指针） →
    /// 查找空闲元数据槽位 → 写入元数据 → 更新超级块 → 持久化。
    pub fn put(
        &mut self,
        name: &str,
        data: &[u8],
        content_type: &str,
        tags: &str,
    ) -> MiniosResult<ObjectId> {
        // 0. 检查重名冲突 —— 必须先检查，避免已分配数据块后发现重名导致资源泄漏。
        if self.find_by_name(name).is_some() {
            return Err(MiniosError::ObjectAlreadyExists(format!(
                "object with name '{}' already exists. Delete it first or use a different name.",
                name
            )));
        }

        let data_len = data.len() as u64;
        let block_count = if data_len == 0 {
            0
        } else {
            ((data_len as usize) + consts::BLOCK_PAYLOAD - 1) / consts::BLOCK_PAYLOAD
        } as u32;

        // 1. 查找空闲元数据槽位，避免数据块已分配但无槽位可写。
        let slot = self
            .find_free_slot()
            .ok_or_else(|| MiniosError::NoSpace("no free metadata slots".into()))?;

        // 2. 分配数据块
        let block_indices = if block_count > 0 {
            self.bitmap.allocate_multi(block_count)?
        } else {
            Vec::new()
        };

        // 3. 写入数据块并建立链表
        for (i, &block_idx) in block_indices.iter().enumerate() {
            let start = i * consts::BLOCK_PAYLOAD;
            let end = ((i + 1) * consts::BLOCK_PAYLOAD).min(data.len());
            let payload = &data[start..end];

            // 下一块索引
            let next = if i + 1 < block_indices.len() {
                block_indices[i + 1]
            } else {
                consts::BLOCK_CHAIN_END
            };

            self.write_data_block(block_idx, payload, next)?;
        }

        let block_ptr_head = block_indices.first().copied().unwrap_or(consts::BLOCK_CHAIN_END);

        // 4. 生成 UUID 和元数据
        let uuid = *Uuid::new_v4().as_bytes();
        let now = current_timestamp();
        let mut entry = MetadataEntry::new(
            uuid,
            name,
            data_len,
            content_type,
            tags,
            block_ptr_head,
            block_count,
            now,
        );

        // 计算校验和后再写入
        entry.update_checksum();
        self.write_metadata_entry(slot, &entry)?;
        self.metadata_cache[slot] = entry;

        // 5. 更新超级块并持久化（位图 -> 超级块 -> fsync）
        self.superblock.total_objects += 1;
        self.superblock.data_area_free_blocks = self.bitmap.free_count();
        self.superblock.touch();
        self.flush_bitmap()?;
        self.flush_superblock()?;
        self.file.flush()?;

        log::info!("PUT object: name={name}, uuid={uuid:x?}, size={data_len}, blocks={block_count}");

        Ok(uuid)
    }

    /// 存储对象，若名称已存在则自动删除旧对象后重写（force overwrite）。
    ///
    /// 流程：检查重名 → 删除旧对象 → 重新分配 → 写入。
    /// 注意：此操作不是原子的——如果中途失败，旧对象已不可恢复。
    pub fn put_overwrite(
        &mut self,
        name: &str,
        data: &[u8],
        content_type: &str,
        tags: &str,
    ) -> MiniosResult<ObjectId> {
        // 若名称已存在，先删除旧对象
        if let Some(slot) = self.find_by_name(name) {
            let old_uuid = self.metadata_cache[slot].uuid;
            log::info!("PUT_OVERWRITE: deleting existing object '{}' (uuid={:x?})", name, old_uuid);
            self.delete(&old_uuid)?;
        }

        // 重新执行普通 put 流程
        self.put(name, data, content_type, tags)
    }

    /// 通过 UUID 仅获取对象元数据摘要（不读取数据块）。返回 `None` 表示未找到。
    pub fn get_summary_by_id(&self, uuid: &ObjectId) -> Option<ObjectSummary> {
        self.find_by_uuid(uuid).map(|slot| self.build_summary(slot))
    }

    /// 通过名称仅获取对象元数据摘要（不读取数据块）。返回 `None` 表示未找到。
    pub fn get_summary_by_name(&self, name: &str) -> Option<ObjectSummary> {
        self.find_by_name(name).map(|slot| self.build_summary(slot))
    }

    /// 通过 UUID 获取对象数据。返回 `None` 表示未找到。
    pub fn get_by_id(&mut self, uuid: &ObjectId) -> MiniosResult<Option<ObjectData>> {
        let slot = match self.find_by_uuid(uuid) {
            Some(s) => s,
            None => return Ok(None),
        };
        // 克隆所需字段以避免借用冲突
        let (block_ptr_head, size) = {
            let entry = &self.metadata_cache[slot];
            (entry.block_ptr_head, entry.size)
        };
        let summary = self.build_summary(slot);
        let data = self.read_block_chain(block_ptr_head, size)?;
        Ok(Some(ObjectData { summary, data }))
    }

    /// 通过名称获取对象数据。返回 `None` 表示未找到。
    pub fn get_by_name(&mut self, name: &str) -> MiniosResult<Option<ObjectData>> {
        let slot = match self.find_by_name(name) {
            Some(s) => s,
            None => return Ok(None),
        };
        let (block_ptr_head, size) = {
            let entry = &self.metadata_cache[slot];
            (entry.block_ptr_head, entry.size)
        };
        let summary = self.build_summary(slot);
        let data = self.read_block_chain(block_ptr_head, size)?;
        Ok(Some(ObjectData { summary, data }))
    }

    /// 通过 UUID 删除对象。返回 `Ok(true)` 表示已删除，`Ok(false)` 表示未找到。
    pub fn delete(&mut self, uuid: &ObjectId) -> MiniosResult<bool> {
        let slot = match self.find_by_uuid(uuid) {
            Some(s) => s,
            None => return Ok(false),
        };

        // 遍历块链并释放所有块
        let block_ptr_head = self.metadata_cache[slot].block_ptr_head;
        let mut block_idx = block_ptr_head;
        let mut freed_blocks = Vec::new();
        while block_idx != consts::BLOCK_CHAIN_END {
            freed_blocks.push(block_idx);
            let (_payload, next) = self.read_data_block(block_idx)?;
            block_idx = next;
        }
        self.bitmap.free_blocks(&freed_blocks);

        // 标记元数据槽位为 tombstone 并写回
        self.metadata_cache[slot].flags = flags::TOMBSTONE;
        self.metadata_cache[slot].update_checksum();
        let entry_clone = self.metadata_cache[slot].clone();
        self.write_metadata_entry(slot, &entry_clone)?;

        // 更新超级块并持久化（位图 -> 超级块 -> fsync）
        self.superblock.total_objects -= 1;
        self.superblock.data_area_free_blocks = self.bitmap.free_count();
        self.superblock.touch();
        self.flush_bitmap()?;
        self.flush_superblock()?;
        self.file.flush()?;

        log::info!("DELETE object: uuid={uuid:x?}, freed_blocks={}", freed_blocks.len());

        Ok(true)
    }

    /// 列出所有活跃对象的基本信息。
    pub fn list(&self) -> Vec<ObjectSummary> {
        self.metadata_cache
            .iter()
            .filter(|e| e.is_active())
            .map(|e| ObjectSummary {
                uuid: e.uuid,
                name: e.name_str().to_string(),
                size: e.size,
                content_type: e.content_type_str().to_string(),
                created_at: e.created_at,
                tags: e.tags_str().to_string(),
                block_count: e.block_count,
            })
            .collect()
    }

    /// 获取存储引擎统计信息。
    pub fn stats(&self) -> StoreStats {
        StoreStats {
            total_objects: self.superblock.total_objects,
            total_blocks: self.superblock.data_area_total_blocks,
            free_blocks: self.bitmap.free_count(),
            used_blocks: self.superblock.data_area_total_blocks - self.bitmap.free_count(),
            file_size: self.superblock.total_file_size(),
            created_at: self.superblock.created_at,
            last_modified: self.superblock.last_modified,
        }
    }

    /// 将超级块和位图持久化到磁盘。
    pub fn flush(&mut self) -> MiniosResult<()> {
        self.flush_superblock()?;
        self.flush_bitmap()?;
        self.file.flush()?;
        Ok(())
    }

    // ─── 内部辅助方法 ───

    /// 按 UUID 查找元数据槽位索引。
    fn find_by_uuid(&self, uuid: &ObjectId) -> Option<usize> {
        self.metadata_cache
            .iter()
            .position(|e| e.is_active() && &e.uuid == uuid)
    }

    /// 按名称查找元数据槽位索引。
    fn find_by_name(&self, name: &str) -> Option<usize> {
        self.metadata_cache
            .iter()
            .position(|e| e.is_active() && e.name_str() == name)
    }

    /// 查找第一个空闲或 tombstone 元数据槽位。
    fn find_free_slot(&self) -> Option<usize> {
        self.metadata_cache
            .iter()
            .position(|e| e.is_free() || e.flags == flags::TOMBSTONE)
    }

    /// 构建指定槽位的 ObjectSummary（不读取数据块）。
    fn build_summary(&self, slot: usize) -> ObjectSummary {
        let entry = &self.metadata_cache[slot];
        ObjectSummary {
            uuid: entry.uuid,
            name: entry.name_str().to_string(),
            size: entry.size,
            content_type: entry.content_type_str().to_string(),
            created_at: entry.created_at,
            tags: entry.tags_str().to_string(),
            block_count: entry.block_count,
        }
    }

    /// 沿着块链读取并拼接数据。
    fn read_block_chain(&mut self, head: u64, size: u64) -> MiniosResult<Vec<u8>> {
        let mut data = Vec::with_capacity(size as usize);

        let mut block_idx = head;
        while block_idx != consts::BLOCK_CHAIN_END {
            let (payload, next) = self.read_data_block(block_idx)?;
            data.extend_from_slice(&payload);
            block_idx = next;
        }

        // 截断到实际大小（最后一块可能不满）
        data.truncate(size as usize);
        Ok(data)
    }

    /// 从元数据条目读取完整对象数据（已弃用，保留以备用）。
    #[allow(dead_code)]
    fn read_object_from_entry(&mut self, entry: &MetadataEntry) -> MiniosResult<ObjectData> {
        let data = self.read_block_chain(entry.block_ptr_head, entry.size)?;
        Ok(ObjectData {
            summary: ObjectSummary {
                uuid: entry.uuid,
                name: entry.name_str().to_string(),
                size: entry.size,
                content_type: entry.content_type_str().to_string(),
                created_at: entry.created_at,
                tags: entry.tags_str().to_string(),
                block_count: entry.block_count,
            },
            data,
        })
    }

    /// 计算数据块在文件中的偏移量。
    fn block_offset(&self, block_idx: u64) -> u64 {
        self.superblock.data_area_offset + block_idx * consts::BLOCK_SIZE
    }

    /// 写入一个数据块（payload + next 指针）。
    fn write_data_block(
        &mut self,
        block_idx: u64,
        payload: &[u8],
        next: u64,
    ) -> MiniosResult<()> {
        assert!(
            payload.len() <= consts::BLOCK_PAYLOAD,
            "payload too large: {} > {}",
            payload.len(),
            consts::BLOCK_PAYLOAD
        );

        let offset = self.block_offset(block_idx);
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(payload)?;

        // 剩余空间填充零
        let padding = consts::BLOCK_PAYLOAD - payload.len();
        if padding > 0 {
            write_zeros(&mut self.file, padding as u64)?;
        }

        // 写入 next 指针
        self.file.write_all(&next.to_le_bytes())?;

        Ok(())
    }

    /// 读取一个数据块，返回 (payload 字节, next 指针)。
    fn read_data_block(&mut self, block_idx: u64) -> MiniosResult<(Vec<u8>, u64)> {
        let offset = self.block_offset(block_idx);
        self.file.seek(SeekFrom::Start(offset))?;

        let mut payload = vec![0u8; consts::BLOCK_PAYLOAD];
        self.file.read_exact(&mut payload)?;

        let mut next_buf = [0u8; 8];
        self.file.read_exact(&mut next_buf)?;
        let next = u64::from_le_bytes(next_buf);

        Ok((payload, next))
    }

    /// 将元数据条目写回文件槽位。
    fn write_metadata_entry(&mut self, slot: usize, entry: &MetadataEntry) -> MiniosResult<()> {
        let offset = self.superblock.metadata_area_offset + slot as u64 * consts::METADATA_ENTRY_SIZE;
        self.file.seek(SeekFrom::Start(offset))?;
        let bytes = entry.to_bytes();
        self.file.write_all(&bytes)?;
        Ok(())
    }

    /// 持久化超级块。
    fn flush_superblock(&mut self) -> MiniosResult<()> {
        self.superblock.write_to(&mut self.file)
    }

    /// 持久化位图。
    fn flush_bitmap(&mut self) -> MiniosResult<()> {
        let bytes = self.bitmap.to_bytes();
        self.file
            .seek(SeekFrom::Start(self.superblock.free_bitmap_offset))?;
        self.file.write_all(&bytes)?;
        Ok(())
    }
}

// ─── 辅助函数 ───

fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// 向文件写入指定数量的零字节。
fn write_zeros(file: &mut File, count: u64) -> std::io::Result<()> {
    const BUF_SIZE: usize = 4096;
    let zeros = [0u8; BUF_SIZE];
    let mut remaining = count;
    while remaining > 0 {
        let chunk = (remaining as usize).min(BUF_SIZE) as u64;
        file.write_all(&zeros[..chunk as usize])?;
        remaining -= chunk;
    }
    Ok(())
}

fn align_up(value: u64, alignment: u64) -> u64 {
    ((value + alignment - 1) / alignment) * alignment
}

// ─── 单元测试 ───

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn temp_store_path(name: &str) -> String {
        let dir = env::temp_dir();
        dir.join(format!("minios_test_engine_{name}.odb"))
            .to_string_lossy()
            .to_string()
    }

    fn create_test_store(path: &str) -> ObjectStore {
        // 删除已有文件
        let _ = std::fs::remove_file(path);
        ObjectStore::create(path, 128, 256).unwrap()
    }

    #[test]
    fn test_create_and_open() {
        let path = temp_store_path("create_and_open");
        let _ = std::fs::remove_file(&path);

        let store = ObjectStore::create(&path, 64, 512).unwrap();
        assert_eq!(store.stats().total_objects, 0);
        assert_eq!(store.stats().total_blocks, 512);
        assert_eq!(store.stats().free_blocks, 512);
        drop(store);

        // 重新打开
        let store = ObjectStore::open(&path).unwrap();
        assert_eq!(store.stats().total_objects, 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_put_and_get_by_id() {
        let path = temp_store_path("put_get_id");
        let mut store = create_test_store(&path);

        let data = b"Hello, minios! This is test data for object storage.";
        let uuid = store.put("test.txt", data, "text/plain", r#"{"env":"test"}"#).unwrap();

        let obj = store.get_by_id(&uuid).unwrap().expect("object should exist");
        assert_eq!(obj.data, data);
        assert_eq!(obj.summary.name, "test.txt");
        assert_eq!(obj.summary.size, data.len() as u64);
        assert_eq!(obj.summary.content_type, "text/plain");
        assert_eq!(obj.summary.tags, r#"{"env":"test"}"#);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_put_and_get_by_name() {
        let path = temp_store_path("put_get_name");
        let mut store = create_test_store(&path);

        let data = b"name-based lookup";
        store.put("myfile.bin", data, "application/octet-stream", "").unwrap();

        let obj = store.get_by_name("myfile.bin").unwrap().expect("object should exist");
        assert_eq!(obj.data, data);

        // 不存在的名称
        assert!(store.get_by_name("no_such_file").unwrap().is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_put_empty_object() {
        let path = temp_store_path("empty");
        let mut store = create_test_store(&path);

        let uuid = store.put("empty.txt", b"", "text/plain", "").unwrap();
        let obj = store.get_by_id(&uuid).unwrap().unwrap();
        assert!(obj.data.is_empty());
        assert_eq!(obj.summary.size, 0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_put_large_object_spans_blocks() {
        let path = temp_store_path("large");
        let mut store = create_test_store(&path);

        // 创建一个跨越 3 个块的对象（4088 * 2 + 1000 = 9176）
        let data = vec![0xABu8; consts::BLOCK_PAYLOAD * 2 + 1000];
        let uuid = store.put("large.bin", &data, "application/octet-stream", "").unwrap();

        let obj = store.get_by_id(&uuid).unwrap().unwrap();
        assert_eq!(obj.data.len(), data.len());
        assert_eq!(obj.data, data);
        assert_eq!(obj.summary.block_count, 3);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_delete() {
        let path = temp_store_path("delete");
        let mut store = create_test_store(&path);

        let uuid1 = store.put("keep.txt", b"keep me", "text/plain", "").unwrap();
        let uuid2 = store.put("delete.txt", b"delete me", "text/plain", "").unwrap();

        assert_eq!(store.list().len(), 2);

        // 删除 uuid2
        assert!(store.delete(&uuid2).unwrap());

        // list 应只含 uuid1
        let list = store.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].uuid, uuid1);

        // get uuid2 应返回 None
        assert!(store.get_by_id(&uuid2).unwrap().is_none());

        // 不影响 uuid1
        let obj = store.get_by_id(&uuid1).unwrap().unwrap();
        assert_eq!(obj.data, b"keep me");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_delete_nonexistent() {
        let path = temp_store_path("delete_nonexistent");
        let mut store = create_test_store(&path);

        let fake_uuid = [0xFFu8; 16];
        assert!(!store.delete(&fake_uuid).unwrap());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_list_empty() {
        let path = temp_store_path("list_empty");
        let store = create_test_store(&path);
        assert!(store.list().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_list_multiple() {
        let path = temp_store_path("list_multi");
        let mut store = create_test_store(&path);

        for i in 0..10 {
            let data = format!("object {i}").into_bytes();
            store.put(&format!("obj_{i}"), &data, "text/plain", "").unwrap();
        }

        let list = store.list();
        assert_eq!(list.len(), 10);
        // 所有名称应不同
        let names: std::collections::HashSet<String> =
            list.iter().map(|o| o.name.clone()).collect();
        assert_eq!(names.len(), 10);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_flush_roundtrip() {
        let path = temp_store_path("flush");
        let mut store = create_test_store(&path);

        store.put("a.txt", b"aaa", "text/plain", "").unwrap();
        store.put("b.txt", b"bbb", "text/plain", "").unwrap();
        store.flush().unwrap();

        drop(store);

        // 重新打开验证
        let mut store2 = ObjectStore::open(&path).unwrap();
        assert_eq!(store2.list().len(), 2);
        assert!(store2.get_by_name("a.txt").unwrap().is_some());
        assert!(store2.get_by_name("b.txt").unwrap().is_some());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_open_rejects_bad_metadata_checksum() {
        let path = temp_store_path("bad_checksum");
        let mut store = create_test_store(&path);
        store.put("bad.txt", b"checksum", "text/plain", "").unwrap();
        store.flush().unwrap();
        drop(store);

        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(consts::BLOCK_SIZE + 80)).unwrap();
        file.write_all(&999u64.to_le_bytes()).unwrap();
        drop(file);

        let result = ObjectStore::open(&path);
        assert!(result.is_err());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_stats() {
        let path = temp_store_path("stats");
        let mut store = create_test_store(&path);

        let initial = store.stats();
        assert_eq!(initial.total_objects, 0);
        assert_eq!(initial.free_blocks, initial.total_blocks);

        store.put("stats_test", &[0u8; 5000], "app/data", "").unwrap();
        let after = store.stats();
        assert_eq!(after.total_objects, 1);
        assert_eq!(after.used_blocks, 2); // 5000 bytes → 2 blocks
        assert_eq!(after.free_blocks + after.used_blocks, after.total_blocks);

        let _ = std::fs::remove_file(&path);
    }

    // ─── 新增测试：元数据摘要获取 ───

    #[test]
    fn test_get_summary_by_id() {
        let path = temp_store_path("summary_by_id");
        let mut store = create_test_store(&path);

        let tags = r#"{"author":"test","version":1}"#;
        let uuid = store.put("doc.txt", b"hello world", "text/plain", tags).unwrap();

        let summary = store.get_summary_by_id(&uuid).expect("should find summary");
        assert_eq!(summary.name, "doc.txt");
        assert_eq!(summary.size, 11);
        assert_eq!(summary.content_type, "text/plain");
        assert_eq!(summary.tags, tags);
        assert_eq!(summary.uuid, uuid);

        // 不存在的 UUID 应返回 None
        assert!(store.get_summary_by_id(&[0xFFu8; 16]).is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_get_summary_by_name() {
        let path = temp_store_path("summary_by_name");
        let mut store = create_test_store(&path);

        let tags = r#"{"key":"value"}"#;
        store.put("image.png", b"png_data", "image/png", tags).unwrap();

        let summary = store.get_summary_by_name("image.png").expect("should find summary");
        assert_eq!(summary.content_type, "image/png");
        assert_eq!(summary.tags, tags);
        assert_eq!(summary.size, 8);

        // 不存在的名称应返回 None
        assert!(store.get_summary_by_name("no_such").is_none());

        let _ = std::fs::remove_file(&path);
    }

    // ─── 新增测试：重名检测 ───

    #[test]
    fn test_put_duplicate_name_rejected() {
        let path = temp_store_path("dup_name");
        let mut store = create_test_store(&path);

        store.put("unique.txt", b"first", "text/plain", "").unwrap();

        // 再次 put 同名对象应返回错误
        let result = store.put("unique.txt", b"second", "text/plain", "");
        assert!(result.is_err());

        // 验证仍然是第一个对象
        let obj = store.get_by_name("unique.txt").unwrap().unwrap();
        assert_eq!(obj.data, b"first");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_put_overwrite_replaces_existing() {
        let path = temp_store_path("overwrite");
        let mut store = create_test_store(&path);

        let uuid1 = store.put("data.bin", b"version 1", "app/bin", r#"{"v":1}"#).unwrap();

        // 使用 put_overwrite 覆盖同名对象
        let uuid2 = store.put_overwrite("data.bin", b"version 2", "app/bin", r#"{"v":2}"#).unwrap();

        // 新 UUID 应不同于旧 UUID（因为创建了新对象）
        assert_ne!(uuid1, uuid2);

        // 旧 UUID 应不可查询
        assert!(store.get_by_id(&uuid1).unwrap().is_none());

        // 新对象应可通过名称查询并具有更新后的内容
        let obj = store.get_by_name("data.bin").unwrap().unwrap();
        assert_eq!(obj.data, b"version 2");
        assert_eq!(obj.summary.tags, r#"{"v":2}"#);

        // 列表应只有一个对象
        assert_eq!(store.list().len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_put_overwrite_new_name_creates() {
        let path = temp_store_path("overwrite_new");
        let mut store = create_test_store(&path);

        // 对不存在的名称使用 put_overwrite 等同于普通 put
        let uuid = store.put_overwrite("new.txt", b"data", "text/plain", "").unwrap();
        let obj = store.get_by_id(&uuid).unwrap().unwrap();
        assert_eq!(obj.data, b"data");

        let _ = std::fs::remove_file(&path);
    }
}
