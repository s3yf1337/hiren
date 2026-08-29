//! Scene graph — resolved draw commands produced by layout resolution.
//!
//! A `ResolvedNode` is fully concrete (no bindings left): the renderer and the
//! input router work with these only. This is the boundary that keeps the
//! launcher UI replaceable — nothing downstream knows where values came from.

use super::theme::AnimateDef;
use super::color::Color;
use std::collections::HashMap;

/// Action attached to a node by the theme's `on_click` binding.
/// The minimal coherent interaction surface between UI and launcher.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeAction {
    /// Launch/activate: explicit index, or the node's repeater index, or current selection.
    Activate(Option<usize>),
    /// Change selection without launching.
    Select(Option<usize>),
    /// Move selection by delta (keyboard-style).
    Move(i32),
    /// Replace the query text.
    SetQuery(String),
    /// Close the launcher.
    Close,
}

impl NodeAction {
    /// If this action targets a specific result index, return it (used for hover).
    pub fn targeted_index(&self) -> Option<usize> {
        match self {
            NodeAction::Activate(i) | NodeAction::Select(i) => *i,
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedNode {
    pub id: String,
    pub kind: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub opacity: f32,
    pub z: i32,
    pub visible: bool,
    pub props: HashMap<String, String>,
    /// For text nodes: resolved text.
    pub text: Option<String>,
    /// Text color.
    pub color: Option<Color>,
    /// Solid background color (gradients stay in `props`).
    pub background: Option<Color>,
    pub radius: f32,
    /// Rotation in degrees around the node center.
    pub rotation: f32,
    /// Horizontal shear in degrees around the node center (parallelogram lean).
    pub skew: f32,
    /// Uniform scale factor around the node center.
    pub scale: f32,
    /// Polygon vertices in node-local coordinates (`Polygon` kind only).
    pub points: Vec<(f32, f32)>,
    /// Animation declarations (with per-instance delays already resolved).
    pub animate: Vec<AnimateDef>,
    /// Optional scissor rect (x, y, w, h) the node is clipped to (repeater
    /// viewports). Rendering and hit-testing both honor it.
    pub clip: Option<(f32, f32, f32, f32)>,
    /// Click action, if the theme attached one.
    pub action: Option<NodeAction>,
    /// Repeater instance index (for actions and stagger).
    pub index: Option<usize>,
}

impl ResolvedNode {
    /// Untransformed bounding rect (center).
    pub fn rect(&self) -> tiny_skia::Rect {
        tiny_skia::Rect::from_xywh(self.x, self.y, self.width, self.height)
            .unwrap_or_else(|| tiny_skia::Rect::from_xywh(0.0, 0.0, 1.0, 1.0).unwrap())
    }

    /// Whether the point hits this node, inverse-transformed for
    /// skew/rotation/scale (polygon nodes test point-in-polygon).
    pub fn hit(&self, px: f64, py: f64) -> bool {
        let (mut x, mut y) = (px as f32, py as f32);
        let (cx, cy) = (self.x + self.width / 2.0, self.y + self.height / 2.0);
        if (self.scale - 1.0).abs() > 1e-4 && self.scale.abs() > 1e-4 {
            x = cx + (x - cx) / self.scale;
            y = cy + (y - cy) / self.scale;
        }
        if self.rotation.abs() > 1e-4 {
            let rad = -self.rotation.to_radians();
            let (dx, dy) = (x - cx, y - cy);
            x = cx + dx * rad.cos() - dy * rad.sin();
            y = cy + dx * rad.sin() + dy * rad.cos();
        }
        if self.skew.abs() > 1e-4 {
            // inverse of x' = x + tan(skew)·y about the center
            let k = self.skew.to_radians().tan();
            let (lx, ly) = (x - cx, y - cy);
            x = cx + (lx - k * ly);
        }
        let mut inside = x >= self.x && x <= self.x + self.width && y >= self.y && y <= self.y + self.height;
        if inside && !self.points.is_empty() {
            inside = point_in_polygon(x - self.x, y - self.y, &self.points);
        }
        if !inside {
            return false;
        }
        match self.clip {
            Some((cx, cy, cw, ch)) => px >= cx as f64 && px <= (cx + cw) as f64 && py >= cy as f64 && py <= (cy + ch) as f64,
            None => true,
        }
    }
}

// ---------------------------------------------------------------------------
// Geometry helpers
// ---------------------------------------------------------------------------

/// Even-odd ray-cast test in polygon-local coordinates.
fn point_in_polygon(px: f32, py: f32, pts: &[(f32, f32)]) -> bool {
    if pts.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = pts.len() - 1;
    for i in 0..pts.len() {
        let (xi, yi) = pts[i];
        let (xj, yj) = pts[j];
        if (yi > py) != (yj > py) && px < (xj - xi) * (py - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

// ---------------------------------------------------------------------------
// Easing helpers (used by the animation system)
// ---------------------------------------------------------------------------

pub fn apply_easing(t: f32, easing: &str) -> f32 {
    let t = t.clamp(0.0, 1.0);
    match easing {
        "linear" => t,
        "ease_in_quad" => t * t,
        "ease_out_quad" => 1.0 - (1.0 - t) * (1.0 - t),
        "ease_in_out_quad" => {
            if t < 0.5 { 2.0 * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(2) / 2.0 }
        }
        "ease_in_cubic" => t.powi(3),
        "ease_out_cubic" => 1.0 - (1.0 - t).powi(3),
        "ease_in_out_cubic" => {
            if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
        }
        "ease_out_quart" => 1.0 - (1.0 - t).powi(4),
        "ease_out_expo" => {
            if t >= 1.0 { 1.0 } else { 1.0 - 2.0f32.powf(-10.0 * t) }
        }
        "ease_out_back" => {
            let c1 = 1.70158;
            let c3 = c1 + 1.0;
            1.0 + c3 * (t - 1.0).powi(3) + c1 * (t - 1.0).powi(2)
        }
        "ease_out_elastic" => {
            if t <= 0.0 {
                0.0
            } else if t >= 1.0 {
                1.0
            } else {
                let c4 = std::f32::consts::TAU / 3.0;
                2.0f32.powf(-10.0 * t) * ((t * 10.0 - 0.75) * c4).sin() + 1.0
            }
        }
        _ => 1.0 - (1.0 - t).powi(3), // default ease_out_cubic
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easing_bounds() {
        for e in ["linear", "ease_out_cubic", "ease_out_back", "ease_out_elastic", "ease_in_out_quad"] {
            assert!((apply_easing(0.0, e)).abs() < 1e-6, "{e} at 0");
            assert!((apply_easing(1.0, e) - 1.0).abs() < 1e-6, "{e} at 1");
        }
    }

    #[test]
    fn hit_with_rotation() {
        let n = ResolvedNode {
            id: "n".into(), kind: "Rectangle".into(),
            x: 100.0, y: 100.0, width: 100.0, height: 50.0,
            opacity: 1.0, z: 0, visible: true, props: HashMap::new(),
            text: None, color: None, background: None, radius: 0.0,
            rotation: 0.0, skew: 0.0, scale: 1.0, points: vec![],
            animate: vec![], action: None, index: None,
            clip: None,
        };
        assert!(n.hit(120.0, 110.0));
        assert!(!n.hit(50.0, 50.0));
        // rotated 90° (CCW): region moves; a local point (160,105) maps to (170,135)
        let mut r = n.clone();
        r.rotation = 90.0;
        assert!(r.hit(170.0, 135.0), "rotated rect covers forward-mapped point");
        assert!(!r.hit(120.0, 110.0), "old top-left corner is outside after rotation");
        assert!(r.hit(150.0, 125.0), "center still inside");
    }
}
