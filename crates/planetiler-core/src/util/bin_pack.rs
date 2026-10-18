//! Port of `util/BinPack.java`: best-effort 1-D bin packing using the
//! [best-fit decreasing](https://en.wikipedia.org/wiki/First-fit-decreasing_bin_packing#Other_variants) algorithm.

/// Returns `items` grouped into an approximately minimum number of bins under `max_bin_size` according to `get_size`.
pub fn pack<T>(items: impl IntoIterator<Item = T>, max_bin_size: i64, get_size: impl Fn(&T) -> i64) -> Vec<Vec<T>> {
    struct Bin<T> {
        size: i64,
        items: Vec<T>,
    }
    let mut descending: Vec<(i64, T)> = items.into_iter().map(|item| (get_size(&item), item)).collect();
    // stable sort, largest first
    descending.sort_by_key(|item| std::cmp::Reverse(item.0));
    let mut bins: Vec<Bin<T>> = Vec::new();
    for (size, item) in descending {
        // Instead of using the first bin that this element fits in, use the "fullest" bin (first one on ties).
        // This makes the algorithm "best-fit decreasing" instead of "first-fit decreasing"
        let mut best: Option<usize> = None;
        for (i, bin) in bins.iter().enumerate() {
            if max_bin_size - bin.size >= size && best.is_none_or(|b| bin.size > bins[b].size) {
                best = Some(i);
            }
        }
        let idx = best.unwrap_or_else(|| {
            bins.push(Bin { size: 0, items: Vec::new() });
            bins.len() - 1
        });
        bins[idx].items.push(item);
        bins[idx].size += size;
    }
    bins.into_iter().map(|b| b.items).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test() {
        type Case<'a> = (i64, &'a [i64], &'a [&'a [i64]]);
        let cases: &[Case] = &[
            (3, &[], &[]),
            (2, &[1], &[&[1]]),
            (2, &[2], &[&[2]]),
            (2, &[3], &[&[3]]),
            (3, &[1, 2, 3], &[&[3], &[2, 1]]),
            (5, &[1, 2, 3], &[&[3, 2], &[1]]),
            (6, &[1, 2, 3], &[&[3, 2, 1]]),
            (2, &[1, 2, 3], &[&[3], &[2], &[1]]),
            (1, &[1, 2, 3], &[&[3], &[2], &[1]]),
        ];
        for (limit, input, expected) in cases {
            let expected: Vec<Vec<i64>> = expected.iter().map(|v| v.to_vec()).collect();
            assert_eq!(expected, pack(input.iter().copied(), *limit, |i| *i), "{limit} {input:?}");
        }
    }
}
