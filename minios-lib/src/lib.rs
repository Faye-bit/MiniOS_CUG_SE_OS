//! Minios (Mini Object Storage) — 简单对象存储服务核心库。
//!
//! 提供对象存储引擎（store.odb）、共享内存缓冲区管理、
//! LRU 缓存、进程间通信协议和守护进程管理等模块。

pub mod common;

pub mod cache;
pub mod daemon;
pub mod protocol;
pub mod shm;
pub mod storage;
