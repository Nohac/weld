//! Native output identity, validated logical layouts, and physical pointer topology.

use std::str::FromStr;

use anyhow::{Context, Result, bail};

use crate::{
    geometry::LogicalRect,
    input::{InputDelta, InputPosition},
    runtime::OutputScaleAdjustment,
    surface::{Extent, LogicalPoint},
};

const LOGICAL_EDGE_EPSILON: f64 = 1.0 / 256.0;
const PHYSICAL_EDGE_EPSILON_MILLIMETERS: f64 = 1.0 / 1024.0;
const ASSUMED_PIXELS_PER_INCH: f64 = 96.0;

#[derive(Clone, Copy, Debug)]
struct PhysicalPoint {
    x_millimeters: f64,
    y_millimeters: f64,
}

impl PhysicalPoint {
    const fn new(x_millimeters: f64, y_millimeters: f64) -> Self {
        Self {
            x_millimeters,
            y_millimeters,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PhysicalDelta {
    x_millimeters: f64,
    y_millimeters: f64,
}

impl PhysicalDelta {
    const fn new(x_millimeters: f64, y_millimeters: f64) -> Self {
        Self {
            x_millimeters,
            y_millimeters,
        }
    }

    fn is_negligible(self) -> bool {
        self.x_millimeters.abs() <= f64::EPSILON && self.y_millimeters.abs() <= f64::EPSILON
    }
}

/// Stable identity for one output during a Weld process lifetime.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OutputId(u64);

impl OutputId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Physical dimensions reported by an output connector.
///
/// These values come from display metadata and are advisory: hardware may
/// omit them or report inaccurate measurements.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputPhysicalSize {
    width_millimeters: u32,
    height_millimeters: u32,
}

/// Origin of the physical dimensions used for output placement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputFootprintProvenance {
    /// Connector metadata supplied nonzero dimensions in millimeters.
    Measured,
    /// Missing connector metadata was replaced with a 96-DPI mode-derived size.
    Assumed96Dpi,
}

/// Scale-independent output placement in physical millimeters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputFootprint {
    x_millimeters: f64,
    y_millimeters: f64,
    width_millimeters: f64,
    height_millimeters: f64,
    provenance: OutputFootprintProvenance,
}

impl OutputFootprint {
    fn from_mode(extent: Extent, physical_size: Option<OutputPhysicalSize>) -> Self {
        let (width_millimeters, height_millimeters, provenance) = match physical_size {
            Some(size) => (
                f64::from(size.width_millimeters()),
                f64::from(size.height_millimeters()),
                OutputFootprintProvenance::Measured,
            ),
            None => (
                f64::from(extent.width) * 25.4 / ASSUMED_PIXELS_PER_INCH,
                f64::from(extent.height) * 25.4 / ASSUMED_PIXELS_PER_INCH,
                OutputFootprintProvenance::Assumed96Dpi,
            ),
        };
        Self {
            x_millimeters: 0.0,
            y_millimeters: 0.0,
            width_millimeters,
            height_millimeters,
            provenance,
        }
    }

    pub const fn x_millimeters(self) -> f64 {
        self.x_millimeters
    }

    pub const fn y_millimeters(self) -> f64 {
        self.y_millimeters
    }

    pub const fn width_millimeters(self) -> f64 {
        self.width_millimeters
    }

    pub const fn height_millimeters(self) -> f64 {
        self.height_millimeters
    }

    pub const fn provenance(self) -> OutputFootprintProvenance {
        self.provenance
    }

    fn max_x(self) -> f64 {
        self.x_millimeters + self.width_millimeters
    }

    fn max_y(self) -> f64 {
        self.y_millimeters + self.height_millimeters
    }

    fn overlaps(self, other: Self) -> bool {
        self.x_millimeters < other.max_x()
            && self.max_x() > other.x_millimeters
            && self.y_millimeters < other.max_y()
            && self.max_y() > other.y_millimeters
    }

    fn contains(self, point: PhysicalPoint) -> bool {
        point.x_millimeters >= self.x_millimeters
            && point.x_millimeters < self.max_x()
            && point.y_millimeters >= self.y_millimeters
            && point.y_millimeters < self.max_y()
    }

    fn clamp(self, point: PhysicalPoint) -> PhysicalPoint {
        PhysicalPoint::new(
            point.x_millimeters.clamp(
                self.x_millimeters,
                (self.max_x() - PHYSICAL_EDGE_EPSILON_MILLIMETERS).max(self.x_millimeters),
            ),
            point.y_millimeters.clamp(
                self.y_millimeters,
                (self.max_y() - PHYSICAL_EDGE_EPSILON_MILLIMETERS).max(self.y_millimeters),
            ),
        )
    }

    fn with_position(self, x_millimeters: f64, y_millimeters: f64) -> Result<Self> {
        if !x_millimeters.is_finite() || !y_millimeters.is_finite() {
            bail!("output footprint position must be finite");
        }
        Ok(Self {
            x_millimeters,
            y_millimeters,
            ..self
        })
    }
}

impl OutputPhysicalSize {
    /// Returns `None` when either reported dimension is zero.
    pub const fn new(width_millimeters: u32, height_millimeters: u32) -> Option<Self> {
        if width_millimeters == 0 || height_millimeters == 0 {
            None
        } else {
            Some(Self {
                width_millimeters,
                height_millimeters,
            })
        }
    }

    pub const fn width_millimeters(self) -> u32 {
        self.width_millimeters
    }

    pub const fn height_millimeters(self) -> u32 {
        self.height_millimeters
    }
}

/// Stable facts about one discovered output, separate from mutable layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputHead {
    id: OutputId,
    name: String,
    physical_size: Option<OutputPhysicalSize>,
}

impl OutputHead {
    pub fn new(
        id: OutputId,
        name: impl Into<String>,
        physical_size: Option<OutputPhysicalSize>,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            physical_size,
        }
    }

    pub const fn id(&self) -> OutputId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn physical_size(&self) -> Option<OutputPhysicalSize> {
        self.physical_size
    }
}

/// Valid logical scale applied to a physical compositor output.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputScale(f64);

impl OutputScale {
    const STEP: f64 = 0.25;

    /// Validates a finite, positive output scale.
    pub fn new(value: f64) -> Result<Self> {
        if !value.is_finite() || value <= 0.0 {
            bail!("output scale must be finite and positive");
        }
        Ok(Self(value))
    }

    /// Returns the validated scale factor.
    pub const fn value(self) -> f64 {
        self.0
    }

    /// Returns the next quarter-step scale in the requested direction.
    pub fn adjust(self, adjustment: OutputScaleAdjustment) -> Option<Self> {
        let next = match adjustment {
            OutputScaleAdjustment::Increase => ((self.0 / Self::STEP).floor() + 1.0) * Self::STEP,
            OutputScaleAdjustment::Decrease if self.0 <= Self::STEP => return None,
            OutputScaleAdjustment::Decrease => {
                (((self.0 / Self::STEP).ceil() - 1.0) * Self::STEP).max(Self::STEP)
            }
        };
        Self::new(next).ok().filter(|next| *next != self)
    }
}

impl Default for OutputScale {
    fn default() -> Self {
        Self(1.0)
    }
}

impl FromStr for OutputScale {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(
            value
                .parse::<f64>()
                .with_context(|| format!("invalid output scale {value:?}"))?,
        )
    }
}

/// Enabled output state shared with application policy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputConfiguration {
    id: OutputId,
    extent: Extent,
    scale: OutputScale,
    position: LogicalPoint,
    footprint: OutputFootprint,
    primary: bool,
}

impl OutputConfiguration {
    pub fn new(
        id: OutputId,
        extent: Extent,
        scale: OutputScale,
        position: LogicalPoint,
        primary: bool,
        physical_size: Option<OutputPhysicalSize>,
    ) -> Result<Self> {
        if extent.width == 0 || extent.height == 0 {
            bail!("output dimensions must be nonzero");
        }
        let configuration = Self {
            id,
            extent,
            scale,
            position,
            footprint: OutputFootprint::from_mode(extent, physical_size),
            primary,
        };
        configuration.validate()?;
        Ok(configuration)
    }

    fn validate(self) -> Result<()> {
        if self.logical_width() < 1.0 || self.logical_height() < 1.0 {
            bail!("output scale leaves less than one logical pixel on an axis");
        }
        if !f64::from(self.position.x).is_finite() || !f64::from(self.position.y).is_finite() {
            bail!("output position must be finite");
        }
        if !self.footprint.width_millimeters.is_finite()
            || !self.footprint.height_millimeters.is_finite()
            || self.footprint.width_millimeters <= 0.0
            || self.footprint.height_millimeters <= 0.0
        {
            bail!("output footprint dimensions must be finite and positive");
        }
        if !self.footprint.x_millimeters.is_finite() || !self.footprint.y_millimeters.is_finite() {
            bail!("output footprint position must be finite");
        }
        Ok(())
    }

    pub const fn id(self) -> OutputId {
        self.id
    }

    pub const fn extent(self) -> Extent {
        self.extent
    }

    pub const fn scale(self) -> OutputScale {
        self.scale
    }

    pub const fn position(self) -> LogicalPoint {
        self.position
    }

    pub const fn footprint(self) -> OutputFootprint {
        self.footprint
    }

    pub const fn is_primary(self) -> bool {
        self.primary
    }

    pub fn logical_width(self) -> f64 {
        f64::from(self.extent.width) / self.scale.value()
    }

    pub fn logical_height(self) -> f64 {
        f64::from(self.extent.height) / self.scale.value()
    }

    pub fn logical_rect(self) -> LogicalRect {
        LogicalRect::from_min_size(
            f64::from(self.position.x),
            f64::from(self.position.y),
            self.logical_width(),
            self.logical_height(),
        )
    }

    pub fn with_scale(self, scale: OutputScale) -> Result<Self> {
        let next = Self { scale, ..self };
        next.validate()?;
        Ok(next)
    }

    pub fn with_position(self, position: LogicalPoint) -> Result<Self> {
        let next = Self { position, ..self };
        next.validate()?;
        Ok(next)
    }

    pub fn with_footprint_position(self, x_millimeters: f64, y_millimeters: f64) -> Result<Self> {
        let next = Self {
            footprint: self.footprint.with_position(x_millimeters, y_millimeters)?,
            ..self
        };
        next.validate()?;
        Ok(next)
    }
}

/// Complete enabled layout at one host-loop revision.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputLayout {
    revision: u64,
    configurations: Vec<OutputConfiguration>,
}

impl OutputLayout {
    pub fn new(revision: u64, configurations: Vec<OutputConfiguration>) -> Result<Self> {
        validate_configurations(&configurations)?;
        Ok(Self {
            revision,
            configurations,
        })
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub fn configurations(&self) -> &[OutputConfiguration] {
        &self.configurations
    }

    pub fn configuration(&self, id: OutputId) -> Option<OutputConfiguration> {
        self.configurations
            .iter()
            .copied()
            .find(|configuration| configuration.id == id)
    }

    pub fn primary(&self) -> Option<OutputConfiguration> {
        self.configurations
            .iter()
            .copied()
            .find(|configuration| configuration.primary)
    }

    pub fn test_replacement(
        &self,
        base_revision: u64,
        configurations: Vec<OutputConfiguration>,
    ) -> Result<Self> {
        if base_revision != self.revision {
            bail!(
                "stale output layout revision {base_revision}; current revision is {}",
                self.revision
            );
        }
        Self::new(self.revision.saturating_add(1), configurations)
    }
}

fn validate_configurations(configurations: &[OutputConfiguration]) -> Result<()> {
    if configurations.is_empty() {
        bail!("an enabled output layout must contain at least one output");
    }
    if configurations
        .iter()
        .filter(|configuration| configuration.primary)
        .count()
        != 1
    {
        bail!("an output layout must contain exactly one primary output");
    }
    for (index, configuration) in configurations.iter().enumerate() {
        if configurations[..index]
            .iter()
            .any(|previous| previous.id == configuration.id)
        {
            bail!(
                "output layout contains duplicate output ID {:?}",
                configuration.id
            );
        }
        let rectangle = OutputRegion::from(*configuration).logical;
        if configurations[..index]
            .iter()
            .map(|previous| OutputRegion::from(*previous).logical)
            .any(|previous| previous.overlaps(rectangle))
        {
            bail!("enabled output rectangles must not overlap");
        }
        let footprint = configuration.footprint;
        if configurations[..index]
            .iter()
            .map(|previous| previous.footprint)
            .any(|previous| previous.overlaps(footprint))
        {
            bail!("enabled output footprints must not overlap");
        }
    }
    Ok(())
}

/// Output topology that resolves relative pointer motion against physical footprints.
///
/// Public pointer positions remain compositor-global logical coordinates. Each
/// relative move is projected into the active output's millimeter footprint,
/// resolved there, and projected back after collision and portal traversal.
#[derive(Clone, Debug)]
pub struct OutputTopology {
    layout: OutputLayout,
    regions: Vec<OutputRegion>,
}

impl OutputTopology {
    pub fn new(layout: OutputLayout) -> Self {
        let regions = layout
            .configurations()
            .iter()
            .copied()
            .map(OutputRegion::from)
            .collect();
        Self { layout, regions }
    }

    pub const fn layout(&self) -> &OutputLayout {
        &self.layout
    }

    pub fn configuration(&self, id: OutputId) -> Option<OutputConfiguration> {
        self.layout.configuration(id)
    }

    pub fn primary_configuration(&self) -> Option<OutputConfiguration> {
        self.layout.primary()
    }

    pub fn output_at(&self, position: InputPosition) -> Option<OutputId> {
        self.region_at(position).map(|region| region.output)
    }

    pub fn primary_position(&self, normalized_x: f64, normalized_y: f64) -> InputPosition {
        let Some(primary) = self.layout.primary().map(OutputRegion::from) else {
            return InputPosition::new(0.0, 0.0);
        };
        let position = InputPosition::new(
            primary.logical.min_x() + normalized_x.clamp(0.0, 1.0) * primary.logical.width(),
            primary.logical.min_y() + normalized_y.clamp(0.0, 1.0) * primary.logical.height(),
        );
        primary.clamp(position)
    }

    pub fn constrain(&self, position: InputPosition) -> InputPosition {
        if self.region_at(position).is_some() {
            return position;
        }
        self.regions
            .iter()
            .map(|rectangle| rectangle.clamp(position))
            .min_by(|left, right| {
                squared_distance(*left, position).total_cmp(&squared_distance(*right, position))
            })
            .unwrap_or(position)
    }

    pub fn move_pointer(&self, start: InputPosition, delta: InputDelta) -> InputPosition {
        let logical_position = self.constrain(start);
        let Some(mut active) = self
            .region_at(logical_position)
            .or_else(|| self.layout.primary().map(OutputRegion::from))
        else {
            return logical_position;
        };
        let mut position = active.logical_to_physical(logical_position);
        let mut remaining = active.logical_delta_to_physical(delta);
        let maximum_transitions = self.regions.len().saturating_mul(4).saturating_add(4);

        for _ in 0..maximum_transitions {
            if remaining.is_negligible() {
                break;
            }
            let candidate = PhysicalPoint::new(
                position.x_millimeters + remaining.x_millimeters,
                position.y_millimeters + remaining.y_millimeters,
            );
            if active.footprint.contains(candidate) {
                return active.physical_to_logical(candidate);
            }

            let vertical_time = boundary_time(
                position.x_millimeters,
                remaining.x_millimeters,
                active.footprint.x_millimeters,
                active.footprint.max_x(),
            );
            let horizontal_time = boundary_time(
                position.y_millimeters,
                remaining.y_millimeters,
                active.footprint.y_millimeters,
                active.footprint.max_y(),
            );
            let time = vertical_time.min(horizontal_time).clamp(0.0, 1.0);
            position = PhysicalPoint::new(
                position.x_millimeters + remaining.x_millimeters * time,
                position.y_millimeters + remaining.y_millimeters * time,
            );
            remaining = PhysicalDelta::new(
                remaining.x_millimeters * (1.0 - time),
                remaining.y_millimeters * (1.0 - time),
            );
            if remaining.is_negligible() {
                break;
            }

            let hit_vertical = (vertical_time - time).abs() <= f64::EPSILON;
            let hit_horizontal = (horizontal_time - time).abs() <= f64::EPSILON;

            if hit_vertical {
                if let Some(next) =
                    self.vertical_neighbor(active, position, remaining.x_millimeters)
                {
                    position = next.enter_vertical(position, remaining.x_millimeters);
                    active = next;
                } else if let Some((next, slide)) =
                    self.vertical_portal_in_path(active, position, remaining)
                {
                    position.y_millimeters += remaining.y_millimeters * slide;
                    remaining = PhysicalDelta::new(
                        remaining.x_millimeters * (1.0 - slide),
                        remaining.y_millimeters * (1.0 - slide),
                    );
                    position = next.enter_vertical(position, remaining.x_millimeters);
                    active = next;
                } else {
                    position.x_millimeters = if remaining.x_millimeters >= 0.0 {
                        active.footprint.max_x() - PHYSICAL_EDGE_EPSILON_MILLIMETERS
                    } else {
                        active.footprint.x_millimeters
                    };
                    remaining.x_millimeters = 0.0;
                }
                continue;
            }
            if hit_horizontal {
                if let Some(next) =
                    self.horizontal_neighbor(active, position, remaining.y_millimeters)
                {
                    position = next.enter_horizontal(position, remaining.y_millimeters);
                    active = next;
                } else if let Some((next, slide)) =
                    self.horizontal_portal_in_path(active, position, remaining)
                {
                    position.x_millimeters += remaining.x_millimeters * slide;
                    remaining = PhysicalDelta::new(
                        remaining.x_millimeters * (1.0 - slide),
                        remaining.y_millimeters * (1.0 - slide),
                    );
                    position = next.enter_horizontal(position, remaining.y_millimeters);
                    active = next;
                } else {
                    position.y_millimeters = if remaining.y_millimeters >= 0.0 {
                        active.footprint.max_y() - PHYSICAL_EDGE_EPSILON_MILLIMETERS
                    } else {
                        active.footprint.y_millimeters
                    };
                    remaining.y_millimeters = 0.0;
                }
            }
        }
        active.physical_to_logical(active.footprint.clamp(position))
    }

    fn region_at(&self, position: InputPosition) -> Option<OutputRegion> {
        self.regions
            .iter()
            .copied()
            .find(|rectangle| rectangle.contains(position))
    }

    fn vertical_neighbor(
        &self,
        current: OutputRegion,
        position: PhysicalPoint,
        direction: f64,
    ) -> Option<OutputRegion> {
        self.regions.iter().copied().find(|candidate| {
            let touches = if direction >= 0.0 {
                physically_equal(current.footprint.max_x(), candidate.footprint.x_millimeters)
            } else {
                physically_equal(current.footprint.x_millimeters, candidate.footprint.max_x())
            };
            touches
                && position.y_millimeters >= candidate.footprint.y_millimeters
                && position.y_millimeters < candidate.footprint.max_y()
        })
    }

    fn horizontal_neighbor(
        &self,
        current: OutputRegion,
        position: PhysicalPoint,
        direction: f64,
    ) -> Option<OutputRegion> {
        self.regions.iter().copied().find(|candidate| {
            let touches = if direction >= 0.0 {
                physically_equal(current.footprint.max_y(), candidate.footprint.y_millimeters)
            } else {
                physically_equal(current.footprint.y_millimeters, candidate.footprint.max_y())
            };
            touches
                && position.x_millimeters >= candidate.footprint.x_millimeters
                && position.x_millimeters < candidate.footprint.max_x()
        })
    }

    fn vertical_portal_in_path(
        &self,
        current: OutputRegion,
        position: PhysicalPoint,
        remaining: PhysicalDelta,
    ) -> Option<(OutputRegion, f64)> {
        self.regions
            .iter()
            .copied()
            .filter(|candidate| {
                if remaining.x_millimeters >= 0.0 {
                    physically_equal(current.footprint.max_x(), candidate.footprint.x_millimeters)
                } else {
                    physically_equal(current.footprint.x_millimeters, candidate.footprint.max_x())
                }
            })
            .filter_map(|candidate| {
                let (minimum, maximum) = current.vertical_portal(candidate)?;
                interval_entry_fraction(
                    position.y_millimeters,
                    remaining.y_millimeters,
                    minimum,
                    maximum,
                    PHYSICAL_EDGE_EPSILON_MILLIMETERS,
                )
                .map(|fraction| (candidate, fraction))
            })
            .min_by(|(_, left), (_, right)| left.total_cmp(right))
    }

    fn horizontal_portal_in_path(
        &self,
        current: OutputRegion,
        position: PhysicalPoint,
        remaining: PhysicalDelta,
    ) -> Option<(OutputRegion, f64)> {
        self.regions
            .iter()
            .copied()
            .filter(|candidate| {
                if remaining.y_millimeters >= 0.0 {
                    physically_equal(current.footprint.max_y(), candidate.footprint.y_millimeters)
                } else {
                    physically_equal(current.footprint.y_millimeters, candidate.footprint.max_y())
                }
            })
            .filter_map(|candidate| {
                let (minimum, maximum) = current.horizontal_portal(candidate)?;
                interval_entry_fraction(
                    position.x_millimeters,
                    remaining.x_millimeters,
                    minimum,
                    maximum,
                    PHYSICAL_EDGE_EPSILON_MILLIMETERS,
                )
                .map(|fraction| (candidate, fraction))
            })
            .min_by(|(_, left), (_, right)| left.total_cmp(right))
    }
}

#[derive(Clone, Copy, Debug)]
struct OutputRegion {
    output: OutputId,
    logical: LogicalRect,
    footprint: OutputFootprint,
}

impl OutputRegion {
    fn contains(self, position: InputPosition) -> bool {
        self.logical.contains(position.x, position.y)
    }

    fn clamp(self, position: InputPosition) -> InputPosition {
        let (x, y) = self
            .logical
            .clamp(position.x, position.y, LOGICAL_EDGE_EPSILON);
        InputPosition::new(x, y)
    }

    fn logical_to_physical(self, position: InputPosition) -> PhysicalPoint {
        PhysicalPoint::new(
            self.footprint.x_millimeters
                + (position.x - self.logical.min_x()) / self.logical.width()
                    * self.footprint.width_millimeters,
            self.footprint.y_millimeters
                + (position.y - self.logical.min_y()) / self.logical.height()
                    * self.footprint.height_millimeters,
        )
    }

    fn logical_delta_to_physical(self, delta: InputDelta) -> PhysicalDelta {
        PhysicalDelta::new(
            delta.x / self.logical.width() * self.footprint.width_millimeters,
            delta.y / self.logical.height() * self.footprint.height_millimeters,
        )
    }

    fn physical_to_logical(self, position: PhysicalPoint) -> InputPosition {
        self.clamp(InputPosition::new(
            self.logical.min_x()
                + (position.x_millimeters - self.footprint.x_millimeters)
                    / self.footprint.width_millimeters
                    * self.logical.width(),
            self.logical.min_y()
                + (position.y_millimeters - self.footprint.y_millimeters)
                    / self.footprint.height_millimeters
                    * self.logical.height(),
        ))
    }

    fn enter_vertical(self, position: PhysicalPoint, direction: f64) -> PhysicalPoint {
        let mut position = self.footprint.clamp(position);
        position.x_millimeters = if direction >= 0.0 {
            self.footprint.x_millimeters
        } else {
            self.footprint.max_x() - PHYSICAL_EDGE_EPSILON_MILLIMETERS
        };
        position
    }

    fn enter_horizontal(self, position: PhysicalPoint, direction: f64) -> PhysicalPoint {
        let mut position = self.footprint.clamp(position);
        position.y_millimeters = if direction >= 0.0 {
            self.footprint.y_millimeters
        } else {
            self.footprint.max_y() - PHYSICAL_EDGE_EPSILON_MILLIMETERS
        };
        position
    }

    fn vertical_portal(self, other: Self) -> Option<(f64, f64)> {
        let minimum = self
            .footprint
            .y_millimeters
            .max(other.footprint.y_millimeters);
        let maximum = self.footprint.max_y().min(other.footprint.max_y());
        (minimum < maximum).then_some((minimum, maximum))
    }

    fn horizontal_portal(self, other: Self) -> Option<(f64, f64)> {
        let minimum = self
            .footprint
            .x_millimeters
            .max(other.footprint.x_millimeters);
        let maximum = self.footprint.max_x().min(other.footprint.max_x());
        (minimum < maximum).then_some((minimum, maximum))
    }
}

impl From<OutputConfiguration> for OutputRegion {
    fn from(configuration: OutputConfiguration) -> Self {
        let left = f64::from(configuration.position.x);
        let top = f64::from(configuration.position.y);
        Self {
            output: configuration.id,
            logical: LogicalRect::from_min_size(
                left,
                top,
                configuration.logical_width(),
                configuration.logical_height(),
            ),
            footprint: configuration.footprint,
        }
    }
}

fn interval_entry_fraction(
    position: f64,
    delta: f64,
    minimum: f64,
    maximum: f64,
    epsilon: f64,
) -> Option<f64> {
    if position >= minimum && position < maximum {
        return Some(0.0);
    }
    if delta > 0.0 && position < minimum {
        let fraction = (minimum - position) / delta;
        (0.0..=1.0).contains(&fraction).then_some(fraction)
    } else if delta < 0.0 && position >= maximum {
        let fraction = (maximum - epsilon - position) / delta;
        (0.0..=1.0).contains(&fraction).then_some(fraction)
    } else {
        None
    }
}

fn boundary_time(position: f64, delta: f64, minimum: f64, maximum: f64) -> f64 {
    if delta > 0.0 {
        (maximum - position) / delta
    } else if delta < 0.0 {
        (minimum - position) / delta
    } else {
        f64::INFINITY
    }
}

fn physically_equal(left: f64, right: f64) -> bool {
    (left - right).abs() <= PHYSICAL_EDGE_EPSILON_MILLIMETERS
}

fn squared_distance(left: InputPosition, right: InputPosition) -> f64 {
    let x = left.x - right.x;
    let y = left.y - right.y;
    x * x + y * y
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configuration(
        id: u64,
        width: u32,
        height: u32,
        scale: f64,
        x: f32,
        y: f32,
        primary: bool,
    ) -> OutputConfiguration {
        OutputConfiguration::new(
            OutputId::new(id),
            Extent::new(width, height),
            OutputScale::new(scale).expect("valid scale"),
            LogicalPoint::new(x, y),
            primary,
            None,
        )
        .expect("valid output")
    }

    fn unequal_topology() -> OutputTopology {
        let primary = configuration(1, 2240, 1400, 1.25, 0.0, 0.0, true);
        let secondary = configuration(2, 1920, 1080, 1.0, 1792.0, 0.0, false)
            .with_footprint_position(primary.footprint().width_millimeters(), 0.0)
            .expect("physical outputs should be adjacent");
        OutputTopology::new(OutputLayout::new(1, vec![primary, secondary]).expect("valid layout"))
    }

    #[test]
    fn stale_layout_replacement_is_rejected_without_changing_the_layout() {
        let layout = unequal_topology().layout().clone();
        assert!(
            layout
                .test_replacement(0, layout.configurations().to_vec())
                .is_err()
        );
        assert_eq!(layout.revision(), 1);
    }

    #[test]
    fn invalid_layout_replacement_leaves_the_current_layout_unchanged() {
        let layout = unequal_topology().layout().clone();
        let original = layout.configurations().to_vec();
        let invalid = vec![
            configuration(1, 800, 600, 1.0, 0.0, 0.0, true),
            configuration(2, 800, 600, 1.0, 400.0, 0.0, false),
        ];

        assert!(layout.test_replacement(layout.revision(), invalid).is_err());
        assert_eq!(layout.configurations(), original);
        assert_eq!(layout.revision(), 1);
    }

    #[test]
    fn physical_overlap_is_rejected_even_when_logical_rectangles_are_disjoint() {
        let configurations = vec![
            configuration(1, 800, 600, 1.0, 0.0, 0.0, true),
            configuration(2, 800, 600, 1.0, 900.0, 0.0, false),
        ];

        assert!(OutputLayout::new(1, configurations).is_err());
    }

    #[test]
    fn assumed_footprint_is_mode_derived_and_scale_independent() {
        let output = configuration(1, 1_920, 1_080, 1.0, 0.0, 0.0, true);
        let scaled = output
            .with_scale(OutputScale::new(2.0).expect("valid scale"))
            .expect("scale should preserve a valid footprint");

        assert_eq!(output.footprint(), scaled.footprint());
        assert_eq!(
            output.footprint().provenance(),
            OutputFootprintProvenance::Assumed96Dpi
        );
    }

    #[test]
    fn assumed_footprints_support_bidirectional_pointer_crossing() {
        let topology = unequal_topology();
        let secondary = topology.move_pointer(
            InputPosition::new(1_790.0, 500.0),
            InputDelta::new(20.0, 0.0),
        );
        assert_eq!(topology.output_at(secondary), Some(OutputId::new(2)));

        let primary = topology.move_pointer(secondary, InputDelta::new(-40.0, 0.0));
        assert_eq!(topology.output_at(primary), Some(OutputId::new(1)));
    }

    #[test]
    fn remaining_motion_keeps_its_physical_distance_across_mixed_scale_outputs() {
        let physical_size = OutputPhysicalSize::new(100, 100);
        let upper = OutputConfiguration::new(
            OutputId::new(1),
            Extent::new(1_000, 1_000),
            OutputScale::new(1.0).expect("valid scale"),
            LogicalPoint::ZERO,
            true,
            physical_size,
        )
        .expect("valid upper output");
        let lower = OutputConfiguration::new(
            OutputId::new(2),
            Extent::new(1_000, 1_000),
            OutputScale::new(2.0).expect("valid scale"),
            LogicalPoint::new(0.0, 1_000.0),
            false,
            physical_size,
        )
        .expect("valid lower output")
        .with_footprint_position(0.0, 100.0)
        .expect("physical outputs should be adjacent");
        let topology = OutputTopology::new(
            OutputLayout::new(1, vec![upper, lower]).expect("valid mixed-scale layout"),
        );

        let moved = topology.move_pointer(
            InputPosition::new(500.0, 900.0),
            InputDelta::new(0.0, 200.0),
        );

        assert_eq!(topology.output_at(moved), Some(OutputId::new(2)));
        assert!((moved.x - 250.0).abs() < 0.01);
        assert!((moved.y - 1_050.0).abs() < 0.01);
    }

    #[test]
    fn pointer_crosses_only_the_shared_edge_interval() {
        let topology = unequal_topology();
        let crossed = topology.move_pointer(
            InputPosition::new(1790.0, 500.0),
            InputDelta::new(10.0, 0.0),
        );
        assert!(crossed.x > 1792.0);
        assert_eq!(topology.output_at(crossed), Some(OutputId::new(2)));

        let blocked = topology.move_pointer(
            InputPosition::new(1790.0, 1100.0),
            InputDelta::new(10.0, 0.0),
        );
        assert!(blocked.x < 1792.0);
        assert_eq!(topology.output_at(blocked), Some(OutputId::new(1)));
    }

    #[test]
    fn diagonal_motion_slides_along_an_exposed_edge_toward_a_portal() {
        let topology = unequal_topology();
        let moved = topology.move_pointer(
            InputPosition::new(1780.0, 1110.0),
            InputDelta::new(100.0, -400.0),
        );
        assert_eq!(topology.output_at(moved), Some(OutputId::new(2)));
        assert!(moved.y < 1080.0);
    }

    #[test]
    fn a_gap_does_not_create_a_pointer_portal() {
        let primary = configuration(1, 800, 600, 1.0, 0.0, 0.0, true);
        let secondary = configuration(2, 800, 600, 1.0, 900.0, 0.0, false)
            .with_footprint_position(primary.footprint().width_millimeters() + 10.0, 0.0)
            .expect("physical gap should be valid");
        let topology = OutputTopology::new(
            OutputLayout::new(1, vec![primary, secondary]).expect("valid layout"),
        );
        let moved = topology.move_pointer(
            InputPosition::new(790.0, 300.0),
            InputDelta::new(500.0, 0.0),
        );
        assert_eq!(topology.output_at(moved), Some(OutputId::new(1)));
    }

    #[test]
    fn primary_absolute_position_includes_its_layout_origin() {
        let secondary = configuration(2, 800, 600, 1.0, 0.0, 0.0, false);
        let primary = configuration(1, 800, 600, 1.0, 0.0, 600.0, true)
            .with_footprint_position(0.0, secondary.footprint().height_millimeters())
            .expect("physical outputs should stack vertically");
        let topology = OutputTopology::new(
            OutputLayout::new(1, vec![primary, secondary]).expect("valid vertical layout"),
        );

        assert_eq!(
            topology.primary_position(0.5, 0.5),
            InputPosition::new(400.0, 900.0)
        );
    }

    #[test]
    fn constrain_rehomes_a_position_after_scale_shrinks_logical_bounds() {
        let topology = OutputTopology::new(
            OutputLayout::new(2, vec![configuration(1, 1_000, 800, 2.0, 0.0, 0.0, true)])
                .expect("valid scaled layout"),
        );

        let constrained = topology.constrain(InputPosition::new(900.0, 700.0));

        assert_eq!(topology.output_at(constrained), Some(OutputId::new(1)));
        assert!(constrained.x < 500.0);
        assert!(constrained.y < 400.0);
    }
}
