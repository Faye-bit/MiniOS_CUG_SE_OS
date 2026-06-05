//! Prometheus 监控指标模块。
//!
//! 使用 prometheus crate 注册并维护所有应用级指标（Gauge / Counter），
//! 对外通过 `encode()` 方法输出 Prometheus 标准文本格式。
//!
//! ## 已注册的指标
//!
//! | 指标名 | 类型 | 说明 |
//! |--------|------|------|
//! | `minios_uptime_seconds` | Gauge | 服务端运行时长（秒） |
//! | `minios_active_connections` | Gauge | 当前活跃客户端连接数 |
//! | `minios_connections_total` | Counter | 累计接受的连接数 |
//! | `minios_requests_total` | Counter | 累计处理的请求数 |
//! | `minios_store_objects` | Gauge | 存储中活跃对象数 |
//! | `minios_store_blocks_free` | Gauge | 空闲数据块数 |
//! | `minios_store_blocks_total` | Gauge | 数据块总数 |
//! | `minios_store_file_size_bytes` | Gauge | 存储文件大小（字节） |
//! | `minios_cache_entries` | Gauge | 当前缓存条目数 |
//! | `minios_cache_hits_total` | Counter | 缓存命中次数 |
//! | `minios_cache_misses_total` | Counter | 缓存未命中次数 |
//! | `minios_cache_evictions_total` | Counter | 缓存淘汰次数 |
//! | `minios_shm_pages_free` | Gauge | 共享内存空闲页数 |
//! | `minios_shm_pages_total` | Gauge | 共享内存总页数 |

use prometheus::{
    self, register_counter, register_gauge, Counter, Encoder, Gauge, TextEncoder,
};

/// 所有 Prometheus 指标的集中注册表。
///
/// 服务端在启动时创建一个 `MetricsRegistry` 实例，
/// 每次处理请求后更新对应的 Gauge / Counter。
/// `/metrics` HTTP 端点调用 `encode()` 输出文本格式。
pub struct MetricsRegistry {
    /// 服务端启动时间（Unix 秒），用于计算 uptime
    start_time_secs: i64,

    // ─── 连接级指标 ───
    /// 当前活跃客户端连接数 (Gauge)
    pub active_connections: Gauge,
    /// 累计接受的 TCP 连接总数 (Counter)
    pub connections_total: Counter,
    /// 累计处理的请求总数 (Counter)
    pub requests_total: Counter,

    // ─── 存储引擎指标 ───
    /// 当前存储的对象总数 (Gauge)
    pub store_objects: Gauge,
    /// 空闲数据块数 (Gauge)
    pub store_blocks_free: Gauge,
    /// 数据块总数 (Gauge)
    pub store_blocks_total: Gauge,
    /// 存储文件大小，单位字节 (Gauge)
    pub store_file_size_bytes: Gauge,

    // ─── 缓存指标 ───
    /// 当前缓存条目数 (Gauge)
    pub cache_entries: Gauge,
    /// 缓存命中次数 (Counter)
    pub cache_hits_total: Counter,
    /// 缓存未命中次数 (Counter)
    pub cache_misses_total: Counter,
    /// 缓存淘汰次数 (Counter)
    pub cache_evictions_total: Counter,

    // ─── 共享内存指标 ───
    /// 共享内存空闲页数 (Gauge)
    pub shm_pages_free: Gauge,
    /// 共享内存总页数 (Gauge)
    pub shm_pages_total: Gauge,
}

impl MetricsRegistry {
    /// 创建一个新的指标注册表，在 Prometheus 默认注册中心登记所有指标。
    ///
    /// # 参数
    /// - `start_time_secs`: 服务端启动时的 Unix 时间戳，用于计算 uptime。
    pub fn new(start_time_secs: i64) -> Self {
        // 容错：逐个创建，出错时打日志而不是 panicking
        let active_connections = register_gauge!(
            "minios_active_connections",
            "Current number of active client connections"
        )
        .unwrap_or_else(|e| {
            log::warn!("failed to register minios_active_connections: {e}");
            Gauge::new("minios_active_connections", "active connections").unwrap()
        });

        let connections_total = register_counter!(
            "minios_connections_total",
            "Total number of accepted connections"
        )
        .unwrap_or_else(|e| {
            log::warn!("failed to register minios_connections_total: {e}");
            Counter::new("minios_connections_total", "total connections").unwrap()
        });

        let requests_total = register_counter!(
            "minios_requests_total",
            "Total number of processed requests"
        )
        .unwrap_or_else(|e| {
            log::warn!("failed to register minios_requests_total: {e}");
            Counter::new("minios_requests_total", "total requests").unwrap()
        });

        let store_objects = register_gauge!(
            "minios_store_objects",
            "Current number of stored objects"
        )
        .unwrap_or_else(|e| {
            log::warn!("failed to register minios_store_objects: {e}");
            Gauge::new("minios_store_objects", "store objects").unwrap()
        });

        let store_blocks_free = register_gauge!(
            "minios_store_blocks_free",
            "Number of free data blocks"
        )
        .unwrap_or_else(|e| {
            log::warn!("failed to register minios_store_blocks_free: {e}");
            Gauge::new("minios_store_blocks_free", "free blocks").unwrap()
        });

        let store_blocks_total = register_gauge!(
            "minios_store_blocks_total",
            "Total number of data blocks"
        )
        .unwrap_or_else(|e| {
            log::warn!("failed to register minios_store_blocks_total: {e}");
            Gauge::new("minios_store_blocks_total", "total blocks").unwrap()
        });

        let store_file_size_bytes = register_gauge!(
            "minios_store_file_size_bytes",
            "Store file size in bytes"
        )
        .unwrap_or_else(|e| {
            log::warn!("failed to register minios_store_file_size_bytes: {e}");
            Gauge::new("minios_store_file_size_bytes", "file size").unwrap()
        });

        let cache_entries = register_gauge!(
            "minios_cache_entries",
            "Current number of cache entries"
        )
        .unwrap_or_else(|e| {
            log::warn!("failed to register minios_cache_entries: {e}");
            Gauge::new("minios_cache_entries", "cache entries").unwrap()
        });

        let cache_hits_total = register_counter!(
            "minios_cache_hits_total",
            "Total number of cache hits"
        )
        .unwrap_or_else(|e| {
            log::warn!("failed to register minios_cache_hits_total: {e}");
            Counter::new("minios_cache_hits_total", "cache hits").unwrap()
        });

        let cache_misses_total = register_counter!(
            "minios_cache_misses_total",
            "Total number of cache misses"
        )
        .unwrap_or_else(|e| {
            log::warn!("failed to register minios_cache_misses_total: {e}");
            Counter::new("minios_cache_misses_total", "cache misses").unwrap()
        });

        let cache_evictions_total = register_counter!(
            "minios_cache_evictions_total",
            "Total number of cache evictions"
        )
        .unwrap_or_else(|e| {
            log::warn!("failed to register minios_cache_evictions_total: {e}");
            Counter::new("minios_cache_evictions_total", "cache evictions").unwrap()
        });

        let shm_pages_free = register_gauge!(
            "minios_shm_pages_free",
            "Number of free shared memory pages"
        )
        .unwrap_or_else(|e| {
            log::warn!("failed to register minios_shm_pages_free: {e}");
            Gauge::new("minios_shm_pages_free", "shm free pages").unwrap()
        });

        let shm_pages_total = register_gauge!(
            "minios_shm_pages_total",
            "Total number of shared memory pages"
        )
        .unwrap_or_else(|e| {
            log::warn!("failed to register minios_shm_pages_total: {e}");
            Gauge::new("minios_shm_pages_total", "shm total pages").unwrap()
        });

        Self {
            start_time_secs,
            active_connections,
            connections_total,
            requests_total,
            store_objects,
            store_blocks_free,
            store_blocks_total,
            store_file_size_bytes,
            cache_entries,
            cache_hits_total,
            cache_misses_total,
            cache_evictions_total,
            shm_pages_free,
            shm_pages_total,
        }
    }

    /// 更新 uptime gauge 为当前时间与启动时间的差值（秒）。
    /// 注意：uptime 作为内联指标直接拼接在 encode() 输出中，
    /// 因此本方法当前为空；保留接口以备将来注册为正式 gauge。
    pub fn update_uptime(&self, _now_secs: i64) {
        // uptime 是"临时" gauge，每次 encode() 前动态计算并拼接
    }

    /// 将当前 uptime 作为临时 gauge 值返回（不在注册表中注册，直接在输出文本中拼接）。
    pub fn uptime_secs(&self, now_secs: i64) -> f64 {
        (now_secs - self.start_time_secs).max(0) as f64
    }

    /// 将所有已注册指标编码为 Prometheus 标准文本格式。
    ///
    /// # 参数
    /// - `now_secs`: 当前 Unix 时间戳，用于计算实时 uptime。
    ///
    /// # 返回值
    /// Prometheus exposition 格式的字节串，可直接写入 HTTP 响应体。
    pub fn encode(&self, now_secs: i64) -> String {
        let encoder = TextEncoder::new();

        // 编码所有已注册的指标族
        let metric_families = prometheus::gather();
        let mut buf = Vec::new();
        if encoder.encode(&metric_families, &mut buf).is_err() {
            return String::new();
        }

        let mut output = String::from_utf8_lossy(&buf).to_string();

        // 手动拼接 uptime 指标（避免为临时值注册/注销的开销）
        let uptime = self.uptime_secs(now_secs);
        output.push_str(&format!(
            "# HELP minios_uptime_seconds Server uptime in seconds\n\
             # TYPE minios_uptime_seconds gauge\n\
             minios_uptime_seconds {:.2}\n",
            uptime
        ));

        output
    }
}

// ─── 单元测试 ───

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_registry_creation() {
        let reg = MetricsRegistry::new(1000);
        assert_eq!(reg.start_time_secs, 1000);

        // 所有 gauge 应初始化为 0
        assert_eq!(reg.active_connections.get(), 0.0);
        assert_eq!(reg.store_objects.get(), 0.0);
        assert_eq!(reg.cache_entries.get(), 0.0);
    }

    #[test]
    fn test_counter_increment() {
        let reg = MetricsRegistry::new(0);

        reg.requests_total.inc();
        reg.requests_total.inc();
        assert_eq!(reg.requests_total.get(), 2.0);

        reg.cache_hits_total.inc();
        assert_eq!(reg.cache_hits_total.get(), 1.0);
    }

    #[test]
    fn test_gauge_set() {
        let reg = MetricsRegistry::new(0);

        reg.store_objects.set(42.0);
        assert_eq!(reg.store_objects.get(), 42.0);

        reg.store_objects.set(0.0);
        assert_eq!(reg.store_objects.get(), 0.0);
    }

    #[test]
    fn test_uptime_calculation() {
        let reg = MetricsRegistry::new(100);
        assert_eq!(reg.uptime_secs(150), 50.0);
        assert_eq!(reg.uptime_secs(100), 0.0);
        // 时间回退也安全（max(0) 保护）
        assert_eq!(reg.uptime_secs(50), 0.0);
    }

    #[test]
    fn test_encode_output_format() {
        let reg = MetricsRegistry::new(100);

        reg.store_objects.set(1.0);
        reg.cache_hits_total.inc();

        let output = reg.encode(150);

        // 输出应包含 HELP/TYPE 行和指标值
        assert!(output.contains("minios_store_objects"));
        assert!(output.contains("minios_cache_hits_total"));
        assert!(output.contains("minios_uptime_seconds"));

        // uptime 值应正确
        assert!(output.contains("minios_uptime_seconds 50"));
    }

    #[test]
    fn test_encode_empty_metrics() {
        let reg = MetricsRegistry::new(0);
        let output = reg.encode(0);

        // 即使没有指标更新，也应包含 uptime
        assert!(output.contains("minios_uptime_seconds"));
        assert!(!output.is_empty());
    }
}
