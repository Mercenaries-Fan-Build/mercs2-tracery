//! The overlay's glyph atlas and text quads: printable ASCII (`' '..='~'`) in one row of fixed-size cells,
//! and screen-space triangles (pre-transformed `XYZRHW | DIFFUSE | TEX1` vertices) that draw a string
//! from it. Plain arithmetic, tested on the host; the Direct3D side is `overlay`.

/// The first and last glyph in the atlas.
pub const FIRST: u8 = b' ';
pub const LAST: u8 = b'~';
/// Glyphs in the atlas, one cell each, left to right.
pub const GLYPHS: u32 = (LAST - FIRST + 1) as u32;
/// The glyph drawn for a byte outside [`FIRST`]..=[`LAST`].
pub const REPLACEMENT: u8 = b'?';
/// The largest texture side the atlas may need.
pub const MAX_SIDE: u32 = 4096;

/// One vertex of `D3DFVF_XYZRHW | D3DFVF_DIFFUSE | D3DFVF_TEX1`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vertex {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub rhw: f32,
    /// `D3DCOLOR`: `0xAARRGGBB`.
    pub color: u32,
    pub u: f32,
    pub v: f32,
}

/// Where each glyph sits in the atlas texture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub cell_w: u32,
    pub cell_h: u32,
    pub tex_w: u32,
    pub tex_h: u32,
}

impl Layout {
    /// The layout for glyph cells of `cell_w` × `cell_h` pixels; the texture sides are powers of two.
    pub fn new(cell_w: u32, cell_h: u32) -> Result<Self, String> {
        if cell_w == 0 || cell_h == 0 {
            return Err(format!("a glyph cell of {cell_w}x{cell_h} pixels has no area"));
        }
        let tex_w = (cell_w * GLYPHS).next_power_of_two();
        let tex_h = cell_h.next_power_of_two();
        if tex_w > MAX_SIDE || tex_h > MAX_SIDE {
            return Err(format!(
                "{GLYPHS} glyph cells of {cell_w}x{cell_h} pixels need a {tex_w}x{tex_h} texture, larger than {MAX_SIDE}"
            ));
        }
        Ok(Layout { cell_w, cell_h, tex_w, tex_h })
    }

    /// The atlas cell of byte `c`.
    pub fn cell(c: u8) -> u32 {
        let c = if (FIRST..=LAST).contains(&c) { c } else { REPLACEMENT };
        u32::from(c - FIRST)
    }

    /// The pixel x of cell `cell`'s left edge.
    pub fn cell_x(&self, cell: u32) -> u32 {
        cell * self.cell_w
    }

    /// `(u0, v0, u1, v1)` of byte `c`.
    pub fn uv(&self, c: u8) -> (f32, f32, f32, f32) {
        let x = self.cell_x(Self::cell(c)) as f32;
        let (w, h) = (self.tex_w as f32, self.tex_h as f32);
        (x / w, 0.0, (x + self.cell_w as f32) / w, self.cell_h as f32 / h)
    }

    /// Two triangles per byte of `text`, its top-left corner at `(x, y)`, each cell drawn `scale` times its
    /// size. A `'\n'` starts the next line at `x`.
    pub fn text(&self, text: &str, x: f32, y: f32, scale: f32, color: u32) -> Vec<Vertex> {
        let (cw, ch) = (self.cell_w as f32 * scale, self.cell_h as f32 * scale);
        let mut out = Vec::with_capacity(text.len() * 6);
        let (mut px, mut py) = (x, y);
        for &b in text.as_bytes() {
            if b == b'\n' {
                px = x;
                py += ch;
                continue;
            }
            let (u0, v0, u1, v1) = self.uv(b);
            out.extend(quad(px, py, px + cw, py + ch, color, (u0, v0, u1, v1)));
            px += cw;
        }
        out
    }

    /// The size in pixels `text` takes when drawn at `scale`: the longest line by the line count.
    pub fn extent(&self, text: &str, scale: f32) -> (f32, f32) {
        let lines: Vec<&str> = text.split('\n').collect();
        let widest = lines.iter().map(|l| l.len()).max().unwrap_or(0);
        (widest as f32 * self.cell_w as f32 * scale, lines.len() as f32 * self.cell_h as f32 * scale)
    }
}

/// Two triangles covering `(x0, y0)..(x1, y1)`. The overlay draws its solid background with no texture
/// bound, so the `uv` of that quad is unused.
pub fn quad(x0: f32, y0: f32, x1: f32, y1: f32, color: u32, (u0, v0, u1, v1): (f32, f32, f32, f32)) -> [Vertex; 6] {
    // Pixel centres sit on half coordinates in Direct3D 9.
    let (x0, y0, x1, y1) = (x0 - 0.5, y0 - 0.5, x1 - 0.5, y1 - 0.5);
    let v = |x, y, u, v| Vertex { x, y, z: 0.0, rhw: 1.0, color, u, v };
    [v(x0, y0, u0, v0), v(x1, y0, u1, v0), v(x0, y1, u0, v1), v(x1, y0, u1, v0), v(x1, y1, u1, v1), v(x0, y1, u0, v1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_texture_holds_every_cell_in_powers_of_two() {
        let l = Layout::new(8, 13).unwrap();
        assert_eq!((l.tex_w, l.tex_h), (1024, 16));
        assert!(l.cell_x(GLYPHS - 1) + l.cell_w <= l.tex_w);
        assert!(Layout::new(0, 13).is_err());
        assert!(Layout::new(64, 13).unwrap_err().contains("larger than 4096"));
    }

    #[test]
    fn bytes_outside_printable_ascii_draw_the_replacement() {
        assert_eq!(Layout::cell(b' '), 0);
        assert_eq!(Layout::cell(b'~'), GLYPHS - 1);
        assert_eq!(Layout::cell(b'\t'), Layout::cell(REPLACEMENT));
        assert_eq!(Layout::cell(0xe9), Layout::cell(REPLACEMENT));
    }

    #[test]
    fn a_glyph_maps_to_its_own_cell() {
        let l = Layout::new(8, 16).unwrap();
        let (u0, v0, u1, v1) = l.uv(b'A');
        let cell = u32::from(b'A' - FIRST) as f32;
        assert_eq!((u0, u1), (cell * 8.0 / 1024.0, (cell + 1.0) * 8.0 / 1024.0));
        assert_eq!((v0, v1), (0.0, 1.0));
    }

    #[test]
    fn text_lays_out_cells_and_lines() {
        let l = Layout::new(8, 16).unwrap();
        let v = l.text("ab\nc", 10.0, 20.0, 2.0, 0xffffffff);
        assert_eq!(v.len(), 18, "three glyphs, six vertices each; the newline draws nothing");
        // 'b' starts one scaled cell to the right; 'c' starts the next line at x.
        assert_eq!((v[6].x, v[6].y), (10.0 + 16.0 - 0.5, 20.0 - 0.5));
        assert_eq!((v[12].x, v[12].y), (10.0 - 0.5, 20.0 + 32.0 - 0.5));
        assert_eq!(l.extent("ab\nc", 2.0), (32.0, 64.0));
    }
}
