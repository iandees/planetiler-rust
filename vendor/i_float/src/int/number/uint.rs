use crate::int::number::product_uint::{CompositeUIntProduct, UIntProduct, UIntProduct64};
use core::fmt::Display;
use core::ops::{Add, AddAssign, BitAnd, BitOr, BitOrAssign, Div, Mul, Shl, ShlAssign, Shr, Sub};

pub trait UIntNumber:
    Copy
    + Clone
    + Ord
    + Send
    + Sync
    + Display
    + Add<Output = Self>
    + Sub<Output = Self>
    + Mul<Output = Self>
    + Div<Output = Self>
    + BitAnd<Output = Self>
    + BitOr<Output = Self>
    + BitOrAssign
    + Shl<u32, Output = Self>
    + Shr<u32, Output = Self>
    + ShlAssign
    + AddAssign
{
    type Product: UIntProduct<Self>;
    const BITS: u32;
    const HALF_BITS: u32;
    const HALF_MASK: Self;
    const LAST_BIT_INDEX: u32;
    const ZERO: Self;
    const ONE: Self;
    const TWO: Self;
    const FOUR: Self;
    const MAX: Self;
    const LAST_BIT: Self;

    fn overflowing_add(self, rhs: Self) -> (Self, bool);
    fn from_u64(value: u64) -> Self;
    fn wrapping_sub(self, rhs: Self) -> Self;
    fn leading_zeros(self) -> u32;
    fn ilog2(self) -> u32;
    fn isqrt(self) -> Self;
    fn to_usize(self) -> usize;
    fn to_f64(self) -> f64;

    /// (vendored addition) Quotient and remainder of the two-word value `high:low` divided by `divisor`.
    ///
    /// Requires `high < divisor` (so the quotient fits in one word).
    #[inline]
    fn div_rem_wide(high: Self, low: Self, divisor: Self) -> (Self, Self) {
        div_rem_wide_bitwise(high, low, divisor)
    }
}

/// The original bit-by-bit two-word division of `CompositeUIntProduct::divide_with_rounding`: quotient and
/// remainder of `high:low` divided by `divisor`, for `high < divisor`.
pub fn div_rem_wide_bitwise<U: UIntNumber>(high: U, low: U, divisor: U) -> (U, U) {
    if high == U::ZERO {
        let q = low / divisor;
        return (q, low - q * divisor);
    }
    let dn = divisor.leading_zeros();
    let norm_divisor = divisor << dn;
    let mut norm_dividend_high = if dn == 0 { high } else { (high << dn) | (low >> (U::BITS - dn)) };
    let mut norm_dividend_low = low << dn;

    let mut quotient = U::ZERO;

    for _ in 0..U::BITS {
        let bit = (norm_dividend_high & U::LAST_BIT) != U::ZERO;
        norm_dividend_high = (norm_dividend_high << 1) | (norm_dividend_low >> U::LAST_BIT_INDEX);
        norm_dividend_low <<= U::ONE;
        quotient <<= U::ONE;
        if norm_dividend_high >= norm_divisor || bit {
            norm_dividend_high = norm_dividend_high.wrapping_sub(norm_divisor);
            quotient |= U::ONE;
        }
    }
    (quotient, norm_dividend_high >> dn)
}

/// `high:low / divisor` for 128-bit words (`high < divisor`) with 64-bit digits: Hacker's Delight `divlu`
/// (Knuth's algorithm D for a two-digit divisor), where each step is a native 128 / 64 bit division.
#[inline]
fn div_rem_u256_by_u128(high: u128, low: u128, divisor: u128) -> (u128, u128) {
    debug_assert!(high < divisor);
    if high == 0 {
        return (low / divisor, low % divisor);
    }
    const B: u128 = 1 << 64;
    const MASK: u128 = B - 1;
    // normalize so the divisor's top bit is set
    let s = divisor.leading_zeros();
    let v = divisor << s;
    let (vn1, vn0) = (v >> 64, v & MASK);
    let un32 = if s == 0 { high } else { (high << s) | (low >> (128 - s)) };
    let un10 = low << s;
    let (un1, un0) = (un10 >> 64, un10 & MASK);

    let mut q1 = un32 / vn1;
    let mut rhat = un32 - q1 * vn1;
    while q1 >= B || q1 * vn0 > (rhat << 64) + un1 {
        q1 -= 1;
        rhat += vn1;
        if rhat >= B {
            break;
        }
    }
    let un21 = (un32 << 64).wrapping_add(un1).wrapping_sub(q1.wrapping_mul(v));

    let mut q0 = un21 / vn1;
    rhat = un21 - q0 * vn1;
    while q0 >= B || q0 * vn0 > (rhat << 64) + un0 {
        q0 -= 1;
        rhat += vn1;
        if rhat >= B {
            break;
        }
    }
    let rem = (un21 << 64).wrapping_add(un0).wrapping_sub(q0.wrapping_mul(v)) >> s;
    ((q1 << 64) | q0, rem)
}

impl UIntNumber for u32 {
    type Product = UIntProduct64;
    const BITS: u32 = 32;
    const HALF_BITS: u32 = 16;
    const HALF_MASK: Self = 0xFFFF;
    const LAST_BIT_INDEX: u32 = 31;
    const ZERO: Self = 0;
    const ONE: Self = 1;
    const TWO: Self = 2;
    const FOUR: Self = 4;
    const MAX: Self = Self::MAX;
    const LAST_BIT: Self = Self::ONE << Self::LAST_BIT_INDEX;

    #[inline(always)]
    fn overflowing_add(self, rhs: Self) -> (Self, bool) {
        self.overflowing_add(rhs)
    }
    #[inline(always)]
    fn from_u64(value: u64) -> Self {
        value as Self
    }

    #[inline(always)]
    fn wrapping_sub(self, rhs: Self) -> Self {
        self.wrapping_sub(rhs)
    }

    #[inline(always)]
    fn leading_zeros(self) -> u32 {
        self.leading_zeros()
    }

    #[inline(always)]
    fn ilog2(self) -> u32 {
        self.ilog2()
    }

    #[inline(always)]
    fn isqrt(self) -> Self {
        self.isqrt()
    }
    #[inline(always)]
    fn to_usize(self) -> usize {
        self as usize
    }

    #[inline(always)]
    fn to_f64(self) -> f64 {
        self as f64
    }

    #[inline(always)]
    fn div_rem_wide(high: Self, low: Self, divisor: Self) -> (Self, Self) {
        let n = ((high as u64) << 32) | low as u64;
        let d = divisor as u64;
        ((n / d) as u32, (n % d) as u32)
    }
}

impl UIntNumber for u64 {
    type Product = CompositeUIntProduct<Self>;
    const BITS: u32 = 64;
    const HALF_BITS: u32 = 32;
    const HALF_MASK: Self = 0xFFFF_FFFF;
    const LAST_BIT_INDEX: u32 = 63;
    const ZERO: Self = 0;
    const ONE: Self = 1;
    const TWO: Self = 2;
    const FOUR: Self = 4;
    const MAX: Self = Self::MAX;
    const LAST_BIT: Self = Self::ONE << Self::LAST_BIT_INDEX;

    #[inline(always)]
    fn overflowing_add(self, rhs: Self) -> (Self, bool) {
        self.overflowing_add(rhs)
    }

    #[inline(always)]
    fn from_u64(value: u64) -> Self {
        value
    }

    #[inline(always)]
    fn wrapping_sub(self, rhs: Self) -> Self {
        self.wrapping_sub(rhs)
    }

    #[inline(always)]
    fn leading_zeros(self) -> u32 {
        self.leading_zeros()
    }

    #[inline(always)]
    fn ilog2(self) -> u32 {
        self.ilog2()
    }

    #[inline(always)]
    fn isqrt(self) -> Self {
        self.isqrt()
    }
    #[inline(always)]
    fn to_usize(self) -> usize {
        self as usize
    }

    #[inline(always)]
    fn to_f64(self) -> f64 {
        self as f64
    }

    #[inline(always)]
    fn div_rem_wide(high: Self, low: Self, divisor: Self) -> (Self, Self) {
        let n = ((high as u128) << 64) | low as u128;
        let d = divisor as u128;
        ((n / d) as u64, (n % d) as u64)
    }
}

impl UIntNumber for u128 {
    type Product = CompositeUIntProduct<Self>;
    const BITS: u32 = 128;
    const HALF_BITS: u32 = 64;
    const HALF_MASK: Self = 0xFFFF_FFFF_FFFF_FFFF;
    const LAST_BIT_INDEX: u32 = 127;
    const ZERO: Self = 0;
    const ONE: Self = 1;
    const TWO: Self = 2;
    const FOUR: Self = 4;
    const MAX: Self = Self::MAX;
    const LAST_BIT: Self = Self::ONE << Self::LAST_BIT_INDEX;
    #[inline(always)]
    fn overflowing_add(self, rhs: Self) -> (Self, bool) {
        self.overflowing_add(rhs)
    }

    #[inline(always)]
    fn from_u64(value: u64) -> Self {
        value as Self
    }

    #[inline(always)]
    fn wrapping_sub(self, rhs: Self) -> Self {
        self.wrapping_sub(rhs)
    }

    #[inline(always)]
    fn leading_zeros(self) -> u32 {
        self.leading_zeros()
    }

    #[inline(always)]
    fn ilog2(self) -> u32 {
        self.ilog2()
    }

    #[inline(always)]
    fn isqrt(self) -> Self {
        self.isqrt()
    }
    #[inline(always)]
    fn to_usize(self) -> usize {
        self as usize
    }

    #[inline(always)]
    fn to_f64(self) -> f64 {
        self as f64
    }

    #[inline(always)]
    fn div_rem_wide(high: Self, low: Self, divisor: Self) -> (Self, Self) {
        div_rem_u256_by_u128(high, low, divisor)
    }
}

#[cfg(test)]
mod div_rem_wide_tests {
    use super::{UIntNumber, div_rem_wide_bitwise};

    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    fn check_u128(high: u128, low: u128, divisor: u128) {
        let (q, r) = u128::div_rem_wide(high, low, divisor);
        assert!(r < divisor);
        // q * divisor + r == high:low (256-bit arithmetic on 64-bit digits)
        let m = u64::MAX as u128;
        let (q1, q0) = (q >> 64, q & m);
        let (d1, d0) = (divisor >> 64, divisor & m);
        let p00 = q0 * d0;
        let p01 = q0 * d1;
        let p10 = q1 * d0;
        let p11 = q1 * d1;
        let mid = (p00 >> 64) + (p01 & m) + (p10 & m);
        let lo = (mid << 64) | (p00 & m);
        let hi = p11 + (p01 >> 64) + (p10 >> 64) + (mid >> 64);
        let (lo, carry) = lo.overflowing_add(r);
        assert_eq!((hi + carry as u128, lo), (high, low), "{high:x}:{low:x} / {divisor:x}");
        if divisor < u128::LAST_BIT {
            assert_eq!((q, r), div_rem_wide_bitwise(high, low, divisor));
        }
    }

    #[test]
    fn u128_matches_bitwise_division() {
        let mut state = 0x4d59_5df4_d0f3_3173u64;
        for _ in 0..200_000 {
            let mut word = || ((next(&mut state) as u128) << 64) | next(&mut state) as u128;
            let a = word();
            let b = word();
            let c = word();
            let shift = (next(&mut state) % 128) as u32;
            let divisor = (a >> shift).max(1);
            let high = b % divisor;
            check_u128(high, c, divisor);
            check_u128(divisor - 1, c, divisor);
            check_u128(divisor - 1, u128::MAX, divisor);
            check_u128(0, c, divisor);
        }
        check_u128(u128::MAX - 1, u128::MAX, u128::MAX);
        check_u128(1, 0, 2);
        check_u128((1 << 64) - 1, u128::MAX, 1 << 64);
    }

    #[test]
    fn u64_and_u32_match_bitwise_division() {
        let mut state = 0x1234_5678_9abc_def1u64;
        for _ in 0..100_000 {
            let shift = next(&mut state) % 64;
            let divisor = ((next(&mut state) >> shift) & (u64::LAST_BIT - 1)).max(1);
            let high = next(&mut state) % divisor;
            let low = next(&mut state);
            assert_eq!(u64::div_rem_wide(high, low, divisor), div_rem_wide_bitwise(high, low, divisor));
            let d32 = (divisor as u32 & (u32::LAST_BIT - 1)).max(1);
            let h32 = (high as u32) % d32;
            assert_eq!(u32::div_rem_wide(h32, low as u32, d32), div_rem_wide_bitwise(h32, low as u32, d32));
        }
    }
}
