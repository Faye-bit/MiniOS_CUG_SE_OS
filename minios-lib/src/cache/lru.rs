//! LRU（最近最少使用）对象缓存。
//!
//! 在服务端进程中维护常用对象数据的内存副本，
//! 使用 HashMap + VecDeque 实现 O(1) 时间复杂度的 get/put 操作。
//! 支持可配置容量、命中率统计和缓存预热。

use crate::common::types::{CacheStats, ObjectData, ObjectId};
use std::collections::{HashMap, VecDeque};

/// LRU 缓存条目。
struct CacheEntry {
    /// 完整对象数据
    data: Vec<u8>,
    /// 对象大小（用于内存统计）
    size: u64,
    /// 对象名称（用于支持按名称查询缓存）
    name: String,
}

/// LRU 对象缓存。
///
/// `map` 存储所有缓存条目（以 UUID 为键），
/// `order` 维护访问顺序（最近使用的在尾部）。
pub struct LruCache {
    /// 最大条目数
    capacity: usize,
    /// 最大内存占用（字节）
    max_memory: u64,
    /// 当前内存占用（字节）
    current_memory: u64,
    /// UUID → 缓存条目
    map: HashMap<ObjectId, CacheEntry>,
    /// 名称 → UUID 索引（用于支持按名称 O(1) 查询缓存）
    name_index: HashMap<String, ObjectId>,
    /// 访问顺序（最近使用在尾部）
    order: VecDeque<ObjectId>,
    /// 命中次数
    hits: u64,
    /// 未命中次数
    misses: u64,
    /// 淘汰次数
    evictions: u64,
}

impl LruCache {
    /// 创建新的 LRU 缓存。
    ///
    /// - `capacity`: 最大条目数
    /// - `max_memory`: 最大内存占用（字节）
    pub fn new(capacity: usize, max_memory: u64) -> Self {
        Self {
            capacity,
            max_memory,
            current_memory: 0,
            map: HashMap::with_capacity(capacity.min(1024)),
            name_index: HashMap::with_capacity(capacity.min(1024)),
            order: VecDeque::with_capacity(capacity.min(1024)),
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }

    /// 查找对象，更新访问顺序和命中/未命中计数。
    ///
    /// 返回 `None` 表示缓存未命中（调用方应从存储引擎加载）。
    pub fn get(&mut self, id: &ObjectId) -> Option<&[u8]> {
        if self.map.contains_key(id) {
            self.hits += 1;
            // 将访问的元素移到队尾
            self.touch(id);
            Some(&self.map[id].data)
        } else {
            self.misses += 1;
            None
        }
    }

    /// 按名称查找对象，更新访问顺序和命中/未命中计数。
    ///
    /// 先通过名称索引找到 UUID，再通过 `get()` 查找数据，
    /// 确保命中/未命中计数与 UUID 查找保持统一。
    /// 返回 `None` 表示缓存未命中（调用方应从存储引擎加载）。
    pub fn get_by_name(&mut self, name: &str) -> Option<&[u8]> {
        if let Some(&uuid) = self.name_index.get(name) {
            // 通过 UUID 查找，命中/未命中由 get() 统一统计
            self.get(&uuid)
        } else {
            self.misses += 1;
            None
        }
    }

    /// 将对象放入缓存。如果缓存满则淘汰 LRU 条目。
    ///
    /// 如果对象已存在于缓存中（同 UUID 或同名称），先移除旧条目再插入。
    pub fn put(&mut self, id: ObjectId, data: Vec<u8>, name: String, size: u64) {
        // 先检查 size 是否超过容量限制，如果是则不缓存
        if size > self.max_memory {
            return;
        }

        // 如果另一条目使用了相同的名称，先移除它
        if let Some(&old_id) = self.name_index.get(&name) {
            if old_id != id {
                self.remove_entry(&old_id);
            }
        }

        // 如果同 UUID 已存在，先移除旧条目
        if self.map.contains_key(&id) {
            self.remove_entry(&id);
        }

        // 淘汰直到有足够空间
        while self.current_memory + size > self.max_memory
            || self.map.len() >= self.capacity
        {
            if !self.evict_one() {
                // 无法淘汰（缓存为空或所有条目都大于请求的空间）
                return;
            }
        }

        self.current_memory += size;
        self.map.insert(id, CacheEntry { data, size, name: name.clone() });
        self.name_index.insert(name, id);
        self.order.push_back(id);
    }

    /// 从缓存中移除指定对象（在 Delete 操作后调用）。
    pub fn invalidate(&mut self, id: &ObjectId) {
        self.remove_entry(id);
    }

    /// 返回命中率（0.0 ~ 1.0）。无查询时返回 0.0。
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }

    /// 获取缓存统计信息。
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            capacity: self.capacity,
            size: self.map.len(),
            memory_used: self.current_memory,
            memory_max: self.max_memory,
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
            hit_rate: self.hit_rate(),
        }
    }

    /// 缓存预热：从存储引擎中预加载最多 `limit` 个活跃对象。
    ///
    /// `store` 是闭包，接受 UUID 返回 ObjectData。
    pub fn warmup(
        &mut self,
        object_ids: &[ObjectId],
        mut loader: impl FnMut(&ObjectId) -> Option<ObjectData>,
        limit: usize,
    ) -> usize {
        let mut loaded = 0;
        for id in object_ids.iter().take(limit) {
            if let Some(obj) = loader(id) {
                self.put(obj.summary.uuid, obj.data, obj.summary.name, obj.summary.size);
                loaded += 1;
            }
        }
        loaded
    }

    // ─── 内部方法 ───

    /// 将指定 ID 移到访问队列尾部（标记为最近使用）。
    fn touch(&mut self, id: &ObjectId) {
        if let Some(pos) = self.order.iter().position(|x| x == id) {
            self.order.remove(pos);
            self.order.push_back(*id);
        }
    }

    /// 移除指定条目。
    fn remove_entry(&mut self, id: &ObjectId) {
        if let Some(entry) = self.map.remove(id) {
            self.current_memory = self.current_memory.saturating_sub(entry.size);
            self.name_index.remove(&entry.name);
            self.order.retain(|x| x != id);
        }
    }

    /// 淘汰队首（最久未使用）条目。
    fn evict_one(&mut self) -> bool {
        if let Some(id) = self.order.pop_front() {
            if self.map.contains_key(&id) {
                self.remove_entry(&id);
                self.evictions += 1;
                return true;
            }
        }
        false
    }
}

// ─── 单元测试 ───

#[cfg(test)]
mod tests {
    use super::*;

    fn make_obj(id: u8, data_size: usize) -> (ObjectId, Vec<u8>) {
        let mut uuid = [0u8; 16];
        uuid[0] = id;
        let data = vec![id; data_size];
        (uuid, data)
    }

    #[test]
    fn test_put_and_get() {
        let mut cache = LruCache::new(10, 1024 * 1024);
        let (id, data) = make_obj(1, 100);
        cache.put(id, data.clone(), "obj1".into(), 100);
        assert_eq!(cache.get(&id), Some(data.as_slice()));
    }

    #[test]
    fn test_miss_returns_none() {
        let mut cache = LruCache::new(10, 1024 * 1024);
        let id = [0xFFu8; 16];
        assert!(cache.get(&id).is_none());
        assert_eq!(cache.stats().misses, 1);
    }

    #[test]
    fn test_hit_rate() {
        let mut cache = LruCache::new(10, 1024 * 1024);
        let (id, data) = make_obj(1, 50);

        cache.put(id, data, "obj1".into(), 50);
        cache.get(&id).unwrap(); // hit
        cache.get(&id).unwrap(); // hit
        cache.get(&[0xFFu8; 16]); // miss

        let stats = cache.stats();
        assert_eq!(stats.hits, 2);
        assert_eq!(stats.misses, 1);
        assert!((stats.hit_rate - 2.0 / 3.0).abs() < 0.001);
    }

    #[test]
    fn test_get_by_name_hit_and_miss() {
        let mut cache = LruCache::new(10, 1024 * 1024);
        let (id, data) = make_obj(1, 50);
        cache.put(id, data.clone(), "photo.png".into(), 50);

        // 按名称命中
        assert_eq!(cache.get_by_name("photo.png"), Some(data.as_slice()));
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 0);

        // 按名称未命中（不存在该名称）
        assert!(cache.get_by_name("nonexistent.png").is_none());
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 1);

        // 确认 hit_rate 被正确更新
        let stats = cache.stats();
        assert!((stats.hit_rate - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_get_by_name_name_conflict() {
        let mut cache = LruCache::new(10, 1024 * 1024);
        let (id1, d1) = make_obj(1, 50);
        let (id2, d2) = make_obj(2, 50);

        // 同名称不同 UUID，第二次 put 应移除第一次的条目
        cache.put(id1, d1, "same_name".into(), 50);
        cache.put(id2, d2.clone(), "same_name".into(), 50);

        // 按名称查找应返回第二次 put 的数据
        assert_eq!(cache.get_by_name("same_name"), Some(d2.as_slice()));
        // 旧 UUID 应不存在
        assert!(cache.get(&id1).is_none());
        assert_eq!(cache.stats().size, 1);
    }

    #[test]
    fn test_eviction_by_count() {
        let mut cache = LruCache::new(2, 1024 * 1024);
        let (id1, d1) = make_obj(1, 10);
        let (id2, d2) = make_obj(2, 10);
        let (id3, d3) = make_obj(3, 10);

        cache.put(id1, d1, "1".into(), 10);
        cache.put(id2, d2, "2".into(), 10);
        cache.put(id3, d3.clone(), "3".into(), 10); // 应淘汰 id1

        assert!(cache.get(&id1).is_none()); // 已淘汰
        assert_eq!(cache.get(&id3), Some(d3.as_slice()));

        let stats = cache.stats();
        assert_eq!(stats.evictions, 1);
    }

    #[test]
    fn test_eviction_by_memory() {
        let mut cache = LruCache::new(100, 200); // 最多 200 字节
        let (id1, d1) = make_obj(1, 150);
        let (id2, d2) = make_obj(2, 100);

        cache.put(id1, d1, "1".into(), 150);
        cache.put(id2, d2.clone(), "2".into(), 100); // 应淘汰 id1 (150+100>200)

        assert!(cache.get(&id1).is_none());
        assert_eq!(cache.get(&id2), Some(d2.as_slice()));
    }

    #[test]
    fn test_too_large_object_not_cached() {
        let mut cache = LruCache::new(10, 100);
        let (id, data) = make_obj(1, 200); // > max_memory
        cache.put(id, data, "big".into(), 200);
        assert!(cache.get(&id).is_none());
        assert_eq!(cache.stats().size, 0);
    }

    #[test]
    fn test_lru_order() {
        let mut cache = LruCache::new(2, 1024);
        let (id1, d1) = make_obj(1, 10);
        let (id2, d2) = make_obj(2, 10);
        let (id3, d3) = make_obj(3, 10);

        cache.put(id1, d1, "1".into(), 10);
        cache.put(id2, d2, "2".into(), 10);

        // 访问 id1，使其变为最近使用
        cache.get(&id1);

        // 插入 id3，应淘汰 id2（id1 是最近使用的）
        cache.put(id3, d3.clone(), "3".into(), 10);
        assert!(cache.get(&id1).is_some()); // 保留
        assert!(cache.get(&id2).is_none()); // 淘汰
        assert_eq!(cache.get(&id3), Some(d3.as_slice()));
    }

    #[test]
    fn test_invalidate() {
        let mut cache = LruCache::new(10, 1024);
        let (id, data) = make_obj(1, 100);
        cache.put(id, data, "obj".into(), 100);
        assert_eq!(cache.stats().size, 1);

        cache.invalidate(&id);
        assert_eq!(cache.stats().size, 0);
        assert!(cache.get(&id).is_none());
    }

    #[test]
    fn test_warmup() {
        let mut cache = LruCache::new(10, 1024 * 1024);
        let ids: Vec<[u8; 16]> = (0..5).map(|i| {
            let mut uuid = [0u8; 16];
            uuid[0] = i;
            uuid
        }).collect();

        let mut call_count = 0;
        let loaded = cache.warmup(
            &ids,
            |id| {
                call_count += 1;
                Some(ObjectData {
                    summary: crate::common::types::ObjectSummary {
                        uuid: *id,
                        name: format!("obj_{}", id[0]),
                        size: 10,
                        content_type: "text/plain".into(),
                        created_at: 0,
                        tags: String::new(),
                        block_count: 0,
                    },
                    data: vec![id[0]; 10],
                })
            },
            3,
        );

        assert_eq!(loaded, 3);
        assert_eq!(call_count, 3);
        assert_eq!(cache.stats().size, 3);
    }
}
