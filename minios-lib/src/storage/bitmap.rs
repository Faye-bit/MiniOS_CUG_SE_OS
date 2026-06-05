//! 自由块位图（Block Bitmap）— 管理数据块区的分配与回收。
//!
//! 位图中每一位对应一个数据块：1 表示空闲，0 表示占用。
//! 内部使用 `Vec<u64>` 存储，支持以 64 位字为单位的快速扫描。
//!
//! 数据块不需要连续分配——它们通过块内的 next 指针形成链表。

use crate::common::error::{MiniosError, MiniosResult};

/// 自由块位图。
///
/// 位索引 i 对应数据块区中的第 i 个块。
/// 位值语义：1 = 空闲，0 = 已占用。
#[derive(Debug, Clone)]
pub struct BlockBitmap {
    /// 数据块总数
    total_blocks: u64,
    /// 当前空闲块数
    free_blocks: u64,
    /// 位图数据，以 u64 数组存储。每个 u64 管理 64 个块的分配状态。
    /// bits[0] 管理块 0-63，bits[1] 管理块 64-127，依此类推。
    bits: Vec<u64>,
}

impl BlockBitmap {
    /// 创建一个新的位图，所有块初始标记为空闲。
    pub fn new(total_blocks: u64) -> Self {
        let words = (total_blocks as usize + 63) / 64; // 计算需要多少个 u64 来存储 total_blocks 位（向上取整）
        let mut bits = vec![u64::MAX; words]; // 初始全 1 表示所有块空闲

        // 最后一个字中超出 total_blocks 的位应标记为"已占用"（不可分配）
        let valid_in_last = total_blocks % 64; // 最后一个字中有效的位数
        if valid_in_last != 0 {
            let mask = (1u64 << valid_in_last) - 1; // 生成一个掩码，低 valid_in_last 位为 1，其余为 0
            let last = bits.last_mut().unwrap(); // 获取最后一个 u64 的可变引用
            *last = mask;
        }

        Self {
            total_blocks,
            free_blocks: total_blocks,
            bits,
        }
    }

    /// 从字节数组反序列化位图。
    ///
    /// `data` 为原始字节（来自 store.odb 的位图区），
    /// `total_blocks` 从超级块中读取。
    pub fn from_bytes(data: &[u8], total_blocks: u64) -> Self {
        let words = (total_blocks as usize + 63) / 64; // 计算需要多少个 u64 来存储 total_blocks 位（向上取整）
        let mut bits = vec![0u64; words]; // 初始化为全 0（所有块占用），后续根据 data 设置空闲位
        // 将字节数据转换为 u64 位图
        for (i, chunk) in data.chunks(8).enumerate() {
            if i >= words { // 安全检查：如果 data 中的位图数据超过了 total_blocks 所需的字数，则忽略多余部分
                break;
            }
            let mut word_bytes = [0u8; 8]; // 用于存储当前 u64 的字节数据
            let len = chunk.len().min(8); // 最后一个 chunk 可能不足 8 字节，需处理
            word_bytes[..len].copy_from_slice(chunk); // 将 chunk 中的字节复制到 word_bytes 中
            bits[i] = u64::from_le_bytes(word_bytes); // 将字节数组转换为 u64，假设数据低位在前
        }
        // 计算 free_blocks 的数量（位图中值为 1 的位数）
        let free_blocks = bits.iter().map(|w| w.count_ones() as u64).sum();

        Self {
            total_blocks,
            free_blocks,
            bits,
        }
    }

    /// 将位图序列化为字节数组。
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.bits.len() * 8);
        for word in &self.bits {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    /// 分配一个空闲块，返回块索引。
    ///
    /// 使用 u64 字级别的 `trailing_zeros()` 指令进行快速扫描。
    pub fn allocate_one(&mut self) -> Option<u64> {
        for (word_idx, word) in self.bits.iter_mut().enumerate() { // 遍历位图中的每个 u64 字
            if *word != 0 { // 如果当前字不全为 0，说明至少有一个空闲块
                let bit_idx = word.trailing_zeros() as usize; // 找到最低位的 1 的索引，即第一个空闲块的位置
                *word &= !(1u64 << bit_idx); // 清除该位（标记为占用）
                self.free_blocks -= 1;
                let block_idx = (word_idx * 64 + bit_idx) as u64; // 计算块索引
                // 防止分配超出 total_blocks 的位
                if block_idx < self.total_blocks {
                    return Some(block_idx);
                } else {
                    // 回滚
                    *word |= 1u64 << bit_idx;
                    self.free_blocks += 1;
                    return None;
                }
            }
        }
        None
    }

    /// 分配 `count` 个空闲块，返回块索引列表。
    ///
    /// 块不需要连续，因为它们通过链表指针串联。
    pub fn allocate_multi(&mut self, count: u32) -> MiniosResult<Vec<u64>> {
        if count as u64 > self.free_blocks {
            return Err(MiniosError::NoSpace(format!(
                "need {} blocks but only {} free",
                count, self.free_blocks
            )));
        }

        let mut allocated = Vec::with_capacity(count as usize); // 用于存储分配的块索引
        for _ in 0..count { // 循环分配 count 个块
            match self.allocate_one() { // 复用 allocate_one 的逻辑来分配单个块
                Some(idx) => allocated.push(idx),
                None => {
                    // 回滚：释放本次循环中已分配的块
                    for idx in &allocated {
                        self.free_block(*idx);
                    }
                    return Err(MiniosError::NoSpace(
                        "allocation failed mid-way (unexpected)".into(),
                    ));
                }
            }
        }
        Ok(allocated)
    }

    /// 释放指定块，标记为空闲。
    pub fn free_block(&mut self, block_idx: u64) {
        assert!( // 安全检查：确保 block_idx 在有效范围内
            block_idx < self.total_blocks,
            "block index {} out of range (total {})",
            block_idx,
            self.total_blocks
        );
        let word_idx = (block_idx / 64) as usize;
        let bit_idx = (block_idx % 64) as usize;
        let mask = 1u64 << bit_idx; // 计算对应位的掩码

        // 仅在块确实被占用时才更新计数（幂等释放）
        if self.bits[word_idx] & mask == 0 {
            self.bits[word_idx] |= mask; // 设置该位为 1（标记为空闲）
            self.free_blocks += 1;
        }
    }

    /// 释放多个块。
    pub fn free_blocks(&mut self, indices: &[u64]) {
        for &idx in indices {
            self.free_block(idx);
        }
    }

    /// 检查指定块是否空闲。
    pub fn is_free(&self, block_idx: u64) -> bool {
        let word_idx = (block_idx / 64) as usize;
        let bit_idx = (block_idx % 64) as usize;
        self.bits[word_idx] & (1u64 << bit_idx) != 0
    }

    /// 返回空闲块数。
    pub fn free_count(&self) -> u64 {
        self.free_blocks
    }

    /// 返回块总数。
    pub fn total_count(&self) -> u64 {
        self.total_blocks
    }
}

// ─── 单元测试 ───

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_bitmap_all_free() {
        let bm = BlockBitmap::new(128);
        assert_eq!(bm.free_count(), 128);
        assert_eq!(bm.total_count(), 128);
        for i in 0..128 {
            assert!(bm.is_free(i));
        }
    }

    #[test]
    fn test_new_bitmap_not_power_of_two() {
        let bm = BlockBitmap::new(100);
        assert_eq!(bm.free_count(), 100);
        // 块 100-127 应不可分配
        for i in 0..100 {
            assert!(bm.is_free(i));
        }
    }

    #[test]
    fn test_allocate_one() {
        let mut bm = BlockBitmap::new(64);
        let idx = bm.allocate_one().unwrap();
        assert!(!bm.is_free(idx));
        assert_eq!(bm.free_count(), 63);
    }

    #[test]
    fn test_allocate_all_and_exhaust() {
        let mut bm = BlockBitmap::new(64);
        let mut allocated = Vec::new();
        while let Some(idx) = bm.allocate_one() {
            allocated.push(idx);
        }
        assert_eq!(allocated.len(), 64);
        assert_eq!(bm.free_count(), 0);
        assert!(bm.allocate_one().is_none());
    }

    #[test]
    fn test_free_block() {
        let mut bm = BlockBitmap::new(64);
        let idx = bm.allocate_one().unwrap();
        assert_eq!(bm.free_count(), 63);
        bm.free_block(idx);
        assert_eq!(bm.free_count(), 64);
        assert!(bm.is_free(idx));
    }

    #[test]
    fn test_allocate_multi() {
        let mut bm = BlockBitmap::new(64);
        let blocks = bm.allocate_multi(10).unwrap();
        assert_eq!(blocks.len(), 10);
        assert_eq!(bm.free_count(), 54);
        // 检查分配的块都不空闲
        for &idx in &blocks {
            assert!(!bm.is_free(idx));
        }
    }

    #[test]
    fn test_allocate_multi_not_enough_space() {
        let mut bm = BlockBitmap::new(10);
        let result = bm.allocate_multi(11);
        assert!(result.is_err());
    }

    #[test]
    fn test_free_and_reallocate() {
        let mut bm = BlockBitmap::new(64);
        let blocks = bm.allocate_multi(20).unwrap();
        bm.free_blocks(&blocks);
        assert_eq!(bm.free_count(), 64);
        // 重新分配应成功
        let blocks2 = bm.allocate_multi(64).unwrap();
        assert_eq!(blocks2.len(), 64);
    }

    #[test]
    fn test_serialize_roundtrip() {
        let mut bm = BlockBitmap::new(128);
        bm.allocate_multi(30).unwrap();
        let bytes = bm.to_bytes();
        let restored = BlockBitmap::from_bytes(&bytes, 128);
        assert_eq!(restored.free_count(), bm.free_count());
        assert_eq!(restored.total_count(), bm.total_count());
        for i in 0..128 {
            assert_eq!(restored.is_free(i), bm.is_free(i));
        }
    }

    #[test]
    fn test_double_free_idempotent() {
        let mut bm = BlockBitmap::new(64);
        let idx = bm.allocate_one().unwrap();
        bm.free_block(idx);
        let count = bm.free_count();
        bm.free_block(idx); // 二次释放不应改变计数
        assert_eq!(bm.free_count(), count);
    }
}
