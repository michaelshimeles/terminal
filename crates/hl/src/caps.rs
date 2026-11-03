//! Hard limits, checked in this crate before anything is signed (the ticket checks them too,
//! but the ticket is not the safety): no order over $100 notional, no leverage above 3×, no
//! market order more than 1% from the mid. They are constants, not settings: nothing in this
//! build can raise them.

use std::fmt;

/// The most an order may be worth: size × the higher of its price and the mid.
pub const MAX_ORDER_NOTIONAL_USD: f64 = 100.0;
/// The most leverage Terminal sets, or places an order under.
pub const MAX_LEVERAGE: u32 = 3;
/// A market order is an IOC limit this far from the mid, at most.
pub const MAX_MARKET_SLIPPAGE: f64 = 0.01;

/// Why an action was refused before signing.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    Notional {
        value: f64,
    },
    Leverage {
        value: u32,
    },
    /// The account's leverage on this market isn't known yet, so an order can't be checked.
    LeverageUnknown {
        coin: String,
    },
    Slippage {
        pct: f64,
    },
    /// No mid price, which a market order is priced from.
    NoMid {
        coin: String,
    },
    /// Anything else (market not tradable, bad size or price).
    Invalid(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Notional { value } => write!(
                f,
                "Order value ${value:.2} is over the ${MAX_ORDER_NOTIONAL_USD:.0} limit for this testnet build."
            ),
            Refusal::Leverage { value } => write!(
                f,
                "Leverage {value}× is over the {MAX_LEVERAGE}× limit for this testnet build. Set {MAX_LEVERAGE}× or less first."
            ),
            Refusal::LeverageUnknown { coin } => {
                write!(f, "Leverage on {coin} isn't known yet; wait for the account to load.")
            }
            Refusal::Slippage { pct } => {
                write!(f, "Slippage {pct:.2}% is over the {:.0}% limit.", MAX_MARKET_SLIPPAGE * 100.0)
            }
            Refusal::NoMid { coin } => write!(f, "No testnet price for {coin} yet; use a limit order or wait."),
            Refusal::Invalid(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Refusal {}

pub fn check_notional(value: f64) -> Result<(), Refusal> {
    if !value.is_finite() || value > MAX_ORDER_NOTIONAL_USD + 1e-9 {
        return Err(Refusal::Notional { value });
    }
    Ok(())
}

pub fn check_leverage(value: u32) -> Result<(), Refusal> {
    if value > MAX_LEVERAGE {
        return Err(Refusal::Leverage { value });
    }
    Ok(())
}

pub fn check_slippage(fraction: f64) -> Result<(), Refusal> {
    if !fraction.is_finite() || fraction < 0.0 || fraction > MAX_MARKET_SLIPPAGE + 1e-12 {
        return Err(Refusal::Slippage { pct: fraction * 100.0 });
    }
    Ok(())
}
