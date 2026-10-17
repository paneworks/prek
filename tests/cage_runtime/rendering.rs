use prek::{context::Snapshot, gestures::Transform, ipc::monotonic};
use serde::Serialize;
use serde_json::Value;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

fn read(path: &Path, limit: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .unwrap()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(
        bytes.len() <= limit,
        "{} exceeds test artifact bound",
        path.display()
    );
    bytes
}
#[derive(Serialize)]
struct Visible {
    window: String,
    label: String,
    receipt: PathBuf,
    pixel: [usize; 2],
    color: [u8; 3],
    size: [f64; 2],
    configured: u64,
    rendered: u64,
    presented: u64,
}
struct Ppm<'a> {
    width: usize,
    height: usize,
    pixels: &'a [u8],
}
impl<'a> Ppm<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, &'static str> {
        let mut cursor = 0;
        let mut token = || -> Result<&[u8], &'static str> {
            loop {
                while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
                    cursor += 1;
                }
                if bytes.get(cursor) != Some(&b'#') {
                    break;
                }
                while bytes.get(cursor).is_some_and(|byte| *byte != b'\n') {
                    cursor += 1;
                }
            }
            let start = cursor;
            while bytes
                .get(cursor)
                .is_some_and(|byte| !byte.is_ascii_whitespace())
            {
                cursor += 1;
            }
            if cursor == start {
                return Err("missing PPM header token");
            }
            Ok(&bytes[start..cursor])
        };
        if token()? != b"P6" {
            return Err("expected binary RGB PPM");
        }
        let mut number = || -> Result<usize, &'static str> {
            let word = std::str::from_utf8(token()?).map_err(|_| "invalid PPM number")?;
            if word.is_empty() || !word.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err("invalid PPM number");
            }
            word.parse().map_err(|_| "PPM number overflow")
        };
        let width = number()?;
        let height = number()?;
        let maximum = number()?;
        if width == 0 || height == 0 || width > 8192 || height > 8192 || maximum != 255 {
            return Err("unsupported PPM dimensions or depth");
        }
        if !bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            return Err("missing PPM data separator");
        }
        if bytes.get(cursor) == Some(&b'\r') && bytes.get(cursor + 1) == Some(&b'\n') {
            cursor += 2;
        } else {
            cursor += 1;
        }
        let length = width
            .checked_mul(height)
            .and_then(|count| count.checked_mul(3))
            .ok_or("PPM image size overflow")?;
        let pixels = bytes.get(cursor..).ok_or("missing PPM data")?;
        if length != pixels.len() || length > 64 * 1024 * 1024 {
            return Err("PPM image is truncated, oversized or has trailing data");
        }
        Ok(Self {
            width,
            height,
            pixels,
        })
    }
    fn marker(&self, point: [usize; 2], expected: [u8; 3]) -> bool {
        let [x, y] = point;
        if x < 2 || y < 2 || x + 2 >= self.width || y + 2 >= self.height {
            return false;
        }
        (y - 2..=y + 2).all(|y| {
            (x - 2..=x + 2).all(|x| {
                self.pixels[(y * self.width + x) * 3..(y * self.width + x) * 3 + 3] == expected
            })
        })
    }
}

pub(super) fn capture(
    snapshot: &Snapshot,
    workspace: i64,
    instance: &str,
    output: &Path,
    stem: &str,
) -> PathBuf {
    let pixels = output.join(format!("{stem}.ppm"));
    let capture = output.join(format!("{stem}.png"));
    let state = snapshot
        .outputs
        .iter()
        .find(|state| state.active_workspace == Some(workspace))
        .unwrap();
    assert_eq!(state.geometry.scale, 1.);
    assert_eq!(state.geometry.transform, Transform::Normal);
    let clients = Command::new(std::env::var_os("PREK_CAGE_HYPRCTL").unwrap())
        .args(["-j", "clients"])
        .env("HYPRLAND_INSTANCE_SIGNATURE", instance)
        .output()
        .unwrap();
    assert!(clients.status.success());
    let clients: Vec<Value> = serde_json::from_slice(&clients.stdout).unwrap();
    let mut visible: Vec<_> = snapshot
        .windows
        .iter()
        .filter(|window| {
            window.mapped
                && window.workspace == workspace
                && window.output == state.geometry.name
                && state.rect().contains_rect(window.rect)
        })
        .map(|window| {
            let (address, stable) = window.id.split_once(':').unwrap();
            let native = clients
                .iter()
                .find(|client| client["address"] == address && client["stableId"] == stable)
                .unwrap();
            let (label, file, color) = match native["title"].as_str().unwrap() {
                "Prek Wayland input receiver" => {
                    ("Prek Wayland input receiver", "receipts.jsonl", [0, 0, 255])
                }
                "hyprland-target" => ("hyprland-target", "hyprland-target.jsonl", [255, 0, 0]),
                "hyprland-guard" => ("hyprland-guard", "hyprland-guard.jsonl", [0, 255, 0]),
                title => panic!("unexpected owned test receiver {title:?}"),
            };
            let x = window.rect.origin.x - state.geometry.origin.x + 12.;
            let y = window.rect.origin.y - state.geometry.origin.y + 12.;
            assert!(x >= 0. && y >= 0. && x.fract() == 0. && y.fract() == 0.);
            Visible {
                window: window.id.clone(),
                label: label.into(),
                receipt: output.join(file),
                pixel: [x as usize, y as usize],
                color,
                size: [window.rect.size.x, window.rect.size.y],
                configured: 0,
                rendered: 0,
                presented: 0,
            }
        })
        .collect();
    assert!(
        visible.len() >= 2,
        "reorder must retain several fully visible app surfaces"
    );
    let until = monotonic().unwrap() + Duration::from_secs(3);
    loop {
        for app in &mut visible {
            let text = String::from_utf8(read(&app.receipt, 2 * 1024 * 1024)).unwrap();
            let rows: Vec<Value> = text
                .split_inclusive('\n')
                .filter(|line| line.ends_with('\n'))
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            app.configured = 0;
            app.rendered = 0;
            app.presented = 0;
            if let Some(configure) = rows.iter().rev().find(|row| row["event"] == "configured")
                && configure["width"].as_f64() == Some(app.size[0])
                && configure["height"].as_f64() == Some(app.size[1])
            {
                app.configured = configure["sequence"].as_u64().unwrap();
                app.rendered = rows
                    .iter()
                    .filter(|row| {
                        row["event"] == "rendered"
                            && row["label"] == app.label
                            && row["width"].as_f64() == Some(app.size[0])
                            && row["height"].as_f64() == Some(app.size[1])
                    })
                    .filter_map(|row| row["drawn_sequence"].as_u64())
                    .max()
                    .unwrap_or(0);
                app.presented = rows
                    .iter()
                    .filter(|row| {
                        row["event"] == "presented"
                            && row["label"] == app.label
                            && row["buffer_width"].as_f64() == Some(app.size[0])
                            && row["buffer_height"].as_f64() == Some(app.size[1])
                    })
                    .filter_map(|row| row["drawn_sequence"].as_u64())
                    .max()
                    .unwrap_or(0);
            }
        }
        if visible.iter().all(|app| {
            app.configured > 0 && app.rendered >= app.configured && app.presented >= app.configured
        }) {
            break;
        }
        assert!(
            monotonic().unwrap() < until,
            "owned receivers did not submit current geometry: {}",
            serde_json::to_string(&visible).unwrap()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let nested: Value =
        serde_json::from_slice(&read(&output.join("nested.json"), 64 * 1024)).unwrap();
    let screenshot = |kind: &str, path: &Path| {
        assert!(
            Command::new(std::env::var_os("PREK_CAGE_SCREENCOPY").unwrap())
                .args(["-t", kind, "-s", "1", "-o", &state.geometry.name])
                .arg(path)
                .env("WAYLAND_DISPLAY", nested["socket"].as_str().unwrap())
                .env_remove("WAYLAND_SOCKET")
                .status()
                .unwrap()
                .success()
        );
    };
    screenshot("ppm", &pixels);
    let bytes = read(&pixels, 64 * 1024 * 1024);
    let image = Ppm::parse(&bytes).unwrap();
    assert_eq!(
        (image.width as f64, image.height as f64),
        (state.geometry.pixel_width, state.geometry.pixel_height)
    );
    for app in &visible {
        assert!(
            image.marker(app.pixel, app.color),
            "{}: actual pixels do not contain {} at {:?}",
            pixels.display(),
            app.label,
            app.pixel
        );
    }
    screenshot("png", &capture);
    fs::write(output.join(format!("{stem}-rendering.json")),serde_json::to_vec(&serde_json::json!({"status":"passed","pixels":pixels,"capture":capture,"visible":visible})).unwrap()).unwrap();
    capture
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ppm_parser_preserves_binary_whitespace_and_checks_exact_dimensions() {
        let mut bytes = b"P6\n# test\n1 1\n255\n".to_vec();
        bytes.extend([32, 10, 13]);
        let image = Ppm::parse(&bytes).unwrap();
        assert_eq!(image.pixels, [32, 10, 13]);
        for malformed in [
            b"P3\n1 1\n255\nxxx".as_slice(),
            b"P6\n0 1\n255\n",
            b"P6\n1 1\n256\nxxx",
            b"P6\n1 1\n255\nxx",
            b"P6\n1 1\n255\nxxxx",
            b"P6\n9999999999999999999999999 1\n255\n",
        ] {
            assert!(Ppm::parse(malformed).is_err());
        }
    }
    #[test]
    fn actual_pixel_check_refuses_black_wrong_label_misalignment_or_partial_markers() {
        let mut bytes = b"P6\n10 10\n255\n".to_vec();
        bytes.resize(bytes.len() + 300, 0);
        assert!(!Ppm::parse(&bytes).unwrap().marker([5, 5], [255, 0, 0]));
        let base = bytes.len() - 300;
        for y in 3..=7 {
            for x in 3..=7 {
                bytes[base + (y * 10 + x) * 3] = 255;
            }
        }
        let image = Ppm::parse(&bytes).unwrap();
        assert!(image.marker([5, 5], [255, 0, 0]));
        assert!(!image.marker([5, 5], [0, 255, 0]));
        assert!(!image.marker([6, 5], [255, 0, 0]));
        bytes[base + (5 * 10 + 5) * 3] = 0;
        assert!(!Ppm::parse(&bytes).unwrap().marker([5, 5], [255, 0, 0]));
    }
}
