//! The last minute of the input channel with a client, as the daemon reports
//! it once a second ([`LinkStats`]): the texts and charts of its row.

use std::collections::VecDeque;

use gtk::{cairo, glib};

use crate::i18n::{tr, trf, trn};
use rustinput_ipc::{LinkStats, human_size};

/// seconds of history shown
pub(crate) const HISTORY: usize = 60;

/// the accent of the RustInput icon, for the latency
const LATENCY: (f64, f64, f64) = (232.0 / 255.0, 105.0 / 255.0, 46.0 / 255.0);
/// blue and green of the GNOME palette, readable on light and dark rows
const SENT: (f64, f64, f64) = (53.0 / 255.0, 132.0 / 255.0, 228.0 / 255.0);
const RECEIVED: (f64, f64, f64) = (38.0 / 255.0, 162.0 / 255.0, 105.0 / 255.0);
pub(crate) const SENT_HEX: &str = "#3584e4";
pub(crate) const RECEIVED_HEX: &str = "#26a269";

/// one second of the history
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Point {
    rtt_us: Option<u32>,
    lost: u32,
    sent: u64,
    received: u64,
}

/// The reports of the last minute with one client.
#[derive(Debug, Default)]
pub(crate) struct History {
    points: VecDeque<Point>,
    last: Option<LinkStats>,
}

impl History {
    pub(crate) fn push(&mut self, stats: &LinkStats) {
        if self.points.len() == HISTORY {
            self.points.pop_front();
        }
        self.points.push_back(Point {
            rtt_us: stats.rtt_us,
            lost: stats.lost,
            sent: stats.sent_per_s,
            received: stats.received_per_s,
        });
        self.last = Some(stats.clone());
    }

    /// the client is not connected any more
    pub(crate) fn clear(&mut self) {
        self.points.clear();
        self.last = None;
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.last.is_none()
    }

    /// the latest round trip time, for the collapsed row
    pub(crate) fn latest_rtt(&self) -> Option<u32> {
        self.last.as_ref().and_then(|s| s.rtt_us)
    }

    fn rtts(&self) -> impl Iterator<Item = u32> + '_ {
        self.points.iter().filter_map(|p| p.rtt_us)
    }

    /// "Latency 1.2 ms · average 1.4 ms · highest 3.1 ms"
    pub(crate) fn latency_text(&self) -> String {
        let Some(latest) = self.latest_rtt() else {
            if self.rtts().next().is_none() {
                return tr("Latency: measured by the computer that connects, \
                           once it has this one as a client")
                .to_owned();
            }
            return tr("Latency: no answer to the last pings").to_owned();
        };
        let count = self.rtts().count() as u64;
        let average = (self.rtts().map(u64::from).sum::<u64>() / count.max(1)) as u32;
        let highest = self.rtts().max().unwrap_or(latest);
        let mut text = trf(
            "Latency {latest} · average {average} · highest {highest}",
            &[
                ("latest", &format_rtt(latest)),
                ("average", &format_rtt(average)),
                ("highest", &format_rtt(highest)),
            ],
        );
        let lost: u32 = self.points.iter().map(|p| p.lost).sum();
        if lost > 0 {
            text.push_str(" · ");
            text.push_str(&trn(
                "{count} ping lost",
                "{count} pings lost",
                lost.into(),
                &[],
            ));
        }
        text
    }

    /// "Traffic ↑ 0.4 KB/s ↓ 0.1 KB/s · since connected ↑ 1.2 MB ↓ 300 KB",
    /// as Pango markup colored like the chart
    pub(crate) fn traffic_markup(&self) -> String {
        let Some(last) = &self.last else {
            return String::new();
        };
        let sent = format!(
            "<span foreground=\"{SENT_HEX}\">↑ {}</span>",
            format_rate(last.sent_per_s)
        );
        let received = format!(
            "<span foreground=\"{RECEIVED_HEX}\">↓ {}</span>",
            format_rate(last.received_per_s)
        );
        let total = format!(
            "↑ {} ↓ {}",
            human_size(last.sent_total),
            human_size(last.received_total)
        );
        // the translation is no markup: only the values are
        let text =
            glib::markup_escape_text(tr("Traffic {sent} {received} · since connected {total}"));
        rustinput_i18n::fill(
            &text,
            &[("sent", &sent), ("received", &received), ("total", &total)],
        )
    }

    /// the latency of the last minute as a line, newest on the right
    pub(crate) fn draw_latency(&self, cr: &cairo::Context, width: i32, height: i32) {
        let top = self.rtts().max().unwrap_or(0) as f64 / 1000.0;
        let scale = nice_scale(top, 1.0);
        frame(cr, width, height, &format!("{} ms", format_number(scale)));
        let values: Vec<Option<f64>> = self
            .points
            .iter()
            .map(|p| p.rtt_us.map(|r| r as f64 / 1000.0))
            .collect();
        line(cr, width, height, &values, scale, LATENCY);
        // lost pings as marks on the bottom
        cr.set_source_rgba(LATENCY.0, LATENCY.1, LATENCY.2, 0.9);
        for (i, point) in self.points.iter().enumerate() {
            if point.lost > 0 {
                let x = x_of(i, self.points.len(), width);
                cr.rectangle(x - 1.0, height as f64 - 4.0, 2.0, 4.0);
                let _ = cr.fill();
            }
        }
    }

    /// bytes per second sent and received in the last minute
    pub(crate) fn draw_traffic(&self, cr: &cairo::Context, width: i32, height: i32) {
        let top = self
            .points
            .iter()
            .map(|p| p.sent.max(p.received))
            .max()
            .unwrap_or(0) as f64
            / 1024.0;
        let scale = nice_scale(top, 1.0);
        frame(cr, width, height, &format!("{} KB/s", format_number(scale)));
        let sent: Vec<Option<f64>> = self
            .points
            .iter()
            .map(|p| Some(p.sent as f64 / 1024.0))
            .collect();
        let received: Vec<Option<f64>> = self
            .points
            .iter()
            .map(|p| Some(p.received as f64 / 1024.0))
            .collect();
        line(cr, width, height, &received, scale, RECEIVED);
        line(cr, width, height, &sent, scale, SENT);
    }
}

/// a round trip time in microseconds as text: "0.4 ms", "12 ms"
pub(crate) fn format_rtt(us: u32) -> String {
    let ms = us as f64 / 1000.0;
    if ms < 0.1 {
        "< 0.1 ms".to_owned()
    } else if ms < 10.0 {
        format!("{ms:.1} ms")
    } else {
        format!("{ms:.0} ms")
    }
}

/// bytes per second as text: "120 B/s", "3.4 KB/s", "12.0 MB/s", in the
/// units of [`human_size`] (1 KB = 1024 bytes)
pub(crate) fn format_rate(bytes_per_s: u64) -> String {
    let b = bytes_per_s as f64;
    if b < 1024.0 {
        format!("{bytes_per_s} B/s")
    } else if b < 1024.0 * 1024.0 {
        format!("{:.1} KB/s", b / 1024.0)
    } else {
        format!("{:.1} MB/s", b / (1024.0 * 1024.0))
    }
}

/// "2", "0.5", "1.5"
fn format_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}

/// the top of a chart showing values up to `top`: a round number above it,
/// at least `least`
fn nice_scale(top: f64, least: f64) -> f64 {
    let top = top.max(least) * 1.1;
    let magnitude = 10f64.powf(top.log10().floor());
    for step in [1.0, 2.0, 5.0, 10.0] {
        if step * magnitude >= top {
            return step * magnitude;
        }
    }
    10.0 * magnitude
}

/// x of point `i` of `n`, the last one on the right edge
fn x_of(i: usize, n: usize, width: i32) -> f64 {
    let step = width as f64 / (HISTORY - 1) as f64;
    width as f64 - (n - 1 - i) as f64 * step
}

/// the grid of a chart, with what its top stands for
fn frame(cr: &cairo::Context, width: i32, height: i32, top: &str) {
    let (w, h) = (width as f64, height as f64);
    cr.set_source_rgba(0.5, 0.5, 0.5, 0.25);
    cr.set_line_width(1.0);
    for y in [0.5, h / 2.0, h - 0.5] {
        cr.move_to(0.0, y.round() + 0.5);
        cr.line_to(w, y.round() + 0.5);
    }
    let _ = cr.stroke();
    cr.set_source_rgba(0.5, 0.5, 0.5, 0.9);
    cr.set_font_size(10.0);
    cr.move_to(2.0, 11.0);
    let _ = cr.show_text(top);
}

/// `values` as a line in `color`, scaled so `scale` is the top; gaps where a
/// value is missing
fn line(
    cr: &cairo::Context,
    width: i32,
    height: i32,
    values: &[Option<f64>],
    scale: f64,
    color: (f64, f64, f64),
) {
    let h = height as f64;
    cr.set_source_rgb(color.0, color.1, color.2);
    cr.set_line_width(1.5);
    cr.set_line_join(cairo::LineJoin::Round);
    let mut drawing = false;
    for (i, value) in values.iter().enumerate() {
        let Some(value) = value else {
            drawing = false;
            continue;
        };
        let x = x_of(i, values.len(), width);
        let y = h - 1.0 - (value / scale).min(1.0) * (h - 2.0);
        if drawing {
            cr.line_to(x, y);
        } else {
            cr.move_to(x, y);
            drawing = true;
        }
    }
    let _ = cr.stroke();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(rtt_us: Option<u32>, lost: u32) -> LinkStats {
        LinkStats {
            handle: Some(0),
            ip: [192, 168, 1, 50].into(),
            rtt_us,
            lost,
            sent_per_s: 450,
            received_per_s: 2_300,
            sent_total: 1_200_000,
            received_total: 300_000,
        }
    }

    #[test]
    fn texts() {
        assert_eq!(format_rtt(40), "< 0.1 ms");
        assert_eq!(format_rtt(1_234), "1.2 ms");
        assert_eq!(format_rtt(23_456), "23 ms");
        assert_eq!(format_rate(450), "450 B/s");
        assert_eq!(format_rate(2_300), "2.2 KB/s");
        assert_eq!(format_rate(12_582_912), "12.0 MB/s");
    }

    #[test]
    fn the_history_keeps_a_minute() {
        let mut history = History::default();
        assert!(history.is_empty());
        history.push(&stats(Some(3_000), 0));
        for _ in 0..HISTORY {
            history.push(&stats(Some(1_000), 0));
        }
        assert_eq!(history.points.len(), HISTORY);
        // the 3 ms from over a minute ago is gone
        assert_eq!(
            history.latency_text(),
            "Latency 1.0 ms · average 1.0 ms · highest 1.0 ms"
        );
        history.push(&stats(None, 2));
        assert_eq!(
            history.latency_text(),
            "Latency: no answer to the last pings"
        );
        history.push(&stats(Some(2_000), 0));
        assert!(
            history
                .latency_text()
                .ends_with("highest 2.0 ms · 2 pings lost")
        );
        assert!(history.traffic_markup().contains("↑ 450 B/s"));
        assert!(history.traffic_markup().contains("↓ 2.2 KB/s"));
        history.clear();
        assert!(history.is_empty());
        assert_eq!(history.latest_rtt(), None);
    }

    #[test]
    fn scales_are_round() {
        assert_eq!(nice_scale(0.3, 1.0), 2.0);
        assert_eq!(nice_scale(1.5, 1.0), 2.0);
        assert_eq!(nice_scale(3.0, 1.0), 5.0);
        assert_eq!(nice_scale(46.0, 1.0), 100.0);
        assert_eq!(nice_scale(95.0, 1.0), 200.0);
    }

    #[test]
    fn charts_are_drawn() {
        let mut history = History::default();
        for i in 0..10 {
            history.push(&stats(if i == 4 { None } else { Some(800 + i * 100) }, 0));
        }
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 300, 44).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        history.draw_latency(&cr, 300, 44);
        history.draw_traffic(&cr, 300, 44);
        drop(cr);
        let data = surface.take_data().unwrap();
        assert!(data.chunks(4).any(|px| px[3] > 0), "something was drawn");
    }
}
