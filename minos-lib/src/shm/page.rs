//! 共享内存页分配器（PageAllocator）— 基于位图的 First-Fit 页分配。
//!
//! 将共享内存的数据区划分为固定大小的页（4KB），
//! 通过位图管理每页的分配状态，支持连续多页分配和单页释放。


/// 共享内存页分配器。
///
/// 位图存储在共享内存中，由服务端和客户端共享访问。
/// 位值为 1 表示空闲，0 表示已占用。
/// 页号从 0 开始，对应第一个数据页。
pub struct PageAllocator {
    /// 指向共享内存中页分配位图的指针
    bitmap_ptr: *mut u8,
    /// 数据页总数
    total_pages: u32,
    /// 指向共享内存控制头中 free_pages 的指针（用于原子更新）
    free_pages_ptr: *mut u32,
}

// 位图在共享内存中，跨进程访问安全
unsafe impl Send for PageAllocator {}

impl PageAllocator {
    /// 创建页分配器。
    ///
    /// # Safety
    /// `bitmap_ptr` 和 `free_pages_ptr` 必须指向共享内存中的有效地址。
    pub unsafe fn new(
        bitmap_ptr: *mut u8,
        total_pages: u32,
        free_pages_ptr: *mut u32,
    ) -> Self {
        Self {
            bitmap_ptr,
            total_pages,
            free_pages_ptr,
        }
    }

    /// 分配 `count` 个连续空闲页，返回起始页号。
    ///
    /// 使用 First-Fit 算法在位图中查找连续的 `count` 个 1 位。
    /// 如果不存在足够大的连续空闲块，返回 `None`（体现外部碎片问题）。
    pub fn alloc_pages(&self, count: u32) -> Option<u32> {
        if count == 0 || count > self.total_pages {
            return None;
        }

        let total_bits = self.total_pages as usize;
        let mut consecutive = 0u32;
        let mut start = 0u32;

        for bit_idx in 0..total_bits {
            if self.is_free(bit_idx as u32) {
                if consecutive == 0 {
                    start = bit_idx as u32;
                }
                consecutive += 1;
                if consecutive == count {
                    // 找到足够连续的空闲页，标记为占用
                    for i in 0..count {
                        self.set_bit(start + i, false);
                    }
                    unsafe {
                        *self.free_pages_ptr -= count;
                    }
                    return Some(start);
                }
            } else {
                consecutive = 0;
            }
        }

        None
    }

    /// 释放从 `start_page` 开始的 `count` 个连续页。
    pub fn free_pages(&self, start_page: u32, count: u32) {
        for i in 0..count {
            let page = start_page + i;
            if !self.is_free(page) {
                self.set_bit(page, true);
                unsafe {
                    *self.free_pages_ptr += 1;
                }
            }
        }
    }

    /// 检查是否有 `count` 个连续空闲页。
    pub fn has_contiguous(&self, count: u32) -> bool {
        if count == 0 || count > self.total_pages {
            return false;
        }

        let total_bits = self.total_pages as usize;
        let mut consecutive = 0u32;

        for bit_idx in 0..total_bits {
            if self.is_free(bit_idx as u32) {
                consecutive += 1;
                if consecutive == count {
                    return true;
                }
            } else {
                consecutive = 0;
            }
        }
        false
    }

    /// 计算碎片率（0.0 ~ 1.0）。
    ///
    /// 碎片率定义为：`1 - (最大连续空闲块 / 总空闲页数)`。
    /// 值越接近 1.0 表示碎片化越严重。
    pub fn fragmentation_ratio(&self) -> f64 {
        let free_count = self.free_count();
        if free_count == 0 {
            return 0.0;
        }

        let max_contiguous = self.max_contiguous_free() as f64;
        let total_free = free_count as f64;
        1.0 - (max_contiguous / total_free)
    }

    /// 获取最大连续空闲块的大小（页数）。
    fn max_contiguous_free(&self) -> u32 {
        let total_bits = self.total_pages as usize;
        let mut max_consecutive = 0u32;
        let mut current = 0u32;

        for bit_idx in 0..total_bits {
            if self.is_free(bit_idx as u32) {
                current += 1;
                max_consecutive = max_consecutive.max(current);
            } else {
                current = 0;
            }
        }
        max_consecutive
    }

    /// 返回空闲页总数。
    fn free_count(&self) -> u32 {
        unsafe { *self.free_pages_ptr }
    }

    /// 返回页总数。
    pub fn total_count(&self) -> u32 {
        self.total_pages
    }

    // ─── 位图内部操作 ───

    /// 检查指定页是否空闲。
    fn is_free(&self, page_idx: u32) -> bool {
        let byte_idx = (page_idx / 8) as usize;
        let bit_idx = (page_idx % 8) as usize;
        unsafe {
            let byte = *self.bitmap_ptr.add(byte_idx);
            byte & (1 << bit_idx) != 0
        }
    }

    /// 设置指定页的空闲状态。
    fn set_bit(&self, page_idx: u32, free: bool) {
        let byte_idx = (page_idx / 8) as usize;
        let bit_idx = (page_idx % 8) as usize;
        unsafe {
            let byte_ptr = self.bitmap_ptr.add(byte_idx);
            if free {
                *byte_ptr |= 1 << bit_idx;
            } else {
                *byte_ptr &= !(1 << bit_idx);
            }
        }
    }
}

// ─── 单元测试 ───

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{alloc_zeroed, dealloc, Layout};

    struct TestHarness {
        bitmap: *mut u8,
        free_pages: *mut u32,
        layout: Layout,
    }

    impl TestHarness {
        fn new(total_pages: u32) -> Self {
            let bitmap_size = ((total_pages as usize + 7) / 8) as usize;
            // free_pages 对齐到 4 字节边界
            let free_offset = (bitmap_size + 3) / 4 * 4;
            let total_size = free_offset + std::mem::size_of::<u32>();
            let layout = Layout::from_size_align(total_size, 8).unwrap();
            let ptr = unsafe { alloc_zeroed(layout) };

            // 初始化位图为全 1（所有页空闲）
            unsafe {
                std::ptr::write_bytes(ptr, 0xFF, bitmap_size);
            }

            // 初始化 free_pages
            let free_pages_ptr = unsafe { ptr.add(free_offset) as *mut u32 };
            unsafe {
                *free_pages_ptr = total_pages;
            }

            // 对于非整字节的尾部，清除超出 total_pages 的位
            let valid_in_last = total_pages % 8;
            if valid_in_last != 0 {
                let last_byte_idx = bitmap_size - 1;
                let mask = (1u8 << valid_in_last) - 1;
                unsafe {
                    *ptr.add(last_byte_idx) = mask;
                }
            }

            Self {
                bitmap: ptr,
                free_pages: free_pages_ptr,
                layout,
            }
        }

        fn allocator(&self) -> PageAllocator {
            unsafe {
                PageAllocator::new(self.bitmap, *self.free_pages, self.free_pages)
            }
        }
    }

    impl Drop for TestHarness {
        fn drop(&mut self) {
            unsafe {
                dealloc(self.bitmap, self.layout);
            }
        }
    }

    #[test]
    fn test_alloc_single_page() {
        let h = TestHarness::new(64);
        let alloc = h.allocator();
        let page = alloc.alloc_pages(1).unwrap();
        assert_eq!(alloc.free_count(), 63);
        alloc.free_pages(page, 1);
        assert_eq!(alloc.free_count(), 64);
    }

    #[test]
    fn test_alloc_multiple_contiguous() {
        let h = TestHarness::new(64);
        let alloc = h.allocator();
        let start = alloc.alloc_pages(8).unwrap();
        assert_eq!(alloc.free_count(), 56);
        // 分配的 8 页应连续
        for i in 0..8 {
            assert!(!alloc.is_free(start + i));
        }
        alloc.free_pages(start, 8);
        assert_eq!(alloc.free_count(), 64);
    }

    #[test]
    fn test_alloc_all_and_exhaust() {
        let h = TestHarness::new(32);
        let alloc = h.allocator();
        let start = alloc.alloc_pages(32).unwrap();
        assert_eq!(start, 0);
        assert_eq!(alloc.free_count(), 0);
        assert!(alloc.alloc_pages(1).is_none());
    }

    #[test]
    fn test_fragmentation() {
        let h = TestHarness::new(10);
        let alloc = h.allocator();

        // 分配页 0,1,2  和 页 4,5,6,7,8,9（跳过页 3）
        let a = alloc.alloc_pages(3).unwrap(); // pages 0,1,2
        let b = alloc.alloc_pages(6).unwrap(); // pages 3,4,5,6,7,8
        // 释放中间块
        alloc.free_pages(a, 3); // 释放页 0,1,2

        // 现在空闲块：0,1,2 和 9（页 9）
        // 最大连续 = 3
        assert!(!alloc.has_contiguous(4));
        assert!(alloc.has_contiguous(3));
        assert_eq!(alloc.free_count(), 4);

        alloc.free_pages(b, 6);
    }

    #[test]
    fn test_has_contiguous() {
        let h = TestHarness::new(100);
        let alloc = h.allocator();
        assert!(alloc.has_contiguous(100));
        alloc.alloc_pages(50).unwrap();
        assert!(alloc.has_contiguous(50));
        assert!(!alloc.has_contiguous(51));
        assert!(alloc.has_contiguous(1));
    }

    #[test]
    fn test_fragmentation_ratio() {
        let h = TestHarness::new(10);
        let alloc = h.allocator();

        // 初始：无碎片
        assert!(alloc.fragmentation_ratio() < 0.01);

        // 制造碎片：分配页0, 页2, 页4, 页6, 页8（交错分配）
        let p0 = alloc.alloc_pages(1).unwrap(); // 0
        let p1 = alloc.alloc_pages(1).unwrap(); // 1
        let p2 = alloc.alloc_pages(1).unwrap(); // 2
        alloc.free_pages(p1, 1); // 释放页1

        // 空闲页不连续：页1,3,4,5,6,7,8,9（但最大连续从页3开始=7）
        // free_count=8, max_contiguous=7, fragmentation ≈ 0.125
        let ratio = alloc.fragmentation_ratio();
        assert!(ratio > 0.0, "fragmentation ratio should be > 0: {ratio}");

        alloc.free_pages(p0, 1);
        alloc.free_pages(p2, 1);
    }

    #[test]
    fn test_alloc_zero_pages() {
        let h = TestHarness::new(64);
        let alloc = h.allocator();
        assert!(alloc.alloc_pages(0).is_none());
    }

    #[test]
    fn test_alloc_too_many() {
        let h = TestHarness::new(10);
        let alloc = h.allocator();
        assert!(alloc.alloc_pages(11).is_none());
    }
}
