use std::time::{Duration, Instant};

/// Frame pacing to maintain target FPS.
pub struct FramePacer {
    start: Instant,
    frame_duration: Duration,
    frame_index: u64,
}

impl FramePacer {
    pub fn new(fps: u32) -> Self {
        let frame_duration = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
        let start = Instant::now();
        Self {
            start,
            frame_duration,
            frame_index: 0,
        }
    }

    pub async fn wait(&mut self) {
        self.frame_index = self.frame_index.saturating_add(1);
        let mut deadline = self.deadline(self.frame_index);
        let now = Instant::now();

        // Skip missed ticks instead of resetting the timeline, which otherwise
        // creates cumulative drift at 60 FPS.
        if deadline + self.frame_duration < now {
            let behind = now.duration_since(deadline);
            let missed = behind.as_nanos().div_ceil(self.frame_duration.as_nanos());
            self.frame_index = self.frame_index.saturating_add(missed as u64);
            deadline = self.deadline(self.frame_index);
        }

        if deadline > now {
            tokio::time::sleep(deadline - now).await;
        }
    }

    fn deadline(&self, frame_index: u64) -> Instant {
        self.start + self.frame_duration.mul_f64(frame_index as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::FramePacer;
    use std::time::{Duration, Instant};

    #[test]
    fn sixty_fps_deadlines_are_anchored_without_drift() {
        let start = Instant::now();
        let pacer = FramePacer {
            start,
            frame_duration: Duration::from_secs_f64(1.0 / 60.0),
            frame_index: 0,
        };
        let frame_60 = pacer.deadline(60);
        let expected = pacer.frame_duration * 60;
        assert!((frame_60 - start).abs_diff(expected) <= Duration::from_micros(2));
    }

    #[test]
    fn thirty_fps_uses_double_frame_spacing() {
        let pacer = FramePacer::new(30);
        assert_eq!(pacer.frame_duration, Duration::from_secs_f64(1.0 / 30.0));
    }
}
