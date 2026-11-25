//! Bounded, opt-in records retained until the present qualifies the frame.
use std::sync::{Mutex, OnceLock};

struct Log {
    from: u64,
    remaining: u64,
    previous_drawn: u64,
    lines: Vec<String>,
    overflow: bool,
    emitted: usize,
    limited: bool,
}

fn log() -> &'static Mutex<Option<Log>> {
    static LOG: OnceLock<Mutex<Option<Log>>> = OnceLock::new();
    LOG.get_or_init(|| {
        let config = std::env::var("VICTORY_FRAME_LOG").ok().and_then(|s| s.parse::<u64>().ok())
            .filter(|n| *n > 0)
            .zip(std::env::var("VICTORY_FRAME_LOG_FROM").ok().and_then(|s| s.parse::<u64>().ok()));
        Mutex::new(config.map(|(remaining, from)| Log {
            from, remaining, previous_drawn: 0, lines: Vec::new(), overflow: false,
            emitted: 0, limited: false,
        }))
    })
}

pub(crate) fn record(make: impl FnOnce() -> String) {
    let mut guard = log().lock().unwrap_or_else(|e| e.into_inner());
    let Some(l) = guard.as_mut() else { return };
    if l.remaining == 0 || l.limited || crate::frame::current() < l.from { return; }
    if l.lines.len() < 8000 - l.emitted { l.lines.push(make()); } else { l.overflow = true; }
}

pub(crate) fn present(frame: u64, drawn: u64) {
    let mut guard = log().lock().unwrap_or_else(|e| e.into_inner());
    let Some(l) = guard.as_mut() else { return };
    let draws = drawn.saturating_sub(l.previous_drawn);
    l.previous_drawn = drawn;
    if frame >= l.from && l.remaining > 0 && draws > 100 && !l.limited {
        l.remaining -= 1;
        for line in l.lines.drain(..) { tracing::info!("frame={frame} {line}"); l.emitted += 1; }
        if l.overflow {
            tracing::info!("VICTORY_FRAME_LOG: 8000 line limit reached");
            l.limited = true;
        }
    }
    l.lines.clear();
    l.overflow = false;
}
