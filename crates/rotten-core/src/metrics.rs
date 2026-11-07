use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct MirrorMetrics {
    pub video_frames: AtomicU64,
    pub video_fps: AtomicU64,
    pub video_dropped: AtomicU64,
    pub audio_real_frames: AtomicU64,
    pub audio_silent_frames: AtomicU64,
    pub audio_queue_depth: AtomicU64,
    pub audio_sync_error_ms: AtomicI64,
    pub video_playout_slack_ms: AtomicI64,
    pub av_skew_ms: AtomicI64,
    pub max_abs_av_skew_ms: AtomicI64,
    pub timeline_correction_us: AtomicI64,
    pub cpu_permille: AtomicU64,
    pub working_set_bytes: AtomicU64,
    pub private_memory_bytes: AtomicU64,
    pub reconnects: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MirrorMetricsSnapshot {
    pub video_frames: u64,
    pub video_fps: u64,
    pub video_dropped: u64,
    pub audio_real_frames: u64,
    pub audio_silent_frames: u64,
    pub audio_queue_depth: u64,
    pub audio_sync_error_ms: i64,
    pub video_playout_slack_ms: i64,
    pub av_skew_ms: i64,
    pub max_abs_av_skew_ms: i64,
    pub timeline_correction_us: i64,
    pub cpu_permille: u64,
    pub working_set_bytes: u64,
    pub private_memory_bytes: u64,
    pub reconnects: u64,
}

impl MirrorMetrics {
    pub fn snapshot(&self) -> MirrorMetricsSnapshot {
        MirrorMetricsSnapshot {
            video_frames: self.video_frames.load(Ordering::Relaxed),
            video_fps: self.video_fps.load(Ordering::Relaxed),
            video_dropped: self.video_dropped.load(Ordering::Relaxed),
            audio_real_frames: self.audio_real_frames.load(Ordering::Relaxed),
            audio_silent_frames: self.audio_silent_frames.load(Ordering::Relaxed),
            audio_queue_depth: self.audio_queue_depth.load(Ordering::Relaxed),
            audio_sync_error_ms: self.audio_sync_error_ms.load(Ordering::Relaxed),
            video_playout_slack_ms: self.video_playout_slack_ms.load(Ordering::Relaxed),
            av_skew_ms: self.av_skew_ms.load(Ordering::Relaxed),
            max_abs_av_skew_ms: self.max_abs_av_skew_ms.load(Ordering::Relaxed),
            timeline_correction_us: self.timeline_correction_us.load(Ordering::Relaxed),
            cpu_permille: self.cpu_permille.load(Ordering::Relaxed),
            working_set_bytes: self.working_set_bytes.load(Ordering::Relaxed),
            private_memory_bytes: self.private_memory_bytes.load(Ordering::Relaxed),
            reconnects: self.reconnects.load(Ordering::Relaxed),
        }
    }
}

#[cfg(target_os = "windows")]
pub fn process_cpu_time_100ns() -> Option<u64> {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetProcessTimes(
            process: *mut c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }

    let mut creation = FileTime::default();
    let mut exit = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    (ok != 0).then(|| {
        let to_u64 = |value: FileTime| (u64::from(value.high) << 32) | u64::from(value.low);
        to_u64(kernel).saturating_add(to_u64(user))
    })
}

#[cfg(target_os = "windows")]
pub fn process_memory_bytes() -> Option<(u64, u64)> {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Default)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
        private_usage: usize,
    }

    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetProcessMemoryInfo(
            process: *mut c_void,
            counters: *mut ProcessMemoryCounters,
            size: u32,
        ) -> i32;
    }

    let mut counters = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        ..Default::default()
    };
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    (ok != 0).then(|| {
        (
            counters.working_set_size as u64,
            counters.private_usage as u64,
        )
    })
}

#[cfg(not(target_os = "windows"))]
pub fn process_cpu_time_100ns() -> Option<u64> {
    None
}

#[cfg(not(target_os = "windows"))]
pub fn process_memory_bytes() -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::MirrorMetrics;

    #[test]
    fn snapshot_reads_all_runtime_counters() {
        let metrics = MirrorMetrics::default();
        metrics.video_frames.store(90, Ordering::Relaxed);
        metrics.video_fps.store(10, Ordering::Relaxed);
        metrics.audio_queue_depth.store(6, Ordering::Relaxed);
        metrics.audio_sync_error_ms.store(125, Ordering::Relaxed);
        metrics.av_skew_ms.store(7, Ordering::Relaxed);
        metrics.max_abs_av_skew_ms.store(18, Ordering::Relaxed);
        metrics.cpu_permille.store(750, Ordering::Relaxed);
        metrics
            .working_set_bytes
            .store(120_000_000, Ordering::Relaxed);
        metrics
            .private_memory_bytes
            .store(130_000_000, Ordering::Relaxed);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.video_frames, 90);
        assert_eq!(snapshot.video_fps, 10);
        assert_eq!(snapshot.audio_queue_depth, 6);
        assert_eq!(snapshot.audio_sync_error_ms, 125);
        assert_eq!(snapshot.av_skew_ms, 7);
        assert_eq!(snapshot.max_abs_av_skew_ms, 18);
        assert_eq!(snapshot.cpu_permille, 750);
        assert_eq!(snapshot.working_set_bytes, 120_000_000);
        assert_eq!(snapshot.private_memory_bytes, 130_000_000);
    }
}
