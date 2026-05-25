//! 共享内存区域（ShmRegion）— POSIX 共享内存生命周期管理。
//!
//! 使用 `shm_open` + `mmap` 创建/打开共享内存区域，
//! 将控制头、页分配位图、请求/响应槽位与数据页组织在连续内存中。

use crate::common::consts;
use crate::common::error::{MinosError, MinosResult};
use std::ffi::CString;
use std::io;

/// 跨进程共享的控制头，位于共享内存区域开头（Page 0）。
///
/// 记录区域几何信息、页分配状态、请求/响应队列元数据。
#[repr(C)]
#[derive(Debug, Clone)]
pub struct ShmControlHeader {
    /// 魔数，固定为 "MOSM"
    pub magic: [u8; 4],
    /// 格式版本
    pub version: u32,
    /// 单个数据页大小（字节），固定 4096
    pub page_size: u32,
    /// 数据页总数
    pub total_pages: u32,
    /// 当前空闲页数
    pub free_pages: u32,
    /// 页分配位图偏移量（从 header 起始计算）
    pub page_bitmap_offset: u32,
    /// 页分配位图大小（字节）
    pub page_bitmap_size: u32,
    /// 最大并发请求槽位数
    pub max_requests: u32,
    /// 请求槽位偏移量
    pub request_slots_offset: u32,
    /// 响应槽位偏移量
    pub response_slots_offset: u32,
    /// 互斥锁偏移量
    pub mutex_offset: u32,
    /// 单个请求/响应槽位大小（字节）
    pub slot_size: u32,
    /// 保留字段，填充至控制页边界（4096 - 48 header bytes = 4048）
    pub _reserved: [u8; 4048],
}

// 编译期大小检查
const _: () = assert!(std::mem::size_of::<ShmControlHeader>() == consts::SHM_PAGE_SIZE as usize);

impl ShmControlHeader {
    /// 创建新的控制头。
    pub fn new(
        total_pages: u32,
        page_bitmap_offset: u32,
        page_bitmap_size: u32,
        max_requests: u32,
        slot_size: u32,
    ) -> Self {
        // 计算各区偏移
        let request_slots_offset = page_bitmap_offset + page_bitmap_size;
        let response_slots_offset = request_slots_offset + max_requests * slot_size;
        let mut mutex_offset = response_slots_offset + max_requests * slot_size;

        // 将互斥锁偏移量对齐到 8 字节边界
        mutex_offset = (mutex_offset + 7) / 8 * 8;

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
            _reserved: [0u8; 4048],
        }
    }

    /// 验证控制头。
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
///
/// 管理整个共享内存区域的生命周期：
/// - `create`：服务端创建新的共享内存区域
/// - `open`：客户端打开已存在的共享内存区域
/// - `destroy`：服务端销毁共享内存区域
pub struct ShmRegion {
    /// mmap 映射的基地址
    ptr: *mut u8,
    /// 区域总大小（字节）
    size: usize,
    /// shm_open 返回的文件描述符
    shm_fd: i32,
    /// 共享内存名称
    name: String,
}

// 共享内存指针可跨线程传递
unsafe impl Send for ShmRegion {}

impl ShmRegion {
    /// 创建新的共享内存区域（服务端调用）。
    ///
    /// `name`：共享内存名称，如 "minos_shm"
    /// `num_data_pages`：数据页数量
    /// `max_requests`：最大并发请求数
    /// `slot_size`：每个请求/响应槽位大小（字节）
    pub fn create(
        name: &str,
        num_data_pages: u32,
        max_requests: u32,
        slot_size: u32,
    ) -> MinosResult<Self> {
        let page_size = consts::SHM_PAGE_SIZE as usize;
        let total_pages = num_data_pages + 1; // +1 for control page (page 0)

        // 位图大小 = ceil(total_pages / 8)
        let bitmap_size = (total_pages as usize + 7) / 8;
        // 对齐到 8 字节
        let bitmap_size = ((bitmap_size + 7) / 8) * 8;

        let region_size = total_pages as usize * page_size;

        let cname = CString::new(name).map_err(|e| {
            MinosError::ShmError(format!("invalid shm name: {e}"))
        })?;

        // 先清理可能残留的同名共享内存
        unsafe { libc::shm_unlink(cname.as_ptr()) };

        // 创建共享内存对象
        let shm_fd = unsafe {
            libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600)
        };
        if shm_fd < 0 {
            return Err(MinosError::ShmError(format!(
                "shm_open failed: {}",
                io::Error::last_os_error()
            )));
        }

        // 设置大小
        let ret = unsafe { libc::ftruncate(shm_fd, region_size as libc::off_t) };
        if ret != 0 {
            unsafe { libc::close(shm_fd) };
            unsafe { libc::shm_unlink(cname.as_ptr()) };
            return Err(MinosError::ShmError(format!(
                "ftruncate failed: {}",
                io::Error::last_os_error()
            )));
        }

        // mmap
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
            unsafe { libc::shm_unlink(cname.as_ptr()) };
            return Err(MinosError::ShmError(format!(
                "mmap failed: {}",
                io::Error::last_os_error()
            )));
        }

        // 初始化控制头
        let bitmap_offset = std::mem::size_of::<ShmControlHeader>() as u32;
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

        // 获取区域大小
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

        // 验证控制头
        region.header().validate()?;

        Ok(region)
    }

    /// 销毁共享内存区域（服务端调用）。
    ///
    /// 解除映射、关闭文件描述符、删除共享内存名称。
    pub fn destroy(self) -> MinosResult<()> {
        let ret = unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.size) };
        if ret != 0 {
            log::warn!("munmap failed: {}", io::Error::last_os_error());
        }

        let ret = unsafe { libc::close(self.shm_fd) };
        if ret != 0 {
            log::warn!("close shm_fd failed: {}", io::Error::last_os_error());
        }

        let cname = CString::new(self.name.as_str()).unwrap();
        let ret = unsafe { libc::shm_unlink(cname.as_ptr()) };
        if ret != 0 {
            log::warn!(
                "shm_unlink '{}' failed: {}",
                self.name,
                io::Error::last_os_error()
            );
        }

        Ok(())
    }

    /// 获取控制头的不可变引用。
    pub fn header(&self) -> &ShmControlHeader {
        unsafe { &*(self.ptr as *const ShmControlHeader) }
    }

    /// 获取控制头的可变引用。
    pub fn header_mut(&mut self) -> &mut ShmControlHeader {
        unsafe { &mut *(self.ptr as *mut ShmControlHeader) }
    }

    /// 获取数据页区域的起始指针。
    pub fn data_area(&self) -> *mut u8 {
        unsafe { self.ptr.add(consts::SHM_PAGE_SIZE as usize) }
    }

    /// 获取指定数据页的指针。
    ///
    /// page 0 是第一个数据页（不包含控制页）。
    pub fn page_ptr(&self, page_idx: u32) -> *mut u8 {
        let offset = (page_idx as usize + 1) * consts::SHM_PAGE_SIZE as usize;
        unsafe { self.ptr.add(offset) }
    }

    /// 获取页分配位图的起始指针。
    pub fn bitmap_ptr(&self) -> *mut u8 {
        let offset = self.header().page_bitmap_offset as usize;
        unsafe { self.ptr.add(offset) }
    }

    /// 获取互斥锁的指针。
    pub fn mutex_ptr(&self) -> *mut u8 {
        let offset = self.header().mutex_offset as usize;
        unsafe { self.ptr.add(offset) }
    }

    /// 区域总大小（字节）。
    pub fn size(&self) -> usize {
        self.size
    }

    /// 将数据拷贝到从 start_page 开始的连续页面中。
    pub fn write_to_pages(&self, start_page: u32, data: &[u8]) {
        let page_size = consts::SHM_PAGE_SIZE as usize;
        for (i, chunk) in data.chunks(page_size).enumerate() {
            let dst = self.page_ptr(start_page + i as u32);
            unsafe {
                std::ptr::copy_nonoverlapping(chunk.as_ptr(), dst, chunk.len());
            }
        }
    }

    /// 从 start_page 开始的连续页面中读取数据。
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
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.size);
            libc::close(self.shm_fd);
        }
    }
}

// ─── 单元测试 ───
//
// 注意：shm_open 在 macOS 上有不同的行为，这些测试仅在 Linux 上运行。

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn test_name(suffix: &str) -> String {
        format!("/minos_test_region_{suffix}")
    }

    #[test]
    fn test_create_and_destroy() {
        let name = test_name("create");
        let _ = std::fs::remove_file(&name); // cleanup leftover on macOS
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
