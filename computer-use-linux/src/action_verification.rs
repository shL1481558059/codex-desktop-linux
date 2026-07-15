use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

pub fn region_change_score(
    before_data_url: &str,
    after_data_url: &str,
    region: PixelRegion,
) -> Result<f64> {
    let before = decode_data_url(before_data_url)?;
    let after = decode_data_url(after_data_url)?;
    let before = image::load_from_memory(&before)
        .context("failed to decode pre-click screenshot")?
        .to_rgb8();
    let after = image::load_from_memory(&after)
        .context("failed to decode post-click screenshot")?
        .to_rgb8();
    if before.dimensions() != after.dimensions() {
        anyhow::bail!(
            "verification screenshots have different dimensions: {:?} and {:?}",
            before.dimensions(),
            after.dimensions()
        );
    }
    let (image_width, image_height) = before.dimensions();
    let right = region.x.saturating_add(region.width).min(image_width);
    let bottom = region.y.saturating_add(region.height).min(image_height);
    if region.x >= right || region.y >= bottom {
        anyhow::bail!("verification region is outside the screenshot");
    }

    let mut absolute_difference = 0_u64;
    let mut channel_count = 0_u64;
    for y in region.y..bottom {
        for x in region.x..right {
            let before_pixel = before.get_pixel(x, y).0;
            let after_pixel = after.get_pixel(x, y).0;
            for channel in 0..3 {
                absolute_difference +=
                    u64::from(before_pixel[channel].abs_diff(after_pixel[channel]));
                channel_count += 1;
            }
        }
    }
    Ok(absolute_difference as f64 / (channel_count as f64 * 255.0))
}

fn decode_data_url(data_url: &str) -> Result<Vec<u8>> {
    let (_, encoded) = data_url
        .split_once(',')
        .context("screenshot data URL has no payload separator")?;
    STANDARD
        .decode(encoded)
        .context("invalid screenshot base64")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn data_url(mut image: image::RgbImage) -> String {
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(std::mem::take(&mut image))
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        format!("data:image/png;base64,{}", STANDARD.encode(bytes))
    }

    #[test]
    fn region_score_ignores_changes_outside_target() {
        let before = image::RgbImage::from_pixel(20, 20, image::Rgb([0, 0, 0]));
        let mut after = before.clone();
        after.put_pixel(19, 19, image::Rgb([255, 255, 255]));

        let score = region_change_score(
            &data_url(before),
            &data_url(after),
            PixelRegion {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
        )
        .unwrap();
        assert_eq!(score, 0.0);
    }

    #[test]
    fn region_score_reports_normalized_pixel_change() {
        let before = image::RgbImage::from_pixel(2, 1, image::Rgb([0, 0, 0]));
        let mut after = before.clone();
        after.put_pixel(0, 0, image::Rgb([255, 255, 255]));

        let score = region_change_score(
            &data_url(before),
            &data_url(after),
            PixelRegion {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            },
        )
        .unwrap();
        assert!((score - 0.5).abs() < f64::EPSILON);
    }
}
