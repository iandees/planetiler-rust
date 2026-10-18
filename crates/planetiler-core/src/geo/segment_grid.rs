//! A static uniform-grid index of segment envelopes for small envelope queries.
//!
//! Built in one counting pass plus one fill pass (no per-node allocations), which makes it much cheaper than an
//! R-tree bulk load for the few hundred to few thousand segments of a tile polygon, while queries for a point (or a
//! tiny envelope) look at one or a few cells. Segments whose envelope covers many cells go to a separate list that
//! every query scans. The cells are built lazily: the first few queries scan all envelopes.

/// Grid index over item envelopes `(min_x, min_y, max_x, max_y)`, each with an id of type `T`.
///
/// The grid cells are only built once there have been more than a few queries: until then (and always for a few
/// items) queries scan every envelope, which is cheaper than building the grid when only a handful of queries come.
pub struct SegmentGrid<T: Copy> {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    boxes: Vec<[f64; 4]>,
    ids: Vec<T>,
    queries: std::cell::Cell<u32>,
    cells: std::cell::OnceCell<Cells>,
}

struct Cells {
    min_x: f64,
    min_y: f64,
    inv_cell_x: f64,
    inv_cell_y: f64,
    nx: usize,
    ny: usize,
    /// `cell_start[c]..cell_start[c + 1]` indexes `entries` for cell `c` (row-major, `x + y * nx`)
    cell_start: Vec<u32>,
    entries: Vec<u32>,
    /// items that cover too many cells, scanned by every query
    big: Vec<u32>,
}

/// Items covering more cells than this go to the list scanned by every query.
const MAX_CELLS_PER_ITEM: usize = 16;
/// Queries answered by a linear scan before the grid is built.
const LINEAR_QUERIES: u32 = 8;
/// Indexes with at most this many items are always scanned linearly.
const LINEAR_ITEMS: usize = 32;

impl<T: Copy> SegmentGrid<T> {
    /// Creates the index over `items`: `(envelope as [min_x, min_y, max_x, max_y], id)`.
    pub fn new(items: impl Iterator<Item = ([f64; 4], T)>) -> SegmentGrid<T> {
        let (lower, _) = items.size_hint();
        let mut boxes = Vec::with_capacity(lower);
        let mut ids = Vec::with_capacity(lower);
        let (mut min_x, mut min_y, mut max_x, mut max_y) =
            (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for (b, id) in items {
            min_x = min_x.min(b[0]);
            min_y = min_y.min(b[1]);
            max_x = max_x.max(b[2]);
            max_y = max_y.max(b[3]);
            boxes.push(b);
            ids.push(id);
        }
        SegmentGrid {
            min_x,
            min_y,
            max_x,
            max_y,
            boxes,
            ids,
            queries: std::cell::Cell::new(0),
            cells: std::cell::OnceCell::new(),
        }
    }

    fn build_cells(&self) -> Cells {
        let n = self.boxes.len();
        let (min_x, min_y) = (self.min_x, self.min_y);
        let (w, h) = (self.max_x - min_x, self.max_y - min_y);
        // about one cell per item, shaped like the extent
        let (nx, ny) = if n == 0 || !(w.is_finite() && h.is_finite()) {
            (1, 1)
        } else if w <= 0.0 || h <= 0.0 {
            if w > h { (n.clamp(1, 4096), 1) } else { (1, n.clamp(1, 4096)) }
        } else {
            let cells = n as f64;
            let nx = ((cells * w / h).sqrt().ceil().clamp(1.0, 4096.0) as usize).min(n);
            let ny = (cells / nx as f64).ceil().clamp(1.0, 4096.0) as usize;
            (nx, ny)
        };
        let inv_cell_x = if w > 0.0 { nx as f64 / w } else { 0.0 };
        let inv_cell_y = if h > 0.0 { ny as f64 / h } else { 0.0 };
        let mut cells = Cells {
            min_x,
            min_y,
            inv_cell_x,
            inv_cell_y,
            nx,
            ny,
            cell_start: vec![0; nx * ny + 1],
            entries: Vec::new(),
            big: Vec::new(),
        };
        // pass 1: count items per cell
        let mut ranges = Vec::with_capacity(n);
        for (i, b) in self.boxes.iter().enumerate() {
            let (x0, x1) = (cells.cell_x(b[0]), cells.cell_x(b[2]));
            let (y0, y1) = (cells.cell_y(b[1]), cells.cell_y(b[3]));
            if (x1 - x0 + 1) * (y1 - y0 + 1) > MAX_CELLS_PER_ITEM {
                cells.big.push(i as u32);
                ranges.push(None);
            } else {
                for y in y0..=y1 {
                    for x in x0..=x1 {
                        cells.cell_start[x + y * nx + 1] += 1;
                    }
                }
                ranges.push(Some((x0, x1, y0, y1)));
            }
        }
        for c in 1..cells.cell_start.len() {
            cells.cell_start[c] += cells.cell_start[c - 1];
        }
        // pass 2: fill (items in increasing index order within each cell)
        cells.entries = vec![0; *cells.cell_start.last().unwrap() as usize];
        let mut next: Vec<u32> = cells.cell_start[..nx * ny].to_vec();
        for (i, range) in ranges.into_iter().enumerate() {
            if let Some((x0, x1, y0, y1)) = range {
                for y in y0..=y1 {
                    for x in x0..=x1 {
                        let c = x + y * nx;
                        cells.entries[next[c] as usize] = i as u32;
                        next[c] += 1;
                    }
                }
            }
        }
        cells
    }

    /// Appends to `out` the id of every item whose envelope intersects (or touches) the envelope `[min, max]`, each
    /// once, in item insertion order.
    pub fn query(&self, min: [f64; 2], max: [f64; 2], out: &mut Vec<T>) {
        let intersects = |b: &[f64; 4]| b[0] <= max[0] && min[0] <= b[2] && b[1] <= max[1] && min[1] <= b[3];
        let cells = match self.cells.get() {
            Some(cells) => cells,
            None => {
                let queries = self.queries.get();
                if self.boxes.len() <= LINEAR_ITEMS || queries < LINEAR_QUERIES {
                    self.queries.set(queries + 1);
                    for (b, id) in self.boxes.iter().zip(&self.ids) {
                        if intersects(b) {
                            out.push(*id);
                        }
                    }
                    return;
                }
                self.cells.get_or_init(|| self.build_cells())
            }
        };
        let (x0, x1) = (cells.cell_x(min[0]), cells.cell_x(max[0]));
        let (y0, y1) = (cells.cell_y(min[1]), cells.cell_y(max[1]));
        let single_cell = x0 == x1 && y0 == y1;
        let mut found: found_buf::Found = Default::default();
        for y in y0..=y1 {
            for x in x0..=x1 {
                let c = x + y * cells.nx;
                for &i in &cells.entries[cells.cell_start[c] as usize..cells.cell_start[c + 1] as usize] {
                    if intersects(&self.boxes[i as usize]) {
                        found.push(i);
                    }
                }
            }
        }
        for &i in &cells.big {
            if intersects(&self.boxes[i as usize]) {
                found.push(i);
            }
        }
        let items = found.as_mut_slice();
        if !single_cell || !cells.big.is_empty() {
            items.sort_unstable();
        }
        let mut last = u32::MAX;
        for &i in items.iter() {
            if i != last {
                out.push(self.ids[i as usize]);
                last = i;
            }
        }
    }
}

impl Cells {
    #[inline]
    fn cell_x(&self, x: f64) -> usize {
        let c = (x - self.min_x) * self.inv_cell_x;
        // NaN and negative values go to cell 0
        if c >= 1.0 { (c as usize).min(self.nx - 1) } else { 0 }
    }

    #[inline]
    fn cell_y(&self, y: f64) -> usize {
        let c = (y - self.min_y) * self.inv_cell_y;
        if c >= 1.0 { (c as usize).min(self.ny - 1) } else { 0 }
    }
}

/// A small inline buffer of item indices (queries usually find a handful).
mod found_buf {
    #[derive(Default)]
    pub(super) struct Found {
        inline: [u32; 16],
        len: usize,
        spill: Vec<u32>,
    }

    impl Found {
        #[inline]
        pub(super) fn push(&mut self, i: u32) {
            if self.len < self.inline.len() {
                self.inline[self.len] = i;
                self.len += 1;
            } else {
                if self.spill.is_empty() {
                    self.spill.extend_from_slice(&self.inline);
                }
                self.spill.push(i);
                self.len += 1;
            }
        }

        pub(super) fn as_mut_slice(&mut self) -> &mut [u32] {
            if self.len <= self.inline.len() { &mut self.inline[..self.len] } else { &mut self.spill }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brute(boxes: &[[f64; 4]], min: [f64; 2], max: [f64; 2]) -> Vec<usize> {
        (0..boxes.len())
            .filter(|&i| {
                let b = boxes[i];
                b[0] <= max[0] && min[0] <= b[2] && b[1] <= max[1] && min[1] <= b[3]
            })
            .collect()
    }

    #[test]
    fn matches_brute_force() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut rand = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        for n in [0, 1, 2, 5, 50, 500] {
            let mut boxes = Vec::new();
            for i in 0..n {
                let (x, y) = (rand() * 256.0, rand() * 256.0);
                // mostly short segments, some long ones, some degenerate
                let len = if i % 17 == 0 {
                    200.0
                } else if i % 5 == 0 {
                    0.0
                } else {
                    4.0
                };
                let (x2, y2) = (x + (rand() - 0.5) * len, y + (rand() - 0.5) * len);
                boxes.push([x.min(x2), y.min(y2), x.max(x2), y.max(y2)]);
            }
            let grid = SegmentGrid::new(boxes.iter().enumerate().map(|(i, b)| (*b, i)));
            for q in 0..300 {
                let (x, y) = (rand() * 300.0 - 20.0, rand() * 300.0 - 20.0);
                let r = if q % 3 == 0 { 0.0 } else { rand() * 10.0 };
                let (min, max) = ([x - r, y - r], [x + r, y + r]);
                let mut out = Vec::new();
                grid.query(min, max, &mut out);
                assert_eq!(brute(&boxes, min, max), out, "n={n} q={q}");
            }
            // query exactly at item corners
            for b in &boxes {
                let mut out = Vec::new();
                grid.query([b[2], b[3]], [b[2], b[3]], &mut out);
                assert_eq!(brute(&boxes, [b[2], b[3]], [b[2], b[3]]), out);
            }
        }
    }

    #[test]
    fn degenerate_extents() {
        // all on a vertical line
        let boxes: Vec<[f64; 4]> = (0..10).map(|i| [1.0, i as f64, 1.0, i as f64 + 1.0]).collect();
        let grid = SegmentGrid::new(boxes.iter().enumerate().map(|(i, b)| (*b, i)));
        let mut out = Vec::new();
        grid.query([1.0, 3.5], [1.0, 3.5], &mut out);
        assert_eq!(vec![3], out);
        out.clear();
        grid.query([1.0, 4.0], [1.0, 4.0], &mut out);
        assert_eq!(vec![3, 4], out);
    }
}
