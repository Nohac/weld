//! Bevy-free logical geometry shared by native compositor subsystems.

/// Axis-aligned rectangle in compositor-logical coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalRect {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl LogicalRect {
    pub const fn from_min_size(min_x: f64, min_y: f64, width: f64, height: f64) -> Self {
        Self {
            min_x,
            min_y,
            max_x: min_x + width,
            max_y: min_y + height,
        }
    }

    pub const fn min_x(self) -> f64 {
        self.min_x
    }

    pub const fn min_y(self) -> f64 {
        self.min_y
    }

    pub const fn max_x(self) -> f64 {
        self.max_x
    }

    pub const fn max_y(self) -> f64 {
        self.max_y
    }

    pub const fn width(self) -> f64 {
        self.max_x - self.min_x
    }

    pub const fn height(self) -> f64 {
        self.max_y - self.min_y
    }

    pub fn contains(self, x: f64, y: f64) -> bool {
        x >= self.min_x && x < self.max_x && y >= self.min_y && y < self.max_y
    }

    pub fn overlaps(self, other: Self) -> bool {
        self.min_x < other.max_x
            && self.max_x > other.min_x
            && self.min_y < other.max_y
            && self.max_y > other.min_y
    }

    pub fn clamp(self, x: f64, y: f64, edge_epsilon: f64) -> (f64, f64) {
        (
            x.clamp(self.min_x, (self.max_x - edge_epsilon).max(self.min_x)),
            y.clamp(self.min_y, (self.max_y - edge_epsilon).max(self.min_y)),
        )
    }
}
