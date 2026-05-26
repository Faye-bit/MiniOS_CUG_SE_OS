//! 错误类型定义。

use thiserror::Error;

/// minios 库所有可能的错误类型。
#[derive(Error, Debug)]
pub enum miniosError {
    /// I/O 错误
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// 存储文件格式无效（魔数不匹配、版本不支持等）
    #[error("Invalid store file: {0}")]
    InvalidStore(String),

    /// 对象未找到
    #[error("Object not found: {0}")]
    ObjectNotFound(String),

    /// 存储空间不足
    #[error("No space left: {0}")]
    NoSpace(String),

    /// 共享内存操作错误
    #[error("Shared memory error: {0}")]
    ShmError(String),

    /// 进程间通信错误
    #[error("IPC error: {0}")]
    IpcError(String),

    /// 缓存错误
    #[error("Cache error: {0}")]
    CacheError(String),

    /// 协议解析错误
    #[error("Protocol error: {0}")]
    ProtocolError(String),

    /// UUID 解析错误
    #[error("Invalid UUID: {0}")]
    InvalidUuid(String),

    /// 守护进程操作错误
    #[error("Daemon error: {0}")]
    DaemonError(String),

    /// 字符串转换错误
    #[error("String conversion error: {0}")]
    StringError(#[from] std::str::Utf8Error),
}

/// 库级别的 Result 类型别名。
pub type miniosResult<T> = Result<T, miniosError>;
