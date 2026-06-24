//! An oscilloscope-style waveform animator for the agent-activity panel.
//!
//! Modelled on `scope-tui`'s amplitude scope: a flowing waveform rendered with
//! braille sub-cell points and a magenta→white gradient. It is driven by live
//! agent "energy" — a near-flat line when the system is idle, a rich, fast
//! scrolling waveform when agents are actively working — and is advanced every
//! render frame so motion is smooth.

pub struct Spectrum {
    phase: f32,
    energy: f32,
}

impl Default for Spectrum {
    fn default() -> Self {
        Self::new()
    }
}

impl Spectrum {
    pub fn new() -> Self {
        Self {
            phase: 0.0,
            energy: 0.0,
        }
    }

    /// Advance one frame. `target_energy` is overall agent activity in `0..=1`;
    /// `dt` is the frame time in seconds.
    pub fn tick(&mut self, target_energy: f32, dt: f32) {
        let dt = dt.clamp(0.0, 0.25);
        let target = target_energy.clamp(0.0, 1.0);
        // Ease toward the target so activity changes glide in.
        self.energy += (target - self.energy) * (1.0 - (-dt * 4.0).exp());
        // Scroll speed rises sharply with activity: near-still at idle, fast when busy.
        self.phase += dt * (0.4 + self.energy * 22.0);
    }

    pub fn energy(&self) -> f32 {
        self.energy
    }

    /// Waveform amplitude in `-1..=1` at horizontal column `i` of `width`.
    /// Scrolls right→left as the phase advances; flat when energy is ~0.
    pub fn sample(&self, i: usize, width: usize) -> f32 {
        let n = width.max(1) as f32;
        let t = (i as f32 / n) * 38.0 - self.phase;
        // Layered sines give a quasi-periodic, audio-like waveform.
        let wave = 0.55 * t.sin()
            + 0.28 * (t * 2.7 + 0.6).sin()
            + 0.20 * (t * 6.3).sin()
            + 0.12 * (t * 11.1).sin();
        // Deterministic jitter scatters the braille points for that dense texture.
        let jitter = pseudo_noise(t * 12.0) * 0.10;
        (self.energy * (wave + jitter)).clamp(-1.0, 1.0)
    }
}

/// Cheap deterministic value noise in `-1..=1`.
fn pseudo_noise(x: f32) -> f32 {
    let s = (x * 127.1).sin() * 43758.547;
    (s - s.floor()) * 2.0 - 1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_waveform_is_flat() {
        let s = Spectrum::new();
        for i in 0..50 {
            assert!(s.sample(i, 50).abs() < 1e-6, "idle should be flat");
        }
    }

    #[test]
    fn energy_produces_a_visible_waveform() {
        let mut s = Spectrum::new();
        for _ in 0..40 {
            s.tick(1.0, 0.05);
        }
        let visible = (0..80).any(|i| s.sample(i, 80).abs() > 0.1);
        assert!(visible, "active energy should produce a waveform");
    }

    #[test]
    fn samples_stay_in_unit_range() {
        let mut s = Spectrum::new();
        for _ in 0..60 {
            s.tick(1.0, 0.05);
        }
        for i in 0..120 {
            let v = s.sample(i, 120);
            assert!((-1.0..=1.0).contains(&v), "sample in range: {v}");
        }
    }
}
