//! View 7: the local internet breakouts, per site and VLAN. Their public
//! IPv4 and IPv6 destinations leave through that site's own uplink instead
//! of the tunnel; for this site with what it does now.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, Wrap};
use wanderlan_core::api::{BreakoutFamily, BreakoutStatus};
use wanderlan_core::config::FieldPath;

use super::{diagnostic_line, list, not_configured, severity_style, titled};
use crate::tui::app::App;
use crate::tui::{format, theme};

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let Some(draft) = &app.draft else {
        not_configured(frame, area);
        return;
    };
    let [uplinks, table_rect, detail] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(4),
        Constraint::Length(3),
    ])
    .areas(area);
    draw_uplinks(frame, app, uplinks);

    let me = draft.local.self_name.clone().unwrap_or_default();
    let live: &[BreakoutStatus] = app.status.as_ref().map_or(&[], |s| &s.breakout);
    let running = app.running.as_ref().and_then(|r| r.network.as_ref());
    let rows = app.breakout_rows();
    let selected = app.breakouts_selected.min(rows.len().saturating_sub(1));
    let header = Row::new(vec![
        "  SITE", "VLAN", "UPLINK", "IPV4", "IPV6", "EXC", "OUT/BACK",
    ])
    .style(theme::header());
    let family = |f: &BreakoutFamily| match &f.dev {
        Some(dev) => Span::styled(format!("out {dev}"), theme::ok()),
        None => Span::styled("tunnel", theme::warn()),
    };
    let table_rows: Vec<Row> = rows
        .iter()
        .map(|(site, vid)| {
            let index = draft
                .network
                .sites
                .iter()
                .position(|s| s.name == *site)
                .unwrap_or_default();
            let entry = draft.network.site(site).and_then(|s| s.breakout.get(vid));
            let marker = match running
                .and_then(|n| n.site(site))
                .map(|s| s.breakout.get(vid))
            {
                Some(Some(old)) if Some(old) == entry => Span::raw("  "),
                Some(Some(_)) => Span::styled("* ", theme::warn()),
                Some(None) | None if running.is_some() => Span::styled("+ ", theme::ok()),
                _ => Span::raw("  "),
            };
            let shown = draft
                .network
                .site(site)
                .map_or_else(|| site.clone(), format::site_with_kind);
            let uplink = entry
                .and_then(|b| b.uplink.clone())
                .unwrap_or_else(|| "auto".into());
            let except = entry.map_or(0, |b| b.except.len());
            // One per segment (bridge) of this site with the VLAN.
            let states: Vec<&BreakoutStatus> = if *site == me {
                live.iter().filter(|b| b.vid == *vid).collect()
            } else {
                Vec::new()
            };
            let (v4, v6, traffic) = match states.first() {
                Some(b) => (
                    family(&b.ipv4),
                    family(&b.ipv6),
                    if states.iter().all(|s| s.ready) {
                        let rx: u64 = states.iter().map(|s| s.rx_bps).sum();
                        let tx: u64 = states.iter().map(|s| s.tx_bps).sum();
                        let segments = if states.len() > 1 {
                            format!(" ({} segments)", states.len())
                        } else {
                            String::new()
                        };
                        format!("{}/{}{segments}", format::rate(rx), format::rate(tx))
                    } else {
                        "setting up".into()
                    },
                ),
                // Another site: only it knows what it does.
                None => (
                    Span::styled("-", theme::dim()),
                    Span::styled("-", theme::dim()),
                    String::new(),
                ),
            };
            Row::new(vec![
                Cell::from(Line::from(vec![marker, Span::raw(shown)])),
                Cell::from(if *vid == wanderlan_core::config::NATIVE_VID {
                    "native".to_string()
                } else {
                    vid.to_string()
                }),
                Cell::from(uplink),
                Cell::from(v4),
                Cell::from(v6),
                Cell::from(if except == 0 {
                    String::new()
                } else {
                    except.to_string()
                }),
                Cell::from(traffic),
            ])
            .style(severity_style(
                draft.worst_within(&FieldPath::site_field(index, &format!("breakout.{vid}"))),
            ))
        })
        .collect();
    let total = table_rows.len();
    let widths = [
        Constraint::Length(18),
        Constraint::Length(4),
        Constraint::Length(9),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(3),
        Constraint::Min(8),
    ];
    let title = format!(" Local breakout: {total} VLAN(s) ");
    let table = Table::new(table_rows, widths)
        .header(header)
        .block(titled(title))
        .row_highlight_style(theme::selected());
    if total == 0 {
        frame.render_widget(
            Paragraph::new(vec![
                Line::raw(""),
                Line::styled(
                    " No VLAN breaks out: everything goes through the tunnels.",
                    theme::dim(),
                ),
                Line::styled(
                    " n lets a VLAN of a site reach the internet through that site's own uplink.",
                    theme::dim(),
                ),
            ])
            .block(titled(" Local breakout ")),
            table_rect,
        );
    } else {
        list::show(
            frame,
            table,
            table_rect,
            &app.scrolls.breakouts,
            Some(selected),
            total,
            list::BOXED,
        );
    }

    // The selected row: its problems, or why a family goes through the
    // tunnel, or what the view is about.
    let mut lines: Vec<Line> = Vec::new();
    if let Some((site, vid)) = rows.get(selected) {
        if let Some(i) = draft.network.sites.iter().position(|s| s.name == *site) {
            let path = FieldPath::site_field(i, &format!("breakout.{vid}"));
            lines.extend(draft.within(&path).map(diagnostic_line));
        }
        let states: Vec<&BreakoutStatus> = if *site == me {
            live.iter().filter(|b| b.vid == *vid).collect()
        } else {
            Vec::new()
        };
        let several = states.len() > 1;
        // Another firewall drops what is forwarded: first, it stops it all.
        if let Some(why) = states.iter().find_map(|b| b.blocked.clone()) {
            lines.push(Line::styled(format!(" {why}"), theme::bad()));
        }
        for b in states {
            // Several segments: each named by its bridge.
            let at = if several {
                format!(" [{}]", b.bridge)
            } else {
                String::new()
            };
            for (name, f) in [("IPv4", &b.ipv4), ("IPv6", &b.ipv6)] {
                match (&f.dev, &f.reason) {
                    (Some(dev), _) => {
                        let via = f
                            .gateway
                            .as_ref()
                            .filter(|g| *g != "direct")
                            .map_or_else(String::new, |g| format!(" via {g}"));
                        lines.push(Line::styled(
                            format!(
                                "{at} {name}: public destinations leave out of {dev}{via}, masqueraded"
                            ),
                            theme::dim(),
                        ));
                    }
                    (None, Some(why)) => lines.push(Line::styled(
                        format!("{at} {name} goes through the tunnel: {why}"),
                        theme::warn(),
                    )),
                    (None, None) => {}
                }
            }
            if !b.except.is_empty() {
                lines.push(Line::styled(
                    format!(" Through the tunnel also: {}", b.except.join(", ")),
                    theme::dim(),
                ));
            }
        }
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            " Devices keep their gateway and DHCP; only public destinations leave locally (private ones and the exceptions use the tunnel).",
            theme::dim(),
        ));
    }
    lines.truncate(3);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), detail);
}

/// The uplinks of this box: the interfaces with a gateway.
fn draw_uplinks(frame: &mut Frame, app: &App, area: Rect) {
    let uplinks = app.status.as_ref().map_or(&[][..], |s| &s.uplinks[..]);
    let with_gateway: Vec<_> = uplinks
        .iter()
        .filter(|u| u.gateway4.is_some() || u.gateway6.is_some())
        .collect();
    let more = with_gateway.len().saturating_sub(2);
    let mut lines: Vec<Line> = with_gateway
        .into_iter()
        .take(2)
        .map(|u| {
            let default = match (u.default4, u.default6) {
                (true, true) => " (default route)",
                (true, false) => " (IPv4 default route)",
                (false, true) => " (IPv6 default route)",
                (false, false) => "",
            };
            let addresses = u.addresses.join(", ");
            Line::from(vec![
                Span::styled(format!(" {}", u.name), theme::bold()),
                Span::styled(default.to_string(), theme::dim()),
                Span::raw(format!("  {addresses}")),
                Span::styled(if u.up { "" } else { "  down" }.to_string(), theme::bad()),
            ])
        })
        .collect();
    if more > 0
        && let Some(last) = lines.last_mut()
    {
        last.spans
            .push(Span::styled(format!("  (+{more} more)"), theme::dim()));
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            " No interface of this box has a gateway (an uplink to the internet).",
            theme::dim(),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines).block(titled(" Uplinks of this box ")),
        area,
    );
}
