//! 请求消息定义 — 客户端通过共享内存发送给服务端的请求。

/// 请求类型枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RequestType {
    Put = 0,
    Get = 1,
    Delete = 2,
    List = 3,
    Status = 4,
    Shutdown = 5,
}

impl RequestType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Put),
            1 => Some(Self::Get),
            2 => Some(Self::Delete),
            3 => Some(Self::List),
            4 => Some(Self::Status),
            5 => Some(Self::Shutdown),
            _ => None,
        }
    }
}

/// 固定大小的请求槽位（256 bytes），存储于共享内存的请求队列中。
///
/// 字段按对齐要求排列（u64 → u32 → u8 → 定长数组），以避免 padding。
#[repr(C)]
#[derive(Debug, Clone)]
pub struct ShmRequest {
    /// 对象数据大小（字节）
    pub size: u64,
    /// 请求时间戳
    pub timestamp: i64,
    /// 客户端 ID（PID）
    pub client_id: u32,
    /// 分配的共享内存页数
    pub num_pages: u32,
    /// 起始共享内存页号
    pub start_page: u32,
    /// 请求类型
    pub request_type: u8,
    /// 槽位状态：0=空闲, 1=待处理, 2=处理中, 3=已完成
    pub status: u8,
    /// 对象 UUID（用于 Get/Delete）
    pub object_id: [u8; 16],
    /// 对象名称（用于 Put/Get），null-terminated
    pub name: [u8; 64],
    /// 内容类型，null-terminated
    pub content_type: [u8; 32],
    /// 自定义标签，null-terminated
    pub tags: [u8; 64],
    /// 保留字段，填充至 256 字节
    pub _reserved: [u8; 50],
}

const _: () = assert!(std::mem::size_of::<ShmRequest>() == 256);

/// 槽位状态常量。
pub mod slot_status {
    pub const FREE: u8 = 0;
    pub const PENDING: u8 = 1;
    pub const PROCESSING: u8 = 2;
    pub const DONE: u8 = 3;
}

impl ShmRequest {
    /// 创建一个空闲槽位。
    pub const fn empty() -> Self {
        Self {
            size: 0,
            timestamp: 0,
            client_id: 0,
            num_pages: 0,
            start_page: 0,
            request_type: 0,
            status: slot_status::FREE,
            object_id: [0u8; 16],
            name: [0u8; 64],
            content_type: [0u8; 32],
            tags: [0u8; 64],
            _reserved: [0u8; 50],
        }
    }

    /// 设置一个字符串字段到定长字节数组（null-terminated）。
    fn pack_str<const N: usize>(s: &str) -> [u8; N] {
        let bytes = s.as_bytes();
        let cap = N - 1;
        let len = bytes.len().min(cap);
        let mut buf = [0u8; N];
        buf[..len].copy_from_slice(&bytes[..len]);
        buf
    }

    /// 创建 Put 请求。
    pub fn new_put(
        client_id: u32,
        name: &str,
        data_size: u64,
        content_type: &str,
        tags: &str,
        start_page: u32,
        num_pages: u32,
    ) -> Self {
        Self {
            size: data_size,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
            client_id,
            num_pages,
            start_page,
            request_type: RequestType::Put as u8,
            status: slot_status::PENDING,
            object_id: [0u8; 16],
            name: Self::pack_str::<64>(name),
            content_type: Self::pack_str::<32>(content_type),
            tags: Self::pack_str::<64>(tags),
            _reserved: [0u8; 50],
        }
    }

    /// 创建 Get 请求。
    pub fn new_get_by_id(client_id: u32, object_id: [u8; 16]) -> Self {
        Self {
            size: 0,
            timestamp: 0,
            client_id,
            num_pages: 0,
            start_page: 0,
            request_type: RequestType::Get as u8,
            status: slot_status::PENDING,
            object_id,
            name: [0u8; 64],
            content_type: [0u8; 32],
            tags: [0u8; 64],
            _reserved: [0u8; 50],
        }
    }

    /// 提取名称字符串。
    pub fn name_str(&self) -> &str {
        fixed_str(&self.name)
    }

    /// 提取内容类型字符串。
    pub fn content_type_str(&self) -> &str {
        fixed_str(&self.content_type)
    }

    /// 提取标签字符串。
    pub fn tags_str(&self) -> &str {
        fixed_str(&self.tags)
    }
}

fn fixed_str(bytes: &[u8]) -> &str {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).unwrap_or("")
}
