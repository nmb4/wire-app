use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceUsage {
    pub cpu_percent: f32,
    pub gpu_percent: Option<f32>,
    pub memory_bytes: u64,
}

pub struct ResourceMonitor {
    usage: Arc<RwLock<ResourceUsage>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ResourceMonitor {
    pub fn start() -> Self {
        let usage = Arc::new(RwLock::new(ResourceUsage::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_usage = usage.clone();
        let worker_stop = stop.clone();
        let worker = thread::Builder::new()
            .name("resource-monitor".into())
            .spawn(move || sample_loop(worker_stop, worker_usage))
            .ok();

        Self {
            usage,
            stop,
            worker,
        }
    }

    pub fn snapshot(&self) -> ResourceUsage {
        self.usage.read().map(|usage| *usage).unwrap_or_default()
    }
}

impl Drop for ResourceMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

fn sample_loop(stop: Arc<AtomicBool>, usage: Arc<RwLock<ResourceUsage>>) {
    let pid = Pid::from_u32(std::process::id());
    let refresh = ProcessRefreshKind::new().with_cpu().with_memory();
    let mut system = System::new();
    let logical_cpus = thread::available_parallelism()
        .map(|count| count.get() as f32)
        .unwrap_or(1.0);
    #[cfg(windows)]
    let mut gpu = windows_gpu::GpuSampler::new();

    // CPU usage is a delta, so seed sysinfo before publishing the first sample.
    system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), refresh);
    while !stop.load(Ordering::Relaxed) {
        thread::park_timeout(Duration::from_secs(1));
        if stop.load(Ordering::Relaxed) {
            break;
        }
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), refresh);
        let Some(process) = system.process(pid) else {
            continue;
        };
        #[cfg(windows)]
        let gpu_percent = gpu.as_mut().and_then(windows_gpu::GpuSampler::sample);
        #[cfg(not(windows))]
        let gpu_percent = None;

        let next = ResourceUsage {
            // sysinfo reports 100% for one fully occupied logical CPU. Normalize this
            // to the whole-machine percentage used by Windows Task Manager.
            cpu_percent: (process.cpu_usage() / logical_cpus).clamp(0.0, 100.0),
            gpu_percent,
            memory_bytes: process.memory(),
        };
        if let Ok(mut current) = usage.write() {
            *current = next;
        }
    }
}

#[cfg(windows)]
mod windows_gpu {
    use std::mem::{size_of, MaybeUninit};

    use windows::core::{w, PCWSTR};
    use windows::Win32::System::Performance::{
        PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
        PdhOpenQueryW, PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W,
        PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
    };

    pub struct GpuSampler {
        query: PDH_HQUERY,
        counter: PDH_HCOUNTER,
        pid_marker: String,
    }

    impl GpuSampler {
        pub fn new() -> Option<Self> {
            let mut query = PDH_HQUERY::default();
            if unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut query) } != 0 {
                return None;
            }
            let mut counter = PDH_HCOUNTER::default();
            let status = unsafe {
                PdhAddEnglishCounterW(
                    query,
                    w!(r"\GPU Engine(*)\Utilization Percentage"),
                    0,
                    &mut counter,
                )
            };
            if status != 0 {
                unsafe {
                    PdhCloseQuery(query);
                }
                return None;
            }
            if unsafe { PdhCollectQueryData(query) } != 0 {
                unsafe {
                    PdhCloseQuery(query);
                }
                return None;
            }
            Some(Self {
                query,
                counter,
                pid_marker: format!("pid_{}_", std::process::id()),
            })
        }

        pub fn sample(&mut self) -> Option<f32> {
            if unsafe { PdhCollectQueryData(self.query) } != 0 {
                return None;
            }

            let mut buffer_size = 0u32;
            let mut item_count = 0u32;
            let status = unsafe {
                PdhGetFormattedCounterArrayW(
                    self.counter,
                    PDH_FMT_DOUBLE,
                    &mut buffer_size,
                    &mut item_count,
                    None,
                )
            };
            if status != PDH_MORE_DATA || buffer_size == 0 {
                return None;
            }

            let unit = size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
            let units = (buffer_size as usize).div_ceil(unit);
            let mut storage = vec![MaybeUninit::<PDH_FMT_COUNTERVALUE_ITEM_W>::uninit(); units];
            let status = unsafe {
                PdhGetFormattedCounterArrayW(
                    self.counter,
                    PDH_FMT_DOUBLE,
                    &mut buffer_size,
                    &mut item_count,
                    Some(storage.as_mut_ptr().cast()),
                )
            };
            if status != 0 || item_count as usize > units {
                return None;
            }

            let items = unsafe {
                std::slice::from_raw_parts(
                    storage.as_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>(),
                    item_count as usize,
                )
            };
            // Task Manager's per-process GPU column reports the busiest engine,
            // not the sum of independent 3D/decode/copy engine percentages.
            // Summing them made a decoder at 8% plus presentation at 18% look
            // like one GPU was 26% saturated, which is not a meaningful total.
            let mut busiest_engine = 0.0f64;
            let mut found = false;
            for item in items {
                let Ok(name) = (unsafe { item.szName.to_string() }) else {
                    continue;
                };
                if !name.to_ascii_lowercase().contains(&self.pid_marker) {
                    continue;
                }
                if item.FmtValue.CStatus != PDH_CSTATUS_VALID_DATA
                    && item.FmtValue.CStatus != PDH_CSTATUS_NEW_DATA
                {
                    continue;
                }
                let value = unsafe { item.FmtValue.Anonymous.doubleValue };
                if value.is_finite() && value > 0.0 {
                    busiest_engine = busiest_engine.max(value);
                }
                found = true;
            }
            found.then_some(busiest_engine.clamp(0.0, 100.0) as f32)
        }
    }

    impl Drop for GpuSampler {
        fn drop(&mut self) {
            unsafe {
                PdhCloseQuery(self.query);
            }
        }
    }
}
