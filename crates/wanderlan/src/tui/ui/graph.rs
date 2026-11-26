//! The graphs: traffic as a mirrored area (in up, out down, each with a
//! scale of its own), round trips with the lost probes, and small text
//! sparklines for tables. The time runs from the left (the start of the
//! range) to now on the right.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Chart, Dataset, GraphType, Paragraph};
use wanderlan_core::api::{RttPoint, TrafficPoint};

use crate::tui::format;
use crate::tui::history::Range;
use crate::tui::theme;

/// The smallest top of a traffic scale (bit/s): idle shows as idle.
const MIN_TOP_BPS: u64 = 1_000;
/// How often the traffic is sampled (the daemon's rates, the answers of the
/// VLAN window): a part of the graph is never narrower.
const TRAFFIC_STEP_MS: u64 = 2_000;
/// How often a peer is probed.
const RTT_STEP_MS: u64 = 5_000;

/// [`buckets`] for `columns` columns of dots, never narrower than `step_ms`
/// (where the samples are farther apart than a column, each column shows
/// the part it lies in: no gaps between them).
#[allow(clippy::too_many_arguments)]
fn columns<T>(
    points: &[T],
    now_ms: u64,
    range_ms: u64,
    columns: usize,
    step_ms: u64,
    at: impl Fn(&T) -> u64,
    values: impl Fn(&T) -> Option<(f64, f64)>,
) -> Vec<Option<(f64, f64)>> {
    let columns = columns.max(1);
    let count = usize::try_from(range_ms / step_ms.max(1))
        .unwrap_or(columns)
        .clamp(1, columns);
    let parts = buckets(points, now_ms, range_ms, count, at, values);
    (0..columns).map(|c| parts[c * count / columns]).collect()
}

/// The next round value at or above `value`: 1, 2 or 5 times a power of ten.
pub fn nice(value: f64) -> f64 {
    if !value.is_finite() || value <= 0.0 {
        return 1.0;
    }
    let power = 10f64.powf(value.log10().floor());
    [1.0, 2.0, 5.0, 10.0]
        .iter()
        .map(|m| m * power)
        .find(|top| *top >= value * 0.999_999)
        .unwrap_or(10.0 * power)
}

/// The mean of the values in each of `count` equal parts of the time
/// before `now_ms` (`range_ms` long); `None` where nothing was recorded.
pub fn buckets<T>(
    points: &[T],
    now_ms: u64,
    range_ms: u64,
    count: usize,
    at: impl Fn(&T) -> u64,
    values: impl Fn(&T) -> Option<(f64, f64)>,
) -> Vec<Option<(f64, f64)>> {
    let count = count.max(1);
    let start = now_ms.saturating_sub(range_ms);
    let mut sums = vec![(0.0, 0.0, 0u32); count];
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    for p in points {
        let t = at(p);
        if t < start || t > now_ms {
            continue;
        }
        let Some((a, b)) = values(p) else { continue };
        let i = ((t - start) as f64 / range_ms.max(1) as f64 * count as f64) as usize;
        let slot = &mut sums[i.min(count - 1)];
        slot.0 += a;
        slot.1 += b;
        slot.2 += 1;
    }
    sums.into_iter()
        .map(|(a, b, n)| (n > 0).then(|| (a / f64::from(n), b / f64::from(n))))
        .collect()
}

/// The traffic of `points` in the last `range` before `now_ms`, mirrored:
/// in (received) above the line, out (sent) below it.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn traffic(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    points: &[TrafficPoint],
    now_ms: u64,
    range: Range,
) {
    let start = now_ms.saturating_sub(range.ms());
    let shown: Vec<&TrafficPoint> = points
        .iter()
        .filter(|p| p.0 >= start && p.0 <= now_ms)
        .collect();
    let peak = |pick: fn(&TrafficPoint) -> u64| shown.iter().map(|p| pick(p)).max().unwrap_or(0);
    let mean = |pick: fn(&TrafficPoint) -> u64| {
        if shown.is_empty() {
            0
        } else {
            shown.iter().map(|p| pick(p)).sum::<u64>() / shown.len() as u64
        }
    };
    let now = |pick: fn(&TrafficPoint) -> u64| shown.last().map_or(0, |p| pick(p));
    // As much as fits: now, mean and peak of each direction; then now only.
    let part = |label: &str, style: Style, pick: fn(&TrafficPoint) -> u64, full: bool| {
        let mut spans = vec![
            Span::styled(format!(" {label} "), style),
            Span::styled(format::rate_unit(now(pick)), style.patch(theme::bold())),
        ];
        if full {
            spans.push(Span::styled(
                format!(
                    " avg {} peak {}",
                    format::rate(mean(pick)),
                    format::rate(peak(pick))
                ),
                theme::dim(),
            ));
        }
        spans
    };
    let variant = |full: bool, named: bool| {
        let mut title = Vec::new();
        if named {
            title.push(Span::styled(format!(" {name} "), theme::title()));
        }
        title.extend(part("in", theme::rx(), |p| p.1, full));
        title.extend(part("  out", theme::tx(), |p| p.2, full));
        title.push(Span::styled(
            format!("  {} (z) ", range.label()),
            theme::dim(),
        ));
        title
    };
    let title = fitting(
        area.width,
        vec![
            variant(true, true),
            variant(true, false),
            variant(false, true),
            variant(false, false),
        ],
    );
    let block = Block::bordered().title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if shown.is_empty() {
        frame.render_widget(
            Paragraph::new(" No traffic recorded yet.").style(theme::dim()),
            inner,
        );
        return;
    }
    // Each direction on its own round scale.
    let rx_top = nice(peak(|p| p.1).max(MIN_TOP_BPS) as f64);
    let tx_top = nice(peak(|p| p.2).max(MIN_TOP_BPS) as f64);
    let rx_label = format::rate(rx_top as u64);
    let tx_label = format::rate(tx_top as u64);
    let label_width = rx_label.len().max(tx_label.len()).max(1) as u16;
    // Two columns of dots per cell (braille).
    let plot = usize::from(inner.width.saturating_sub(label_width + 1)).max(1) * 2;
    let parts = columns(
        &shown,
        now_ms,
        range.ms(),
        plot,
        TRAFFIC_STEP_MS,
        |p| p.0,
        |p| Some((p.1 as f64, p.2 as f64)),
    );
    let mut rx = Vec::new();
    let mut tx = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if let Some((r, t)) = part {
            rx.push((i as f64, r / rx_top));
            tx.push((i as f64, -t / tx_top));
        }
    }
    let width = plot as f64;
    let zero = [(0.0, 0.0), (width, 0.0)];
    let chart = Chart::new(vec![
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(theme::dim())
            .data(&zero),
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Bar)
            .style(theme::rx())
            .data(&rx),
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Bar)
            .style(theme::tx())
            .data(&tx),
    ])
    .x_axis(
        Axis::default()
            .bounds([0.0, width])
            .labels(range.ticks().map(|t| Span::styled(t, theme::dim()))),
    )
    .y_axis(Axis::default().bounds([-1.0, 1.0]).labels(vec![
        Span::styled(tx_label, theme::tx()),
        Span::styled("0", theme::dim()),
        Span::styled(rx_label, theme::rx()),
    ]));
    frame.render_widget(chart, inner);
}

/// The round trips of `points` in the last `range` before `now_ms`; lost
/// probes as red dots on the base line.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn rtt(frame: &mut Frame, area: Rect, points: &[RttPoint], now_ms: u64, range: Range) {
    let start = now_ms.saturating_sub(range.ms());
    let shown: Vec<&RttPoint> = points
        .iter()
        .filter(|p| p.0 >= start && p.0 <= now_ms)
        .collect();
    let answered: Vec<f64> = shown.iter().filter_map(|p| p.1.map(f64::from)).collect();
    let lost = shown.len() - answered.len();
    let mean = if answered.is_empty() {
        0.0
    } else {
        answered.iter().sum::<f64>() / answered.len() as f64
    };
    let jitter = if answered.len() > 1 {
        answered
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .sum::<f64>()
            / (answered.len() - 1) as f64
    } else {
        0.0
    };
    let mut title = vec![Span::styled(" Round trip ", theme::title())];
    match shown.last().and_then(|p| p.1) {
        Some(ms) => title.push(Span::styled(format::rtt(f64::from(ms)), theme::bold())),
        None if shown.is_empty() => {}
        None => title.push(Span::styled("lost", theme::bad())),
    }
    if !answered.is_empty() {
        title.push(Span::styled(
            format!(" avg {} jitter {}", format::rtt(mean), format::rtt(jitter)),
            theme::dim(),
        ));
    }
    if lost > 0 {
        title.push(Span::styled(
            format!(
                "  lost {lost} ({:.0} %)",
                100.0 * lost as f64 / shown.len() as f64
            ),
            theme::bad(),
        ));
    }
    title.push(Span::styled(
        format!("  {} (z) ", range.label()),
        theme::dim(),
    ));
    let short = title.iter().take(2).cloned().collect();
    let block = Block::bordered().title(fitting(area.width, vec![title, short]));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if shown.is_empty() {
        frame.render_widget(
            Paragraph::new(" No probes recorded yet.").style(theme::dim()),
            inner,
        );
        return;
    }
    let top = nice(answered.iter().copied().fold(1.0, f64::max));
    let top_label = format::rtt(top);
    let plot = usize::from(inner.width.saturating_sub(top_label.len() as u16 + 1)).max(1) * 2;
    let parts = columns(
        &shown,
        now_ms,
        range.ms(),
        plot,
        RTT_STEP_MS,
        |p| p.0,
        |p| {
            Some((
                p.1.map_or(0.0, f64::from),
                if p.1.is_some() { 1.0 } else { 0.0 },
            ))
        },
    );
    // Answered parts as lines (a gap where none was answered), lost
    // probes on the base line.
    let mut runs: Vec<Vec<(f64, f64)>> = vec![Vec::new()];
    let mut lost_at = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        match part {
            // The mean over the answered ones of this part.
            Some((sum, share)) if *share > 0.0 => {
                if let Some(run) = runs.last_mut() {
                    run.push((i as f64, sum / share / top));
                }
            }
            Some(_) => {
                lost_at.push((i as f64, 0.0));
                runs.push(Vec::new());
            }
            None => {
                if runs.last().is_some_and(|r| !r.is_empty()) {
                    runs.push(Vec::new());
                }
            }
        }
    }
    let mut datasets: Vec<Dataset> = runs
        .iter()
        .filter(|r| !r.is_empty())
        .map(|r| {
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(theme::title())
                .data(r)
        })
        .collect();
    datasets.push(
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Scatter)
            .style(theme::bad())
            .data(&lost_at),
    );
    let chart = Chart::new(datasets)
        .x_axis(Axis::default().bounds([0.0, plot as f64]))
        .y_axis(Axis::default().bounds([0.0, 1.0]).labels(vec![
            Span::styled("0", theme::dim()),
            Span::styled(top_label, theme::dim()),
        ]));
    frame.render_widget(chart, inner);
}

/// The first title that fits into a block `width` wide (the last one
/// otherwise).
fn fitting(width: u16, variants: Vec<Vec<Span<'static>>>) -> Line<'static> {
    let room = usize::from(width.saturating_sub(2));
    let mut last = Line::default();
    for spans in variants {
        let line = Line::from(spans);
        if line.width() <= room {
            return line;
        }
        last = line;
    }
    last
}

/// A sparkline of `values` in text (`▁▂▃▄▅▆▇█`), scaled to `max`; a
/// missing value is a space.
pub fn spark(values: &[Option<u64>], max: u64) -> String {
    const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    values
        .iter()
        .map(|v| match v {
            None => ' ',
            Some(v) => {
                #[allow(clippy::cast_possible_truncation)]
                let level = (u128::from(*v) * 7 / u128::from(max.max(1))) as usize;
                LEVELS[level.min(7)]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_scales() {
        assert!((nice(0.0) - 1.0).abs() < 1e-9);
        assert!((nice(1_300.0) - 2_000.0).abs() < 1e-9);
        assert!((nice(2_000.0) - 2_000.0).abs() < 1e-9);
        assert!((nice(41_000_000.0) - 50_000_000.0).abs() < 1e-3);
        assert!((nice(7.0) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn means_per_part_and_gaps() {
        let points = [
            TrafficPoint(500, 10, 0),
            TrafficPoint(1_500, 30, 0),
            TrafficPoint(9_000, 5, 5),
            TrafficPoint(50_000, 99, 99),
        ];
        let parts = buckets(
            &points,
            10_000,
            10_000,
            5,
            |p| p.0,
            |p| Some((p.1 as f64, p.2 as f64)),
        );
        assert_eq!(parts[0], Some((20.0, 0.0)), "the mean of the first two");
        assert_eq!(parts[1], None, "nothing recorded: a gap");
        assert_eq!(parts[4], Some((5.0, 5.0)));
        assert_eq!(parts.len(), 5, "after now: left out");
    }

    #[test]
    fn parts_never_narrower_than_the_samples() {
        // One sample every 2 s over 20 s on 40 columns: 10 parts, each on
        // 4 columns, no gaps.
        let points: Vec<TrafficPoint> = (0..10)
            .map(|i| TrafficPoint(1_000 + i * 2_000, 5, 5))
            .collect();
        let cols = columns(
            &points,
            20_000,
            20_000,
            40,
            2_000,
            |p| p.0,
            |p| Some((p.1 as f64, p.2 as f64)),
        );
        assert_eq!(cols.len(), 40);
        assert!(cols.iter().all(Option::is_some), "{cols:?}");
    }

    #[test]
    fn text_sparklines() {
        assert_eq!(spark(&[Some(0), Some(50), Some(100), None], 100), "▁▄█ ");
    }
}
