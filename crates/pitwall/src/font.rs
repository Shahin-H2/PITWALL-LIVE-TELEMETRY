//! Signed-distance-field typography.
//!
//! # What was wrong before
//!
//! The first version stored glyphs as a 5×7 bitmap in an 8×8 cell and sampled
//! it with a linear filter. At label sizes that is roughly 1:1 and looks fine.
//! The gear indicator, however, is drawn at `tach_radius * 0.85` — about 200 px
//! on a 900 px window and 477 px at 4K. Magnifying a 7-pixel-tall bitmap by
//! 28-68× does not produce a large glyph; it produces thirty-five fat gradient
//! blobs. No DPI correction, sampler mode, or canvas-scaling fix addresses
//! that, because the information is simply not in the source.
//!
//! # The fix: store distance, not coverage
//!
//! A bitmap stores "is this texel inside the glyph" — a step function, which
//! is exactly the thing bilinear interpolation cannot reconstruct. An SDF
//! stores "how far is this texel from the glyph's edge", a smooth field that
//! interpolates *correctly*. The shader recovers a razor-sharp edge at any
//! magnification with one `smoothstep` around the 0.5 iso-contour, and gets
//! outlines and glows for free from the same sample.
//!
//! # This SDF is exact, not rasterised
//!
//! Typical SDF font pipelines rasterise glyphs at high resolution and then run
//! a distance transform, which bakes in the rasteriser's error. We can do
//! better here, because the source is not an image: each lit cell in the 5×7
//! grid is treated as a **disc**, and a glyph is the union of its discs. The
//! signed distance to a union of discs has a closed form —
//! `min over discs of (|p - centre| - r)` — so every texel gets an
//! analytically exact distance. There is no rasterisation step and therefore
//! no rasterisation error.
//!
//! The disc radius is chosen so that diagonally adjacent cells just merge,
//! which is what turns a dot grid into connected, rounded letterforms — the
//! look of a machined instrument face rather than a spreadsheet.
//!
//! # Sharp corners
//!
//! A single-channel SDF rounds corners tighter than about one texel; that is
//! the known limitation that multi-channel SDF (MSDF) exists to solve. It does
//! not matter here because these letterforms are built from discs and are
//! round by construction. If this font is ever replaced with a real typeface
//! with mitred corners, MSDF is the upgrade path — same shader, three channels
//! and a median() instead of one.

/// Texels per cell. The glyph grid is 7 units wide × 9 tall including padding,
/// so this is exactly 8 texels per glyph unit.
pub const CELL_W: u32 = 56;
pub const CELL_H: u32 = 72;
pub const COLS: u32 = 16;
pub const ROWS: u32 = 6;
pub const ATLAS_W: u32 = CELL_W * COLS; // 896
pub const ATLAS_H: u32 = CELL_H * ROWS; // 432

/// The glyph's ink occupies a 5×7 box; the cell covers -1..6 × -1..8, giving
/// one unit of padding on every side for the distance field to run into.
pub const INK_W: f32 = 5.0;
pub const INK_H: f32 = 7.0;
pub const CELL_UNITS_W: f32 = 7.0;
pub const CELL_UNITS_H: f32 = 9.0;
const PAD: f32 = 1.0;

/// Half the stroke width, in glyph units.
///
/// Every adjacent pair of lit cells is joined by a **capsule**, so the stroke
/// is exactly `2 * PEN_R` wide everywhere and the pen radius is a free
/// typographic choice rather than a connectivity constraint.
///
/// The tempting shortcut — take the union of discs and pick a radius large
/// enough that neighbours overlap — does not work, and the failure is subtle
/// enough to survive an inside/outside test. Two discs 1.0 apart with r = 0.52
/// do overlap, but their union is 1.04 units wide at each centre and only
/// 0.29 at the waist between them: a 3.6× pulse along every stroke. At label
/// sizes that is invisible; magnified to a 200 px gear numeral it is a chain
/// of beads with scalloped edges. Capsules remove the pulse entirely.
///
/// 0.36 gives a 0.72-unit stroke: confident enough to read at a glance on a
/// moving car, light enough that the counters in `0`, `8` and `B` stay open.
const PEN_R: f32 = 0.36;

/// Distance range the stored byte covers, in glyph units. Everything beyond
/// saturates. Four units is ample for the edge, plus room for outlines and
/// glows driven off the same sample.
const SPREAD: f32 = 4.0;

/// ASCII 32 (' ') through 90 ('Z'). Five columns per glyph; each byte's low
/// seven bits are the rows, top to bottom.
#[rustfmt::skip]
const GLYPHS: [[u8; 5]; 59] = [
    [0x00,0x00,0x00,0x00,0x00], // ' '
    [0x00,0x00,0x5F,0x00,0x00], // !
    [0x00,0x07,0x00,0x07,0x00], // "
    [0x14,0x7F,0x14,0x7F,0x14], // #
    [0x24,0x2A,0x7F,0x2A,0x12], // $
    [0x23,0x13,0x08,0x64,0x62], // %
    [0x36,0x49,0x55,0x22,0x50], // &
    [0x00,0x05,0x03,0x00,0x00], // '
    [0x00,0x1C,0x22,0x41,0x00], // (
    [0x00,0x41,0x22,0x1C,0x00], // )
    [0x14,0x08,0x3E,0x08,0x14], // *
    [0x08,0x08,0x3E,0x08,0x08], // +
    [0x00,0x50,0x30,0x00,0x00], // ,
    [0x08,0x08,0x08,0x08,0x08], // -
    [0x00,0x60,0x60,0x00,0x00], // .
    [0x20,0x10,0x08,0x04,0x02], // /
    [0x3E,0x51,0x49,0x45,0x3E], // 0
    [0x00,0x42,0x7F,0x40,0x00], // 1
    [0x42,0x61,0x51,0x49,0x46], // 2
    [0x21,0x41,0x45,0x4B,0x31], // 3
    [0x18,0x14,0x12,0x7F,0x10], // 4
    [0x27,0x45,0x45,0x45,0x39], // 5
    [0x3C,0x4A,0x49,0x49,0x30], // 6
    [0x01,0x71,0x09,0x05,0x03], // 7
    [0x36,0x49,0x49,0x49,0x36], // 8
    [0x06,0x49,0x49,0x29,0x1E], // 9
    [0x00,0x36,0x36,0x00,0x00], // :
    [0x00,0x56,0x36,0x00,0x00], // ;
    [0x08,0x14,0x22,0x41,0x00], // <
    [0x14,0x14,0x14,0x14,0x14], // =
    [0x00,0x41,0x22,0x14,0x08], // >
    [0x02,0x01,0x51,0x09,0x06], // ?
    [0x32,0x49,0x79,0x41,0x3E], // @
    [0x7E,0x11,0x11,0x11,0x7E], // A
    [0x7F,0x49,0x49,0x49,0x36], // B
    [0x3E,0x41,0x41,0x41,0x22], // C
    [0x7F,0x41,0x41,0x22,0x1C], // D
    [0x7F,0x49,0x49,0x49,0x41], // E
    [0x7F,0x09,0x09,0x09,0x01], // F
    [0x3E,0x41,0x49,0x49,0x7A], // G
    [0x7F,0x08,0x08,0x08,0x7F], // H
    [0x00,0x41,0x7F,0x41,0x00], // I
    [0x20,0x40,0x41,0x3F,0x01], // J
    [0x7F,0x08,0x14,0x22,0x41], // K
    [0x7F,0x40,0x40,0x40,0x40], // L
    [0x7F,0x02,0x0C,0x02,0x7F], // M
    [0x7F,0x04,0x08,0x10,0x7F], // N
    [0x3E,0x41,0x41,0x41,0x3E], // O
    [0x7F,0x09,0x09,0x09,0x06], // P
    [0x3E,0x41,0x51,0x21,0x5E], // Q
    [0x7F,0x09,0x19,0x29,0x46], // R
    [0x46,0x49,0x49,0x49,0x31], // S
    [0x01,0x01,0x7F,0x01,0x01], // T
    [0x3F,0x40,0x40,0x40,0x3F], // U
    [0x1F,0x20,0x40,0x20,0x1F], // V
    [0x3F,0x40,0x38,0x40,0x3F], // W
    [0x63,0x14,0x08,0x14,0x63], // X
    [0x07,0x08,0x70,0x08,0x07], // Y
    [0x61,0x51,0x49,0x45,0x43], // Z
];

/// A stroke: a capsule from `a` to `b`. A single lit cell is a degenerate
/// capsule (a == b), i.e. a disc.
type Stroke = ((f32, f32), (f32, f32));

/// Decompose a glyph into stroke primitives.
///
/// Every lit cell contributes a dot (a degenerate capsule, which rounds the
/// stroke ends), and **every** adjacent pair — orthogonal and diagonal — is
/// joined by a capsule. Linking orthogonals is not redundant: it is what makes
/// the stroke a constant-width ribbon instead of a chain of beads.
fn strokes(glyph: &[u8; 5]) -> Vec<Stroke> {
    let lit = |c: i32, r: i32| -> bool {
        (0..5).contains(&c)
            && (0..7).contains(&r)
            && (glyph[c as usize] >> r) & 1 == 1
    };
    let centre = |c: i32, r: i32| (c as f32 + 0.5, r as f32 + 0.5);

    let mut out = Vec::with_capacity(48);
    for c in 0..5i32 {
        for r in 0..7i32 {
            if !lit(c, r) {
                continue;
            }
            out.push((centre(c, r), centre(c, r)));

            // Join neighbours. Only the four "forward" directions, so each
            // pair is linked once rather than twice.
            for (dc, dr) in [(1i32, 0i32), (0, 1), (1, 1), (1, -1)] {
                if lit(c + dc, r + dr) {
                    out.push((centre(c, r), centre(c + dc, r + dr)));
                }
            }
        }
    }
    out
}

/// Signed distance from a point to a capsule of radius [`PEN_R`].
#[inline]
fn sd_capsule(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (pax, pay) = (p.0 - a.0, p.1 - a.1);
    let (bax, bay) = (b.0 - a.0, b.1 - a.1);
    let denom = bax * bax + bay * bay;
    // Project p onto the segment, clamped to its ends.
    let h = if denom > 1e-9 {
        ((pax * bax + pay * bay) / denom).clamp(0.0, 1.0)
    } else {
        0.0 // degenerate: the capsule is a disc
    };
    let dx = pax - bax * h;
    let dy = pay - bay * h;
    (dx * dx + dy * dy).sqrt() - PEN_R
}

/// Signed distance to the union of a glyph's strokes. Negative inside.
///
/// The union of SDFs is their minimum, and every primitive here has a closed
/// form — so this is analytically exact at every point, with no rasterisation
/// step to introduce error.
#[inline]
fn sd_glyph(p: (f32, f32), strokes: &[Stroke]) -> f32 {
    let mut best = f32::MAX;
    for &(a, b) in strokes {
        let d = sd_capsule(p, a, b);
        if d < best {
            best = d;
        }
    }
    best
}

/// Bake the atlas. Returns tightly packed R8 texels.
///
/// Encoding: `byte = 255 * clamp(0.5 - d / SPREAD, 0, 1)`, so the glyph edge
/// sits at exactly 0.5 and inside is greater. The shader thresholds there.
pub fn build_atlas() -> Vec<u8> {
    let mut px = vec![0u8; (ATLAS_W * ATLAS_H) as usize];

    // An empty cell must still read as "far outside", not as zero distance,
    // or the space character renders as a solid block.
    let empty = ((0.5f32 - CELL_UNITS_W / SPREAD).clamp(0.0, 1.0) * 255.0) as u8;
    px.fill(empty);

    for (i, glyph) in GLYPHS.iter().enumerate() {
        let d = strokes(glyph);
        if d.is_empty() {
            continue; // space: leave the "far outside" fill
        }
        let cell_index = i as u32;
        let cx0 = (cell_index % COLS) * CELL_W;
        let cy0 = (cell_index / COLS) * CELL_H;

        for ty in 0..CELL_H {
            for tx in 0..CELL_W {
                // Texel centre -> glyph space. The cell spans -PAD..INK+PAD.
                let gx = -PAD + (tx as f32 + 0.5) / CELL_W as f32 * CELL_UNITS_W;
                let gy = -PAD + (ty as f32 + 0.5) / CELL_H as f32 * CELL_UNITS_H;
                let dist = sd_glyph((gx, gy), &d);
                let encoded = (0.5 - dist / SPREAD).clamp(0.0, 1.0);
                px[((cy0 + ty) * ATLAS_W + cx0 + tx) as usize] = (encoded * 255.0) as u8;
            }
        }
    }
    px
}

/// Map a character to its atlas index, folding lowercase and substituting '?'.
/// Whether the atlas carries this character.
///
/// The atlas is the printable ASCII range up to `Z` — digits, capitals and
/// punctuation, which is every character an instrument cluster needs. Anything
/// else draws as `?`, which is how a stray `→` or `±` in a label turns into a
/// question mark on screen. [`DrawList`](crate::ui::DrawList) counts the
/// substitutions so a test can catch it instead of a screenshot.
pub fn is_renderable(c: char) -> bool {
    let up = c.to_ascii_uppercase() as u32;
    (32..=90).contains(&up)
}

pub fn glyph_index(c: char) -> f32 {
    let up = c.to_ascii_uppercase() as u32;
    if (32..=90).contains(&up) {
        up as f32
    } else {
        b'?' as f32
    }
}

/// Layout metrics for one glyph, derived from a requested cap height.
///
/// `size` is the **ink** height (the 7-unit box), which is what a caller means
/// by "20 px text". The drawn quad is larger, because it must include the
/// padding the distance field lives in — get this wrong and glyphs are clipped
/// at their extremities exactly where the antialiasing lives.
pub struct Metrics {
    /// Quad width in pixels, padding included.
    pub quad_w: f32,
    /// Quad height in pixels, padding included.
    pub quad_h: f32,
    /// Padding on every side, in pixels. Subtract from the pen position to get
    /// the quad's top-left corner.
    pub pad: f32,
    /// Width of the ink itself, for measuring and centring.
    pub ink_w: f32,
    /// Pen advance to the next glyph.
    pub advance: f32,
}

pub fn metrics(size: f32) -> Metrics {
    let unit = size / INK_H;
    Metrics {
        quad_w: unit * CELL_UNITS_W,
        quad_h: unit * CELL_UNITS_H,
        pad: unit * PAD,
        ink_w: unit * INK_W,
        // 5 units of ink plus 1.25 of tracking reads as comfortably spaced
        // without looking loose at instrument sizes.
        advance: unit * (INK_W + 1.25),
    }
}

/// Advance width of a whole string, excluding the trailing tracking.
pub fn text_width(size: f32, s: &str) -> f32 {
    let n = s.chars().count();
    if n == 0 {
        return 0.0;
    }
    let m = metrics(size);
    m.advance * (n - 1) as f32 + m.ink_w
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the atlas once for the whole test module. Rebuilding it per
    /// glyph made `diagonal_strokes_stay_connected` take 7 seconds.
    fn atlas() -> &'static [u8] {
        static ATLAS: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
        ATLAS.get_or_init(build_atlas)
    }

    fn cell_of(code: u32) -> Vec<u8> {
        let a = atlas();
        let ci = code - 32;
        let cx0 = (ci % COLS) * CELL_W;
        let cy0 = (ci / COLS) * CELL_H;
        let mut out = Vec::with_capacity((CELL_W * CELL_H) as usize);
        for y in cy0..cy0 + CELL_H {
            for x in cx0..cx0 + CELL_W {
                out.push(a[(y * ATLAS_W + x) as usize]);
            }
        }
        out
    }

    #[test]
    fn atlas_is_the_expected_size() {
        assert_eq!(build_atlas().len(), (ATLAS_W * ATLAS_H) as usize);
    }

    #[test]
    fn glyph_interiors_are_above_the_threshold_and_exteriors_below() {
        // '0' has ink at its left edge and a hole in the middle.
        let cell = cell_of(b'0' as u32);
        let at = |gx: f32, gy: f32| -> u8 {
            let tx = ((gx + PAD) / CELL_UNITS_W * CELL_W as f32) as u32;
            let ty = ((gy + PAD) / CELL_UNITS_H * CELL_H as f32) as u32;
            cell[(ty.min(CELL_H - 1) * CELL_W + tx.min(CELL_W - 1)) as usize]
        };
        assert!(at(0.5, 3.5) > 128, "left stroke of '0' should be inside");
        assert!(at(4.5, 3.5) > 128, "right stroke of '0' should be inside");
        // Note: this is a *slashed* zero — the centre column is lit by the
        // diagonal — so the counter is sampled off the slash, not on it.
        assert!(at(1.5, 2.5) < 128, "the counter of '0' should be outside");
        assert!(at(-0.9, 3.5) < 128, "padding should be outside");
    }

    #[test]
    fn counters_are_open_not_merely_technically_outside() {
        // The failure mode this guards is a font that passes an inside/outside
        // test but looks clogged: if the pen is too fat the hole in '0' clears
        // by a hair and reads as a smudge when magnified to 200 px.
        let cell = cell_of(b'0' as u32);
        let at = |gx: f32, gy: f32| -> u8 {
            let tx = ((gx + PAD) / CELL_UNITS_W * CELL_W as f32) as u32;
            let ty = ((gy + PAD) / CELL_UNITS_H * CELL_H as f32) as u32;
            cell[(ty.min(CELL_H - 1) * CELL_W + tx.min(CELL_W - 1)) as usize]
        };
        // Decoded distance at the centre of the counter, in glyph units.
        let v = at(1.5, 2.5) as f32 / 255.0;
        let dist_units = (0.5 - v) * SPREAD;
        assert!(
            dist_units > 0.35,
            "counter clears by only {dist_units:.2} units; the pen is too fat"
        );
    }

    #[test]
    fn space_reads_as_far_outside_everywhere() {
        // The bug this guards: a zero-filled cell decodes as distance 0, i.e.
        // exactly on the edge, and every space renders as a solid block.
        for v in cell_of(b' ' as u32) {
            assert!(v < 128, "space texel {v} is not outside the glyph");
        }
    }

    #[test]
    fn the_field_is_monotonic_moving_away_from_ink() {
        // Walking left from the left stroke of 'I' (a single centre column)
        // must give monotonically decreasing "insideness".
        let cell = cell_of(b'I' as u32);
        let sample = |gx: f32| -> u8 {
            let tx = ((gx + PAD) / CELL_UNITS_W * CELL_W as f32) as u32;
            let ty = ((3.5 + PAD) / CELL_UNITS_H * CELL_H as f32) as u32;
            cell[(ty * CELL_W + tx.min(CELL_W - 1)) as usize]
        };
        let mut prev = 255u8;
        for step in 0..12 {
            let v = sample(2.5 - step as f32 * 0.25);
            assert!(v <= prev, "field not monotonic at step {step}: {v} > {prev}");
            prev = v;
        }
    }

    #[test]
    fn every_adjacency_is_bridged() {
        // Check the midpoint of every adjacency in every glyph. A missing link
        // shows up as a gap (diagonals) or a waist (orthogonals) once the
        // glyph is magnified.
        for (i, glyph) in GLYPHS.iter().enumerate() {
            let code = 32 + i as u32;
            let cell = cell_of(code);
            let at = |gx: f32, gy: f32| -> u8 {
                let tx = ((gx + PAD) / CELL_UNITS_W * CELL_W as f32) as u32;
                let ty = ((gy + PAD) / CELL_UNITS_H * CELL_H as f32) as u32;
                cell[(ty.min(CELL_H - 1) * CELL_W + tx.min(CELL_W - 1)) as usize]
            };
            let lit = |c: i32, r: i32| {
                (0..5).contains(&c) && (0..7).contains(&r) && (glyph[c as usize] >> r) & 1 == 1
            };
            for c in 0..5i32 {
                for r in 0..7i32 {
                    if !lit(c, r) {
                        continue;
                    }
                    for (dc, dr) in [(1i32, 0i32), (0, 1), (1, 1), (1, -1)] {
                        if !lit(c + dc, r + dr) {
                            continue;
                        }
                        let mx = c as f32 + 0.5 + dc as f32 * 0.5;
                        let my = r as f32 + 0.5 + dr as f32 * 0.5;
                        assert!(
                            at(mx, my) > 128,
                            "glyph {:?}: link ({c},{r})->({},{}) is not bridged",
                            code as u8 as char,
                            c + dc,
                            r + dr
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn metrics_place_ink_inside_the_quad() {
        let m = metrics(70.0);
        assert!((m.quad_h - 90.0).abs() < 0.01, "9 units at 10 px/unit");
        assert!((m.quad_w - 70.0).abs() < 0.01, "7 units at 10 px/unit");
        assert!((m.pad - 10.0).abs() < 0.01, "1 unit of padding per side");
        assert!((m.ink_w - 50.0).abs() < 0.01, "5 units of ink");
        assert!(m.advance > m.ink_w, "tracking must add space after the ink");
        // The ink plus both pads is exactly the quad.
        assert!((m.ink_w + m.pad * 2.0 - m.quad_w).abs() < 0.01);
    }

    #[test]
    fn text_width_matches_laid_out_glyphs() {
        let size = 20.0;
        let m = metrics(size);
        assert!((text_width(size, "A") - m.ink_w).abs() < 0.01, "one glyph is its ink");
        assert!(
            (text_width(size, "AB") - (m.advance + m.ink_w)).abs() < 0.01,
            "two glyphs are one advance plus one ink"
        );
        assert_eq!(text_width(size, ""), 0.0);
    }

    #[test]
    fn lowercase_folds_to_uppercase() {
        assert_eq!(glyph_index('a'), glyph_index('A'));
        assert_eq!(glyph_index('~'), b'?' as f32);
    }
}
