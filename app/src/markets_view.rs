//! Cross-asset panels: WEI-style world markets overview, US Treasury yield curve and
//! security search.

use std::collections::HashMap;

use egui::{Align2, Color32, Pos2, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};
use tr_core::catalog::{self, Group, Listing};
use tr_core::{AssetClass, Security};
use tr_data::{Quote, SearchHit, YieldCurve, YieldCurves};

use crate::congress_view::Action;
use crate::theme::{self, amber, amber_dim, mono, muted, text, ui_font};

/// Every market key shown on the overview panel.
pub fn overview_keys() -> Vec<String> {
    Group::OVERVIEW.iter().flat_map(|g| catalog::group(*g).map(Listing::key)).collect()
}

/// Net change text: basis points for yields, price units otherwise.
pub fn change_text(sec: &Security, q: &Quote) -> String {
    if sec.class == AssetClass::Govt {
        format!("{:+.1}bp", q.change() * 100.0)
    } else {
        let d = sec.decimals(q.last);
        format!("{:+.d$}", q.change())
    }
}

fn sparkline(ui: &Ui, rect: Rect, values: &[f64], color: Color32) {
    let v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
    if v.len() < 2 {
        return;
    }
    let (lo, hi) = v.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), &x| (a.min(x), b.max(x)));
    let span = (hi - lo).max(hi.abs() * 1e-9).max(1e-12);
    let pts: Vec<Pos2> = v
        .iter()
        .enumerate()
        .map(|(i, &x)| {
            pos2(
                rect.left() + rect.width() * i as f32 / (v.len() - 1) as f32,
                rect.bottom() - rect.height() * ((x - lo) / span) as f32,
            )
        })
        .collect();
    ui.painter().add(egui::Shape::line(pts, Stroke::new(1.2, color)));
}

fn row(ui: &mut Ui, l: &Listing, quotes: &HashMap<String, Quote>, out: &mut Vec<Action>, width: f32) {
    let sec = l.security();
    let key = l.key();
    let q = quotes.get(&key);
    let h = theme::dp(16.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, h), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 3.0, theme::control());
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.clicked() {
        out.push(Action::OpenChart(key.clone()));
    }
    let p = ui.painter_at(rect);
    let y = rect.center().y;
    let x = rect.left() + 2.0;
    p.text(pos2(x, y), Align2::LEFT_CENTER, l.ticker, mono(12.0), amber());
    let right = rect.right() - 2.0;
    // Narrow columns drop the net change, then the sparkline, then the name.
    let compact = width < 330.0;
    let last_right = if compact { right - 66.0 } else { right - 150.0 };
    let mut name_right = last_right - 92.0;
    match q {
        Some(q) => {
            let c = if q.change() >= 0.0 { theme::up() } else { theme::down() };
            let d = sec.decimals(q.last);
            let pct =
                if sec.class == AssetClass::Govt { change_text(&sec, q) } else { format!("{:+.2}%", q.change_pct()) };
            p.text(pos2(right, y), Align2::RIGHT_CENTER, pct, mono(12.0), c);
            if !compact {
                p.text(pos2(right - 66.0, y), Align2::RIGHT_CENTER, change_text(&sec, q), mono(12.0), c);
            }
            p.text(pos2(last_right, y), Align2::RIGHT_CENTER, format!("{:.d$}", q.last), mono(12.0), text());
            let spark = Rect::from_min_size(pos2(name_right - 52.0, rect.top() + 3.0), vec2(48.0, h - 6.0));
            if spark.left() > rect.left() + 180.0 {
                sparkline(ui, spark, &q.spark, c);
                name_right = spark.left() - 6.0;
            }
        }
        None => {
            p.text(pos2(right, y), Align2::RIGHT_CENTER, "…", mono(12.0), muted());
        }
    }
    let name_left = x + 70.0;
    if name_right - name_left > 36.0 {
        let clip = Rect::from_min_max(pos2(name_left, rect.top()), pos2(name_right, rect.bottom()));
        p.with_clip_rect(clip).text(pos2(name_left, y), Align2::LEFT_CENTER, l.name, ui_font(11.5), muted());
    }
}

/// WEI-style overview: regional indices, futures, Treasuries, FX, crypto, commodities.
pub fn wei(ui: &mut Ui, quotes: &HashMap<String, Quote>, updated: Option<&str>) -> Vec<Action> {
    let mut out = Vec::new();
    let width = ui.available_width();
    let cols = ((width / 460.0).floor() as usize).clamp(1, 3);
    let col_w = (width - 8.0 * (cols - 1) as f32) / cols as f32;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("WORLD MARKETS").font(mono(12.5)).color(amber()));
        ui.label(RichText::new(updated.unwrap_or("loading quotes…")).font(mono(11.0)).color(muted()));
        ui.label(RichText::new("click a row to chart it").font(mono(11.0)).color(muted()));
    });
    egui::ScrollArea::vertical().id_salt("wei").auto_shrink([false, false]).show(ui, |ui| {
        // Distribute groups across columns, keeping them in display order.
        let groups = Group::OVERVIEW;
        let per = groups.len().div_ceil(cols);
        ui.horizontal_top(|ui| {
            for c in 0..cols {
                ui.vertical(|ui| {
                    ui.set_width(col_w);
                    for g in groups.iter().skip(c * per).take(per) {
                        let (r, _) = ui.allocate_exact_size(vec2(col_w, 20.0), Sense::hover());
                        ui.painter().rect_filled(r, theme::CONTROL_RADIUS, theme::header());
                        ui.painter().text(
                            r.left_center() + vec2(6.0, 0.0),
                            Align2::LEFT_CENTER,
                            g.label(),
                            ui_font(12.0),
                            text(),
                        );
                        let hdr = if *g == Group::Rates { "YIELD     CHG" } else { "LAST     NET CHG    %CHG" };
                        ui.painter().text(
                            r.right_center() - vec2(4.0, 0.0),
                            Align2::RIGHT_CENTER,
                            hdr,
                            mono(10.5),
                            amber_dim(),
                        );
                        for l in catalog::group(*g) {
                            row(ui, l, quotes, &mut out, col_w);
                        }
                        ui.add_space(6.0);
                    }
                });
            }
        });
    });
    out
}

/// Tenor label for the curve axis (`1M`, `10Y`).
pub fn tenor_label(years: f64) -> String {
    if years < 1.0 { format!("{}M", (years * 12.0).round() as i64) } else { format!("{}Y", years.round() as i64) }
}

/// Yield curve chart and tenor table.
pub fn curve(ui: &mut Ui, curves: Option<&YieldCurves>) -> Vec<Action> {
    let mut out = Vec::new();
    let Some(c) = curves else {
        ui.label(RichText::new("Loading US Treasury par yield curve (Treasury.gov)…").font(mono(12.0)).color(muted()));
        return out;
    };
    let date =
        |t: i64| chrono::DateTime::from_timestamp(t, 0).map(|d| d.format("%d %b %Y").to_string()).unwrap_or_default();
    let lines: Vec<(&YieldCurve, Color32, String)> = [
        Some((&c.current, amber(), format!("Current {}", date(c.current.ts)))),
        c.month_ago.as_ref().map(|m| (m, Color32::from_gray(150), format!("1M ago {}", date(m.ts)))),
        c.year_ago.as_ref().map(|y| (y, theme::dem(), format!("1Y ago {}", date(y.ts)))),
    ]
    .into_iter()
    .flatten()
    .collect();
    ui.horizontal(|ui| {
        for (_, col, label) in &lines {
            ui.label(RichText::new(format!("■ {label}")).font(mono(11.5)).color(*col));
        }
    });
    let avail = ui.available_size();
    let plot_h = (avail.y * 0.58).max(120.0);
    let (rect, _) = ui.allocate_exact_size(vec2(avail.x, plot_h), Sense::hover());
    let p = ui.painter_at(rect);
    let plot = Rect::from_min_max(rect.min + vec2(8.0, 8.0), rect.max - vec2(56.0, 22.0));
    let tenors: Vec<&Listing> = catalog::treasury_curve();
    let n = tenors.len().max(2);
    let all: Vec<f64> = lines.iter().flat_map(|(cv, _, _)| cv.points.iter().map(|p| p.yield_pct)).collect();
    let (lo, hi) = all.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), &x| (a.min(x), b.max(x)));
    if !lo.is_finite() {
        return out;
    }
    let pad = ((hi - lo) * 0.12).max(0.1);
    let (lo, hi) = (lo - pad, hi + pad);
    let x_of = |i: usize| plot.left() + plot.width() * i as f32 / (n - 1) as f32;
    let y_of = |v: f64| plot.bottom() - plot.height() * ((v - lo) / (hi - lo)) as f32;
    for t in tr_chart::scale::price_ticks(lo, hi, 6) {
        let y = y_of(t);
        p.hline(plot.x_range(), y, Stroke::new(1.0, theme::grid()));
        p.text(pos2(plot.right() + 6.0, y), Align2::LEFT_CENTER, format!("{t:.2}%"), mono(10.5), muted());
    }
    for (i, l) in tenors.iter().enumerate() {
        let x = x_of(i);
        p.vline(x, plot.y_range(), Stroke::new(1.0, theme::grid()));
        p.text(
            pos2(x, plot.bottom() + 10.0),
            Align2::CENTER_CENTER,
            tenor_label(l.tenor_years.unwrap_or(0.0)),
            mono(10.5),
            muted(),
        );
    }
    for (cv, col, _) in lines.iter().rev() {
        let pts: Vec<Pos2> =
            tenors.iter().enumerate().filter_map(|(i, l)| cv.get(l.ticker).map(|v| pos2(x_of(i), y_of(v)))).collect();
        p.add(egui::Shape::line(pts.clone(), Stroke::new(if *col == amber() { 2.2 } else { 1.4 }, *col)));
        for q in pts {
            p.circle_filled(q, 2.6, *col);
        }
    }
    // Table: yield and changes per tenor, plus key spreads.
    let bp = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(a), Some(b)) => format!("{:+7.1}", (a - b) * 100.0),
        _ => "    n/a".into(),
    };
    ui.label(
        RichText::new(" TENOR   YIELD   1M CHG(bp)  1Y CHG(bp)")
            .font(mono(11.5))
            .color(amber())
            .background_color(theme::header()),
    );
    egui::ScrollArea::vertical().id_salt("curve-table").auto_shrink([false, false]).show(ui, |ui| {
        for l in &tenors {
            let cur = c.current.get(l.ticker);
            let m = c.month_ago.as_ref().and_then(|x| x.get(l.ticker));
            let y = c.year_ago.as_ref().and_then(|x| x.get(l.ticker));
            let line = format!(
                " {:<6} {:>6}   {}      {}",
                tenor_label(l.tenor_years.unwrap_or(0.0)),
                cur.map(|v| format!("{v:.3}")).unwrap_or_default(),
                bp(cur, m),
                bp(cur, y)
            );
            let r = ui.add(
                egui::Label::new(RichText::new(line).font(mono(12.0)).color(text())).extend().sense(Sense::click()),
            );
            if r.clicked() {
                out.push(Action::OpenChart(l.key()));
            }
            r.on_hover_text(format!("Click to chart {}", l.key()));
        }
        let spread = |a: &str, b: &str| match (c.current.get(a), c.current.get(b)) {
            (Some(x), Some(y)) => format!("{:+.1}bp", (x - y) * 100.0),
            _ => "n/a".into(),
        };
        ui.label(
            RichText::new(format!(
                " 2s10s {}   3m10y {}   5s30s {}",
                spread("US10Y", "US2Y"),
                spread("US10Y", "US3M"),
                spread("US30Y", "US5Y")
            ))
            .font(mono(12.0))
            .color(amber()),
        );
    });
    out
}

/// A search hit for a HIP-3 perp, labelled with its venue.
pub fn perp_hit(key: &str) -> SearchHit {
    let name = tr_core::perps::link_for(key).map_or_else(
        || format!("Hyperliquid {} perp", tr_core::perps::venue(key).unwrap_or("HIP-3")),
        |l| l.name.to_string(),
    );
    SearchHit { key: key.to_string(), name, class: AssetClass::Perp, exchange: Some(tr_core::perps::badge(key)) }
}

/// Merge catalog matches, then HIP-3 perps among `perps`, then network hits, without duplicates.
pub fn merged_hits(query: &str, network: &[SearchHit], perps: &[&str]) -> Vec<SearchHit> {
    let mut out: Vec<SearchHit> = catalog::search(query, 30)
        .into_iter()
        .map(|l| SearchHit { key: l.key(), name: l.name.to_string(), class: l.class, exchange: None })
        .collect();
    out.extend(tr_core::perps::search(query, perps.iter().copied(), 10).into_iter().map(perp_hit));
    for h in network {
        if !out.iter().any(|o| o.key == h.key) {
            out.push(h.clone());
        }
    }
    out
}

pub fn class_color(c: AssetClass) -> Color32 {
    match c {
        AssetClass::Equity => text(),
        AssetClass::Etf => Color32::from_rgb(160, 200, 255),
        AssetClass::Index => Color32::from_rgb(255, 215, 120),
        AssetClass::Fx => Color32::from_rgb(120, 230, 200),
        AssetClass::Crypto => Color32::from_rgb(255, 150, 60),
        AssetClass::Govt => Color32::from_rgb(190, 160, 255),
        AssetClass::Cmdty => Color32::from_rgb(230, 200, 90),
        AssetClass::Perp => theme::perp(),
    }
}

/// Security search panel (`SECF <query>`). `perps` are the HIP-3 perps it can offer.
pub fn search(ui: &mut Ui, query: &mut String, network: &[SearchHit], perps: &[&str]) -> (Vec<Action>, bool) {
    let mut out = Vec::new();
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new("Find:").font(mono(12.0)).color(muted()));
        changed = ui.add(egui::TextEdit::singleline(query).font(mono(12.5)).desired_width(260.0)).changed();
        ui.label(
            RichText::new("ticker or name · add CRNCY/GOVT/CMDTY/INDEX/EQUITY to filter")
                .font(mono(11.0))
                .color(muted()),
        );
    });
    ui.label(
        RichText::new(" MARKET KEY            CLASS   NAME")
            .font(mono(11.5))
            .color(amber())
            .background_color(theme::header()),
    );
    let hits = merged_hits(query, network, perps);
    if hits.is_empty() && !query.trim().is_empty() {
        ui.label(RichText::new("No matches.").font(mono(12.0)).color(muted()));
    }
    egui::ScrollArea::vertical().id_salt("secf").auto_shrink([false, false]).show(ui, |ui| {
        for h in &hits {
            let line = format!(
                " {:<20}  {:<6}  {}{}",
                h.key,
                h.class.label(),
                h.name,
                h.exchange.as_deref().map(|e| format!("  ({e})")).unwrap_or_default()
            );
            let r = ui.add(
                egui::Label::new(RichText::new(line).font(mono(12.0)).color(class_color(h.class)))
                    .extend()
                    .sense(Sense::click()),
            );
            if r.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if r.clicked() {
                out.push(Action::OpenChart(h.key.clone()));
            }
        }
    });
    (out, changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overview_and_labels() {
        let keys = overview_keys();
        assert!(keys.contains(&"SPX Index".to_string()));
        assert!(keys.contains(&"BTCUSD Curncy".to_string()));
        assert!(keys.contains(&"US10Y Govt".to_string()));
        assert!(keys.contains(&"CL1 Comdty".to_string()));
        assert_eq!(tenor_label(0.25), "3M");
        assert_eq!(tenor_label(10.0), "10Y");
    }

    #[test]
    fn change_formatting() {
        let q = Quote { symbol: "US10Y Govt".into(), last: 4.25, prev_close: 4.22, ts: 0, spark: vec![] };
        assert_eq!(change_text(&Security::parse("US10Y Govt").unwrap(), &q), "+3.0bp");
        let q = Quote { symbol: "EURUSD Curncy".into(), last: 1.0912, prev_close: 1.0900, ts: 0, spark: vec![] };
        assert_eq!(change_text(&Security::parse("EURUSD Curncy").unwrap(), &q), "+0.0012");
    }

    #[test]
    fn merged_search_dedups() {
        let net = vec![
            SearchHit {
                key: "BTCUSD Curncy".into(),
                name: "Bitcoin USD".into(),
                class: AssetClass::Crypto,
                exchange: None,
            },
            SearchHit {
                key: "BITO US Equity".into(),
                name: "ProShares Bitcoin ETF".into(),
                class: AssetClass::Etf,
                exchange: None,
            },
        ];
        let m = merged_hits("bitcoin", &net, &[]);
        assert_eq!(m.iter().filter(|h| h.key == "BTCUSD Curncy").count(), 1);
        assert!(m.iter().any(|h| h.key == "BITO US Equity"));
    }

    /// The listed instrument comes first; its perp follows, labelled as one.
    #[test]
    fn search_offers_perps_after_listed_instruments() {
        let m = merged_hits("aapl", &[], &["xyz:AAPL", "xyz:GOLD"]);
        let keys: Vec<&str> = m.iter().map(|h| h.key.as_str()).collect();
        assert_eq!(keys, ["AAPL US Equity", "xyz:AAPL"]);
        assert_eq!((m[1].class, m[1].name.as_str()), (AssetClass::Perp, "Apple Inc perp"));
        assert_eq!(m[1].exchange.as_deref(), Some("PERP · HL xyz"));
        assert_eq!(perp_hit("xyz:SKHX").name, "Hyperliquid xyz perp");
        assert!(merged_hits("aapl", &[], &[]).iter().all(|h| h.class != AssetClass::Perp));
    }
}
