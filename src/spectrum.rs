//! A spectroscope-style bar animator for the agent-activity panel.
//!
//! Bars rise quickly toward a target derived from live agent "energy" and fall
//! with gravity, with floating peak caps — like a classic audio EQ visualizer.
//! It is ticked every render frame (not just on data refresh) so the spectrum
//! stays alive and dances even between the ~1s snapshot updates.

/// A fixed, generous bar count; the renderer samples the first `width` of these.
pub const BARS: usize = 256;

pub struct Spectrum {
    heights: Vec<f32>,
    peaks: Vec<f32>,
    phase: f32,
    energy: f32,
}

impl Spectrum {
    pub fn new(bars: usize) -> Self {
        let bars = bars.max(1);
        Self {
            heights: vec![0.0; bars],
            peaks: vec![0.0; bars],
            phase: 0.0,
            energy: 0.0,
        }
    }

    pub fn bars(&self) -> usize {
        self.heights.len()
    }

    /// Advance one frame. `target_energy` is overall agent activity in `0..=1`;
    /// `dt` is the frame time in seconds.
    pub fn tick(&mut self, target_energy: f32, dt: f32) {
        let dt = dt.clamp(0.0, 0.25);
        let target_energy = target_energy.clamp(0.0, 1.0);

        // Smoothly approach the target energy so jumps in activity ease in.
        self.energy += (target_energy - self.energy) * (1.0 - (-dt * 5.0).exp());
        // Phase scrolls faster when the swarm is busy.
        self.phase += dt * (0.6 + self.energy * 3.0);

        let n = self.heights.len() as f32;
        for i in 0..self.heights.len() {
            let x = i as f32;
            // A slow swell times a faster "bar" component: neighbouring bars differ
            // and valleys open up between them like a classic EQ.
            let a = 0.55 + 0.45 * (x * 0.27 + self.phase).sin();
            let b = 0.50 + 0.50 * (x * 0.85 - self.phase * 0.5).sin();
            // Low "frequencies" (left) carry a touch more energy, like real audio.
            let env = 1.0 - 0.4 * (x / n);
            let target = (self.energy * a * b * env).clamp(0.0, 1.0);

            if target > self.heights[i] {
                // Snap up toward the target.
                self.heights[i] += (target - self.heights[i]) * (1.0 - (-dt * 20.0).exp());
            } else {
                // Fall under gravity.
                self.heights[i] = (self.heights[i] - dt * 0.9).max(0.0);
            }

            if self.heights[i] > self.peaks[i] {
                self.peaks[i] = self.heights[i];
            } else {
                // Peak caps drift down slowly, never below the bar.
                self.peaks[i] = (self.peaks[i] - dt * 0.45).max(self.heights[i]);
            }
        }
    }

    pub fn heights(&self) -> &[f32] {
        &self.heights
    }

    pub fn peaks(&self) -> &[f32] {
        &self.peaks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn energy_raises_bars_over_time() {
        let mut s = Spectrum::new(64);
        for _ in 0..30 {
            s.tick(1.0, 0.05);
        }
        let max = s.heights().iter().cloned().fold(0.0_f32, f32::max);
        assert!(max > 0.2, "sustained energy should raise some bars, got {max}");
    }

    #[test]
    fn bars_decay_toward_zero_without_energy() {
        let mut s = Spectrum::new(64);
        for _ in 0..20 {
            s.tick(1.0, 0.05);
        }
        for _ in 0..200 {
            s.tick(0.0, 0.05);
        }
        let max = s.heights().iter().cloned().fold(0.0_f32, f32::max);
        assert!(max < 0.05, "bars should fall to ~0 under gravity, got {max}");
    }

    #[test]
    fn heights_stay_in_unit_range_and_peaks_lead() {
        let mut s = Spectrum::new(64);
        for _ in 0..50 {
            s.tick(0.8, 0.05);
        }
        for (h, p) in s.heights().iter().zip(s.peaks().iter()) {
            assert!((0.0..=1.0).contains(h), "height in range: {h}");
            assert!(*p >= *h - 1e-6, "peak >= height: {p} vs {h}");
        }
    }
}
