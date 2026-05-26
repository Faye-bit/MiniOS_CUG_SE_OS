//! 响应消息定义 — 服务端通过共享内存返回给客户端的响应。

/// 响应状态码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ResponseStatus {
    Ok = 0,
    NotFound = 1,
    NoSpace = 2,
    Error = 3,
    InvalidRequest = 4,
}

impl ResponseStatus {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Ok),
            1 => Some(Self::NotFound),
            2 => Some(Self::NoSpace),
            3 => Some(Self::Error),
            4 => Some(Self::InvalidRequest),
            _ => None,
        }
    }

    pub fn to_u8(self) -> u8 {
        self as u8
    }
}

/// 固定大小的响应槽位（256 bytes），存储于共享内存的响应队列中。
///
/// 字段按对齐要求排列（u64 → u32 → u8 → 定长数组），以避免 padding。
#[repr(C)]
#[derive(Debug, Clone)]
pub struct ShmResponse {
    /// 对象数据大小（用于 Get）
    pub size: u64,
    /// 客户端 ID
    pub client_id: u32,
    /// 分配的共享内存页数
    pub num_pages: u32,
    /// 起始共享内存页号
    pub start_page: u32,
    /// List 操作返回的对象数量
    pub list_count: u32,
    /// 响应状态码
    pub status_code: u8,
    /// 槽位状态：0=空闲, 1=已填充
    pub slot_status: u8,
    /// 对象 UUID（Put 返回的 ID 或 Get/Delete 确认的 ID）
    pub object_id: [u8; 16],
    /// 错误消息或状态文本（128 bytes），null-terminated
    pub message: [u8; 128],
    /// 保留字段，填充至 256 字节
    pub _reserved: [u8; 86],
}

const _: () = assert!(std::mem::size_of::<ShmResponse>() == 256);

/// 响应槽位状态。
pub mod slot_status {
    pub const FREE: u8 = 0;
    pub const FILLED: u8 = 1;
}

impl ShmResponse {
    /// 创建一个空闲槽位。
    pub const fn empty() -> Self {
        Self {
            size: 0,
            client_id: 0,
            num_pages: 0,
            start_page: 0,
            list_count: 0,
            status_code: 0,
            slot_status: slot_status::FREE,
            object_id: [0u8; 16],
            message: [0u8; 128],
            _reserved: [0u8; 86],
        }
    }

    /// 创建成功响应。
    pub fn ok(client_id: u32, object_id: [u8; 16], size: u64) -> Self {
        Self {
            size,
            client_id,
            num_pages: 0,
            start_page: 0,
            list_count: 0,
            status_code: ResponseStatus::Ok as u8,
            slot_status: slot_status::FILLED,
            object_id,
            message: [0u8; 128],
            _reserved: [0u8; 86],
        }
    }

    /// 创建错误响应。
    pub fn error(client_id: u32, status: ResponseStatus, msg: &str) -> Self {
        let mut message = [0u8; 128];
        let bytes = msg.as_bytes();
        let len = bytes.len().min(127);
        message[..len].copy_from_slice(&bytes[..len]);

        Self {
            size: 0,
            client_id,
            num_pages: 0,
            start_page: 0,
            list_count: 0,
            status_code: status as u8,
            slot_status: slot_status::FILLED,
            object_id: [0u8; 16],
            message,
            _reserved: [0u8; 86],
        }
    }

    /// 提取消息字符串。
    pub fn message_str(&self) -> &str {
        let end = self
            .message
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.message.len());
        std::str::from_utf8(&self.message[..end]).unwrap_or("")
    }
}
