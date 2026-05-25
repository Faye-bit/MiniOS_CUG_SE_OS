//! 共享内存区域（ShmRegion）— POSIX 共享内存生命周期管理。

use crate::common::consts;
use crate::common::error::{MinosError, MinosResult};
use std::ffi::CString;
use std::io;

/// 跨进程共享的控制头，位于共享内存区域 Page 0 的开头。
///
/// **布局说明**：控制头 ~48 字节，不填充到整页。位图、请求/响应槽位、
/// 互斥锁等紧随其后，全部放在 4096 字节的控制页内。数据页从 Page 1 开始。
#[repr(C)]
#[derive(Debug, Clone)]
pub struct ShmControlHeader {
    /// 魔数，固定为 "MOSM"
    pub magic: [u8; 4],
    /// 格式版本
    pub version: u32,
    /// 单个数据页大小（字节），固定 4096
    pub page_size: u32,
    /// 数据页总数（不含控制页）
    pub total_pages: u32,
    /// 当前空闲页数
    pub free_pages: u32,
    /// 页分配位图偏移量（从控制页起始计算）
    pub page_bitmap_offset: u32,
    /// 页分配位图大小（字节）
    pub page_bitmap_size: u32,
    /// 最大并发请求槽位数
    pub max_requests: u32,
    /// 请求槽位偏移量
    pub request_slots_offset: u32,
    /// 响应槽位偏移量
    pub response_slots_offset: u32,
    /// 互斥锁偏移量（8 字节对齐）
    pub mutex_offset: u32,
    /// 单个请求/响应槽位大小（字节）
    pub slot_size: u32,
}

impl ShmControlHeader {
    /// 创建新的控制头。`total_pages` 为数据页数量。
    pub fn new(
        total_pages: u32,
        page_bitmap_offset: u32,
        page_bitmap_size: u32,
        max_requests: u32,
        slot_size: u32,
    ) -> Self {
        let request_slots_offset = page_bitmap_offset + page_bitmap_size;
        let response_slots_offset = request_slots_offset + max_requests * slot_size;
        let mutex_offset = response_slots_offset + max_requests * slot_size;
        // 对齐到 8 字节
        let mutex_offset = (mutex_offset + 7) / 8 * 8;

        Self {
            magic: consts::SHM_MAGIC,
            version: 1,
            page_size: consts::SHM_PAGE_SIZE,
            total_pages,
            free_pages: total_pages,
            page_bitmap_offset,
            page_bitmap_size,
            max_requests,
            request_slots_offset,
            response_slots_offset,
            mutex_offset,
            slot_size,
        }
    }

    /// 验证控制头魔数。
    pub fn validate(&self) -> MinosResult<()> {
        if self.magic != consts::SHM_MAGIC {
            return Err(MinosError::ShmError("bad shm magic".into()));
        }
        if self.page_size != consts::SHM_PAGE_SIZE {
            return Err(MinosError::ShmError(format!(
                "unsupported page size: {}",
                self.page_size
            )));
        }
        Ok(())
    }
}

/// 共享内存区域。
pub struct ShmRegion {
    /// mmap 基地址
    ptr: *mut u8,
    /// 区域总大小（字节）
    size: usize,
    /// shm_open 文件描述符
    shm_fd: i32,
    /// 共享内存名称
    name: String,
}

unsafe impl Send for ShmRegion {}

impl ShmRegion {
    /// 创建新的共享内存区域（服务端调用）。
    pub fn create(
        name: &str,
        num_data_pages: u32,
        max_requests: u32,
        slot_size: u32,
    ) -> MinosResult<Self> {
        let page_size = consts::SHM_PAGE_SIZE as usize;

        // 控制页内容：header + bitmap + slots + mutex
        let header_size = std::mem::size_of::<ShmControlHeader>();
        let bitmap_size = ((num_data_pages as usize + 7) / 8 + 7) / 8 * 8; // 8 字节对齐
        let control_page_used = {
            let mutex_unaligned = header_size + bitmap_size + 2 * max_requests as usize * slot_size as usize;
            (mutex_unaligned + 7) / 8 * 8 + std::mem::size_of::<libc::pthread_mutex_t>()
        };
        assert!(
            control_page_used <= page_size,
            "control page overflow: {control_page_used} > {page_size}"
        );

        // 总页数 = 1 控制页 + N 数据页
        let total_pages = 1 + num_data_pages as usize;
        let region_size = total_pages * page_size;

        let cname = CString::new(name).map_err(|e| {
            MinosError::ShmError(format!("invalid shm name: {e}"))
        })?;

        unsafe { libc::shm_unlink(cname.as_ptr()) };

        let shm_fd = unsafe { libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600) };
        if shm_fd < 0 {
            return Err(MinosError::ShmError(format!(
                "shm_open failed: {}",
                io::Error::last_os_error()
            )));
        }

        let ret = unsafe { libc::ftruncate(shm_fd, region_size as libc::off_t) };
        if ret != 0 {
            unsafe {
                libc::close(shm_fd);
                libc::shm_unlink(cname.as_ptr());
            };
            return Err(MinosError::ShmError(format!(
                "ftruncate failed: {}",
                io::Error::last_os_error()
            )));
        }

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                region_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                shm_fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            unsafe {
                libc::close(shm_fd);
                libc::shm_unlink(cname.as_ptr());
            };
            return Err(MinosError::ShmError(format!(
                "mmap failed: {}",
                io::Error::last_os_error()
            )));
        }

        // 写入控制头
        let bitmap_offset = header_size as u32;
        let header = ShmControlHeader::new(
            num_data_pages,
            bitmap_offset,
            bitmap_size as u32,
            max_requests,
            slot_size,
        );
        unsafe {
            std::ptr::write(ptr as *mut ShmControlHeader, header);
        }

        Ok(Self {
            ptr: ptr as *mut u8,
            size: region_size,
            shm_fd,
            name: name.to_string(),
        })
    }

    /// 打开已存在的共享内存区域（客户端调用）。
    pub fn open(name: &str) -> MinosResult<Self> {
        let cname = CString::new(name).map_err(|e| {
            MinosError::ShmError(format!("invalid shm name: {e}"))
        })?;

        let shm_fd = unsafe { libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0o600) };
        if shm_fd < 0 {
            return Err(MinosError::ShmError(format!(
                "shm_open '{}' failed: {}",
                name,
                io::Error::last_os_error()
            )));
        }

        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::fstat(shm_fd, &mut stat) };
        if ret != 0 {
            unsafe { libc::close(shm_fd) };
            return Err(MinosError::ShmError(format!(
                "fstat failed: {}",
                io::Error::last_os_error()
            )));
        }
        let region_size = stat.st_size as usize;

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                region_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                shm_fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            unsafe { libc::close(shm_fd) };
            return Err(MinosError::ShmError(format!(
                "mmap failed: {}",
                io::Error::last_os_error()
            )));
        }

        let region = Self {
            ptr: ptr as *mut u8,
            size: region_size,
            shm_fd,
            name: name.to_string(),
        };

        region.header().validate()?;
        Ok(region)
    }

    /// 销毁共享内存区域（服务端调用），防止 Drop 重复释放。
    pub fn destroy(mut self) -> MinosResult<()> {
        let ptr = self.ptr;
        let size = self.size;
        let shm_fd = self.shm_fd;
        let name = std::mem::take(&mut self.name);

        self.ptr = std::ptr::null_mut();
        self.shm_fd = -1;
        drop(self);

        unsafe { libc::munmap(ptr as *mut libc::c_void, size) };
        unsafe { libc::close(shm_fd) };

        let cname = CString::new(name.as_str()).unwrap();
        unsafe { libc::shm_unlink(cname.as_ptr()) };

        Ok(())
    }

    /// 获取控制头引用。
    pub fn header(&self) -> &ShmControlHeader {
        unsafe { &*(self.ptr as *const ShmControlHeader) }
    }

    /// 获取数据页区域起始指针（Page 1 起点）。
    pub fn data_area(&self) -> *mut u8 {
        unsafe { self.ptr.add(consts::SHM_PAGE_SIZE as usize) }
    }

    /// 获取指定数据页指针（page_idx 从 0 开始）。
    pub fn page_ptr(&self, page_idx: u32) -> *mut u8 {
        let offset = (page_idx as usize + 1) * consts::SHM_PAGE_SIZE as usize;
        unsafe { self.ptr.add(offset) }
    }

    /// 获取页分配位图指针（位于控制页内）。
    pub fn bitmap_ptr(&self) -> *mut u8 {
        let offset = self.header().page_bitmap_offset as usize;
        unsafe { self.ptr.add(offset) }
    }

    /// 获取互斥锁指针（位于控制页内）。
    pub fn mutex_ptr(&self) -> *mut u8 {
        let offset = self.header().mutex_offset as usize;
        unsafe { self.ptr.add(offset) }
    }

    /// 区域总大小。
    pub fn size(&self) -> usize {
        self.size
    }

    /// 写数据到连续页面。
    pub fn write_to_pages(&self, start_page: u32, data: &[u8]) {
        let page_size = consts::SHM_PAGE_SIZE as usize;
        for (i, chunk) in data.chunks(page_size).enumerate() {
            let dst = self.page_ptr(start_page + i as u32);
            unsafe {
                std::ptr::copy_nonoverlapping(chunk.as_ptr(), dst, chunk.len());
            }
        }
    }

    /// 从连续页面读取数据。
    pub fn read_from_pages(&self, start_page: u32, size: u64) -> Vec<u8> {
        let mut data = vec![0u8; size as usize];
        let page_size = consts::SHM_PAGE_SIZE as usize;
        let mut offset = 0;
        let num_pages = (size as usize + page_size - 1) / page_size;
        for i in 0..num_pages {
            let src = self.page_ptr(start_page + i as u32);
            let remaining = size as usize - offset;
            let chunk = remaining.min(page_size);
            unsafe {
                std::ptr::copy_nonoverlapping(src, data.as_mut_ptr().add(offset), chunk);
            }
            offset += chunk;
        }
        data
    }
}

impl Drop for ShmRegion {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                libc::munmap(self.ptr as *mut libc::c_void, self.size);
            }
        }
        if self.shm_fd >= 0 {
            unsafe {
                libc::close(self.shm_fd);
            }
        }
    }
}

// ─── 单元测试 ───

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn test_name(suffix: &str) -> String {
        format!("/minos_test_region_{suffix}")
    }

    #[test]
    fn test_create_and_destroy() {
        let name = test_name("create");
        let _ = std::fs::remove_file(&name);
        let region = ShmRegion::create(&name, 16, 8, 256).unwrap();
        let header = region.header();
        assert_eq!(header.magic, consts::SHM_MAGIC);
        assert_eq!(header.total_pages, 16);
        assert!(header.validate().is_ok());
        region.destroy().unwrap();
    }

    #[test]
    fn test_write_read_pages() {
        let name = test_name("wr");
        let _ = std::fs::remove_file(&name);
        let region = ShmRegion::create(&name, 8, 4, 256).unwrap();

        let data: Vec<u8> = (0..10000u16).map(|v| (v % 256) as u8).collect();
        region.write_to_pages(0, &data);

        let read_back = region.read_from_pages(0, data.len() as u64);
        assert_eq!(read_back, data);

        region.destroy().unwrap();
    }

    #[test]
    fn test_open_existing() {
        let name = test_name("open");
        let _ = std::fs::remove_file(&name);
        let region = ShmRegion::create(&name, 4, 4, 256).unwrap();
        let data = b"hello shared memory!".to_vec();
        region.write_to_pages(0, &data);

        let region2 = ShmRegion::open(&name).unwrap();
        let read_back = region2.read_from_pages(0, data.len() as u64);
        assert_eq!(read_back, data);

        drop(region2);
        region.destroy().unwrap();
    }
}
