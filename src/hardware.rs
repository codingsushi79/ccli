//! Hardware telemetry: CPU (global and per-core), memory, load, thermals, and
//! GPUs where we can see them.
//!
//! Sampling is done on the daemon's timer, never on a request, so a slow
//! `nvidia-smi` can't stall the TUI.

use std::process::Command;
use std::time::{Duration, Instant};

use sysinfo::{Components, MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::model::{GpuInfo, HardwareSnapshot, TempSensor};

/// GPUs change slowly and shelling out is comparatively expensive.
const GPU_INTERVAL: Duration = Duration::from_secs(5);

pub struct HardwareMonitor {
    system: System,
    components: Components,
    pid: Option<Pid>,
    gpus: Vec<GpuInfo>,
    gpu_checked: Option<Instant>,
    gpu_available: bool,
    static_info: (String, String, usize, String),
}

impl HardwareMonitor {
    pub fn new() -> Self {
        let mut system = System::new();
        system.refresh_cpu_all();
        system.refresh_memory();
        let cpu_brand = system
            .cpus()
            .first()
            .map(|c| c.brand().trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown CPU".into());
        let arch = System::cpu_arch();
        let os = format!(
            "{} {}",
            System::name().unwrap_or_else(|| "unknown".into()),
            System::os_version().unwrap_or_default()
        )
        .trim()
        .to_string();
        let physical = System::physical_core_count().unwrap_or(system.cpus().len());
        Self {
            system,
            components: Components::new_with_refreshed_list(),
            pid: sysinfo::get_current_pid().ok(),
            gpus: Vec::new(),
            gpu_checked: None,
            gpu_available: true,
            static_info: (cpu_brand, arch, physical, os),
        }
    }

    pub fn sample(&mut self) -> HardwareSnapshot {
        self.system.refresh_cpu_all();
        self.system
            .refresh_memory_specifics(MemoryRefreshKind::everything());
        if let Some(pid) = self.pid {
            self.system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                true,
                ProcessRefreshKind::nothing().with_cpu().with_memory(),
            );
        }
        self.components.refresh(false);

        let per_core: Vec<f32> = self.system.cpus().iter().map(|c| c.cpu_usage()).collect();
        let freq = self
            .system
            .cpus()
            .iter()
            .map(|c| c.frequency())
            .max()
            .unwrap_or(0);

        let mut temps: Vec<TempSensor> = self
            .components
            .iter()
            .filter_map(|c| {
                c.temperature().map(|t| TempSensor {
                    label: c.label().to_string(),
                    celsius: t,
                    max: c.max(),
                })
            })
            .collect();
        temps.sort_by(|a, b| {
            b.celsius
                .partial_cmp(&a.celsius)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let load = System::load_average();
        let (proc_cpu, proc_mem) = self
            .pid
            .and_then(|pid| self.system.process(pid))
            .map(|p| (p.cpu_usage(), p.memory()))
            .unwrap_or((0.0, 0));

        self.refresh_gpus();

        let (cpu_brand, cpu_arch, physical, os) = self.static_info.clone();
        HardwareSnapshot {
            cpu_brand,
            cpu_arch,
            cores_physical: physical,
            cores_logical: per_core.len(),
            cpu_usage: self.system.global_cpu_usage(),
            per_core,
            freq_mhz: freq,
            mem_used: self.system.used_memory(),
            mem_total: self.system.total_memory(),
            swap_used: self.system.used_swap(),
            swap_total: self.system.total_swap(),
            load_avg: [load.one, load.five, load.fifteen],
            temps,
            gpus: self.gpus.clone(),
            host_uptime_secs: System::uptime(),
            proc_cpu,
            proc_mem,
            os,
        }
    }

    fn refresh_gpus(&mut self) {
        if !self.gpu_available {
            return;
        }
        if let Some(last) = self.gpu_checked
            && last.elapsed() < GPU_INTERVAL
        {
            return;
        }
        self.gpu_checked = Some(Instant::now());
        match query_nvidia() {
            Some(gpus) => self.gpus = gpus,
            None => {
                // No nvidia-smi (or it failed): stop paying for the probe.
                self.gpu_available = false;
                self.gpus.clear();
            }
        }
    }
}

fn query_nvidia() -> Option<Vec<GpuInfo>> {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,fan.speed",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let gpus: Vec<GpuInfo> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(index, line)| {
            let f: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
            let num = |i: usize| f.get(i).and_then(|v| v.parse::<f32>().ok());
            GpuInfo {
                index,
                name: f.first().copied().unwrap_or("GPU").to_string(),
                util_percent: num(1),
                mem_used_mb: num(2).map(|v| v as u64),
                mem_total_mb: num(3).map(|v| v as u64),
                temp_c: num(4),
                power_w: num(5),
                fan_percent: num(6),
            }
        })
        .collect();
    Some(gpus)
}
