use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const GB: f32 = 1024.0 * 1024.0 * 1024.0;

#[derive(Clone, Copy, Default)]
pub struct Metrics {
    pub vram_used: f32,
    pub vram_total: f32,
    pub gpu_util: u32,
    pub ram_used: f32,
    pub ram_total: f32,
    pub swap_used: f32,
}

/// Samples once per second on its own thread; NVML loads there so it never delays the first frame.
pub fn spawn() -> Arc<Mutex<Metrics>> {
    let shared = Arc::new(Mutex::new(Metrics::default()));
    let out = shared.clone();
    thread::spawn(move || {
        let nvml = nvml_wrapper::Nvml::init().ok();
        // Every card counts: llama.cpp splits a model's layers across all of them.
        let gpus: Vec<_> = nvml.as_ref().map(|n| (0..n.device_count().unwrap_or(0)).filter_map(|i| n.device_by_index(i).ok()).collect()).unwrap_or_default();
        let mut sys = sysinfo::System::new();
        loop {
            sys.refresh_memory();
            let mut m = Metrics {
                ram_used: sys.used_memory() as f32 / GB,
                ram_total: sys.total_memory() as f32 / GB,
                swap_used: sys.used_swap() as f32 / GB,
                ..Default::default()
            };
            for d in &gpus {
                if let Ok(mem) = d.memory_info() {
                    m.vram_used += mem.used as f32 / GB;
                    m.vram_total += mem.total as f32 / GB;
                }
                m.gpu_util = m.gpu_util.max(d.utilization_rates().map(|u| u.gpu).unwrap_or(0));
            }
            *out.lock().unwrap() = m;
            thread::sleep(Duration::from_secs(1));
        }
    });
    shared
}
