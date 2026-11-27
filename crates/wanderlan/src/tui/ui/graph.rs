//! The graphs: traffic as a mirrored area (in up, out down, each with a
//! scale of its own), round trips with the lost probes, and small text
//! sparklines for tables. The time runs from the left (the start of the
//! range) to now on the right.
//!
//! A graph stands still between two samples: every sample has columns of
//! its own (see [`grid`]), whenever the graph is drawn; it moves by whole
//! samples when a new one comes.

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
/// How often a peer is probed (about: after each probe the next one).
const RTT_STEP_MS: u64 = 5_000;
/// The widest label of a traffic scale (`500.0k`): the plot keeps its width
/// whatever the scale.
const RATE_LABEL_WIDTH: u16 = 6;

/// The mean of the values of `points` in each of `columns` columns, the
/// newest sample on the right; `None` where nothing was recorded.
///
/// The columns depend on the samples alone, not on the time of drawing:
/// sample `s` (its time rounded to `interval_ms` around the time the
/// samples are taken, so that their jitter does not move them) has the columns `s·P/R` up to
/// `(s+1)·P/R` of an endless strip (`P` columns for the `R` samples of
/// `range_ms`; several samples share a column where they do not fit). The
/// strip ends at the newest sample, or, when the samples stop, where they
/// should be (whole samples before `now_ms`).
pub fn grid<T>(
    points: &[T],
    interval_ms: u64,
    range_ms: u64,
    columns: usize,
    now_ms: u64,
    at: impl Fn(&T) -> u64,
    values: impl Fn(&T) -> Option<(f64, f64)>,
) -> Vec<Option<(f64, f64)>> {
    let count = columns.max(1);
    let interval = interval_ms.max(1);
    let cols = u128::try_from(count).unwrap_or(u128::MAX);
    let slots = u128::from((range_ms / interval).max(1));
    let Some(newest_at) = points.iter().map(&at).max() else {
        return vec![None; count];
    };
    // Rounded around the time the samples are taken (the newest tells):
    // their jitter never carries one into the slot of another.
    let phase = newest_at % interval;
    let number = |t: u64| {
        u128::from(
            t.saturating_add(interval + interval / 2)
                .saturating_sub(phase)
                / interval,
        )
    };
    let start = |s: u128| s * cols / slots;
    let end = |s: u128| ((s + 1) * cols / slots).max(start(s) + 1);
    let newest = number(newest_at);
    // Two samples late: the strip moves on without them.
    let edge = newest.max(number(now_ms).saturating_sub(2));
    let last = end(edge) - 1;
    let first = (last + 1).saturating_sub(cols);
    let mut sums = vec![(0.0, 0.0, 0u32); count];
    for p in points {
        let s = number(at(p));
        if s > edge {
            continue;
        }
        let Some((a, b)) = values(p) else { continue };
        for c in start(s).max(first)..end(s).min(last + 1) {
            let Some(slot) = usize::try_from(c - first)
                .ok()
                .and_then(|i| sums.get_mut(i))
            else {
                continue;
            };
            slot.0 += a;
            slot.1 += b;
            slot.2 += 1;
        }
    }
    sums.into_iter()
        .map(|(a, b, n)| (n > 0).then(|| (a / f64::from(n), b / f64::from(n))))
        .collect()
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

/// The traffic of `points` (sampled every `interval_ms`) in the last
/// `range` before `now_ms`, mirrored: in (received) above the line, out
/// (sent) below it.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_arguments
)]
pub fn traffic(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    points: &[TrafficPoint],
    interval_ms: u64,
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
    // Which form is chosen by the widest numbers: it does not change with
    // them.
    let part =
        |label: &str, style: Style, pick: fn(&TrafficPoint) -> u64, full: bool, widest: bool| {
            let (now, mean, peak) = if widest {
                (
                    "999.9 Mbit/s".to_string(),
                    "999.9M".to_string(),
                    "999.9M".to_string(),
                )
            } else {
                (
                    format::rate_unit(now(pick)),
                    format::rate(mean(pick)),
                    format::rate(peak(pick)),
                )
            };
            let mut spans = vec![
                Span::styled(format!(" {label} "), style),
                Span::styled(now, style.patch(theme::bold())),
            ];
            if full {
                spans.push(Span::styled(
                    format!(" avg {mean} peak {peak}"),
                    theme::dim(),
                ));
            }
            spans
        };
    let variant = |full: bool, named: bool, widest: bool| {
        let mut title = Vec::new();
        if named {
            title.push(Span::styled(format!(" {name} "), theme::title()));
        }
        title.extend(part("in", theme::rx(), |p| p.1, full, widest));
        title.extend(part("  out", theme::tx(), |p| p.2, full, widest));
        title.push(Span::styled(
            format!("  {} (z) ", range.label()),
            theme::dim(),
        ));
        title
    };
    let forms = [(true, true), (true, false), (false, true), (false, false)];
    let chosen = fitting_index(
        area.width,
        &forms
            .iter()
            .map(|(full, named)| variant(*full, *named, true))
            .collect::<Vec<_>>(),
    );
    let (full, named) = forms[chosen];
    let title = Line::from(variant(full, named, false));
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
    // Two columns of dots per cell (braille); the labels as wide as the
    // widest, so that the plot keeps its width.
    let plot = usize::from(inner.width.saturating_sub(RATE_LABEL_WIDTH + 1)).max(1) * 2;
    // All points: the strip has its own start (filtered by the time, its
    // oldest column would lose its sample every other second).
    let parts = grid(
        points,
        interval_ms,
        range.ms(),
        plot,
        now_ms,
        |p| p.0,
        |p| Some((p.1 as f64, p.2 as f64)),
    );
    // Each direction on its own round scale, over what is drawn.
    let top = |pick: fn(&(f64, f64)) -> f64| {
        let peak = parts.iter().flatten().map(pick).fold(0.0, f64::max);
        nice(peak.max(MIN_TOP_BPS as f64))
    };
    let rx_top = top(|p| p.0);
    let tx_top = top(|p| p.1);
    let label = |top: f64| {
        format!(
            "{:>w$}",
            format::rate(top as u64),
            w = usize::from(RATE_LABEL_WIDTH)
        )
    };
    let rx_label = label(rx_top);
    let tx_label = label(tx_top);
    let mut rx = Vec::new();
    let mut tx = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if let Some((r, t)) = part {
            rx.push((i as f64, r / rx_top));
            tx.push((i as f64, -t / tx_top));
        }
    }
    // One point per column of dots (the last one at the right edge).
    let width = plot.saturating_sub(1).max(1) as f64;
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
    let rtt_columns = |plot: usize| {
        grid(
            points,
            RTT_STEP_MS,
            range.ms(),
            plot,
            now_ms,
            |p| p.0,
            |p| {
                Some((
                    p.1.map_or(0.0, f64::from),
                    if p.1.is_some() { 1.0 } else { 0.0 },
                ))
            },
        )
    };
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
    let short: Vec<Span<'static>> = title.iter().take(2).cloned().collect();
    let chosen = fitting_index(area.width, &[title.clone(), short.clone()]);
    let block = Block::bordered().title(Line::from(if chosen == 0 { title } else { short }));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if shown.is_empty() {
        frame.render_widget(
            Paragraph::new(" No probes recorded yet.").style(theme::dim()),
            inner,
        );
        return;
    }
    let first_top = nice(answered.iter().copied().fold(1.0, f64::max));
    let plot = usize::from(
        inner
            .width
            .saturating_sub(format::rtt(first_top).len() as u16 + 1),
    )
    .max(1)
        * 2;
    let parts = rtt_columns(plot);
    // The scale over what is drawn (the strip may hold a probe a little
    // older than the range).
    let drawn = parts
        .iter()
        .flatten()
        .filter(|(_, share)| *share > 0.0)
        .map(|(sum, share)| sum / share)
        .fold(first_top, f64::max);
    let top = nice(drawn);
    let top_label = format::rtt(top);
    // A probe that came a little late leaves its slot empty: the line goes
    // on over a gap of up to two probes.
    let per_probe = (plot as u64 * RTT_STEP_MS)
        .div_ceil(range.ms().max(1))
        .max(1);
    let bridge = usize::try_from(2 * per_probe).unwrap_or(usize::MAX);
    // Answered parts as lines (a gap where none was answered for longer),
    // lost probes on the base line.
    let mut runs: Vec<Vec<(f64, f64)>> = vec![Vec::new()];
    let mut lost_at = Vec::new();
    let mut empty = 0;
    for (i, part) in parts.iter().enumerate() {
        match part {
            // The mean over the answered ones of this part.
            Some((sum, share)) if *share > 0.0 => {
                if empty > bridge && runs.last().is_some_and(|r| !r.is_empty()) {
                    runs.push(Vec::new());
                }
                empty = 0;
                if let Some(run) = runs.last_mut() {
                    run.push((i as f64, sum / share / top));
                }
            }
            Some(_) => {
                empty = 0;
                lost_at.push((i as f64, 0.0));
                runs.push(Vec::new());
            }
            None => empty += 1,
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
        .x_axis(Axis::default().bounds([0.0, plot.saturating_sub(1).max(1) as f64]))
        .y_axis(Axis::default().bounds([0.0, 1.0]).labels(vec![
            Span::styled("0", theme::dim()),
            Span::styled(top_label, theme::dim()),
        ]));
    frame.render_widget(chart, inner);
}

/// The first of `variants` that fits as the title of a block `width` wide
/// (the last one otherwise).
fn fitting_index(width: u16, variants: &[Vec<Span<'static>>]) -> usize {
    let room = usize::from(width.saturating_sub(2));
    variants
        .iter()
        .position(|spans| Line::from(spans.clone()).width() <= room)
        .unwrap_or(variants.len().saturating_sub(1))
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

    fn values(p: &TrafficPoint) -> Option<(f64, f64)> {
        #[allow(clippy::cast_precision_loss)]
        Some((p.1 as f64, p.2 as f64))
    }

    #[test]
    fn means_per_column_and_gaps() {
        // A sample every 2 s, one missing; 5 samples on 5 columns.
        let points = [
            TrafficPoint(2_000, 10, 0),
            TrafficPoint(4_000, 30, 0),
            TrafficPoint(8_000, 5, 5),
            TrafficPoint(10_000, 7, 7),
        ];
        let cols = grid(&points, 2_000, 10_000, 5, 10_500, |p| p.0, values);
        assert_eq!(
            cols,
            [
                Some((10.0, 0.0)),
                Some((30.0, 0.0)),
                None,
                Some((5.0, 5.0)),
                Some((7.0, 7.0))
            ]
        );
        // Three columns per sample on 15 columns; the newest on the right.
        let cols = grid(&points, 2_000, 10_000, 15, 10_500, |p| p.0, values);
        assert_eq!(&cols[12..], [Some((7.0, 7.0)); 3]);
        assert_eq!(&cols[6..9], [None; 3]);
        // Nothing recorded for a while: the strip moves on without them.
        let later = grid(&points, 2_000, 10_000, 5, 20_000, |p| p.0, values);
        assert_eq!(later[4], None);
        assert!(
            grid(&[], 2_000, 10_000, 5, 0, |p: &TrafficPoint| p.0, values)
                .iter()
                .all(Option::is_none)
        );
    }

    /// The picture stands still between two samples, whatever the phase and
    /// the jitter of their times, and moves on by the new one only.
    #[test]
    fn the_picture_stands_still_between_samples() {
        let jitter = |i: u64| (i * 7_919) % 41;
        for phase in [15u64, 990, 1_975] {
            let at = |i: u64| 1_000_000 + i * 2_000 + phase + jitter(i);
            let points: Vec<TrafficPoint> = (0..400)
                .map(|i| TrafficPoint(at(i), (i % 7) * 100, i % 3))
                .collect();
            let newest = at(399);
            let mut more = points.clone();
            more.push(TrafficPoint(at(400), 900, 0));
            for cols in [146usize, 226, 300, 452] {
                let draw = |points: &[TrafficPoint], now: u64| {
                    grid(points, 2_000, 600_000, cols, now, |p| p.0, values)
                };
                let a = draw(&points, newest + 10);
                assert!(
                    a.iter().all(Option::is_some),
                    "phase {phase}, {cols}: no gaps"
                );
                for later in [1_000, 1_900] {
                    assert_eq!(a, draw(&points, newest + later), "phase {phase}, {cols}");
                }
                // The new sample: the rest moves left by whole columns (the
                // newest column may take it in where samples share one).
                let d = draw(&more, at(400) + 10);
                let n = cols - 1;
                assert!(
                    (0..=2).any(|k| a[k..n] == d[..n - k]),
                    "phase {phase}, {cols}: moved by whole columns"
                );
            }
        }
    }

    #[test]
    fn text_sparklines() {
        assert_eq!(spark(&[Some(0), Some(50), Some(100), None], 100), "▁▄█ ");
    }
}
