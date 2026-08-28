//! Animation system — expressive motion, not CSS transitions.
//!
//! Every animated property is tracked by key (`node-id:property`). When the
//! layout resolves a new target for a key, a transition starts from the current
//! interpolated value. Springs use an integrated damped harmonic oscillator
//! (stateless deterministic integration, so no per-frame bookkeeping is lost).

use std::collections::HashMap;
use std::time::Instant;

use super::node::apply_easing;

#[derive(Debug, Clone)]
pub struct SpringParams {
    pub stiffness: f32,
    pub damping: f32,
    pub mass: f32,
}

impl SpringParams {
    /// Pleasant default: slight overshoot, settles in ~400 ms.
    pub fn default_params() -> Self {
        Self { stiffness: 170.0, damping: 22.0, mass: 1.0 }
    }
}

#[derive(Debug, Clone)]
pub enum Motion {
    Easing { from: f32, to: f32, duration_ms: u32, delay_ms: u32, easing: String, started: Instant },
    Spring { from: f32, to: f32, delay_ms: u32, params: SpringParams, started: Instant },
}

impl Motion {
    pub fn value(&self, now: Instant) -> f32 {
        let elapsed_ms = now.duration_since(self.started()).as_secs_f32() * 1000.0;
        match self {
            Motion::Easing { from, to, duration_ms, delay_ms, easing, .. } => {
                if elapsed_ms <= *delay_ms as f32 {
                    return *from;
                }
                let t = ((elapsed_ms - *delay_ms as f32) / *duration_ms as f32).clamp(0.0, 1.0);
                from + (to - from) * apply_easing(t, easing)
            }
            Motion::Spring { from, to, delay_ms, params, .. } => {
                if elapsed_ms <= *delay_ms as f32 {
                    return *from;
                }
                let t = (elapsed_ms - *delay_ms as f32).min(5000.0) / 1000.0;
                integrate_spring(*from, *to, params, t)
            }
        }
    }

    pub fn settled(&self, now: Instant) -> bool {
        let elapsed = now.duration_since(self.started()).as_secs_f32();
        match self {
            Motion::Easing { duration_ms, delay_ms, .. } => elapsed * 1000.0 >= (*delay_ms + *duration_ms) as f32,
            Motion::Spring { to, params, .. } => {
                if elapsed >= 5.0 {
                    return true;
                }
                let x = integrate_spring(self.start_value(), *to, params, elapsed);
                (x - *to).abs() < 0.01
            }
        }
    }

    fn started(&self) -> Instant {
        match self {
            Motion::Easing { started, .. } | Motion::Spring { started, .. } => *started,
        }
    }

    fn start_value(&self) -> f32 {
        match self {
            Motion::Easing { from, .. } | Motion::Spring { from, .. } => *from,
        }
    }
}

/// Deterministic spring integration: x'' = (-k(x-to) - c·x') / m
fn integrate_spring(from: f32, to: f32, p: &SpringParams, t: f32) -> f32 {
    const DT: f32 = 1.0 / 240.0;
    let (mut x, mut v) = (from, 0.0f32);
    let steps = (t / DT).ceil() as usize;
    for _ in 0..steps.min(1200) {
        let a = (-p.stiffness * (x - to) - p.damping * v) / p.mass;
        v += a * DT;
        x += v * DT;
    }
    x
}

#[derive(Debug, Clone)]
pub struct AnimatedValue {
    motion: Motion,
}

impl AnimatedValue {
    pub fn new(from: f32, to: f32, duration_ms: u32, delay_ms: u32, easing: &str) -> Self {
        Self {
            motion: Motion::Easing { from, to, duration_ms, delay_ms, easing: easing.into(), started: Instant::now() },
        }
    }
    pub fn spring(from: f32, to: f32, delay_ms: u32, params: SpringParams) -> Self {
        Self { motion: Motion::Spring { from, to, delay_ms, params, started: Instant::now() } }
    }
    pub fn value(&self) -> f32 {
        self.motion.value(Instant::now())
    }
    pub fn settled(&self) -> bool {
        self.motion.settled(Instant::now())
    }
    pub fn target(&self) -> f32 {
        match &self.motion {
            Motion::Easing { to, .. } | Motion::Spring { to, .. } => *to,
        }
    }
}

/// Tracks animated values across frames.
#[derive(Default)]
pub struct AnimationState {
    anims: HashMap<String, AnimatedValue>,
    /// Last resolved value per key (before animation) — used as the `from` for
    /// transitions when a new target arrives.
    current: HashMap<String, f32>,
}

impl AnimationState {
    /// Request the animated value for `key` toward `target`.
    ///
    /// * `from_override` — start value for the first appearance (enter animation),
    ///   e.g. `from = 0` for a fade-in.
    /// * `delay_ms` — final (already per-instance resolved) delay.
    #[allow(clippy::too_many_arguments)]
    pub fn animate(
        &mut self,
        key: &str,
        target: f32,
        duration_ms: u32,
        delay_ms: u32,
        easing: &str,
        spring: Option<super::theme::SpringDef>,
        from_override: Option<f32>,
    ) -> f32 {
        let eps = 0.01;
        let current = self.current.get(key).copied();
        let running = self.anims.get(key);

        // No animation needed: key unknown and no from_override, or target already reached.
        let needs_anim = match (running, current) {
            (Some(a), _) => (a.target() - target).abs() > eps,
            (None, Some(v)) => (v - target).abs() > eps,
            (None, None) => from_override.is_some(),
        };

        if needs_anim {
            let from = running
                .map(|a| a.value())
                .or(current)
                .unwrap_or_else(|| from_override.unwrap_or(target));
            let value = match spring {
                Some(s) => AnimatedValue::spring(from, target, delay_ms, SpringParams { stiffness: s.stiffness, damping: s.damping, mass: s.mass }),
                None if easing == "spring" => AnimatedValue::spring(from, target, delay_ms, SpringParams::default_params()),
                None => AnimatedValue::new(from, target, duration_ms, delay_ms, easing),
            };
            self.anims.insert(key.to_string(), value);
        }

        let v = self.anims.get(key).map(|a| a.value()).unwrap_or(target);
        self.current.insert(key.to_string(), v);
        v
    }

    /// Any transition still in motion? The window loop keeps ticking while true.
    pub fn is_active(&self) -> bool {
        self.anims.values().any(|a| !a.settled())
    }

    /// Remove settled animations (called each frame).
    pub fn tick(&mut self) {
        self.anims.retain(|_, a| !a.settled());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spring_overshoots_and_settles() {
        let p = SpringParams::default_params();
        let x05 = integrate_spring(0.0, 100.0, &p, 0.15);
        let x_final = integrate_spring(0.0, 100.0, &p, 3.0);
        assert!(x05 > 0.0, "spring moves toward target");
        assert!((x_final - 100.0).abs() < 0.1, "spring settles at target, got {x_final}");
    }

    #[test]
    fn easing_transitions_and_finish() {
        let mut st = AnimationState::default();
        let v1 = st.animate("sel:y", 100.0, 200, 0, "ease_out_cubic", None, None);
        assert_eq!(v1, 100.0); // first appearance without from_override: no anim
        let v2 = st.animate("sel:y", 200.0, 200, 0, "ease_out_cubic", None, None);
        assert!(v2 < 200.0, "starts from previous value");
        assert!(st.is_active());
    }

    #[test]
    fn enter_animation_from() {
        let mut st = AnimationState::default();
        let v = st.animate("row-0:opacity", 1.0, 300, 0, "ease_out_cubic", None, Some(0.0));
        assert!(v < 0.2, "enters from 0, got {v}");
        assert!(st.is_active());
    }
}
