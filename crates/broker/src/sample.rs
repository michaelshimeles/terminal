//! A deterministic simulated portfolio for offline mode (`TR_PROVIDER=sample`) and demos.
//! It is always flagged `simulated` and labelled as such in the UI.

use chrono::{Duration, TimeZone, Utc};

use crate::model::{Account, Activity, Connection, InstrumentKind, Order, Portfolio, Position};

fn account(id: &str, name: &str, number: &str, institution: &str, kind: &str, cash: f64, bp: f64) -> Account {
    let connection_id = if institution.contains("Exchange") { "sim-conn-exchange" } else { "sim-conn-broker" }.into();
    Account {
        connection_id,
        id: id.into(),
        name: name.into(),
        number: number.into(),
        institution: institution.into(),
        currency: "USD".into(),
        total: None,
        cash: Some(cash),
        buying_power: Some(bp),
        kind: kind.into(),
        status: "open".into(),
        is_paper: false,
        synced: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn position(
    account: &str,
    symbol: &str,
    desc: &str,
    kind: InstrumentKind,
    units: f64,
    price: f64,
    cost: f64,
) -> Position {
    Position {
        account_id: account.into(),
        symbol: symbol.into(),
        description: desc.into(),
        kind,
        units,
        price: Some(price),
        cost_basis: Some(cost),
        multiplier: 1.0,
        currency: "USD".into(),
        cash_equivalent: false,
        underlying: None,
        market: None,
    }
}

/// Sample accounts at a broker, an IRA and a crypto exchange.
pub fn portfolio() -> Portfolio {
    use InstrumentKind as K;
    let now = Utc::now();
    let mut accounts = vec![
        account(
            "sim-brokerage",
            "Individual Brokerage",
            "SIM-0000-4417",
            "Simulated Broker",
            "Margin",
            8_420.55,
            16_841.10,
        ),
        account("sim-ira", "Roth IRA", "SIM-0000-9032", "Simulated Broker", "Roth IRA", 1_250.00, 1_250.00),
        account("sim-crypto", "Crypto", "SIM-0000-2206", "Simulated Exchange", "Cash", 615.30, 615.30),
    ];
    let mut positions = vec![
        position("sim-brokerage", "AAPL", "Apple Inc.", K::Stock, 120.0, 339.50, 187.40),
        position("sim-brokerage", "NVDA", "NVIDIA Corporation", K::Stock, 85.0, 226.50, 128.10),
        position("sim-brokerage", "MSFT", "Microsoft Corporation", K::Stock, 40.0, 512.30, 401.75),
        position("sim-brokerage", "TSLA", "Tesla, Inc.", K::Stock, -15.0, 291.20, 318.60),
        position("sim-brokerage", "QQQ", "Invesco QQQ Trust", K::Etf, 30.0, 602.15, 488.90),
        position("sim-ira", "SPY", "SPDR S&P 500 ETF Trust", K::Etf, 55.0, 777.40, 512.35),
        position("sim-ira", "VXUS", "Vanguard Total International Stock ETF", K::Etf, 140.0, 71.85, 58.20),
        position("sim-crypto", "BTC", "Bitcoin", K::Crypto, 0.42, 82_500.0, 61_230.0),
        position("sim-crypto", "ETH", "Ethereum", K::Crypto, 3.5, 2_513.0, 2_890.0),
    ];
    let mut call =
        position("sim-brokerage", "AAPL  261120C00350000", "AAPL 20 Nov 26 350 C", K::Option, 2.0, 7.25, 5.10);
    call.multiplier = 100.0;
    call.underlying = Some("AAPL".into());
    positions.push(call);
    for a in &mut accounts {
        let held: f64 = positions.iter().filter(|p| p.account_id == a.id).filter_map(|p| p.market_value()).sum();
        a.total = Some(a.cash.unwrap_or(0.0) + held);
        a.synced = Some(now - Duration::minutes(4));
    }

    let at = |days: i64, h: u32| {
        let d = (now - Duration::days(days)).date_naive();
        Utc.from_utc_datetime(&d.and_hms_opt(h, 30, 0).unwrap())
    };
    let order = |acct: &str,
                 id: &str,
                 sym: &str,
                 kind: InstrumentKind,
                 action: &str,
                 status: &str,
                 ty: &str,
                 qty: f64,
                 limit: Option<f64>,
                 exec: Option<f64>,
                 days: i64| Order {
        account_id: acct.into(),
        id: id.into(),
        symbol: sym.into(),
        description: String::new(),
        kind,
        action: action.into(),
        status: status.into(),
        order_type: ty.into(),
        time_in_force: if status == "PENDING" { "GTC".into() } else { "Day".into() },
        quantity: Some(qty),
        filled: Some(if status == "EXECUTED" { qty } else { 0.0 }),
        limit_price: limit,
        stop_price: None,
        execution_price: exec,
        placed: Some(at(days, 14)),
        updated: Some(at(days, 15)),
        underlying: None,
        market: None,
    };
    let orders = vec![
        order("sim-brokerage", "sim-o-1", "NVDA", K::Stock, "BUY", "PENDING", "Limit", 10.0, Some(205.00), None, 0),
        order("sim-brokerage", "sim-o-2", "AAPL", K::Stock, "SELL", "PENDING", "Limit", 20.0, Some(365.00), None, 1),
        order("sim-brokerage", "sim-o-3", "MSFT", K::Stock, "BUY", "EXECUTED", "Market", 10.0, None, Some(498.12), 3),
        order(
            "sim-brokerage",
            "sim-o-4",
            "TSLA",
            K::Stock,
            "SELL_SHORT",
            "EXECUTED",
            "Limit",
            15.0,
            Some(318.60),
            Some(318.60),
            6,
        ),
        order("sim-ira", "sim-o-5", "SPY", K::Etf, "BUY", "EXECUTED", "Market", 5.0, None, Some(760.44), 9),
        order("sim-crypto", "sim-o-6", "ETH", K::Crypto, "BUY", "CANCELED", "Limit", 1.0, Some(2_300.0), None, 12),
    ];
    let act = |acct: &str,
               id: &str,
               kind: &str,
               sym: Option<&str>,
               desc: &str,
               units: Option<f64>,
               price: Option<f64>,
               amount: f64,
               days: i64| Activity {
        account_id: acct.into(),
        id: id.into(),
        kind: kind.into(),
        symbol: sym.map(Into::into),
        description: desc.into(),
        units,
        price,
        amount: Some(amount),
        fee: Some(0.0),
        currency: "USD".into(),
        trade_date: Some(at(days, 14)),
        settlement_date: Some(at(days - 1, 0)),
        market: None,
    };
    let activities = vec![
        act(
            "sim-brokerage",
            "sim-a-1",
            "BUY",
            Some("MSFT"),
            "Bought 10 MSFT @ 498.12",
            Some(10.0),
            Some(498.12),
            -4_981.20,
            3,
        ),
        act(
            "sim-brokerage",
            "sim-a-2",
            "SELL",
            Some("TSLA"),
            "Sold short 15 TSLA @ 318.60",
            Some(-15.0),
            Some(318.60),
            4_779.00,
            6,
        ),
        act("sim-ira", "sim-a-3", "DIVIDEND", Some("SPY"), "SPY quarterly dividend", None, None, 98.45, 8),
        act("sim-ira", "sim-a-4", "BUY", Some("SPY"), "Bought 5 SPY @ 760.44", Some(5.0), Some(760.44), -3_802.20, 9),
        act("sim-ira", "sim-a-5", "CONTRIBUTION", None, "Annual contribution", None, None, 7_000.00, 10),
        act("sim-brokerage", "sim-a-6", "INTEREST", None, "Cash sweep interest", None, None, 31.27, 14),
        act("sim-crypto", "sim-a-7", "BUY", Some("BTC"), "Bought 0.05 BTC", Some(0.05), Some(79_840.0), -3_992.00, 21),
    ];
    let connection = |id: &str, brokerage: &str, slug: &str, days: i64| Connection {
        id: id.into(),
        brokerage: brokerage.into(),
        slug: slug.into(),
        name: "Connection-1".into(),
        disabled: false,
        created: Some(now - Duration::days(days)),
        updated: Some(now - Duration::minutes(4)),
        trading: false,
    };
    let connections = vec![
        connection("sim-conn-broker", "Simulated Broker", "SIMULATED-BROKER", 240),
        connection("sim-conn-exchange", "Simulated Exchange", "SIMULATED-EXCHANGE", 90),
    ];
    Portfolio {
        accounts,
        positions,
        orders,
        activities,
        connections,
        fetched: Some(now),
        warnings: Vec::new(),
        simulated: true,
        demo: false,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sample_is_consistent() {
        let pf = super::portfolio();
        assert!(pf.simulated);
        for p in &pf.positions {
            assert!(pf.account(&p.account_id).is_some(), "{} has an account", p.symbol);
            assert!(p.market_key().is_some(), "{} maps to a market key", p.symbol);
        }
        let t = pf.totals(None, |_| None);
        let sum: f64 = pf.accounts.iter().filter_map(|a| a.total).sum();
        assert!((t.value - sum).abs() < 1e-6);
        assert!(t.pnl > 0.0);
        assert!(pf.orders.iter().any(|o| o.is_open()));
        for c in &pf.connections {
            assert!(pf.accounts_of(&c.id).count() > 0, "{} holds accounts", c.brokerage);
        }
    }
}
