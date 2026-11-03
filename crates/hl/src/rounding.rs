//! Tick and lot rules for perps: prices have at most five significant figures and at most
//! `6 − szDecimals` decimals (integers are always valid); sizes have at most `szDecimals`
//! decimals. Wire strings carry no trailing zeros, as the Python SDK's `float_to_wire`.

use anyhow::{Result, bail};

/// Which way to round when a value doesn't fit the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Round {
    Nearest,
    /// Towards zero, e.g. a market buy's price cap, so slippage never exceeds the cap.
    Down,
    /// Away from zero, e.g. a market sell's price floor.
    Up,
}

fn round_to(x: f64, decimals: i32, dir: Round) -> f64 {
    let scale = 10f64.powi(decimals);
    let v = x * scale;
    // A hair of tolerance so 226.4 stays 226.4 when the float is 226.39999999.
    let r = match dir {
        Round::Nearest => v.round(),
        Round::Down => (v + 1e-7).floor(),
        Round::Up => (v - 1e-7).ceil(),
    };
    r / scale
}

/// The most decimals a perp price may have at this level.
pub fn price_decimals(px: f64, sz_decimals: u32) -> i32 {
    let max = 6 - sz_decimals.min(6) as i32;
    if px.abs() >= 100_000.0 {
        return 0;
    }
    let sig = 4 - px.abs().log10().floor() as i32;
    sig.clamp(0, max)
}

/// A valid perp price: five significant figures and `6 − szDecimals` decimals at most.
pub fn round_price(px: f64, sz_decimals: u32, dir: Round) -> f64 {
    if !px.is_finite() || px <= 0.0 {
        return px;
    }
    let d = price_decimals(px, sz_decimals);
    let r = round_to(px, d, dir);
    // Rounding up can add a digit (99999.5 → 100000); one more pass settles it.
    round_to(r, price_decimals(r, sz_decimals), dir)
}

/// A valid size: `szDecimals` decimals, rounded down so an order never grows.
pub fn round_size(sz: f64, sz_decimals: u32) -> f64 {
    round_to(sz, sz_decimals as i32, Round::Down)
}

/// Whether `px` is already on the price grid.
pub fn price_ok(px: f64, sz_decimals: u32) -> bool {
    px.is_finite() && px > 0.0 && (round_price(px, sz_decimals, Round::Nearest) - px).abs() <= px.abs() * 1e-12
}

/// `float_to_wire`: eight decimals at most, no trailing zeros, never `-0`. Refuses a value
/// that would change by rounding.
pub fn to_wire(x: f64) -> Result<String> {
    if !x.is_finite() {
        bail!("not a number");
    }
    let s = format!("{x:.8}");
    if (s.parse::<f64>()? - x).abs() >= 1e-12 {
        bail!("{x} has more than 8 decimals");
    }
    let t = s.trim_end_matches('0').trim_end_matches('.');
    Ok(if t == "-0" || t.is_empty() { "0".into() } else { t.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_keep_five_significant_figures() {
        // xyz:AAPL (szDecimals 3): three decimals at most.
        assert_eq!(round_price(226.4, 3, Round::Nearest), 226.4);
        assert_eq!(round_price(226.456, 3, Round::Nearest), 226.46, "five significant figures");
        assert_eq!(round_price(4.56789, 3, Round::Nearest), 4.568, "and 6 − 3 = 3 decimals");
        // BTC (szDecimals 5): one decimal at most, integers always valid.
        assert_eq!(round_price(76662.57, 5, Round::Nearest), 76663.0);
        assert_eq!(round_price(123456.7, 5, Round::Nearest), 123457.0, "integers are valid past five figures");
        assert_eq!(round_price(1234.56, 5, Round::Nearest), 1234.6);
        // Small prices.
        assert_eq!(round_price(0.0123456, 0, Round::Nearest), 0.012346);
        assert_eq!(round_price(0.000123456, 0, Round::Nearest), 0.000123, "six decimals at most");
    }

    #[test]
    fn directed_rounding_never_overshoots() {
        assert_eq!(round_price(226.456, 3, Round::Down), 226.45);
        assert_eq!(round_price(226.451, 3, Round::Up), 226.46);
        assert_eq!(round_price(99999.6, 0, Round::Up), 100000.0);
        assert_eq!(round_price(226.4, 3, Round::Down), 226.4, "on-grid values stay put");
        assert_eq!(round_price(226.4, 3, Round::Up), 226.4);
    }

    #[test]
    fn sizes_round_down_to_lot() {
        assert_eq!(round_size(0.123456, 5), 0.12345);
        assert_eq!(round_size(0.5, 3), 0.5);
        assert_eq!(round_size(0.0009, 3), 0.0, "too small rounds to nothing");
        assert_eq!(round_size(0.3, 1), 0.3, "0.3 is not 0.2999…");
    }

    #[test]
    fn wire_strings_match_float_to_wire() {
        assert_eq!(to_wire(100.0).unwrap(), "100");
        assert_eq!(to_wire(1670.1).unwrap(), "1670.1");
        assert_eq!(to_wire(0.0147).unwrap(), "0.0147");
        assert_eq!(to_wire(0.0002).unwrap(), "0.0002");
        assert_eq!(to_wire(-0.0).unwrap(), "0");
        assert_eq!(to_wire(0.0).unwrap(), "0");
        assert!(to_wire(0.123456789).is_err(), "would round");
        assert!(to_wire(f64::NAN).is_err());
        assert!(price_ok(226.4, 3) && !price_ok(226.456, 3));
    }
}
