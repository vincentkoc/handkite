//! Exact, bounded pixels for native targets. No inferred rotation or viewport offset.
use super::*;
use crate::wda_ui_admission::Attributes;
use std::io::Cursor;

pub(super) struct Pixels {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) rgba: Vec<u8>,
}
#[derive(Clone, PartialEq)]
pub(super) struct Region {
    viewport: [f64; 4],
    bounds: [u32; 4],
    digest: [u8; 32],
}
const MAX_PIXELS: usize = 16_777_216;

impl Pixels {
    pub(super) fn decode(bytes: &[u8]) -> Result<Self, AdapterError> {
        if bytes.len() > 32 * 1024 * 1024 {
            return Err(AdapterError);
        }
        let mut decoder = png::Decoder::new(Cursor::new(bytes));
        decoder.set_limits(png::Limits {
            bytes: 64 * 1024 * 1024,
        });
        decoder.set_transformations(png::Transformations::EXPAND);
        let mut reader = decoder.read_info().map_err(|_| AdapterError)?;
        if reader.info().bit_depth == png::BitDepth::Sixteen {
            return Err(AdapterError);
        }
        let (width, height) = (reader.info().width, reader.info().height);
        let count = (width as usize)
            .checked_mul(height as usize)
            .filter(|n| *n > 0 && *n <= MAX_PIXELS)
            .ok_or(AdapterError)?;
        if width > 4096 || height > 4096 || reader.info().animation_control.is_some() {
            return Err(AdapterError);
        }
        let size = reader
            .output_buffer_size()
            .filter(|n| *n <= MAX_PIXELS * 4)
            .ok_or(AdapterError)?;
        let mut data = vec![0; size];
        let info = reader.next_frame(&mut data).map_err(|_| AdapterError)?;
        if info.bit_depth != png::BitDepth::Eight {
            return Err(AdapterError);
        }
        let channels = match info.color_type {
            png::ColorType::Grayscale => 1,
            png::ColorType::GrayscaleAlpha => 2,
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
            _ => return Err(AdapterError),
        };
        if info.buffer_size() != count * channels {
            return Err(AdapterError);
        }
        let mut rgba = Vec::with_capacity(count * 4);
        for pixel in data[..info.buffer_size()].chunks_exact(channels) {
            match channels {
                1 => rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], 255]),
                2 => rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]),
                3 => rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]),
                4 => rgba.extend_from_slice(pixel),
                _ => unreachable!(),
            }
        }
        Ok(Self {
            width,
            height,
            rgba,
        })
    }
    pub(super) fn validate_mapping(
        &self,
        viewport: [f64; 4],
        scale: f64,
        window: [f64; 2],
    ) -> Result<(), AdapterError> {
        if viewport[0] != 0.0
            || viewport[1] != 0.0
            || viewport[2] != window[0]
            || viewport[3] != window[1]
            || !scale.is_finite()
            || !(1.0..=4.0).contains(&scale)
            || !window.iter().all(|n| n.is_finite() && *n > 0.0)
            || (self.width as f64 - window[0] * scale).abs() > 1.0
            || (self.height as f64 - window[1] * scale).abs() > 1.0
        {
            return Err(AdapterError);
        }
        Ok(())
    }

    pub(super) fn region_bounds(
        &self,
        viewport: [f64; 4],
        attrs: &Attributes,
    ) -> Result<[u32; 4], AdapterError> {
        let [vx, vy, vw, vh] = viewport;
        if vx != 0.0 || vy != 0.0 || !vw.is_finite() || !vh.is_finite() || vw <= 0.0 || vh <= 0.0 {
            return Err(AdapterError);
        }
        let sx = self.width as f64 / vw;
        let sy = self.height as f64 / vh;
        // Native screen and raster dimensions can differ by one pixel. Require a common
        // scale within that exact bound, and include the uncertainty in every
        // guarded region. Larger distortion, offsets and rotation still refuse.
        if !(1.0..=4.0).contains(&sx)
            || !(1.0..=4.0).contains(&sy)
            || (sx - sy).abs() > 1.0 / vw + 1.0 / vh
        {
            return Err(AdapterError);
        }
        let n = |key: &str| {
            attrs
                .get(key)
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|n| n.is_finite())
                .ok_or(AdapterError)
        };
        let (x, y, w, h) = (n("x")?, n("y")?, n("width")?, n("height")?);
        if x < 0.0 || y < 0.0 || w <= 0.0 || h <= 0.0 || x + w > vw || y + h > vh {
            return Err(AdapterError);
        }
        // Include the entire one-pixel uncertainty even when both axes round
        // equally; equal aspect ratios do not prove the native scale is exact.
        let margin = 1.0;
        let bounds = [
            (x * sx - margin).floor().max(0.0) as u32,
            (y * sy - margin).floor().max(0.0) as u32,
            ((x + w) * sx + margin).ceil().min(self.width as f64) as u32,
            ((y + h) * sy + margin).ceil().min(self.height as f64) as u32,
        ];
        let [left, top, right, bottom] = bounds;
        if right > self.width || bottom > self.height || right <= left || bottom <= top {
            return Err(AdapterError);
        }
        Ok(bounds)
    }

    pub(super) fn region(
        &self,
        viewport: [f64; 4],
        attrs: &Attributes,
        budget: &mut usize,
    ) -> Result<Region, AdapterError> {
        let bounds = self.region_bounds(viewport, attrs)?;
        let [left, top, right, bottom] = bounds;
        let count = (right - left) as usize * (bottom - top) as usize;
        *budget = budget
            .checked_add(count)
            .filter(|n| *n <= MAX_PIXELS)
            .ok_or(AdapterError)?;
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        for row in top..bottom {
            let start = (row as usize * self.width as usize + left as usize) * 4;
            hash.update(&self.rgba[start..start + (right - left) as usize * 4]);
        }
        let digest = hash
            .finish()
            .as_ref()
            .try_into()
            .map_err(|_| AdapterError)?;
        Ok(Region {
            viewport,
            bounds,
            digest,
        })
    }
}

pub(super) fn viewport(xml: &str) -> Result<[f64; 4], AdapterError> {
    let nodes = super::targets::xml_nodes(xml)?;
    let mut apps = nodes
        .iter()
        .filter(|a| a.get("type").map(String::as_str) == Some("XCUIElementTypeApplication"));
    let app = apps.next().ok_or(AdapterError)?;
    if apps.next().is_some() {
        return Err(AdapterError);
    }
    let mut out = [0.0; 4];
    for (i, key) in ["x", "y", "width", "height"].iter().enumerate() {
        out[i] = app
            .get(*key)
            .and_then(|v| v.parse().ok())
            .ok_or(AdapterError)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn image(width: u32, height: u32, data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(data)
                .unwrap();
        }
        bytes
    }
    fn attrs() -> Attributes {
        [("x", "0"), ("y", "0"), ("width", "2"), ("height", "2")]
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect()
    }
    #[test]
    fn target_pixels_are_exact_but_outside_animation_is_allowed() {
        let original = vec![255; 4 * 4 * 4];
        let first = Pixels::decode(&image(4, 4, &original)).unwrap();
        let region = first.region([0., 0., 4., 4.], &attrs(), &mut 0).unwrap();
        let mut outside = original.clone();
        outside[60] = 0;
        assert!(
            region
                == Pixels::decode(&image(4, 4, &outside))
                    .unwrap()
                    .region([0., 0., 4., 4.], &attrs(), &mut 0)
                    .unwrap()
        );
        let mut inside = original;
        inside[0] = 0;
        assert!(
            region
                != Pixels::decode(&image(4, 4, &inside))
                    .unwrap()
                    .region([0., 0., 4., 4.], &attrs(), &mut 0)
                    .unwrap()
        );
    }
    #[test]
    fn mapping_dimensions_offsets_rotation_and_budget_fail_closed() {
        let pixels = Pixels::decode(&image(4, 4, &[255; 64])).unwrap();
        assert!(
            pixels
                .validate_mapping([0., 0., 2., 2.], 2., [2., 2.])
                .is_ok()
        );
        assert!(
            pixels
                .validate_mapping([0., 0., 2., 2.], 3., [2., 2.])
                .is_err()
        );
        assert!(
            pixels
                .validate_mapping([0., 0., 2., 2.], 2., [4., 1.])
                .is_err()
        );

        for viewport in [
            [1., 0., 4., 4.],
            [0., 1., 4., 4.],
            [0., 0., 4., 2.],
            [0., 0., 0., 4.],
            [0., 0., f64::NAN, 4.],
        ] {
            assert!(pixels.region(viewport, &attrs(), &mut 0).is_err());
        }
        let mut exhausted = MAX_PIXELS;
        assert!(
            pixels
                .region([0., 0., 4., 4.], &attrs(), &mut exhausted)
                .is_err()
        );
        assert!(Pixels::decode(b"not png").is_err());
        let mut oversized = image(4, 4, &[255; 64]);
        oversized.resize(32 * 1024 * 1024 + 1, 0);
        assert!(Pixels::decode(&oversized).is_err());
    }
    #[test]
    fn single_pixel_raster_quantization_expands_the_guard_region() {
        let pixels = Pixels {
            width: 1178,
            height: 2556,
            rgba: vec![],
        };
        let viewport = [0., 0., 393., 852.];
        assert!(pixels.validate_mapping(viewport, 3., [393., 852.]).is_ok());
        let bounds = pixels.region_bounds(viewport, &attrs()).unwrap();
        assert_eq!(bounds, [0, 0, 7, 7]);
        let distorted = Pixels {
            width: 1177,
            height: 2556,
            rgba: vec![],
        };
        assert!(
            distorted
                .validate_mapping(viewport, 3., [393., 852.])
                .is_err()
        );
        assert!(
            pixels
                .validate_mapping([1., 0., 393., 852.], 3., [393., 852.])
                .is_err()
        );
        assert!(pixels.validate_mapping(viewport, 3., [852., 393.]).is_err());
    }

    #[test]
    fn equal_axis_quantization_keeps_fractional_native_boundary_pixels() {
        let pixels = Pixels {
            width: 299,
            height: 299,
            rgba: vec![],
        };
        let viewport = [0., 0., 100., 100.];
        assert!(pixels.validate_mapping(viewport, 3., [100., 100.]).is_ok());
        let attrs = [
            ("x", "49.1"),
            ("y", "49.1"),
            ("width", "1"),
            ("height", "1"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        assert_eq!(
            pixels.region_bounds(viewport, &attrs).unwrap(),
            [145, 145, 151, 151]
        );
    }

    #[test]
    fn sixteen_bit_png_is_refused_without_lossy_truncation() {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Sixteen);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&[0x12, 0x34])
                .unwrap();
        }
        assert!(Pixels::decode(&bytes).is_err());
    }
}
