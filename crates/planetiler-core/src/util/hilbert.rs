//! Port of `util/Hilbert.java`: fast Hilbert curve index <-> (x, y) conversion for levels up to 16.
//!
//! Based on <https://github.com/rawrunprotected/hilbert_curves> (public domain).

fn deinterleave(mut tx: u32) -> u32 {
    tx &= 0x5555_5555;
    tx = (tx | (tx >> 1)) & 0x3333_3333;
    tx = (tx | (tx >> 2)) & 0x0F0F_0F0F;
    tx = (tx | (tx >> 4)) & 0x00FF_00FF;
    tx = (tx | (tx >> 8)) & 0x0000_FFFF;
    tx
}

fn interleave(mut tx: u32) -> u32 {
    tx = (tx | (tx << 8)) & 0x00FF_00FF;
    tx = (tx | (tx << 4)) & 0x0F0F_0F0F;
    tx = (tx | (tx << 2)) & 0x3333_3333;
    tx = (tx | (tx << 1)) & 0x5555_5555;
    tx
}

fn prefix_scan(mut tx: u32) -> u32 {
    tx ^= tx >> 8;
    tx ^= tx >> 4;
    tx ^= tx >> 2;
    tx ^= tx >> 1;
    tx
}

/// Extract the X coordinate from a value packed by [`hilbert_position_to_xy`].
pub fn extract_x(xy: u64) -> u32 {
    (xy >> 32) as u32
}

/// Extract the Y coordinate from a value packed by [`hilbert_position_to_xy`].
pub fn extract_y(xy: u64) -> u32 {
    xy as u32
}

/// Convert a position along the hilbert curve at `level` to `x << 32 | y`.
pub fn hilbert_position_to_xy(level: u32, pos: u64) -> u64 {
    if level == 0 {
        return 0;
    }
    let ipos = (pos << (32 - 2 * level)) as u32;
    let i0 = deinterleave(ipos);
    let i1 = deinterleave(ipos >> 1);
    let t0 = (i0 | i1) ^ 0xFFFF;
    let t1 = i0 & i1;
    let prefix_t0 = prefix_scan(t0);
    let prefix_t1 = prefix_scan(t1);
    let a = ((i0 ^ 0xFFFF) & prefix_t1) | (i0 & prefix_t0);
    let result_x = (a ^ i1) >> (16 - level);
    let result_y = (a ^ i0 ^ i1) >> (16 - level);
    ((result_x as u64) << 32) | result_y as u64
}

/// Convert `(x, y)` at `level` to a position along the hilbert curve.
pub fn hilbert_xy_to_index(level: u32, x: u32, y: u32) -> u64 {
    if level == 0 {
        return 0;
    }
    let x = x << (16 - level);
    let y = y << (16 - level);

    let a1 = x ^ y;
    let b1 = 0xFFFF ^ a1;
    let c1 = 0xFFFF ^ (x | y);
    let d1 = x & (y ^ 0xFFFF);

    let mut ha = a1 | (b1 >> 1);
    let mut hb = (a1 >> 1) ^ a1;
    let mut hc = ((c1 >> 1) ^ (b1 & (d1 >> 1))) ^ c1;
    let mut hd = ((a1 & (c1 >> 1)) ^ (d1 >> 1)) ^ d1;

    let (a2, b2, c2, d2) = (ha, hb, hc, hd);
    ha = (a2 & (a2 >> 2)) ^ (b2 & (b2 >> 2));
    hb = (a2 & (b2 >> 2)) ^ (b2 & ((a2 ^ b2) >> 2));
    hc ^= (a2 & (c2 >> 2)) ^ (b2 & (d2 >> 2));
    hd ^= (b2 & (c2 >> 2)) ^ ((a2 ^ b2) & (d2 >> 2));

    let (a3, b3, c3, d3) = (ha, hb, hc, hd);
    ha = (a3 & (a3 >> 4)) ^ (b3 & (b3 >> 4));
    hb = (a3 & (b3 >> 4)) ^ (b3 & ((a3 ^ b3) >> 4));
    hc ^= (a3 & (c3 >> 4)) ^ (b3 & (d3 >> 4));
    hd ^= (b3 & (c3 >> 4)) ^ ((a3 ^ b3) & (d3 >> 4));

    let (a4, b4, c4, d4) = (ha, hb, hc, hd);
    hc ^= (a4 & (c4 >> 8)) ^ (b4 & (d4 >> 8));
    hd ^= (b4 & (c4 >> 8)) ^ ((a4 ^ b4) & (d4 >> 8));
    let _ = (ha, hb);

    let a = hc ^ (hc >> 1);
    let b = hd ^ (hd >> 1);
    let i0 = x ^ y;
    let i1 = b | (0xFFFF ^ (i0 | a));
    (((interleave(i1) << 1) | interleave(i0)) >> (32 - 2 * level)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for level in 0..=10u32 {
            let dim = 1u32 << level;
            let mut seen = vec![false; (dim as usize) * (dim as usize)];
            for x in 0..dim {
                for y in 0..dim {
                    let idx = hilbert_xy_to_index(level, x, y);
                    assert!(!seen[idx as usize]);
                    seen[idx as usize] = true;
                    let xy = hilbert_position_to_xy(level, idx);
                    assert_eq!((x, y), (extract_x(xy), extract_y(xy)), "level={level}");
                }
            }
        }
    }

    #[test]
    fn level_16_corners() {
        let max = (1u32 << 16) - 1;
        for (x, y) in [(0, 0), (max, 0), (0, max), (max, max), (12345, 54321)] {
            let idx = hilbert_xy_to_index(16, x, y);
            let xy = hilbert_position_to_xy(16, idx);
            assert_eq!((x, y), (extract_x(xy), extract_y(xy)));
        }
    }
}
