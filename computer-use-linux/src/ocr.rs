use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{process::Command, time::timeout};

const OCR_TIMEOUT: Duration = Duration::from_secs(12);
const RAPIDOCR_PREFIX: &str = "__CODEX_OCR_JSON__";
const RAPIDOCR_SCRIPT: &str = r#"
import json
import sys
from rapidocr import RapidOCR

engine = RapidOCR(params={"Rec.lang_type": "ch"})
result = engine(sys.argv[1])

def as_list(value):
    if value is None:
        return []
    if hasattr(value, "tolist"):
        return value.tolist()
    return list(value)

print("__CODEX_OCR_JSON__" + json.dumps({
    "boxes": as_list(getattr(result, "boxes", None)),
    "txts": as_list(getattr(result, "txts", None)),
    "scores": as_list(getattr(result, "scores", None)),
}, ensure_ascii=False))
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OcrBounds {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OcrObservation {
    pub bounds: OcrBounds,
    pub text: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OcrResult {
    pub backend: String,
    pub observations: Vec<OcrObservation>,
}

pub async fn recognize(data_url: &str, mime_type: &str) -> Result<OcrResult> {
    let bytes = decode_data_url(data_url)?;
    let path = temporary_image_path(mime_type);
    fs::write(&path, bytes)
        .with_context(|| format!("failed to write OCR image {}", path.display()))?;
    let result = recognize_path(&path).await;
    let _ = fs::remove_file(path);
    result
}

async fn recognize_path(path: &Path) -> Result<OcrResult> {
    match recognize_rapidocr(path).await {
        Ok(result) => Ok(result),
        Err(rapid_error) => recognize_tesseract(path).await.with_context(|| {
            format!("RapidOCR unavailable ({rapid_error:#}); Tesseract fallback also failed")
        }),
    }
}

async fn recognize_rapidocr(path: &Path) -> Result<OcrResult> {
    let output = timeout(
        OCR_TIMEOUT,
        Command::new("python3")
            .arg("-c")
            .arg(RAPIDOCR_SCRIPT)
            .arg(path)
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .context("RapidOCR timed out")?
    .context("failed to start RapidOCR Python")?;
    if !output.status.success() {
        bail!(
            "RapidOCR exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8(output.stdout).context("RapidOCR output was not UTF-8")?;
    let json = stdout
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(RAPIDOCR_PREFIX))
        .context("RapidOCR did not return its JSON marker")?;
    let payload: RapidOcrPayload = serde_json::from_str(json).context("invalid RapidOCR JSON")?;
    let count = payload
        .boxes
        .len()
        .min(payload.txts.len())
        .min(payload.scores.len());
    let mut observations = Vec::with_capacity(count);
    for index in 0..count {
        let Some(bounds) = polygon_bounds(&payload.boxes[index]) else {
            continue;
        };
        let text = payload.txts[index].trim();
        if text.is_empty() {
            continue;
        }
        observations.push(OcrObservation {
            bounds,
            text: text.to_string(),
            confidence: payload.scores[index].clamp(0.0, 1.0),
        });
    }
    Ok(OcrResult {
        backend: "rapidocr-python".to_string(),
        observations,
    })
}

async fn recognize_tesseract(path: &Path) -> Result<OcrResult> {
    let output = timeout(
        OCR_TIMEOUT,
        Command::new("tesseract")
            .arg(path)
            .arg("stdout")
            .args(["-l", "chi_sim+eng", "--psm", "11", "tsv"])
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .context("Tesseract timed out")?
    .context("failed to start Tesseract")?;
    if !output.status.success() {
        bail!(
            "Tesseract exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8(output.stdout).context("Tesseract TSV was not UTF-8")?;
    Ok(OcrResult {
        backend: "tesseract-cli".to_string(),
        observations: parse_tesseract_tsv(&stdout),
    })
}

#[derive(Debug, Deserialize)]
struct RapidOcrPayload {
    #[serde(default)]
    boxes: Vec<Vec<Vec<f64>>>,
    #[serde(default)]
    txts: Vec<String>,
    #[serde(default)]
    scores: Vec<f64>,
}

fn polygon_bounds(points: &[Vec<f64>]) -> Option<OcrBounds> {
    let coordinates = points
        .iter()
        .filter(|point| point.len() >= 2 && point[0].is_finite() && point[1].is_finite())
        .map(|point| (point[0], point[1]))
        .collect::<Vec<_>>();
    if coordinates.is_empty() {
        return None;
    }
    let min_x = coordinates
        .iter()
        .map(|point| point.0)
        .fold(f64::INFINITY, f64::min)
        .floor()
        .max(0.0);
    let min_y = coordinates
        .iter()
        .map(|point| point.1)
        .fold(f64::INFINITY, f64::min)
        .floor()
        .max(0.0);
    let max_x = coordinates
        .iter()
        .map(|point| point.0)
        .fold(f64::NEG_INFINITY, f64::max)
        .ceil()
        .max(min_x + 1.0);
    let max_y = coordinates
        .iter()
        .map(|point| point.1)
        .fold(f64::NEG_INFINITY, f64::max)
        .ceil()
        .max(min_y + 1.0);
    Some(OcrBounds {
        x: min_x.min(u32::MAX as f64) as u32,
        y: min_y.min(u32::MAX as f64) as u32,
        width: (max_x - min_x).min(u32::MAX as f64) as u32,
        height: (max_y - min_y).min(u32::MAX as f64) as u32,
    })
}

#[derive(Debug)]
struct TesseractWord {
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    confidence: f64,
    text: String,
}

fn parse_tesseract_tsv(tsv: &str) -> Vec<OcrObservation> {
    let mut lines: BTreeMap<(u32, u32, u32, u32), Vec<TesseractWord>> = BTreeMap::new();
    for row in tsv.lines().skip(1) {
        let fields = row.splitn(12, '\t').collect::<Vec<_>>();
        if fields.len() != 12 || fields[0] != "5" {
            continue;
        }
        let text = fields[11].trim();
        let Some(confidence) = fields[10].parse::<f64>().ok().filter(|value| *value >= 0.0) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let parse_u32 = |index: usize| fields[index].parse::<u32>().ok();
        let Some(key) = parse_u32(1)
            .zip(parse_u32(2))
            .zip(parse_u32(3))
            .zip(parse_u32(4))
            .map(|(((page, block), paragraph), line)| (page, block, paragraph, line))
        else {
            continue;
        };
        let Some((left, top, width, height)) = parse_u32(6)
            .zip(parse_u32(7))
            .zip(parse_u32(8))
            .zip(parse_u32(9))
            .map(|(((left, top), width), height)| (left, top, width, height))
        else {
            continue;
        };
        if width == 0 || height == 0 {
            continue;
        }
        lines.entry(key).or_default().push(TesseractWord {
            left,
            top,
            width,
            height,
            confidence: confidence / 100.0,
            text: text.to_string(),
        });
    }

    lines
        .into_values()
        .filter_map(|words| {
            let left = words.iter().map(|word| word.left).min()?;
            let top = words.iter().map(|word| word.top).min()?;
            let right = words
                .iter()
                .map(|word| word.left.saturating_add(word.width))
                .max()?;
            let bottom = words
                .iter()
                .map(|word| word.top.saturating_add(word.height))
                .max()?;
            let confidence =
                words.iter().map(|word| word.confidence).sum::<f64>() / words.len() as f64;
            Some(OcrObservation {
                bounds: OcrBounds {
                    x: left,
                    y: top,
                    width: right.saturating_sub(left),
                    height: bottom.saturating_sub(top),
                },
                text: words
                    .iter()
                    .map(|word| word.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                confidence,
            })
        })
        .collect()
}

fn decode_data_url(data_url: &str) -> Result<Vec<u8>> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let (_, encoded) = data_url
        .split_once(',')
        .context("screenshot data URL has no payload separator")?;
    STANDARD
        .decode(encoded)
        .context("invalid screenshot base64")
}

fn temporary_image_path(mime_type: &str) -> PathBuf {
    let extension = if mime_type == "image/jpeg" {
        "jpg"
    } else {
        "png"
    };
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "codex-computer-use-ocr-{}-{nonce}.{extension}",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tesseract_words_are_grouped_into_searchable_lines() {
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
5\t1\t1\t1\t1\t1\t10\t20\t30\t12\t95.0\t公益站\n\
5\t1\t1\t1\t1\t2\t44\t20\t48\t12\t85.0\t自动签到\n\
5\t1\t1\t1\t2\t1\t10\t50\t24\t12\t90.0\t保存\n";
        let observations = parse_tesseract_tsv(tsv);

        assert_eq!(observations.len(), 2);
        assert_eq!(observations[0].text, "公益站 自动签到");
        assert_eq!(
            observations[0].bounds,
            OcrBounds {
                x: 10,
                y: 20,
                width: 82,
                height: 12
            }
        );
        assert!((observations[0].confidence - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn rapidocr_polygon_is_converted_to_pixel_bounds() {
        let bounds = polygon_bounds(&[
            vec![10.2, 20.8],
            vec![90.1, 20.1],
            vec![90.8, 40.7],
            vec![10.0, 40.9],
        ])
        .unwrap();

        assert_eq!(
            bounds,
            OcrBounds {
                x: 10,
                y: 20,
                width: 81,
                height: 21
            }
        );
    }
}
