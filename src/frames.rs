//! Pictures as small grids of cell averages, and how much of one changed since the last: enough to
//! tell a picture that moved from one that didn't, whatever size it came at. OBS's frames are only
//! compared with OBS's, and the screen's with the screen's.

use anyhow::{bail, Context, Result};

/// Cells across every grid; its height keeps the picture's shape.
pub const ACROSS: usize = 256;

/// A picture as cells of its mean colour.
#[derive(Debug, Clone, PartialEq)]
pub struct Grid {
    pub width: usize,
    pub height: usize,
    cells: Vec<[u8; 3]>,
}

/// Part of a picture, as fractions of its width and height (0 to 1): an OBS window on a display.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Area {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Area {
    fn covers(&self, cx: f64, cy: f64) -> bool {
        cx >= self.x && cx < self.x + self.width && cy >= self.y && cy < self.y + self.height
    }
}

/// How many of the cells looked at changed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Change {
    pub cells: usize,
    pub of: usize,
}

impl Change {
    pub fn fraction(&self) -> f64 {
        if self.of == 0 {
            0.0
        } else {
            self.cells as f64 / self.of as f64
        }
    }
}

impl Grid {
    /// A PNG (8 or 16 bits, grey or colour, with or without alpha) as a grid.
    pub fn from_png(bytes: &[u8]) -> Result<Grid> {
        let mut decoder = png::Decoder::new(bytes);
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info().context("not a PNG")?;
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).context("the PNG didn't decode")?;
        let channels = match info.color_type {
            png::ColorType::Grayscale => 1,
            png::ColorType::GrayscaleAlpha => 2,
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
            png::ColorType::Indexed => bail!("an indexed PNG wasn't expanded"),
        };
        Ok(Grid::from_pixels(&buf, info.width as usize, info.height as usize, info.line_size, channels))
    }

    /// Box-averages the pixels into cells: `stride` bytes a row, `channels` a pixel (grey is one, grey
    /// with alpha two; alpha is left out).
    pub fn from_pixels(buf: &[u8], width: usize, height: usize, stride: usize, channels: usize) -> Grid {
        let gw = ACROSS.min(width.max(1));
        let gh = ((height * gw) as f64 / width.max(1) as f64).round().max(1.0) as usize;
        let mut sums = vec![[0u64; 3]; gw * gh];
        let mut counts = vec![0u64; gw * gh];
        let column: Vec<usize> = (0..width).map(|x| x * gw / width).collect();
        for y in 0..height {
            let row = &buf[y * stride..];
            let base = (y * gh / height) * gw;
            for (x, cx) in column.iter().enumerate() {
                let p = &row[x * channels..x * channels + channels];
                let (r, g, b) = if channels < 3 { (p[0], p[0], p[0]) } else { (p[0], p[1], p[2]) };
                let s = &mut sums[base + cx];
                s[0] += r as u64;
                s[1] += g as u64;
                s[2] += b as u64;
                counts[base + cx] += 1;
            }
        }
        let cells = sums
            .iter()
            .zip(&counts)
            .map(|(s, &n)| {
                let n = n.max(1);
                [(s[0] / n) as u8, (s[1] / n) as u8, (s[2] / n) as u8]
            })
            .collect();
        Grid { width: gw, height: gh, cells }
    }

    /// The cells whose colour moved by more than `threshold` (of 255) in any channel, leaving out
    /// those whose centre is in one of `masked`. A grid of another size is all changed.
    pub fn changed(&self, before: &Grid, threshold: u8, masked: &[Area]) -> Change {
        if (self.width, self.height) != (before.width, before.height) {
            return Change { cells: self.cells.len(), of: self.cells.len() };
        }
        let (mut cells, mut of) = (0, 0);
        for (i, (a, b)) in self.cells.iter().zip(&before.cells).enumerate() {
            let (cx, cy) = ((i % self.width) as f64 + 0.5, (i / self.width) as f64 + 0.5);
            if masked.iter().any(|m| m.covers(cx / self.width as f64, cy / self.height as f64)) {
                continue;
            }
            of += 1;
            if a.iter().zip(b).any(|(p, q)| p.abs_diff(*q) > threshold) {
                cells += 1;
            }
        }
        Change { cells, of }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A grey picture with a white square at (x, y), `side` pixels across, as PNG bytes.
    pub fn picture(width: u32, height: u32, square: Option<(u32, u32, u32)>) -> Vec<u8> {
        let mut px = vec![60u8; (width * height * 3) as usize];
        if let Some((sx, sy, side)) = square {
            for y in sy..(sy + side).min(height) {
                for x in sx..(sx + side).min(width) {
                    let i = ((y * width + x) * 3) as usize;
                    px[i..i + 3].copy_from_slice(&[250, 250, 250]);
                }
            }
        }
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&px).unwrap();
        out
    }

    #[test]
    fn the_same_picture_is_unchanged() {
        let a = Grid::from_png(&picture(640, 360, Some((100, 100, 40)))).unwrap();
        let b = Grid::from_png(&picture(640, 360, Some((100, 100, 40)))).unwrap();
        assert_eq!((a.width, a.height), (256, 144));
        assert_eq!(a.changed(&b, 4, &[]).cells, 0);
    }

    #[test]
    fn a_moved_square_changes_cells_at_any_size() {
        for (w, h) in [(640, 360), (4112, 2658)] {
            let a = Grid::from_png(&picture(w, h, Some((w / 4, h / 4, w / 20)))).unwrap();
            let b = Grid::from_png(&picture(w, h, Some((w / 2, h / 4, w / 20)))).unwrap();
            let change = b.changed(&a, 12, &[]);
            assert!(change.fraction() > 0.002, "{w}x{h}: {change:?}");
        }
    }

    #[test]
    fn a_masked_area_is_left_out() {
        let a = Grid::from_png(&picture(640, 360, Some((10, 10, 60)))).unwrap();
        let b = Grid::from_png(&picture(640, 360, None)).unwrap();
        let mask = Area { x: 0.0, y: 0.0, width: 0.2, height: 0.3 };
        assert!(b.changed(&a, 12, &[]).cells > 0);
        let change = b.changed(&a, 12, &[mask]);
        assert_eq!(change.cells, 0);
        assert!(change.of < 256 * 144);
    }

    #[test]
    fn another_size_is_all_changed() {
        let a = Grid::from_png(&picture(640, 360, None)).unwrap();
        let b = Grid::from_png(&picture(640, 400, None)).unwrap();
        assert_eq!(b.changed(&a, 12, &[]).fraction(), 1.0);
    }
}
