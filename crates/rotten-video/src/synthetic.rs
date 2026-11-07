use rotten_core::error::Result;

/// Synthetic test pattern generator (no display server required).
pub struct SyntheticSource {
    width: u32,
    height: u32,
    frame: u64,
}

impl SyntheticSource {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            frame: 0,
        }
    }

    pub fn next_frame(&mut self) -> Result<(Vec<u8>, u32, u32)> {
        let w = self.width;
        let h = self.height;
        let mut rgba = vec![0u8; (w * h * 4) as usize];

        let t = (self.frame % 256) as u32;
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                rgba[i] = x.wrapping_mul(3).wrapping_add(t) as u8;
                rgba[i + 1] = y.wrapping_mul(5).wrapping_add(t) as u8;
                rgba[i + 2] = t as u8;
                rgba[i + 3] = 255;
            }
        }

        self.frame += 1;
        Ok((rgba, w, h))
    }
}
