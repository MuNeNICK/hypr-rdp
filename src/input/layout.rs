use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::display::geometry::{PresentationGeometry, Size};

#[derive(Clone, Debug)]
pub(crate) struct OutputLayoutSnapshot {
    pub(crate) output_name: String,
    pub(crate) output_w: u32,
    pub(crate) output_h: u32,
    pub(crate) layout_extent_w: u32,
    pub(crate) layout_extent_h: u32,
    pub(crate) output_offset_x: u32,
    pub(crate) output_offset_y: u32,
    pub(crate) presentation_geometry: PresentationGeometry,
    pub(crate) geometry_generation: u32,
}

/// Input-owned validated layout and geometry, prepared without shared state.
///
/// The geometry generation is intentionally absent: it is computed against the
/// currently committed snapshot at publication time by
/// [`SharedOutputLayout::apply_prepared`].
#[derive(Clone, Debug)]
pub(crate) struct PreparedOutputLayout {
    output_name: String,
    output_w: u32,
    output_h: u32,
    layout_extent_w: u32,
    layout_extent_h: u32,
    output_offset_x: u32,
    output_offset_y: u32,
    presentation_geometry: PresentationGeometry,
}

#[derive(Debug, Default)]
pub struct SharedOutputLayout {
    inner: Mutex<Option<OutputLayoutSnapshot>>,
}

impl SharedOutputLayout {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update_from_output(&self, output_name: &str) -> Result<()> {
        let (
            output_w,
            output_h,
            layout_extent_w,
            layout_extent_h,
            output_offset_x,
            output_offset_y,
        ) = query_layout(output_name)?;
        let prepared = Self::prepare_from_layout_query(
            output_name,
            output_w,
            output_h,
            layout_extent_w,
            layout_extent_h,
            output_offset_x,
            output_offset_y,
            (output_w, output_h),
        )?;
        self.apply_prepared(prepared);
        Ok(())
    }

    pub fn update_from_output_with_presentation(
        &self,
        output_name: &str,
        presentation: (u32, u32),
    ) -> Result<()> {
        let prepared = Self::prepare_from_output_with_presentation(output_name, presentation)?;
        self.apply_prepared(prepared);
        Ok(())
    }

    /// Query and validate the monitor layout without mutating shared state.
    ///
    /// The returned value is input-owned and can be prepared off the async
    /// worker; publication happens later through [`Self::apply_prepared`].
    pub(crate) fn prepare_from_output_with_presentation(
        output_name: &str,
        presentation: (u32, u32),
    ) -> Result<PreparedOutputLayout> {
        let (
            output_w,
            output_h,
            layout_extent_w,
            layout_extent_h,
            output_offset_x,
            output_offset_y,
        ) = query_layout(output_name)?;
        Self::prepare_from_layout_query(
            output_name,
            output_w,
            output_h,
            layout_extent_w,
            layout_extent_h,
            output_offset_x,
            output_offset_y,
            presentation,
        )
    }

    /// Publish a prepared layout and advance the geometry generation.
    ///
    /// The generation is computed against the currently committed snapshot
    /// under a single lock, so a stale prepared value cannot overwrite a newer
    /// committed state with a stale generation. Reapplying the same geometry
    /// keeps the existing generation.
    pub(crate) fn apply_prepared(&self, prepared: PreparedOutputLayout) {
        let snapshot = match self.inner.lock() {
            Ok(mut guard) => {
                let geometry_generation = guard
                    .as_ref()
                    .map(|old| {
                        if old.presentation_geometry == prepared.presentation_geometry {
                            old.geometry_generation
                        } else {
                            old.geometry_generation.saturating_add(1)
                        }
                    })
                    .unwrap_or(0);
                let snapshot = OutputLayoutSnapshot {
                    output_name: prepared.output_name,
                    output_w: prepared.output_w,
                    output_h: prepared.output_h,
                    layout_extent_w: prepared.layout_extent_w,
                    layout_extent_h: prepared.layout_extent_h,
                    output_offset_x: prepared.output_offset_x,
                    output_offset_y: prepared.output_offset_y,
                    presentation_geometry: prepared.presentation_geometry,
                    geometry_generation,
                };
                *guard = Some(snapshot.clone());
                snapshot
            }
            Err(_) => return,
        };
        tracing::info!(
            output = %snapshot.output_name,
            source_w = snapshot.output_w,
            source_h = snapshot.output_h,
            presentation_w = snapshot.presentation_geometry.presentation().width,
            presentation_h = snapshot.presentation_geometry.presentation().height,
            geometry_generation = snapshot.geometry_generation,
            layout_extent_w = snapshot.layout_extent_w,
            layout_extent_h = snapshot.layout_extent_h,
            output_offset_x = snapshot.output_offset_x,
            output_offset_y = snapshot.output_offset_y,
            "Updated input layout mapping"
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_from_layout_query(
        output_name: &str,
        output_w: u32,
        output_h: u32,
        layout_extent_w: u32,
        layout_extent_h: u32,
        output_offset_x: u32,
        output_offset_y: u32,
        presentation: (u32, u32),
    ) -> Result<PreparedOutputLayout> {
        let source = Size::new(output_w, output_h).context("output has invalid source size")?;
        let presentation = Size::new(presentation.0, presentation.1)
            .context("output has invalid presentation size")?;
        let presentation_geometry = PresentationGeometry::new(source, presentation);
        Ok(PreparedOutputLayout {
            output_name: output_name.to_string(),
            output_w,
            output_h,
            layout_extent_w,
            layout_extent_h,
            output_offset_x,
            output_offset_y,
            presentation_geometry,
        })
    }

    pub(crate) fn snapshot(&self) -> Option<OutputLayoutSnapshot> {
        self.inner.lock().ok()?.clone()
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_snapshot_for_test(
        output_name: &str,
        output_w: u32,
        output_h: u32,
        layout_extent_w: u32,
        layout_extent_h: u32,
        output_offset_x: u32,
        output_offset_y: u32,
        presentation: (u32, u32),
    ) -> Result<PreparedOutputLayout> {
        Self::prepare_from_layout_query(
            output_name,
            output_w,
            output_h,
            layout_extent_w,
            layout_extent_h,
            output_offset_x,
            output_offset_y,
            presentation,
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update_snapshot_for_test(
        &self,
        output_name: &str,
        output_w: u32,
        output_h: u32,
        layout_extent_w: u32,
        layout_extent_h: u32,
        output_offset_x: u32,
        output_offset_y: u32,
        presentation: (u32, u32),
    ) -> Result<()> {
        let prepared = Self::prepare_snapshot_for_test(
            output_name,
            output_w,
            output_h,
            layout_extent_w,
            layout_extent_h,
            output_offset_x,
            output_offset_y,
            presentation,
        )?;
        self.apply_prepared(prepared);
        Ok(())
    }
}

/// Query Hyprland monitor layout to compute coordinate mapping.
/// Returns (layout_total_w, layout_total_h, output_offset_x, output_offset_y)
/// Returns (output_w, output_h, layout_total_w, layout_total_h, output_offset_x, output_offset_y)
fn query_layout(output_name: &str) -> Result<(u32, u32, u32, u32, u32, u32)> {
    let monitors_val = crate::hyprland::monitors()?;
    layout_from_monitors(&monitors_val, output_name)
}

fn required_i64(monitor: &Value, field: &str) -> Result<i64> {
    monitor
        .get(field)
        .and_then(Value::as_i64)
        .with_context(|| format!("monitor has missing or invalid '{field}'"))
}

/// Compute output and global layout bounds from Hyprland monitor JSON.
fn layout_from_monitors(
    monitors_val: &Value,
    output_name: &str,
) -> Result<(u32, u32, u32, u32, u32, u32)> {
    let monitors = monitors_val.as_array().context("expected monitors array")?;
    if monitors.is_empty() {
        bail!("no monitors found");
    }

    // Find layout bounds
    let mut min_x = i64::MAX;
    let mut min_y = i64::MAX;
    let mut max_x = i64::MIN;
    let mut max_y = i64::MIN;
    let mut target = None;

    for m in monitors {
        let name = m
            .get("name")
            .and_then(Value::as_str)
            .context("monitor has missing or invalid 'name'")?;
        let x = required_i64(m, "x")?;
        let y = required_i64(m, "y")?;
        let w = required_i64(m, "width")?;
        let h = required_i64(m, "height")?;
        if w <= 0 || h <= 0 {
            bail!("monitor '{}' has invalid dimensions: {}x{}", name, w, h);
        }
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + w);
        max_y = max_y.max(y + h);

        if name == output_name {
            target = Some((x, y, w, h));
        }
    }

    let (target_x, target_y, target_w, target_h) =
        target.context(format!("output '{}' not found", output_name))?;
    let layout_w = u32::try_from(max_x - min_x).context("layout width is out of range")?;
    let layout_h = u32::try_from(max_y - min_y).context("layout height is out of range")?;
    if layout_w == 0 || layout_h == 0 {
        bail!("invalid layout bounds: {}x{}", layout_w, layout_h);
    }
    let offset_x = u32::try_from(target_x - min_x).context("output x offset is out of range")?;
    let offset_y = u32::try_from(target_y - min_y).context("output y offset is out of range")?;

    Ok((
        u32::try_from(target_w).context("output width is out of range")?,
        u32::try_from(target_h).context("output height is out of range")?,
        layout_w,
        layout_h,
        offset_x,
        offset_y,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn layout_parser_preserves_negative_monitor_offsets() {
        let monitors = json!([
            { "name": "DP-1", "x": -1920, "y": 0, "width": 1920, "height": 1080 },
            { "name": "hypr-rdp-1", "x": 0, "y": 180, "width": 1280, "height": 720 }
        ]);

        assert_eq!(
            layout_from_monitors(&monitors, "hypr-rdp-1").expect("layout parses"),
            (1280, 720, 3200, 1080, 1920, 180)
        );
    }

    #[test]
    fn layout_parser_rejects_missing_or_invalid_dimensions() {
        let missing_width = json!([
            { "name": "hypr-rdp-1", "x": 0, "y": 0, "height": 720 }
        ]);
        let string_width = json!([
            { "name": "hypr-rdp-1", "x": 0, "y": 0, "width": "1280", "height": 720 }
        ]);

        assert!(layout_from_monitors(&missing_width, "hypr-rdp-1").is_err());
        assert!(layout_from_monitors(&string_width, "hypr-rdp-1").is_err());
    }

    #[test]
    fn layout_parser_rejects_zero_or_negative_dimensions() {
        let zero_width = json!([
            { "name": "hypr-rdp-1", "x": 0, "y": 0, "width": 0, "height": 720 }
        ]);
        let negative_height = json!([
            { "name": "hypr-rdp-1", "x": 0, "y": 0, "width": 1280, "height": -1 }
        ]);

        assert!(layout_from_monitors(&zero_width, "hypr-rdp-1").is_err());
        assert!(layout_from_monitors(&negative_height, "hypr-rdp-1").is_err());
    }

    #[test]
    fn output_layout_generation_advances_on_presentation_or_source_geometry_change() {
        let layout = SharedOutputLayout::new();

        layout
            .update_snapshot_for_test("DP-1", 3840, 2160, 3840, 2160, 0, 0, (3840, 2160))
            .expect("initial snapshot");
        assert_eq!(layout.snapshot().unwrap().geometry_generation, 0);

        layout
            .update_snapshot_for_test("DP-1", 3840, 2160, 3840, 2160, 0, 0, (3840, 2160))
            .expect("same snapshot");
        assert_eq!(layout.snapshot().unwrap().geometry_generation, 0);

        layout
            .update_snapshot_for_test("DP-1", 3840, 2160, 3840, 2160, 0, 0, (1920, 1080))
            .expect("presentation resize");
        assert_eq!(layout.snapshot().unwrap().geometry_generation, 1);

        layout
            .update_snapshot_for_test("DP-1", 2560, 1440, 2560, 1440, 0, 0, (1920, 1080))
            .expect("source resize");
        assert_eq!(layout.snapshot().unwrap().geometry_generation, 2);
    }

    #[test]
    fn preparation_does_not_publish_or_advance_generation() {
        let layout = SharedOutputLayout::new();

        let prepared = SharedOutputLayout::prepare_snapshot_for_test(
            "DP-1",
            3840,
            2160,
            3840,
            2160,
            0,
            0,
            (1920, 1080),
        )
        .expect("prepared layout");
        assert!(layout.snapshot().is_none(), "preparation must not publish");

        layout
            .update_snapshot_for_test("DP-1", 3840, 2160, 3840, 2160, 0, 0, (3840, 2160))
            .expect("committed snapshot");
        assert_eq!(layout.snapshot().unwrap().geometry_generation, 0);

        layout.apply_prepared(prepared);
        let snapshot = layout.snapshot().expect("published snapshot");
        assert_eq!(snapshot.geometry_generation, 1);
        assert_eq!(snapshot.presentation_geometry.presentation().width, 1920);
        assert_eq!(snapshot.presentation_geometry.presentation().height, 1080);
    }

    #[test]
    fn applying_prepared_generation_depends_on_latest_committed_state() {
        let layout = SharedOutputLayout::new();

        let prepared = SharedOutputLayout::prepare_snapshot_for_test(
            "DP-1",
            3840,
            2160,
            3840,
            2160,
            0,
            0,
            (1920, 1080),
        )
        .expect("prepared layout");

        layout
            .update_snapshot_for_test("DP-1", 3840, 2160, 3840, 2160, 0, 0, (3840, 2160))
            .expect("committed snapshot");
        layout
            .update_snapshot_for_test("DP-1", 2560, 1440, 2560, 1440, 0, 0, (2560, 1440))
            .expect("newer committed snapshot");
        assert_eq!(layout.snapshot().unwrap().geometry_generation, 1);

        layout.apply_prepared(prepared);
        let snapshot = layout.snapshot().expect("published snapshot");
        assert_eq!(snapshot.geometry_generation, 2);
        assert_eq!(snapshot.output_w, 3840);
        assert_eq!(snapshot.output_h, 2160);
    }

    #[test]
    fn preparation_validation_failure_leaves_no_changes() {
        let layout = SharedOutputLayout::new();

        assert!(SharedOutputLayout::prepare_snapshot_for_test(
            "DP-1",
            3840,
            2160,
            3840,
            2160,
            0,
            0,
            (0, 1080),
        )
        .is_err());
        assert!(
            layout.snapshot().is_none(),
            "failed preparation must not publish"
        );
    }

    #[test]
    fn preparation_rejects_zero_source_or_presentation_dimensions() {
        let zero_source = SharedOutputLayout::prepare_snapshot_for_test(
            "DP-1",
            0,
            2160,
            3840,
            2160,
            0,
            0,
            (1920, 1080),
        );
        let zero_source_height = SharedOutputLayout::prepare_snapshot_for_test(
            "DP-1",
            3840,
            0,
            3840,
            2160,
            0,
            0,
            (1920, 1080),
        );
        let zero_presentation = SharedOutputLayout::prepare_snapshot_for_test(
            "DP-1",
            3840,
            2160,
            3840,
            2160,
            0,
            0,
            (1920, 0),
        );

        assert!(zero_source.is_err());
        assert!(zero_source_height.is_err());
        assert!(zero_presentation.is_err());
    }

    #[test]
    fn prepared_value_keeps_validated_source_and_presentation_fields() {
        let layout = SharedOutputLayout::new();

        let prepared = SharedOutputLayout::prepare_snapshot_for_test(
            "DP-1",
            3840,
            2160,
            5000,
            3000,
            100,
            200,
            (1920, 1080),
        )
        .expect("prepared layout");
        layout.apply_prepared(prepared);

        let snapshot = layout.snapshot().expect("published snapshot");
        assert_eq!(snapshot.output_name, "DP-1");
        assert_eq!(snapshot.output_w, 3840);
        assert_eq!(snapshot.output_h, 2160);
        assert_eq!(snapshot.layout_extent_w, 5000);
        assert_eq!(snapshot.layout_extent_h, 3000);
        assert_eq!(snapshot.output_offset_x, 100);
        assert_eq!(snapshot.output_offset_y, 200);
        assert_eq!(snapshot.presentation_geometry.presentation().width, 1920);
        assert_eq!(snapshot.presentation_geometry.presentation().height, 1080);
        assert_eq!(snapshot.geometry_generation, 0);
    }
}
