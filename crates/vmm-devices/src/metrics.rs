// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VMM metrics collection.
//!
//! Lock-free atomic counters for vCPU, disk, and network I/O.
//! Zero overhead when not being read — just atomic increments on
//! the hot path.
//!
//! Exposed via the control socket `metrics` command (JSON) and
//! optionally via a Prometheus-compatible HTTP endpoint.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Relaxed ordering for counters — we don't need cross-counter
/// consistency, just monotonic increments visible eventually.
const ORD: Ordering = Ordering::Relaxed;

/// Per-vCPU metrics.
pub struct VcpuMetrics {
    /// Total VM exits.
    pub exits: AtomicU64,
    /// VM exits by type.
    pub exits_pio: AtomicU64,
    pub exits_mmio: AtomicU64,
    pub exits_hlt: AtomicU64,
    pub exits_msr: AtomicU64,
    pub exits_other: AtomicU64,
}

impl VcpuMetrics {
    pub fn new() -> Self {
        Self {
            exits: AtomicU64::new(0),
            exits_pio: AtomicU64::new(0),
            exits_mmio: AtomicU64::new(0),
            exits_hlt: AtomicU64::new(0),
            exits_msr: AtomicU64::new(0),
            exits_other: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn record_exit(&self, exit_code: u32) {
        self.exits.fetch_add(1, ORD);
        match exit_code {
            1 => self.exits_pio.fetch_add(1, ORD),     // Inout
            2 | 10 => self.exits_mmio.fetch_add(1, ORD), // Mmio | InstEmul
            3 | 4 => self.exits_msr.fetch_add(1, ORD),  // Rdmsr | Wrmsr
            11 => self.exits_hlt.fetch_add(1, ORD),     // Hlt
            _ => self.exits_other.fetch_add(1, ORD),
        };
    }

    pub fn to_json(&self, vcpu_id: i32) -> String {
        format!(
            r#"{{"vcpu":{},"exits":{},"exits_pio":{},"exits_mmio":{},"exits_hlt":{},"exits_msr":{},"exits_other":{}}}"#,
            vcpu_id,
            self.exits.load(ORD),
            self.exits_pio.load(ORD),
            self.exits_mmio.load(ORD),
            self.exits_hlt.load(ORD),
            self.exits_msr.load(ORD),
            self.exits_other.load(ORD),
        )
    }

    pub fn to_prometheus(&self, vcpu_id: i32) -> String {
        let id = vcpu_id;
        format!(
            "vmm_vcpu_exits_total{{vcpu=\"{}\"}} {}\n\
             vmm_vcpu_exits{{vcpu=\"{}\",type=\"pio\"}} {}\n\
             vmm_vcpu_exits{{vcpu=\"{}\",type=\"mmio\"}} {}\n\
             vmm_vcpu_exits{{vcpu=\"{}\",type=\"hlt\"}} {}\n\
             vmm_vcpu_exits{{vcpu=\"{}\",type=\"msr\"}} {}\n\
             vmm_vcpu_exits{{vcpu=\"{}\",type=\"other\"}} {}\n",
            id, self.exits.load(ORD),
            id, self.exits_pio.load(ORD),
            id, self.exits_mmio.load(ORD),
            id, self.exits_hlt.load(ORD),
            id, self.exits_msr.load(ORD),
            id, self.exits_other.load(ORD),
        )
    }
}

/// Block device metrics (shared between virtio-blk and NVMe).
pub struct BlockMetrics {
    pub reads: AtomicU64,
    pub writes: AtomicU64,
    pub flushes: AtomicU64,
    pub read_bytes: AtomicU64,
    pub write_bytes: AtomicU64,
    pub read_errors: AtomicU64,
    pub write_errors: AtomicU64,
    /// Cumulative read latency in microseconds.
    pub read_latency_us: AtomicU64,
    /// Cumulative write latency in microseconds.
    pub write_latency_us: AtomicU64,
    /// Start time for uptime calculation.
    start: Instant,
}

impl BlockMetrics {
    pub fn new() -> Self {
        Self {
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            flushes: AtomicU64::new(0),
            read_bytes: AtomicU64::new(0),
            write_bytes: AtomicU64::new(0),
            read_errors: AtomicU64::new(0),
            write_errors: AtomicU64::new(0),
            read_latency_us: AtomicU64::new(0),
            write_latency_us: AtomicU64::new(0),
            start: Instant::now(),
        }
    }

    #[inline]
    pub fn record_read(&self, bytes: u64, latency_us: u64, success: bool) {
        self.reads.fetch_add(1, ORD);
        self.read_bytes.fetch_add(bytes, ORD);
        self.read_latency_us.fetch_add(latency_us, ORD);
        if !success {
            self.read_errors.fetch_add(1, ORD);
        }
    }

    #[inline]
    pub fn record_write(&self, bytes: u64, latency_us: u64, success: bool) {
        self.writes.fetch_add(1, ORD);
        self.write_bytes.fetch_add(bytes, ORD);
        self.write_latency_us.fetch_add(latency_us, ORD);
        if !success {
            self.write_errors.fetch_add(1, ORD);
        }
    }

    #[inline]
    pub fn record_flush(&self) {
        self.flushes.fetch_add(1, ORD);
    }

    pub fn avg_read_latency_us(&self) -> u64 {
        let reads = self.reads.load(ORD);
        if reads == 0 { 0 } else { self.read_latency_us.load(ORD) / reads }
    }

    pub fn avg_write_latency_us(&self) -> u64 {
        let writes = self.writes.load(ORD);
        if writes == 0 { 0 } else { self.write_latency_us.load(ORD) / writes }
    }

    pub fn to_json(&self, device: &str) -> String {
        format!(
            r#"{{"device":"{}","reads":{},"writes":{},"flushes":{},"read_bytes":{},"write_bytes":{},"read_errors":{},"write_errors":{},"avg_read_latency_us":{},"avg_write_latency_us":{}}}"#,
            device,
            self.reads.load(ORD),
            self.writes.load(ORD),
            self.flushes.load(ORD),
            self.read_bytes.load(ORD),
            self.write_bytes.load(ORD),
            self.read_errors.load(ORD),
            self.write_errors.load(ORD),
            self.avg_read_latency_us(),
            self.avg_write_latency_us(),
        )
    }

    pub fn to_prometheus(&self, device: &str) -> String {
        format!(
            "vmm_block_reads_total{{device=\"{}\"}} {}\n\
             vmm_block_writes_total{{device=\"{}\"}} {}\n\
             vmm_block_flushes_total{{device=\"{}\"}} {}\n\
             vmm_block_read_bytes_total{{device=\"{}\"}} {}\n\
             vmm_block_write_bytes_total{{device=\"{}\"}} {}\n\
             vmm_block_read_errors_total{{device=\"{}\"}} {}\n\
             vmm_block_write_errors_total{{device=\"{}\"}} {}\n\
             vmm_block_read_latency_us_avg{{device=\"{}\"}} {}\n\
             vmm_block_write_latency_us_avg{{device=\"{}\"}} {}\n",
            device, self.reads.load(ORD),
            device, self.writes.load(ORD),
            device, self.flushes.load(ORD),
            device, self.read_bytes.load(ORD),
            device, self.write_bytes.load(ORD),
            device, self.read_errors.load(ORD),
            device, self.write_errors.load(ORD),
            device, self.avg_read_latency_us(),
            device, self.avg_write_latency_us(),
        )
    }
}

/// Network device metrics.
pub struct NetMetrics {
    pub tx_packets: AtomicU64,
    pub rx_packets: AtomicU64,
    pub tx_bytes: AtomicU64,
    pub rx_bytes: AtomicU64,
    pub interrupts: AtomicU64,
    pub kicks: AtomicU64,
}

impl NetMetrics {
    pub fn new() -> Self {
        Self {
            tx_packets: AtomicU64::new(0),
            rx_packets: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            rx_bytes: AtomicU64::new(0),
            interrupts: AtomicU64::new(0),
            kicks: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn record_kick(&self) {
        self.kicks.fetch_add(1, ORD);
    }

    #[inline]
    pub fn record_interrupt(&self) {
        self.interrupts.fetch_add(1, ORD);
    }

    pub fn to_json(&self, device: &str) -> String {
        format!(
            r#"{{"device":"{}","tx_packets":{},"rx_packets":{},"tx_bytes":{},"rx_bytes":{},"interrupts":{},"kicks":{}}}"#,
            device,
            self.tx_packets.load(ORD),
            self.rx_packets.load(ORD),
            self.tx_bytes.load(ORD),
            self.rx_bytes.load(ORD),
            self.interrupts.load(ORD),
            self.kicks.load(ORD),
        )
    }

    pub fn to_prometheus(&self, device: &str) -> String {
        format!(
            "vmm_net_tx_packets_total{{device=\"{}\"}} {}\n\
             vmm_net_rx_packets_total{{device=\"{}\"}} {}\n\
             vmm_net_tx_bytes_total{{device=\"{}\"}} {}\n\
             vmm_net_rx_bytes_total{{device=\"{}\"}} {}\n\
             vmm_net_interrupts_total{{device=\"{}\"}} {}\n\
             vmm_net_kicks_total{{device=\"{}\"}} {}\n",
            device, self.tx_packets.load(ORD),
            device, self.rx_packets.load(ORD),
            device, self.tx_bytes.load(ORD),
            device, self.rx_bytes.load(ORD),
            device, self.interrupts.load(ORD),
            device, self.kicks.load(ORD),
        )
    }
}

/// Global VMM metrics registry.
///
/// Shared via `Arc<VmmMetrics>` between the control socket,
/// optional Prometheus endpoint, and all device/vCPU threads.
pub struct VmmMetrics {
    pub vcpus: Vec<VcpuMetrics>,
    pub block_devices: Vec<(String, BlockMetrics)>,
    pub net_devices: Vec<(String, NetMetrics)>,
    pub start_time: Instant,
}

impl VmmMetrics {
    pub fn new(num_cpus: u32) -> Self {
        let vcpus = (0..num_cpus)
            .map(|_| VcpuMetrics::new())
            .collect();
        Self {
            vcpus,
            block_devices: Vec::new(),
            net_devices: Vec::new(),
            start_time: Instant::now(),
        }
    }

    pub fn add_block_device(&mut self, name: String) -> &BlockMetrics {
        self.block_devices.push((name, BlockMetrics::new()));
        &self.block_devices.last().unwrap().1
    }

    pub fn add_net_device(&mut self, name: String) -> &NetMetrics {
        self.net_devices.push((name, NetMetrics::new()));
        &self.net_devices.last().unwrap().1
    }

    /// Dump all metrics as JSON.
    pub fn to_json(&self) -> String {
        let uptime = self.start_time.elapsed().as_secs();

        let vcpu_json: Vec<String> = self.vcpus.iter()
            .enumerate()
            .map(|(i, v)| v.to_json(i as i32))
            .collect();

        let block_json: Vec<String> = self.block_devices.iter()
            .map(|(name, m)| m.to_json(name))
            .collect();

        let net_json: Vec<String> = self.net_devices.iter()
            .map(|(name, m)| m.to_json(name))
            .collect();

        format!(
            r#"{{"uptime_secs":{},"vcpus":[{}],"block_devices":[{}],"net_devices":[{}]}}"#,
            uptime,
            vcpu_json.join(","),
            block_json.join(","),
            net_json.join(","),
        )
    }

    /// Dump all metrics in Prometheus text format.
    pub fn to_prometheus(&self) -> String {
        let mut out = String::with_capacity(4096);

        out.push_str(&format!(
            "# HELP vmm_uptime_seconds VMM uptime\n\
             # TYPE vmm_uptime_seconds gauge\n\
             vmm_uptime_seconds {}\n",
            self.start_time.elapsed().as_secs(),
        ));

        out.push_str("# HELP vmm_vcpu_exits_total Total VM exits per vCPU\n");
        out.push_str("# TYPE vmm_vcpu_exits_total counter\n");
        out.push_str("# HELP vmm_vcpu_exits VM exits by type per vCPU\n");
        out.push_str("# TYPE vmm_vcpu_exits counter\n");
        for (i, v) in self.vcpus.iter().enumerate() {
            out.push_str(&v.to_prometheus(i as i32));
        }

        out.push_str("# HELP vmm_block_reads_total Block read operations\n");
        out.push_str("# TYPE vmm_block_reads_total counter\n");
        out.push_str("# HELP vmm_block_writes_total Block write operations\n");
        out.push_str("# TYPE vmm_block_writes_total counter\n");
        out.push_str("# HELP vmm_block_read_bytes_total Bytes read\n");
        out.push_str("# TYPE vmm_block_read_bytes_total counter\n");
        out.push_str("# HELP vmm_block_write_bytes_total Bytes written\n");
        out.push_str("# TYPE vmm_block_write_bytes_total counter\n");
        for (name, m) in &self.block_devices {
            out.push_str(&m.to_prometheus(name));
        }

        out.push_str("# HELP vmm_net_tx_packets_total Network TX packets\n");
        out.push_str("# TYPE vmm_net_tx_packets_total counter\n");
        out.push_str("# HELP vmm_net_rx_packets_total Network RX packets\n");
        out.push_str("# TYPE vmm_net_rx_packets_total counter\n");
        for (name, m) in &self.net_devices {
            out.push_str(&m.to_prometheus(name));
        }

        out
    }
}
