//! The scalar geometry library from SQLite's `geopoly` extension
//! (`ext/rtree/geopoly.c`), byte-compatible with the real `sqlite3` CLI.
//!
//! This module ports the polygon representation, the lenient GeoJSON / BLOB
//! parser, and every scalar helper (`area`, `bbox`, `ccw`, `regular`,
//! `contains_point`, `overlap`, `within`, `xform`, and the rendering to JSON /
//! SVG / BLOB). The `geopoly` *virtual table* is intentionally not implemented —
//! only the scalar functions and the `geopoly_group_bbox` aggregate, which live
//! in [`crate::exec::func`] and [`crate::exec::mod`] respectively and call into
//! the helpers here.
//!
//! Coordinates are stored as `f32` (SQLite's `GeoCoord = float`); geometric math
//! widens each back to `f64`. Coordinate text in `geopoly_json` is rendered with
//! SQLite's `%!g` formatter applied to the widened value, reusing graphite's
//! printf so the output is byte-identical.

use crate::value::Value;
use alloc::string::String;
use alloc::vec::Vec;

/// A parsed polygon: `nVertex` distinct vertices (the closing repeat that a JSON
/// ring carries is dropped on parse). `xy[2*i]` / `xy[2*i+1]` are vertex `i`'s
/// X and Y, matching SQLite's `GeoX`/`GeoY` layout.
#[derive(Clone)]
pub(crate) struct GeoPoly {
    pub xy: Vec<f32>,
}

impl GeoPoly {
    fn n_vertex(&self) -> usize {
        self.xy.len() / 2
    }
    fn x(&self, i: usize) -> f32 {
        self.xy[i * 2]
    }
    fn y(&self, i: usize) -> f32 {
        self.xy[i * 2 + 1]
    }

    /// Serialize to the on-disk geopoly BLOB: `0x01` marker, 24-bit big-endian
    /// vertex count, then `2*N` little-endian f32 coordinates (closing vertex
    /// omitted).
    pub(crate) fn to_blob(&self) -> Vec<u8> {
        let n = self.n_vertex();
        let mut out = Vec::with_capacity(4 + 8 * n);
        out.push(0x01);
        out.push(((n >> 16) & 0xff) as u8);
        out.push(((n >> 8) & 0xff) as u8);
        out.push((n & 0xff) as u8);
        for &c in &self.xy {
            out.extend_from_slice(&c.to_le_bytes());
        }
        out
    }

    /// Render the closed JSON ring `[[x,y],...,[x0,y0]]`. Each coordinate is
    /// `%!g` of the value, exactly as SQLite's `geopoly_json` does.
    pub(crate) fn to_json(&self) -> String {
        let n = self.n_vertex();
        let mut s = String::from("[");
        for i in 0..n {
            s.push('[');
            s.push_str(&fmt_bang_g(self.x(i)));
            s.push(',');
            s.push_str(&fmt_bang_g(self.y(i)));
            s.push_str("],");
        }
        // Closing vertex is a repeat of the first.
        s.push('[');
        s.push_str(&fmt_bang_g(self.x(0)));
        s.push(',');
        s.push_str(&fmt_bang_g(self.y(0)));
        s.push_str("]]");
        s
    }

    /// Render as an SVG `<polyline>`. Coordinates use plain `%g` (no forced
    /// `.0`). `extra` holds the already-textified trailing arguments; each
    /// non-empty one is appended space-separated, mirroring SQLite.
    pub(crate) fn to_svg(&self, extra: &[Option<String>]) -> String {
        let n = self.n_vertex();
        let mut s = String::from("<polyline points=");
        let mut sep = '\'';
        for i in 0..n {
            s.push(sep);
            s.push_str(&fmt_g(self.x(i)));
            s.push(',');
            s.push_str(&fmt_g(self.y(i)));
            sep = ' ';
        }
        s.push(' ');
        s.push_str(&fmt_g(self.x(0)));
        s.push(',');
        s.push_str(&fmt_g(self.y(0)));
        s.push('\'');
        for z in extra.iter().flatten() {
            if !z.is_empty() {
                s.push(' ');
                s.push_str(z);
            }
        }
        s.push_str("></polyline>");
        s
    }

    /// Signed enclosed area (SQLite's `geopolyArea`): the trapezoid sum
    /// `Σ (x_i − x_{i+1})·(y_i + y_{i+1})·0.5`. Positive for CCW, negative for
    /// CW winding.
    ///
    /// Precision detail: in C, `float − float` and `float + float` stay `float`
    /// (the usual arithmetic conversions do not widen to `double`), and only the
    /// `* 0.5` promotes the `float` product to `double`. Reproduce that exactly —
    /// compute each `(x0−x1)`, `(y0+y1)` and their product in f32, then widen for
    /// the `* 0.5` and the accumulation — otherwise the last few digits diverge.
    pub(crate) fn area(&self) -> f64 {
        let n = self.n_vertex();
        let mut area = 0.0f64;
        for i in 0..n - 1 {
            let term = (self.x(i) - self.x(i + 1)) * (self.y(i) + self.y(i + 1));
            area += term as f64 * 0.5;
        }
        // Final segment: last vertex back to the first.
        let term = (self.x(n - 1) - self.x(0)) * (self.y(n - 1) + self.y(0));
        area += term as f64 * 0.5;
        area
    }

    /// Return a copy wound counter-clockwise: if the signed area is negative the
    /// vertices `1..N` are reversed in place (vertex 0 stays put), matching
    /// SQLite's `geopoly_ccw`.
    pub(crate) fn ccw(&self) -> GeoPoly {
        let mut p = self.clone();
        if p.area() < 0.0 {
            let n = p.n_vertex();
            let (mut ii, mut jj) = (1usize, n - 1);
            while ii < jj {
                p.xy.swap(ii * 2, jj * 2);
                p.xy.swap(ii * 2 + 1, jj * 2 + 1);
                ii += 1;
                jj -= 1;
            }
        }
        p
    }

    /// Axis-aligned bounding box as a CCW rectangle GeoPoly:
    /// (minx,miny),(maxx,miny),(maxx,maxy),(minx,maxy).
    pub(crate) fn bbox(&self) -> GeoPoly {
        let (mnx, mxx, mny, mxy) = self.bbox_coords();
        GeoPoly::from_bbox(mnx, mxx, mny, mxy)
    }

    /// The raw (minx, maxx, miny, maxy) bounds, computed with the same
    /// f64-widening / `else if` comparison chain as SQLite so ties resolve
    /// identically.
    pub(crate) fn bbox_coords(&self) -> (f32, f32, f32, f32) {
        let n = self.n_vertex();
        let mut mnx = self.x(0);
        let mut mxx = self.x(0);
        let mut mny = self.y(0);
        let mut mxy = self.y(0);
        for ii in 1..n {
            let r = self.x(ii) as f64;
            if r < mnx as f64 {
                mnx = r as f32;
            } else if r > mxx as f64 {
                mxx = r as f32;
            }
            let r = self.y(ii) as f64;
            if r < mny as f64 {
                mny = r as f32;
            } else if r > mxy as f64 {
                mxy = r as f32;
            }
        }
        (mnx, mxx, mny, mxy)
    }

    /// Build the CCW bounding-box rectangle from explicit bounds.
    pub(crate) fn from_bbox(mnx: f32, mxx: f32, mny: f32, mxy: f32) -> GeoPoly {
        GeoPoly {
            xy: alloc::vec![mnx, mny, mxx, mny, mxx, mxy, mnx, mxy],
        }
    }

    /// Apply the affine transform `x1 = A*x + B*y + E`, `y1 = C*x + D*y + F`
    /// (SQLite's `geopoly_xform`), computing each result in f64 then casting to
    /// f32.
    pub(crate) fn xform(&self, a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> GeoPoly {
        let n = self.n_vertex();
        let mut out = GeoPoly {
            xy: self.xy.clone(),
        };
        for ii in 0..n {
            let x0 = self.x(ii) as f64;
            let y0 = self.y(ii) as f64;
            out.xy[ii * 2] = (a * x0 + b * y0 + e) as f32;
            out.xy[ii * 2 + 1] = (c * x0 + d * y0 + f) as f32;
        }
        out
    }

    /// `geopoly_contains_point`: +2 inside, +1 on the boundary, 0 outside — via
    /// SQLite's ray/parity test over `point_beneath_line`.
    pub(crate) fn contains_point(&self, x0: f64, y0: f64) -> i64 {
        let n = self.n_vertex();
        let mut v = 0;
        let mut cnt = 0i64;
        let mut ii = 0usize;
        while ii < n - 1 {
            v = point_beneath_line(
                x0,
                y0,
                self.x(ii) as f64,
                self.y(ii) as f64,
                self.x(ii + 1) as f64,
                self.y(ii + 1) as f64,
            );
            if v == 2 {
                break;
            }
            cnt += v;
            ii += 1;
        }
        if v != 2 {
            v = point_beneath_line(
                x0,
                y0,
                self.x(ii) as f64,
                self.y(ii) as f64,
                self.x(0) as f64,
                self.y(0) as f64,
            );
        }
        if v == 2 {
            1
        } else if ((v + cnt) & 1) == 0 {
            0
        } else {
            2
        }
    }
}

/// Construct a regular `n`-gon centered at (x,y) with circumradius `r`, matching
/// SQLite's `geopoly_regular`. Returns `None` when `n < 3` or `r <= 0`; `n` is
/// clamped to 1000.
pub(crate) fn regular(x: f64, y: f64, r: f64, n: i64) -> Option<GeoPoly> {
    if n < 3 || r <= 0.0 {
        return None;
    }
    let n = if n > 1000 { 1000 } else { n } as usize;
    let mut xy = Vec::with_capacity(2 * n);
    for i in 0..n {
        let r_angle = 2.0 * GEOPOLY_PI * (i as f64) / (n as f64);
        let px = x - r * geopoly_sine(r_angle - 0.5 * GEOPOLY_PI);
        let py = y + r * geopoly_sine(r_angle);
        xy.push(px as f32);
        xy.push(py as f32);
    }
    Some(GeoPoly { xy })
}

/// SQLite's `GEOPOLY_PI` literal. Using `core::f64::consts::PI` instead would
/// diverge in the last bits from SQLite's hard-coded constant, so keep the exact
/// text (hence the `approx_constant` allow).
#[allow(clippy::approx_constant, clippy::excessive_precision)]
const GEOPOLY_PI: f64 = 3.1415926535897932385;

/// SQLite's `geopolySine`: a polynomial sine approximation valid for
/// `-0.5*pi <= r <= 2*pi`. This exact polynomial (not `libm`) is what makes
/// `geopoly_regular` byte-reproducible.
fn geopoly_sine(mut r: f64) -> f64 {
    if r >= 1.5 * GEOPOLY_PI {
        r -= 2.0 * GEOPOLY_PI;
    }
    if r >= 0.5 * GEOPOLY_PI {
        -geopoly_sine(r - GEOPOLY_PI)
    } else {
        let r2 = r * r;
        let r3 = r2 * r;
        let r5 = r3 * r2;
        0.9996949 * r - 0.1656700 * r3 + 0.0075134 * r5
    }
}

/// SQLite's `pointBeneathLine`: whether (x0,y0) lies on (+2), below (+1), or
/// neither (0) the segment (x1,y1)->(x2,y2). The left-most X endpoint is not
/// part of the segment.
fn point_beneath_line(x0: f64, y0: f64, x1: f64, y1: f64, x2: f64, y2: f64) -> i64 {
    if x0 == x1 && y0 == y1 {
        return 2;
    }
    if x1 < x2 {
        if x0 <= x1 || x0 > x2 {
            return 0;
        }
    } else if x1 > x2 {
        if x0 <= x2 || x0 > x1 {
            return 0;
        }
    } else {
        // Vertical segment.
        if x0 != x1 {
            return 0;
        }
        if y0 < y1 && y0 < y2 {
            return 0;
        }
        if y0 > y1 && y0 > y2 {
            return 0;
        }
        return 2;
    }
    let y = y1 + (y2 - y1) * (x0 - x1) / (x2 - x1);
    if y0 == y {
        2
    } else if y0 < y {
        1
    } else {
        0
    }
}

/// Render a coordinate with SQLite's `%!g` (trim trailing zeros but keep one
/// fractional digit), by reusing graphite's printf so it is byte-identical.
fn fmt_bang_g(c: f32) -> String {
    match crate::exec::datetime::printf(&[Value::Text(String::from("%!g")), Value::Real(c as f64)])
    {
        Value::Text(s) => s,
        _ => String::new(),
    }
}

/// Render a coordinate with plain `%g` (used by SVG, no forced `.0`).
fn fmt_g(c: f32) -> String {
    match crate::exec::datetime::printf(&[Value::Text(String::from("%g")), Value::Real(c as f64)]) {
        Value::Text(s) => s,
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Interpret a SQL value as a polygon, accepting either the geopoly BLOB format
/// or GeoJSON text. Returns `None` for any other type or a malformed polygon —
/// the geopoly functions surface that as SQL NULL.
pub(crate) fn parse_value(v: &Value) -> Option<GeoPoly> {
    match v {
        Value::Blob(b) => parse_blob(b),
        Value::Text(s) => parse_json(s.as_bytes()),
        _ => None,
    }
}

/// The `geopoly_group_bbox` step outcome for one input value, mirroring the
/// interaction of `geopolyFuncParam` / `geopolyParseJson` / `geopolyBBox` with
/// the step function's `rc` check.
pub(crate) enum BBoxStep {
    /// A valid polygon — fold its bounding box into the accumulator.
    Poly(GeoPoly),
    /// Parse produced no polygon but left `rc == SQLITE_OK` (SQLite's
    /// `geopolyParseJson` only sets an error once it has seen the opening `[`;
    /// text that never starts with `[` returns rc OK). `geopolyBBox` then
    /// zero-fills the coordinate array, so the step folds in an all-zero bbox.
    ZeroBox,
    /// Parse set `rc != SQLITE_OK` (a NULL/non-text value, a bad BLOB, or a
    /// bracket-opened-but-malformed ring); the step skips the row entirely.
    Skip,
}

/// Classify one `geopoly_group_bbox` input, reproducing SQLite's `rc` semantics
/// exactly (see [`BBoxStep`]).
pub(crate) fn bbox_step(v: &Value) -> BBoxStep {
    match v {
        Value::Blob(b) => match parse_blob(b) {
            Some(p) => BBoxStep::Poly(p),
            // A malformed BLOB leaves rc SQLITE_OK in geopolyFuncParam's BLOB
            // branch, but `p` is NULL — however a too-short BLOB never reaches
            // that branch and stays SQLITE_ERROR (the `else` type path). SQLite
            // skips both: a NULL `p` with rc OK still zero-fills only when the
            // *value type* was recognised. Empirically every invalid BLOB is
            // skipped, so treat all BLOB parse failures as Skip.
            None => BBoxStep::Skip,
        },
        Value::Text(s) => {
            let bytes = s.as_bytes();
            match parse_json(bytes) {
                Some(p) => BBoxStep::Poly(p),
                None => {
                    // geopolyParseJson sets rc=SQLITE_ERROR only after consuming
                    // the opening `[`. Text whose first non-space byte is not `[`
                    // returns with rc still SQLITE_OK -> a zero bbox.
                    let mut i = 0;
                    while i < bytes.len() && is_geo_space(bytes[i]) {
                        i += 1;
                    }
                    if bytes.get(i) == Some(&b'[') {
                        BBoxStep::Skip
                    } else {
                        BBoxStep::ZeroBox
                    }
                }
            }
        }
        // A NULL or numeric value is the `else` type path: rc=SQLITE_ERROR, skip.
        _ => BBoxStep::Skip,
    }
}

/// Decode a geopoly BLOB. The header is `enc(1) + nVertex(3 big-endian)`, and
/// the byte length must be exactly `4 + 8*N`. `enc == 0` means big-endian
/// floats (byte-swapped), `enc == 1` little-endian.
fn parse_blob(a: &[u8]) -> Option<GeoPoly> {
    // Minimum is a 4-byte header plus 6 coordinates (matches SQLite's guard).
    if a.len() < 4 + 6 * 4 {
        return None;
    }
    let n_vertex = ((a[1] as usize) << 16) + ((a[2] as usize) << 8) + a[3] as usize;
    if (a[0] != 0 && a[0] != 1) || (n_vertex * 2 * 4 + 4) != a.len() {
        return None;
    }
    let little_endian = a[0] == 1;
    let mut xy = Vec::with_capacity(n_vertex * 2);
    for i in 0..n_vertex * 2 {
        let off = 4 + i * 4;
        let bytes = [a[off], a[off + 1], a[off + 2], a[off + 3]];
        let f = if little_endian {
            f32::from_le_bytes(bytes)
        } else {
            f32::from_be_bytes(bytes)
        };
        xy.push(f);
    }
    Some(GeoPoly { xy })
}

/// A GeoJSON parser mirroring SQLite's `geopolyParseJson` byte-for-byte,
/// including its leniencies: extra whitespace, a trailing comma before `]`,
/// and more than two numbers per coordinate (extras past the first two are
/// consumed but ignored). The ring must be closed (first == last vertex) and
/// have at least four coordinate pairs; the closing repeat is dropped.
fn parse_json(z: &[u8]) -> Option<GeoPoly> {
    let mut p = Parser { z, pos: 0 };
    let mut verts: Vec<f32> = Vec::new(); // flattened x,y,x,y,...
    if p.skip_space() != b'[' {
        return None;
    }
    p.pos += 1;
    while p.skip_space() == b'[' {
        let mut ii = 0;
        p.pos += 1;
        // A fresh vertex slot; extra coordinates past the pair overwrite slot 0/1
        // targets that are ignored, so we only ever push two values per vertex.
        let mut vx = 0.0f32;
        let mut vy = 0.0f32;
        let mut have_vertex = false;
        loop {
            let mut val = 0.0f32;
            if !p.parse_number(&mut val) {
                break;
            }
            if ii == 0 {
                vx = val;
            } else if ii == 1 {
                vy = val;
            }
            ii += 1;
            if ii == 2 {
                verts.push(vx);
                verts.push(vy);
                have_vertex = true;
            }
            let c = p.skip_space();
            p.pos += 1;
            if c == b',' {
                continue;
            }
            if c == b']' && ii >= 2 {
                break;
            }
            // Malformed coordinate.
            let _ = have_vertex;
            return None;
        }
        if p.skip_space() == b',' {
            p.pos += 1;
            continue;
        }
        break;
    }
    let n_vertex = verts.len() / 2;
    if p.skip_space() == b']'
        && n_vertex >= 4
        && verts[0] == verts[(n_vertex - 1) * 2]
        && verts[1] == verts[(n_vertex - 1) * 2 + 1]
    {
        p.pos += 1;
        if p.skip_space() != 0 {
            return None;
        }
        // Drop the redundant closing vertex.
        verts.truncate((n_vertex - 1) * 2);
        Some(GeoPoly { xy: verts })
    } else {
        None
    }
}

struct Parser<'a> {
    z: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    /// The byte at the current position, or 0 (NUL) past the end — SQLite reads
    /// a NUL-terminated buffer, so out-of-range reads behave as end-of-string.
    fn at(&self, i: usize) -> u8 {
        self.z.get(i).copied().unwrap_or(0)
    }

    /// Skip SQLite's geopoly whitespace set (space, `\t`, `\n`, `\r`) and return
    /// the next byte.
    fn skip_space(&mut self) -> u8 {
        while is_geo_space(self.at(self.pos)) {
            self.pos += 1;
        }
        self.at(self.pos)
    }

    /// Port of `geopolyParseNumber`: recognise a JSON number at the current
    /// position, write its f32 value into `*out`, and advance. Returns false if
    /// the next token is not a well-formed number. Leading zeros, a bare leading
    /// `.`, a `-.`, and a doubled/naked exponent are all rejected, matching
    /// SQLite.
    fn parse_number(&mut self, out: &mut f32) -> bool {
        let c = self.skip_space();
        let start = self.pos;
        // Signed offset into the buffer relative to `start`, so `z(-1)` reads the
        // byte before the number (SQLite's `z[j-1]` on a NUL-terminated buffer);
        // out-of-range reads yield 0, matching the terminator.
        let z = |j: isize| -> u8 {
            let idx = start as isize + j;
            if idx < 0 {
                0
            } else {
                self.at(idx as usize)
            }
        };
        let mut j = 0isize;
        let mut seen_dp = false;
        let mut seen_e = false;
        let mut c = c;
        if c == b'-' {
            j = 1;
            c = z(j);
        }
        // Reject a leading zero followed by another digit (e.g. "00", "01").
        if c == b'0' && z(j + 1).is_ascii_digit() {
            return false;
        }
        loop {
            c = z(j);
            if c.is_ascii_digit() {
                j += 1;
                continue;
            }
            if c == b'.' {
                // A '.' immediately after '-' ("-.") is not a number.
                if z(j - 1) == b'-' {
                    return false;
                }
                if seen_dp {
                    return false;
                }
                seen_dp = true;
                j += 1;
                continue;
            }
            if c == b'e' || c == b'E' {
                // The character before the exponent must be a digit.
                if z(j - 1) < b'0' {
                    return false;
                }
                if seen_e {
                    // SQLite returns -1 here (a hard error); for our purposes any
                    // non-success means "not a number".
                    return false;
                }
                seen_dp = true;
                seen_e = true;
                let mut cc = z(j + 1);
                if cc == b'+' || cc == b'-' {
                    j += 1;
                    cc = z(j + 1);
                }
                if !cc.is_ascii_digit() {
                    return false;
                }
                j += 1;
                continue;
            }
            break;
        }
        // The last consumed character must be a digit.
        if z(j - 1) < b'0' {
            return false;
        }
        let j = j as usize;
        // Parse the number the way SQLite does: atof over the remaining buffer
        // from the first non-space character, cast to f32. Feeding the exact
        // token substring to Rust's f64 parser is equivalent for well-formed
        // tokens (which this is, per the scan above).
        let tok = &self.z[start..start + j];
        // tok is ASCII digits/sign/dot/exponent only.
        let Ok(s) = core::str::from_utf8(tok) else {
            return false;
        };
        let Ok(r) = s.parse::<f64>() else {
            return false;
        };
        *out = r as f32;
        self.pos = start + j;
        true
    }
}

fn is_geo_space(c: u8) -> bool {
    // SQLite's geopolyIsSpace table: space (0x20), '\t' (0x09), '\n' (0x0a),
    // '\r' (0x0d).
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

// ---------------------------------------------------------------------------
// Overlap / within
// ---------------------------------------------------------------------------

/// Determine the overlap relationship between two polygons, returning SQLite's
/// `geopolyOverlap` code:
///
/// * 0 — disjoint (edge-touching only counts as disjoint)
/// * 1 — the boundaries cross / partial overlap
/// * 2 — `p1` is completely contained within `p2`
/// * 3 — `p2` is completely contained within `p1`
/// * 4 — `p1` and `p2` are the same polygon
///
/// This is a direct port of the sweep-line algorithm: build add/remove events
/// for every non-vertical edge sorted by X, and at each distinct X sweep the
/// active edges (sorted by Y then slope), detecting boundary crossings and
/// tracking, per side-mask, whether a gap between differently-masked edges was
/// ever seen.
pub(crate) fn overlap(p1: &GeoPoly, p2: &GeoPoly) -> i64 {
    let mut segs: Vec<Segment> = Vec::new();
    let mut events: Vec<Event> = Vec::new();
    add_segments(&mut segs, &mut events, p1, 1);
    add_segments(&mut segs, &mut events, p2, 2);

    // Sort events by X into a list, reproducing SQLite's bottom-up merge sort
    // (`geopolySortEventsByX`) exactly — its tie-break at equal X (the "right"
    // / newer operand wins on `<=`) is load-bearing for the sweep, so a plain
    // stable sort is not sufficient.
    let order = sort_events_by_x(&events);

    // `active` is the sweep status as an ordered list of segment indices.
    let mut active: Vec<usize> = Vec::new();
    let mut a_overlap = [0u8; 4];
    let mut need_sort = false;
    let mut r_x = if let Some(&first) = order.first() {
        if events[first].x == 0.0 {
            -1.0
        } else {
            0.0
        }
    } else {
        0.0
    };

    let mut ei = 0usize;
    while ei < order.len() {
        let ev = events[order[ei]];
        if ev.x != r_x {
            r_x = ev.x;
            if need_sort {
                sort_active_by_y_c(&mut active, &segs);
                need_sort = false;
            }
            // First pass: mask over the current y values (before recompute).
            let mut prev: Option<usize> = None;
            let mut i_mask = 0usize;
            for &si in &active {
                if let Some(pi) = prev {
                    if segs[pi].y != segs[si].y {
                        a_overlap[i_mask] = 1;
                    }
                }
                i_mask ^= segs[si].side as usize;
                prev = Some(si);
            }
            // Second pass: recompute each segment's y at r_x, detect crossings.
            prev = None;
            i_mask = 0;
            for &si in &active {
                let y = segs[si].c * r_x + segs[si].b;
                segs[si].y = y;
                if let Some(pi) = prev {
                    if segs[pi].y > segs[si].y && segs[pi].side != segs[si].side {
                        return 1;
                    } else if segs[pi].y != segs[si].y {
                        a_overlap[i_mask] = 1;
                    }
                }
                i_mask ^= segs[si].side as usize;
                prev = Some(si);
            }
        }
        if ev.etype == 0 {
            // Add a segment: reset its y to y0 and prepend to the active list.
            let si = ev.seg;
            segs[si].y = segs[si].y0 as f64;
            active.insert(0, si);
            need_sort = true;
        } else {
            // Remove a segment.
            if let Some(pos) = active.iter().position(|&s| s == ev.seg) {
                active.remove(pos);
            }
        }
        ei += 1;
    }

    if a_overlap[3] == 0 {
        0
    } else if a_overlap[1] != 0 && a_overlap[2] == 0 {
        3
    } else if a_overlap[1] == 0 && a_overlap[2] != 0 {
        2
    } else if a_overlap[1] == 0 && a_overlap[2] == 0 {
        4
    } else {
        1
    }
}

/// `geopoly_within(P1,P2)`: derived from `overlap` — 2 when equal, 1 when P2 is
/// contained in P1 (overlap code 2 in SQLite's within mapping)… actually
/// mirrors SQLite exactly: within = (overlap==2 ? 1 : overlap==4 ? 2 : 0).
pub(crate) fn within(p1: &GeoPoly, p2: &GeoPoly) -> i64 {
    let x = overlap(p1, p2);
    if x == 2 {
        1
    } else if x == 4 {
        2
    } else {
        0
    }
}

#[derive(Clone, Copy)]
struct Event {
    x: f64,
    etype: u8, // 0 = add, 1 = remove
    seg: usize,
}

struct Segment {
    c: f64,  // slope
    b: f64,  // intercept: y = C*x + B
    y: f64,  // current y
    y0: f32, // initial y
    side: u8,
    // `idx` (segment index within its side) is unused for the return value;
    // only side and geometry matter.
}

/// Add every non-vertical edge of `poly` as a segment with its add/remove
/// events, exactly as SQLite's `geopolyAddSegments` + `geopolyAddOneSegment`.
fn add_segments(segs: &mut Vec<Segment>, events: &mut Vec<Event>, poly: &GeoPoly, side: u8) {
    let n = poly.n_vertex();
    for i in 0..n - 1 {
        add_one_segment(
            segs,
            events,
            poly.x(i),
            poly.y(i),
            poly.x(i + 1),
            poly.y(i + 1),
            side,
        );
    }
    // Closing edge: last vertex back to the first.
    add_one_segment(
        segs,
        events,
        poly.x(n - 1),
        poly.y(n - 1),
        poly.x(0),
        poly.y(0),
        side,
    );
}

fn add_one_segment(
    segs: &mut Vec<Segment>,
    events: &mut Vec<Event>,
    mut x0: f32,
    mut y0: f32,
    mut x1: f32,
    mut y1: f32,
    side: u8,
) {
    if x0 == x1 {
        return; // Ignore vertical segments.
    }
    if x0 > x1 {
        core::mem::swap(&mut x0, &mut x1);
        core::mem::swap(&mut y0, &mut y1);
    }
    // Precision detail: SQLite computes the slope as `(y1-y0)/(x1-x0)` entirely
    // in `float` (GeoCoord) and only then widens to the `double` field `C`. The
    // intercept `B = y1 - x1*C` promotes the `float` y1/x1 to double against the
    // double C. Reproduce that exactly — a naive all-f64 slope diverges in the
    // last ULP and can turn an exact containment into a spurious crossing.
    let c = ((y1 - y0) / (x1 - x0)) as f64;
    let b = y1 as f64 - x1 as f64 * c;
    let si = segs.len();
    segs.push(Segment {
        c,
        b,
        y: 0.0,
        y0,
        side,
    });
    events.push(Event {
        x: x0 as f64,
        etype: 0,
        seg: si,
    });
    events.push(Event {
        x: x1 as f64,
        etype: 1,
        seg: si,
    });
}

/// Sort the active segment list by current y, then slope C — SQLite's
/// `geopolySortSegmentsByYAndC`. Reproduced as an index-list merge sort with the
/// same tie-break (`geopolySegmentMerge`: right wins when the `(y then C)` key is
/// `< 0`, i.e. left is kept on ties) so the resulting order matches byte-for-byte.
fn sort_active_by_y_c(active: &mut Vec<usize>, segs: &[Segment]) {
    // geopolySortSegmentsByYAndC pulls items off the head of the input list and
    // bottom-up merges them; the input is walked head-first (the current active
    // order). We replicate that on the index vector.
    let merge = |left: &[usize], right: &[usize]| -> Vec<usize> {
        let mut out = Vec::with_capacity(left.len() + right.len());
        let (mut i, mut j) = (0usize, 0usize);
        while i < left.len() && j < right.len() {
            let a = &segs[left[i]];
            let b = &segs[right[j]];
            let mut r = b.y - a.y;
            if r == 0.0 {
                r = b.c - a.c;
            }
            // C: `if r<0.0` take right, else take left.
            if r < 0.0 {
                out.push(right[j]);
                j += 1;
            } else {
                out.push(left[i]);
                i += 1;
            }
        }
        out.extend_from_slice(&left[i..]);
        out.extend_from_slice(&right[j..]);
        out
    };
    let mut buckets: Vec<Option<Vec<usize>>> = Vec::new();
    for &si in active.iter() {
        let mut run = alloc::vec![si];
        let mut i = 0;
        while i < buckets.len() {
            if let Some(b) = buckets[i].take() {
                run = merge(&b, &run);
                i += 1;
            } else {
                break;
            }
        }
        if i < buckets.len() {
            buckets[i] = Some(run);
        } else {
            buckets.push(Some(run));
        }
    }
    let mut result: Vec<usize> = Vec::new();
    for b in buckets.into_iter().flatten() {
        result = merge(&b, &result);
    }
    *active = result;
}

/// Sort the event indices by X into a list, reproducing SQLite's
/// `geopolySortEventsByX` + `geopolyEventMerge` bottom-up merge sort. The merge
/// keeps the "right" (newer) operand ahead on an X tie (`pRight->x <= pLeft->x`),
/// which the sweep relies on.
fn sort_events_by_x(events: &[Event]) -> Vec<usize> {
    let merge = |left: &[usize], right: &[usize]| -> Vec<usize> {
        let mut out = Vec::with_capacity(left.len() + right.len());
        let (mut i, mut j) = (0usize, 0usize);
        while i < left.len() && j < right.len() {
            // C: `if( pRight->x <= pLeft->x )` take right, else take left.
            if events[right[j]].x <= events[left[i]].x {
                out.push(right[j]);
                j += 1;
            } else {
                out.push(left[i]);
                i += 1;
            }
        }
        out.extend_from_slice(&left[i..]);
        out.extend_from_slice(&right[j..]);
        out
    };
    let mut buckets: Vec<Option<Vec<usize>>> = Vec::new();
    for si in 0..events.len() {
        let mut run = alloc::vec![si];
        let mut i = 0;
        while i < buckets.len() {
            if let Some(b) = buckets[i].take() {
                run = merge(&b, &run);
                i += 1;
            } else {
                break;
            }
        }
        if i < buckets.len() {
            buckets[i] = Some(run);
        } else {
            buckets.push(Some(run));
        }
    }
    let mut result: Vec<usize> = Vec::new();
    for b in buckets.into_iter().flatten() {
        result = merge(&b, &result);
    }
    result
}
