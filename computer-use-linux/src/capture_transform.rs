use crate::screenshot::ScreenshotCapture;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

pub const SCREENSHOT_ARTIFACT_TTL: Duration = Duration::from_secs(120);
const SCREENSHOT_ARTIFACT_LIMIT: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientInsets {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowSnapshot {
    pub window_id: u64,
    pub frame_rect: DesktopRect,
    pub client_rect: Option<DesktopRect>,
    pub client_insets: Option<ClientInsets>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureSegment {
    pub image_x: u32,
    pub image_y: u32,
    pub image_width: u32,
    pub image_height: u32,
    pub desktop_rect: DesktopRect,
    pub scale_x: f64,
    pub scale_y: f64,
}

#[derive(Debug, Clone)]
pub struct CaptureTransform {
    pub screenshot_id: String,
    pub image_width: u32,
    pub image_height: u32,
    pub desktop_rect: DesktopRect,
    pub segments: Vec<CaptureSegment>,
    pub window: Option<WindowSnapshot>,
    pub monitor_layout_fingerprint: String,
    pub captured_at: Instant,
}

impl CaptureTransform {
    pub fn from_capture(
        capture: &ScreenshotCapture,
        monitor_layout_fingerprint: String,
        window: Option<WindowSnapshot>,
        captured_at: Instant,
    ) -> Result<Self, String> {
        if capture.width == 0
            || capture.height == 0
            || capture.coordinate_width == 0
            || capture.coordinate_height == 0
        {
            return Err("screenshot has invalid zero-sized coordinate metadata".to_string());
        }
        let desktop_rect = DesktopRect {
            x: capture.coordinate_origin_x,
            y: capture.coordinate_origin_y,
            width: capture.coordinate_width,
            height: capture.coordinate_height,
        };
        Ok(Self {
            screenshot_id: capture.screenshot_id.clone(),
            image_width: capture.width,
            image_height: capture.height,
            desktop_rect,
            segments: vec![CaptureSegment {
                image_x: 0,
                image_y: 0,
                image_width: capture.width,
                image_height: capture.height,
                desktop_rect,
                scale_x: f64::from(capture.coordinate_width) / f64::from(capture.width),
                scale_y: f64::from(capture.coordinate_height) / f64::from(capture.height),
            }],
            window,
            monitor_layout_fingerprint,
            captured_at,
        })
    }

    pub fn map_pixel(&self, x: i32, y: i32) -> Result<(i32, i32), String> {
        if x < 0 || y < 0 || x as u32 >= self.image_width || y as u32 >= self.image_height {
            return Err(format!(
                "screenshot coordinate {x},{y} is outside image {}x{}",
                self.image_width, self.image_height
            ));
        }
        let x = x as u32;
        let y = y as u32;
        let segment = self
            .segments
            .iter()
            .find(|segment| {
                x >= segment.image_x
                    && y >= segment.image_y
                    && x < segment.image_x.saturating_add(segment.image_width)
                    && y < segment.image_y.saturating_add(segment.image_height)
            })
            .ok_or_else(|| "screenshot coordinate has no capture transform segment".to_string())?;
        map_segment_pixel(segment, x, y)
    }

    pub fn validate_context(
        &self,
        monitor_layout_fingerprint: &str,
        window: Option<WindowSnapshot>,
        now: Instant,
    ) -> Result<(), String> {
        if now.saturating_duration_since(self.captured_at) > SCREENSHOT_ARTIFACT_TTL {
            return Err("STALE_SCREENSHOT: screenshot mapping expired".to_string());
        }
        if monitor_layout_fingerprint != self.monitor_layout_fingerprint {
            return Err("STALE_SCREENSHOT: monitor layout changed after capture".to_string());
        }
        if let Some(expected) = self.window {
            let current = window.ok_or_else(|| {
                "STALE_SCREENSHOT: captured window is no longer available".to_string()
            })?;
            if current != expected {
                return Err(
                    "STALE_SCREENSHOT: captured window geometry changed after capture".to_string(),
                );
            }
        }
        Ok(())
    }
}

fn map_segment_pixel(segment: &CaptureSegment, x: u32, y: u32) -> Result<(i32, i32), String> {
    if segment.image_width == 0
        || segment.image_height == 0
        || segment.desktop_rect.width == 0
        || segment.desktop_rect.height == 0
    {
        return Err("capture transform segment has invalid zero-sized bounds".to_string());
    }
    let local_x = u64::from(x - segment.image_x);
    let local_y = u64::from(y - segment.image_y);
    if !segment.scale_x.is_finite()
        || !segment.scale_y.is_finite()
        || segment.scale_x <= 0.0
        || segment.scale_y <= 0.0
    {
        return Err("capture transform segment has invalid axis scales".to_string());
    }
    let scaled_x = ((local_x as f64 + 0.5) * segment.scale_x).floor();
    let scaled_y = ((local_y as f64 + 0.5) * segment.scale_y).floor();
    let desktop_x = i64::from(segment.desktop_rect.x)
        .checked_add(scaled_x as i64)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| "screenshot x coordinate overflowed desktop coordinates".to_string())?;
    let desktop_y = i64::from(segment.desktop_rect.y)
        .checked_add(scaled_y as i64)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| "screenshot y coordinate overflowed desktop coordinates".to_string())?;
    Ok((desktop_x, desktop_y))
}

#[derive(Debug, Clone)]
pub struct ScreenshotArtifact {
    pub capture: ScreenshotCapture,
    pub transform: CaptureTransform,
}

#[derive(Debug, Default)]
pub struct ScreenshotArtifactCache {
    artifacts: VecDeque<ScreenshotArtifact>,
}

impl ScreenshotArtifactCache {
    pub fn insert(&mut self, artifact: ScreenshotArtifact) {
        self.remove_expired(Instant::now());
        self.artifacts
            .retain(|item| item.transform.screenshot_id != artifact.transform.screenshot_id);
        self.artifacts.push_front(artifact);
        self.artifacts.truncate(SCREENSHOT_ARTIFACT_LIMIT);
    }

    pub fn get(&mut self, screenshot_id: &str, now: Instant) -> Result<ScreenshotArtifact, String> {
        self.remove_expired(now);
        self.artifacts
            .iter()
            .find(|artifact| artifact.transform.screenshot_id == screenshot_id)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "Unknown or expired screenshot_id {screenshot_id:?}. Capture a new screenshot and use its ID."
                )
            })
    }

    fn remove_expired(&mut self, now: Instant) {
        self.artifacts.retain(|artifact| {
            now.saturating_duration_since(artifact.transform.captured_at) <= SCREENSHOT_ARTIFACT_TTL
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screenshot::{
        prepare_screenshot_payload, RawScreenshotCapture, ScreenshotPayloadOptions,
    };
    use image::ImageFormat;
    use std::io::Cursor;

    fn capture(id: &str, width: u32, height: u32) -> ScreenshotCapture {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        let mut capture = prepare_screenshot_payload(
            RawScreenshotCapture {
                bytes,
                source: "test".to_string(),
                width,
                height,
            },
            ScreenshotPayloadOptions {
                max_width: Some(width),
                max_height: Some(height),
                max_bytes: Some(4 * 1024 * 1024),
                ..Default::default()
            },
        )
        .unwrap();
        capture.screenshot_id = id.to_string();
        capture
    }

    fn artifact(id: &str, captured_at: Instant) -> ScreenshotArtifact {
        let capture = capture(id, 100, 50);
        let transform =
            CaptureTransform::from_capture(&capture, "layout-a".to_string(), None, captured_at)
                .unwrap();
        ScreenshotArtifact { capture, transform }
    }

    #[test]
    fn maps_independent_x_y_scales_and_negative_origins() {
        let mut capture = capture("shot", 100, 100);
        capture.coordinate_width = 200;
        capture.coordinate_height = 300;
        capture.coordinate_origin_x = -200;
        capture.coordinate_origin_y = -100;
        let transform =
            CaptureTransform::from_capture(&capture, "layout-a".to_string(), None, Instant::now())
                .unwrap();

        assert_eq!(transform.segments[0].scale_x, 2.0);
        assert_eq!(transform.segments[0].scale_y, 3.0);
        assert_eq!(transform.map_pixel(50, 50).unwrap(), (-99, 51));
        assert_eq!(transform.map_pixel(99, 99).unwrap(), (-1, 198));
    }

    #[test]
    fn cache_keeps_multiple_recent_screenshots() {
        let now = Instant::now();
        let mut cache = ScreenshotArtifactCache::default();
        cache.insert(artifact("first", now));
        cache.insert(artifact("second", now));

        assert_eq!(
            cache.get("first", now).unwrap().transform.screenshot_id,
            "first"
        );
        assert_eq!(
            cache.get("second", now).unwrap().transform.screenshot_id,
            "second"
        );
    }

    #[test]
    fn cache_rejects_expired_screenshots() {
        let captured_at = Instant::now();
        let mut cache = ScreenshotArtifactCache::default();
        cache.insert(artifact("old", captured_at));

        let error = cache
            .get(
                "old",
                captured_at + SCREENSHOT_ARTIFACT_TTL + Duration::from_millis(1),
            )
            .unwrap_err();
        assert!(error.contains("Unknown or expired"));
    }

    #[test]
    fn transform_rejects_layout_and_window_geometry_changes() {
        let now = Instant::now();
        let window = WindowSnapshot {
            window_id: 7,
            frame_rect: DesktopRect {
                x: 10,
                y: 20,
                width: 800,
                height: 600,
            },
            client_rect: None,
            client_insets: None,
        };
        let capture = capture("shot", 100, 50);
        let transform =
            CaptureTransform::from_capture(&capture, "layout-a".to_string(), Some(window), now)
                .unwrap();

        assert!(transform
            .validate_context("layout-b", Some(window), now)
            .unwrap_err()
            .contains("monitor layout changed"));
        let moved = WindowSnapshot {
            frame_rect: DesktopRect {
                x: 11,
                ..window.frame_rect
            },
            ..window
        };
        assert!(transform
            .validate_context("layout-a", Some(moved), now)
            .unwrap_err()
            .contains("window geometry changed"));
    }
}
