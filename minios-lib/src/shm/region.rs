//! 共享内存区域（ShmRegion）— POSIX 共享内存生命周期管理。

use crate::common::consts;
use crate::common::error::{miniosError, miniosResult};
use crate::shm::sync::ShmMutex;
use std::ffi::CString;
use std::io;

/// 跨进程共享的控制头，位于共享内存区域 Page 0 开头。
///
/// 控制页（4096 bytes）布局：
/// - ShmControlHeader（~32 bytes）
/// - Page Bitmap（ceil(total_pages/8) bytes，8 字节对齐）
/// - pthread_mutex_t（跨进程页分配锁）
/// - 剩余空间保留
/// - 数据页从 Page 1 开始
#[repr(C)]
#[derive(Debug, Clone)]
pub struct ShmControlHeader {
    /// 魔数 "MOSM"
    pub magic: [u8; 4],
    /// 格式版本
    pub version: u32,
    /// 单页大小 (4096)
    pub page_size: u32,
    /// 数据页总数（不含控制页）
    pub total_pages: u32,
    /// 当前空闲数据页数
    pub free_pages: u32,
    /// 页分配位图偏移量（从控制页起始计算）
    pub page_bitmap_offset: u32,
    /// 页分配位图大小（字节，8 字节对齐）
    pub page_bitmap_size: u32,
}

impl ShmControlHeader {
    pub fn new(total_pages: u32, page_bitmap_offset: u32, page_bitmap_size: u32) -> Self {
        Self {
            magic: consts::SHM_MAGIC,
            version: 1,
            page_size: consts::SHM_PAGE_SIZE,
            total_pages,
            free_pages: total_pages,
            page_bitmap_offset,
            page_bitmap_size,
        }
    }

    pub fn validate(&self) -> miniosResult<()> {
        if self.magic != consts::SHM_MAGIC {
            return Err(miniosError::ShmError("bad shm magic".into()));
        }
        if self.page_size != consts::SHM_PAGE_SIZE {
            return Err(miniosError::ShmError(format!(
                "unsupported page size: {}",
                self.page_size
            )));
        }
        Ok(())
    }
}

/// 计算互斥锁在控制页中的偏移量（位图之后，按 pthread_mutex_t 对齐）。
fn page_mutex_offset(bitmap_offset: u32, bitmap_size: u32) -> usize {
    let raw = bitmap_offset as usize + bitmap_size as usize;
    let align = std::mem::align_of::<libc::pthread_mutex_t>();
    (raw + align - 1) / align * align
}

/// 共享内存区域。
pub struct ShmRegion {
    ptr: *mut u8,
    size: usize,
    shm_fd: i32,
    name: String,
    /// 保护页分配位图的跨进程互斥锁（位于控制页中）
    page_mutex: ShmMutex,
}

unsafe impl Send for ShmRegion {}

impl ShmRegion {
    /// 创建共享内存区域（服务端调用）。
    pub fn create(name: &str, num_data_pages: u32) -> miniosResult<Self> {
        let page_size = consts::SHM_PAGE_SIZE as usize;

        let header_size = std::mem::size_of::<ShmControlHeader>();
        let bitmap_size = ((num_data_pages as usize + 7) / 8 + 7) / 8 * 8;
        let bitmap_offset = header_size as u32;
        let bitmap_size_u32 = bitmap_size as u32;
        let mutex_off = page_mutex_offset(bitmap_offset, bitmap_size_u32);
        let mutex_size = std::mem::size_of::<libc::pthread_mutex_t>();
        assert!(
            mutex_off + mutex_size <= page_size,
            "control page overflow: header={header_size} + bitmap={bitmap_size} + mutex={mutex_size} = {} > {page_size}",
            mutex_off + mutex_size
        );

        let total_pages = 1 + num_data_pages as usize; // 1 控制页 + N 数据页
        let region_size = total_pages * page_size;

        let cname = CString::new(name).map_err(|e| {
            miniosError::ShmError(format!("invalid shm name: {e}"))
        })?;

        unsafe { libc::shm_unlink(cname.as_ptr()) };

        let shm_fd = unsafe { libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600) };
        if shm_fd < 0 {
            return Err(miniosError::ShmError(format!(
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
            return Err(miniosError::ShmError(format!(
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
            return Err(miniosError::ShmError(format!(
                "mmap failed: {}",
                io::Error::last_os_error()
            )));
        }

        let region_ptr = ptr as *mut u8;

        // 写入控制头（位图紧随 header 之后）
        let header = ShmControlHeader::new(num_data_pages, bitmap_offset, bitmap_size_u32);
        unsafe {
            std::ptr::write(ptr as *mut ShmControlHeader, header);
        }

        // 在位图之后初始化跨进程互斥锁
        let page_mutex = unsafe { ShmMutex::init_at(region_ptr.add(mutex_off))? };

        Ok(Self {
            ptr: region_ptr,
            size: region_size,
            shm_fd,
            name: name.to_string(),
            page_mutex,
        })
    }

    /// 打开已存在的共享内存区域（客户端调用）。
    pub fn open(name: &str) -> miniosResult<Self> {
        let cname = CString::new(name).map_err(|e| {
            miniosError::ShmError(format!("invalid shm name: {e}"))
        })?;

        let shm_fd = unsafe { libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0o600) };
        if shm_fd < 0 {
            return Err(miniosError::ShmError(format!(
                "shm_open '{}' failed: {}",
                name,
                io::Error::last_os_error()
            )));
        }

        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::fstat(shm_fd, &mut stat) };
        if ret != 0 {
            unsafe { libc::close(shm_fd) };
            return Err(miniosError::ShmError(format!(
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
            return Err(miniosError::ShmError(format!(
                "mmap failed: {}",
                io::Error::last_os_error()
            )));
        }

        let region_ptr = ptr as *mut u8;

        // 在打开互斥锁前先验证控制头
        let header_ref = unsafe { &*(region_ptr as *const ShmControlHeader) };
        header_ref.validate()?;

        // 计算互斥锁偏移并打开（不重新初始化）
        let mutex_off = page_mutex_offset(header_ref.page_bitmap_offset, header_ref.page_bitmap_size);
        let page_mutex = unsafe { ShmMutex::open_at(region_ptr.add(mutex_off)) };

        Ok(Self {
            ptr: region_ptr,
            size: region_size,
            shm_fd,
            name: name.to_string(),
            page_mutex,
        })
    }

    /// 销毁共享内存区域，防止 Drop 重复释放。
    pub fn destroy(mut self) -> miniosResult<()> {
        let ptr = self.ptr;
        let size = self.size;
        let shm_fd = self.shm_fd;
        let name = std::mem::take(&mut self.name);

        // 在 unmap 前销毁互斥锁
        unsafe { self.page_mutex.destroy()?; }

        self.ptr = std::ptr::null_mut();
        self.shm_fd = -1;
        drop(self);

        unsafe { libc::munmap(ptr as *mut libc::c_void, size) };
        unsafe { libc::close(shm_fd) };
        let cname = CString::new(name.as_str()).unwrap();
        unsafe { libc::shm_unlink(cname.as_ptr()) };

        Ok(())
    }

    pub fn header(&self) -> &ShmControlHeader {
        unsafe { &*(self.ptr as *const ShmControlHeader) }
    }

    /// 数据页区域起点（Page 1）。
    pub fn data_area(&self) -> *mut u8 {
        unsafe { self.ptr.add(consts::SHM_PAGE_SIZE as usize) }
    }

    /// 指定数据页指针（page_idx 从 0 开始）。
    pub fn page_ptr(&self, page_idx: u32) -> *mut u8 {
        let offset = (page_idx as usize + 1) * consts::SHM_PAGE_SIZE as usize;
        unsafe { self.ptr.add(offset) }
    }

    /// 位图指针（位于控制页内 header 之后）。
    pub fn bitmap_ptr(&self) -> *mut u8 {
        let offset = self.header().page_bitmap_offset as usize;
        unsafe { self.ptr.add(offset) }
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// 锁定页分配互斥锁（跨进程）。
    pub fn lock_page_mutex(&self) -> miniosResult<()> {
        self.page_mutex.lock()
    }

    /// 解锁页分配互斥锁（跨进程）。
    pub fn unlock_page_mutex(&self) -> miniosResult<()> {
        self.page_mutex.unlock()
    }

    /// 写数据到连续页面。
    pub fn write_to_pages(&self, start_page: u32, data: &[u8]) {
        let page_size = consts::SHM_PAGE_SIZE as usize;
        for (i, chunk) in data.chunks(page_size).enumerate() {
            let dst = self.page_ptr(start_page + i as u32);
            unsafe { std::ptr::copy_nonoverlapping(chunk.as_ptr(), dst, chunk.len()) };
        }
    }

    /// 从连续页面读数据。
    pub fn read_from_pages(&self, start_page: u32, size: u64) -> Vec<u8> {
        let mut data = vec![0u8; size as usize];
        let page_size = consts::SHM_PAGE_SIZE as usize;
        let mut offset = 0;
        let num_pages = (size as usize + page_size - 1) / page_size;
        for i in 0..num_pages {
            let src = self.page_ptr(start_page + i as u32);
            let remaining = size as usize - offset;
            let chunk = remaining.min(page_size);
            unsafe { std::ptr::copy_nonoverlapping(src, data.as_mut_ptr().add(offset), chunk) };
            offset += chunk;
        }
        data
    }
}

impl Drop for ShmRegion {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.size) };
        }
        if self.shm_fd >= 0 {
            unsafe { libc::close(self.shm_fd) };
        }
    }
}

// ─── 单元测试 ───

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn test_name(suffix: &str) -> String {
        format!("/minios_test_region_{suffix}")
    }

    #[test]
    fn test_create_and_destroy() {
        let name = test_name("create");
        let _ = std::fs::remove_file(&name);
        let region = ShmRegion::create(&name, 16).unwrap();
        assert_eq!(region.header().magic, consts::SHM_MAGIC);
        assert_eq!(region.header().total_pages, 16);
        region.destroy().unwrap();
    }

    #[test]
    fn test_write_read_pages() {
        let name = test_name("wr");
        let _ = std::fs::remove_file(&name);
        let region = ShmRegion::create(&name, 8).unwrap();
        let data: Vec<u8> = (0..10000u16).map(|v| (v % 256) as u8).collect();
        region.write_to_pages(0, &data);
        assert_eq!(region.read_from_pages(0, data.len() as u64), data);
        region.destroy().unwrap();
    }

    #[test]
    fn test_open_existing() {
        let name = test_name("open");
        let _ = std::fs::remove_file(&name);
        let region = ShmRegion::create(&name, 4).unwrap();
        region.write_to_pages(0, b"hello shared memory!");
        let region2 = ShmRegion::open(&name).unwrap();
        assert_eq!(
            region2.read_from_pages(0, 21),
            b"hello shared memory!".to_vec()
        );
        drop(region2);
        region.destroy().unwrap();
    }
}
