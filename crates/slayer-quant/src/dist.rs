//! Standard normal distribution machinery.
//!
//! CDF: Hart's rational approximation (Hart 1968, as restated in West,
//! "Better approximations to cumulative normal functions", Wilmott 2005) —
//! double-precision accurate (|ε| ≲ 1e-15) across the full support, unlike
//! the single-precision Abramowitz–Stegun polynomial the legacy codebase
//! used (see `docs/spec/04-vol-distribution.md`).
//!
//! Inverse CDF: Wichura's AS 241 / PPND16 (Applied Statistics 37, 1988),
//! accurate to ~1e-16 for p in (0, 1).

// Coefficient tables below are transcribed verbatim from the published
// papers; some literals carry digits beyond f64 resolution and are kept
// as-published for provenance rather than truncated to appease the lint.
#![allow(clippy::excessive_precision)]

use std::f64::consts::TAU;

/// 1/√(2π), the normalizing constant of the standard normal density.
const INV_SQRT_TAU: f64 = 0.398_942_280_401_432_7;

/// √(2π), used by the Hart tail branch.
const SQRT_TAU: f64 = 2.506_628_274_631_000_5;

/// |x| beyond which Φ(x) is 0 or 1 to double precision.
const HART_CUTOFF: f64 = 37.0;

/// Boundary between Hart's central rational approximation and its
/// continued-fraction tail branch (√2 · 5).
const HART_CENTRAL_BOUND: f64 = 7.071_067_811_865_475;

/// Numerator coefficients of Hart's central rational approximation,
/// highest degree first.
const HART_NUM: [f64; 7] = [
    3.526_249_659_989_11e-2,
    0.700_383_064_443_688,
    6.373_962_203_531_65,
    33.912_866_078_383,
    112.079_291_497_871,
    221.213_596_169_931,
    220.206_867_912_376,
];

/// Denominator coefficients of Hart's central rational approximation,
/// highest degree first.
const HART_DEN: [f64; 8] = [
    8.838_834_764_831_84e-2,
    1.755_667_163_182_64,
    16.064_177_579_207,
    86.780_732_202_946_1,
    296.564_248_779_674,
    637.333_633_378_831,
    793.826_512_519_948,
    440.413_735_824_752,
];

/// Standard normal probability density φ(x).
#[must_use]
pub fn norm_pdf(x: f64) -> f64 {
    INV_SQRT_TAU * (-0.5 * x * x).exp()
}

/// Standard normal cumulative distribution Φ(x), double-precision accurate.
#[must_use]
pub fn norm_cdf(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    let xabs = x.abs();
    if xabs > HART_CUTOFF {
        return if x > 0.0 { 1.0 } else { 0.0 };
    }
    let e = (-0.5 * xabs * xabs).exp();
    let tail = if xabs < HART_CENTRAL_BOUND {
        let num = HART_NUM.iter().fold(0.0, |acc, c| acc * xabs + c);
        let den = HART_DEN.iter().fold(0.0, |acc, c| acc * xabs + c);
        e * num / den
    } else {
        // Continued fraction: xabs + 4/(xabs + 3/(xabs + 2/(xabs + 1/(xabs + 0.65))))
        let mut build = xabs + 0.65;
        for k in [4.0, 3.0, 2.0, 1.0] {
            build = xabs + k / build;
        }
        e / (build * SQRT_TAU)
    };
    if x > 0.0 { 1.0 - tail } else { tail }
}

/// AS 241 central-branch bound on |p − ½|.
const PPND_CENTRAL_BOUND: f64 = 0.425;
/// AS 241 central-branch constant: `0.180625 = PPND_CENTRAL_BOUND²`.
const PPND_CENTRAL_R0: f64 = 0.180_625;
/// AS 241 boundary between the intermediate and far-tail branches, in
/// r = √(−ln p) space.
const PPND_TAIL_BOUND: f64 = 5.0;
/// Shift applied to r in the intermediate branch.
const PPND_MID_SHIFT: f64 = 1.6;

/// AS 241 central-branch numerator coefficients, highest degree first.
const PPND_A: [f64; 8] = [
    2.509_080_928_730_122_7e3,
    3.343_057_558_358_812_8e4,
    6.726_577_092_700_87e4,
    4.592_195_393_154_987e4,
    1.373_169_376_550_946_1e4,
    1.971_590_950_306_551_3e3,
    1.331_416_678_917_843_8e2,
    3.387_132_872_796_366_5,
];
/// AS 241 central-branch denominator coefficients, highest degree first.
const PPND_B: [f64; 8] = [
    5.226_495_278_852_545_5e3,
    2.872_908_573_572_194_3e4,
    3.930_789_580_009_271e4,
    2.121_379_430_158_659_7e4,
    5.394_196_021_424_751e3,
    6.871_870_074_920_579e2,
    4.231_333_070_160_091e1,
    1.0,
];
/// AS 241 intermediate-branch numerator coefficients, highest degree first.
const PPND_C: [f64; 8] = [
    7.745_450_142_783_414e-4,
    2.272_384_498_926_918_4e-2,
    2.417_807_251_774_506e-1,
    1.270_458_252_452_368_4,
    3.647_848_324_763_204_5,
    5.769_497_221_460_691,
    4.630_337_846_156_545,
    1.423_437_110_749_683_5,
];
/// AS 241 intermediate-branch denominator coefficients, highest degree first.
const PPND_D: [f64; 8] = [
    1.050_750_071_644_416_9e-9,
    5.475_938_084_995_345e-4,
    1.519_866_656_361_645_7e-2,
    1.481_039_764_274_800_8e-1,
    6.897_673_349_851e-1,
    1.676_384_830_183_803_8,
    2.053_191_626_637_758_8,
    1.0,
];
/// AS 241 far-tail numerator coefficients, highest degree first.
const PPND_E: [f64; 8] = [
    2.010_334_399_292_288_1e-7,
    2.711_555_568_743_487_6e-5,
    1.242_660_947_388_078_4e-3,
    2.653_218_952_657_612_4e-2,
    2.965_605_718_285_048_9e-1,
    1.784_826_539_917_291_3,
    5.463_784_911_164_114,
    6.657_904_643_501_103,
];
/// AS 241 far-tail denominator coefficients, highest degree first.
const PPND_F: [f64; 8] = [
    2.044_263_103_389_939_7e-15,
    1.421_511_758_316_446e-7,
    1.846_318_317_510_054_8e-5,
    7.868_691_311_456_133e-4,
    1.487_536_129_085_061_5e-2,
    1.369_298_809_227_358e-1,
    5.998_322_065_558_88e-1,
    1.0,
];

fn poly(coeffs: &[f64; 8], r: f64) -> f64 {
    coeffs.iter().fold(0.0, |acc, c| acc * r + c)
}

/// Inverse standard normal CDF Φ⁻¹(p) for p ∈ (0, 1).
///
/// Returns `NaN` outside the open interval — a probability of exactly 0 or 1
/// has no finite quantile and callers must handle it explicitly.
#[must_use]
pub fn norm_cdf_inv(p: f64) -> f64 {
    if !(0.0..=1.0).contains(&p) || p == 0.0 || p == 1.0 || p.is_nan() {
        return f64::NAN;
    }
    let q = p - 0.5;
    if q.abs() <= PPND_CENTRAL_BOUND {
        let r = PPND_CENTRAL_R0 - q * q;
        return q * poly(&PPND_A, r) / poly(&PPND_B, r);
    }
    let r = if q < 0.0 { p } else { 1.0 - p };
    let mut r = (-r.ln()).sqrt();
    let val = if r <= PPND_TAIL_BOUND {
        r -= PPND_MID_SHIFT;
        poly(&PPND_C, r) / poly(&PPND_D, r)
    } else {
        r -= PPND_TAIL_BOUND;
        poly(&PPND_E, r) / poly(&PPND_F, r)
    };
    if q < 0.0 { -val } else { val }
}

/// φ(x) expressed via [`TAU`] for callers needing the log-density.
#[must_use]
pub fn norm_log_pdf(x: f64) -> f64 {
    -0.5 * x * x - 0.5 * TAU.ln()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// scipy.stats.norm oracle values (see quant/ golden generator).
    const CDF_CASES: [(f64, f64); 5] = [
        (0.0, 0.5),
        (0.35, 0.636_830_651_175_619_1),
        (1.96, 0.975_002_104_851_779_5),
        (-1.0, 0.158_655_253_931_457_07),
        (0.15, 0.559_617_692_370_242_5),
    ];

    #[test]
    fn cdf_matches_scipy_to_1e14() {
        for (x, want) in CDF_CASES {
            assert!((norm_cdf(x) - want).abs() < 1e-14, "cdf({x})");
        }
    }

    #[test]
    fn cdf_symmetry_and_saturation() {
        assert!((norm_cdf(3.0) + norm_cdf(-3.0) - 1.0).abs() < 1e-15);
        assert_eq!(norm_cdf(40.0), 1.0);
        assert_eq!(norm_cdf(-40.0), 0.0);
    }

    #[test]
    fn inverse_matches_scipy_to_1e13() {
        let cases = [
            (0.975, 1.959_963_984_540_054),
            (0.9, 1.281_551_565_544_600_4),
            (0.999, 3.090_232_306_167_813),
            (1e-10, -6.361_340_902_404_056),
            (0.5, 0.0),
        ];
        for (p, want) in cases {
            assert!((norm_cdf_inv(p) - want).abs() < 1e-13, "ppf({p})");
        }
    }

    #[test]
    fn cdf_inverse_roundtrip() {
        for i in 1..2000 {
            let p = f64::from(i) / 2000.0;
            let x = norm_cdf_inv(p);
            assert!((norm_cdf(x) - p).abs() < 1e-12, "roundtrip p={p}");
        }
    }

    #[test]
    fn inverse_rejects_boundary() {
        assert!(norm_cdf_inv(0.0).is_nan());
        assert!(norm_cdf_inv(1.0).is_nan());
        assert!(norm_cdf_inv(-0.1).is_nan());
    }

    #[test]
    fn pdf_peak_and_symmetry() {
        assert!((norm_pdf(0.0) - INV_SQRT_TAU).abs() < 1e-16);
        assert!((norm_pdf(1.3) - norm_pdf(-1.3)).abs() < 1e-16);
        assert!((norm_log_pdf(0.7) - norm_pdf(0.7).ln()).abs() < 1e-12);
    }
}
