//! # smooth_scroll
//! Global Lenis smooth scrolling physics engine (`duration = 1.2`, `lerp = 0.1`).
//!
//! ## Mathematical Model (Lenis / Studio Freight Parity)
//! - **Inertia Duration**: `DURATION = 1.2` seconds
//! - **Linear Interpolation Factor**: `LERP = 0.1` (at 60Hz reference frame rate)
//! - **Frame-rate Independent Damping**:
//!   $$\alpha(\Delta t) = 1 - (1 - 0.1)^{\Delta t \times 60}$$
//! - **Duration Envelope**:
//!   $$E(\tau) = \min\bigl(1.0,\; 1.001 - 2^{-10 \tau}\bigr), \quad \tau = \frac{t_{\text{elapsed}}}{1.2}$$
//! - **Zero Idle Polling**: The frame stepper runs only while at least one scroll axis has
//!   `is_animating == true` and immediately halts upon sub-pixel convergence (`< 0.1px`).

use std::time::Instant;

/// Canonical Lenis inertia duration in seconds.
pub const LENIS_DURATION_SEC: f64 = 1.2;

/// Canonical Lenis damping interpolation coefficient (`lerp = 0.1`).
pub const LENIS_LERP: f64 = 0.1;

/// Sub-pixel convergence threshold (in pixels) to snap and stop timer.
const CONVERGENCE_EPSILON_PX: f64 = 0.12;

/// Single 1D scroll axis governed by the Lenis (`duration = 1.2`, `lerp = 0.1`) model.
#[derive(Debug, Clone)]
pub struct LenisAxis {
    /// Current interpolated scroll offset in positive pixels (`[0.0, max_scroll]`).
    pub current: f64,
    /// Target scroll offset in positive pixels (`[0.0, max_scroll]`).
    pub target: f64,
    /// Maximum scrollable extent (`(content_extent - viewport_extent).max(0.0)`).
    pub max_scroll: f64,
    /// Elapsed time (seconds) since the last wheel/programmatic impulse.
    pub elapsed_sec: f64,
    /// Whether this axis is actively interpolating toward `target`.
    pub is_animating: bool,
}

impl Default for LenisAxis {
    fn default() -> Self {
        Self::new()
    }
}

impl LenisAxis {
    #[inline]
    pub const fn new() -> Self {
        Self {
            current: 0.0,
            target: 0.0,
            max_scroll: 0.0,
            elapsed_sec: 0.0,
            is_animating: false,
        }
    }

    /// Updates the maximum scrollable extent and clamps `current` and `target`.
    #[inline]
    pub fn set_bounds(&mut self, max_scroll: f64) {
        self.max_scroll = max_scroll.max(0.0);
        self.target = self.target.clamp(0.0, self.max_scroll);
        if !self.is_animating {
            self.current = self.current.clamp(0.0, self.max_scroll);
        }
    }

    /// Applies a wheel or touchpad delta impulse (where positive `wheel_delta_y` means scrolling up,
    /// so scroll offset decreases).
    #[inline]
    pub fn push_wheel_delta(&mut self, wheel_delta_px: f64, max_scroll: f64) {
        self.max_scroll = max_scroll.max(0.0);
        // Wheel delta from Slint: positive = wheel up (scroll toward 0), negative = wheel down (scroll toward max_scroll)
        let next_target = (self.target - wheel_delta_px * 1.15).clamp(0.0, self.max_scroll);
        self.target = next_target;
        self.elapsed_sec = 0.0;
        if (self.target - self.current).abs() >= CONVERGENCE_EPSILON_PX {
            self.is_animating = true;
        } else {
            self.current = self.target;
            self.is_animating = false;
        }
    }

    /// Smoothly animates toward a specific target offset using the Lenis curve.
    #[inline]
    pub fn smooth_scroll_to(&mut self, target_px: f64, max_scroll: f64) {
        self.max_scroll = max_scroll.max(0.0);
        self.target = target_px.clamp(0.0, self.max_scroll);
        self.elapsed_sec = 0.0;
        if (self.target - self.current).abs() >= CONVERGENCE_EPSILON_PX {
            self.is_animating = true;
        } else {
            self.current = self.target;
            self.is_animating = false;
        }
    }

    /// Immediately jumps to `pos_px` without inertia (e.g., when dragging scrollbar thumb directly
    /// or switching sessions).
    #[inline]
    pub fn jump_to(&mut self, pos_px: f64, max_scroll: f64) {
        self.max_scroll = max_scroll.max(0.0);
        let clamped = pos_px.clamp(0.0, self.max_scroll);
        self.current = clamped;
        self.target = clamped;
        self.elapsed_sec = 0.0;
        self.is_animating = false;
    }

    /// Advances the Lenis physics state by `dt_sec` seconds.
    /// Returns `Some(new_current_px)` if the position changed, or `None` if idle.
    #[inline]
    pub fn step(&mut self, dt_sec: f64) -> Option<f64> {
        if !self.is_animating {
            return None;
        }

        let dt = dt_sec.clamp(0.001, 0.1);
        self.elapsed_sec += dt;

        let diff = self.target - self.current;
        if diff.abs() < CONVERGENCE_EPSILON_PX || self.elapsed_sec >= LENIS_DURATION_SEC {
            self.current = self.target;
            self.is_animating = false;
            return Some(self.current);
        }

        // 1. Frame-rate independent exponential damping factor (`lerp = 0.1` @ 60fps)
        let damp_factor = 1.0 - (1.0 - LENIS_LERP).powf(dt * 60.0);

        // 2. Lenis duration=1.2s exponential decay envelope `min(1, 1.001 - 2^(-10 * t))`
        let progress = (self.elapsed_sec / LENIS_DURATION_SEC).clamp(0.0, 1.0);
        let envelope = (1.001 - 2.0_f64.powf(-10.0 * progress)).clamp(0.0, 1.0);

        // Blend primary lerp damping with terminal duration envelope so motion is both ultra-responsive
        // initially (`lerp = 0.1`) and guaranteed to settle smoothly within `duration = 1.2s`.
        let effective_alpha = damp_factor.max(envelope * dt * 8.5).clamp(0.0, 1.0);
        self.current += diff * effective_alpha;

        if (self.target - self.current).abs() < CONVERGENCE_EPSILON_PX {
            self.current = self.target;
            self.is_animating = false;
        }

        Some(self.current)
    }
}

/// Multi-axis Lenis smooth scroll manager for the entire client UI.
pub struct LenisScrollManager {
    pub chat: LenisAxis,
    pub sidebar: LenisAxis,
    pub input: LenisAxis,
    pub code_horizontal: LenisAxis,
    last_tick: Option<Instant>,
}

impl Default for LenisScrollManager {
    fn default() -> Self {
        Self::new()
    }
}

impl LenisScrollManager {
    pub const fn new() -> Self {
        Self {
            chat: LenisAxis::new(),
            sidebar: LenisAxis::new(),
            input: LenisAxis::new(),
            code_horizontal: LenisAxis::new(),
            last_tick: None,
        }
    }

    #[inline]
    pub fn any_active(&self) -> bool {
        self.chat.is_animating
            || self.sidebar.is_animating
            || self.input.is_animating
            || self.code_horizontal.is_animating
    }

    /// Marks the start of an animation cycle if currently idle.
    #[inline]
    pub fn touch_clock(&mut self) {
        if self.last_tick.is_none() {
            self.last_tick = Some(Instant::now());
        }
    }

    /// Steps all active axes using wall-clock delta time.
    pub fn step_all(&mut self) -> (Option<f32>, Option<f32>, Option<f32>, Option<f32>) {
        let now = Instant::now();
        let dt_sec = self
            .last_tick
            .map(|prev| now.duration_since(prev).as_secs_f64())
            .unwrap_or(1.0 / 60.0);
        self.last_tick = Some(now);

        let c = self.chat.step(dt_sec).map(|v| v as f32);
        let s = self.sidebar.step(dt_sec).map(|v| v as f32);
        let i = self.input.step(dt_sec).map(|v| v as f32);
        let h = self.code_horizontal.step(dt_sec).map(|v| v as f32);

        if !self.any_active() {
            self.last_tick = None;
        }

        (c, s, i, h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lenis_axis_monotonic_convergence_and_duration_bound() {
        let mut axis = LenisAxis::new();
        axis.push_wheel_delta(-240.0, 2000.0); // Scroll down by 240 * 1.15 = 276px
        assert!(axis.is_animating);
        assert!(axis.target > 270.0);

        // First 60Hz frame should advance by approximately 10% (`lerp = 0.1`)
        let first = axis.step(1.0 / 60.0).expect("Must step");
        assert!(
            (first - axis.target * 0.1).abs() < 1.0,
            "First step {} should be ~10% of {}",
            first,
            axis.target
        );

        let mut prev = first;
        let mut frames = 1;
        while axis.is_animating && frames < 100 {
            let next = axis.step(1.0 / 60.0).unwrap();
            assert!(next >= prev, "Must converge monotonically");
            prev = next;
            frames += 1;
        }

        assert!(
            !axis.is_animating,
            "Must finish animating within 1.2s (72 frames @ 60Hz), took {} frames",
            frames
        );
        assert!(frames <= 73);
        assert!((axis.current - axis.target).abs() < 1e-6);
    }

    #[test]
    fn test_lenis_frame_rate_independence_60hz_vs_120hz() {
        let mut axis_60 = LenisAxis::new();
        let mut axis_120 = LenisAxis::new();

        axis_60.smooth_scroll_to(500.0, 1000.0);
        axis_120.smooth_scroll_to(500.0, 1000.0);

        // Advance both by 0.25 seconds
        for _ in 0..15 {
            axis_60.step(1.0 / 60.0);
        }
        for _ in 0..30 {
            axis_120.step(1.0 / 120.0);
        }

        assert!(
            (axis_60.current - axis_120.current).abs() < 2.0,
            "60Hz ({}) and 120Hz ({}) positions after 0.25s must match closely",
            axis_60.current,
            axis_120.current
        );
    }
}
