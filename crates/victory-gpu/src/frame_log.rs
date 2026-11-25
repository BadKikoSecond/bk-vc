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

use std::borrow::Cow;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The full command log is independent of the older detailed mode above.
const DEFAULT_CAP: usize = 20_000;

#[derive(Clone, Copy)]
struct Config {
    from: u64,
    frames: u64,
    cap: usize,
}

impl Config {
    fn from_values(
        enabled: Option<&str>,
        from: Option<&str>,
        frames: Option<&str>,
        cap: Option<&str>,
    ) -> Option<Self> {
        if enabled != Some("1") {
            return None;
        }
        let from = from?.parse().ok()?;
        let frames: u64 = frames?.parse().ok()?;
        if frames == 0 {
            return None;
        }
        let cap = cap
            .and_then(|value| value.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_CAP);
        Some(Self { from, frames, cap })
    }

    fn from_env() -> Option<Self> {
        let enabled = std::env::var("VICTORY_GFX_LOG").ok();
        let from = std::env::var("VICTORY_GFX_LOG_FROM").ok();
        let frames = std::env::var("VICTORY_GFX_LOG_FRAMES").ok();
        let cap = std::env::var("VICTORY_GFX_LOG_CAP").ok();
        Self::from_values(
            enabled.as_deref(),
            from.as_deref(),
            frames.as_deref(),
            cap.as_deref(),
        )
    }
}

static CONFIG: OnceLock<Option<Config>> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static FROM: AtomicU64 = AtomicU64::new(0);
static STOPPED: AtomicBool = AtomicBool::new(false);

fn config() -> &'static Option<Config> {
    CONFIG.get_or_init(|| {
        let config = Config::from_env();
        ACTIVE.store(config.is_some(), Ordering::Relaxed);
        FROM.store(config.map_or(0, |c| c.from), Ordering::Relaxed);
        config
    })
}

/// Whether this thread may collect command metadata for a candidate frame.
pub(crate) fn candidate() -> bool {
    config().is_some()
        && ACTIVE.load(Ordering::Relaxed)
        && !STOPPED.load(Ordering::Relaxed)
        && crate::frame::current() >= FROM.load(Ordering::Relaxed)
}

/// Why a command took its actual branch. `executed` means recorded onto the
/// host command list; GPU submission and completion happen later.
pub(crate) struct Issue {
    pub(crate) status: &'static str,
    pub(crate) reason: Cow<'static, str>,
}

impl Issue {
    pub(crate) fn executed() -> Self {
        Self {
            status: "executed",
            reason: "none".into(),
        }
    }

    pub(crate) fn skipped(reason: impl Into<Cow<'static, str>>) -> Self {
        Self {
            status: "skipped",
            reason: reason.into(),
        }
    }

    pub(crate) fn missing(reason: impl Into<Cow<'static, str>>) -> Self {
        Self {
            status: "missing",
            reason: reason.into(),
        }
    }

    pub(crate) fn making(reason: impl Into<Cow<'static, str>>) -> Self {
        Self {
            status: "making",
            reason: reason.into(),
        }
    }

    pub(crate) fn refused(reason: impl Into<Cow<'static, str>>) -> Self {
        Self {
            status: "refused",
            reason: reason.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextureState {
    Ready,
    Unmapped,
    Waiting,
    Null,
    Unknown,
}

impl TextureState {
    fn name(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Unmapped => "unmapped",
            Self::Waiting => "waiting",
            Self::Null => "null",
            Self::Unknown => "unknown",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Null => 4,
            Self::Unmapped => 3,
            Self::Waiting => 2,
            Self::Ready => 1,
            Self::Unknown => 0,
        }
    }
}

thread_local! {
    /// The most useful texture slot seen while binding this command. A
    /// problem outranks a healthy slot; equal problems keep the first.
    static TEXTURE: RefCell<Option<(u32, TextureState)>> = const { RefCell::new(None) };
}

pub(crate) fn begin_texture_scan() {
    if candidate() {
        TEXTURE.with(|texture| *texture.borrow_mut() = None);
    }
}

pub(crate) fn note_texture(slot: u32, state: TextureState) {
    if !candidate() {
        return;
    }
    TEXTURE.with(|texture| {
        let mut texture = texture.borrow_mut();
        if texture
            .as_ref()
            .is_none_or(|&(_, old)| state.rank() > old.rank())
        {
            *texture = Some((slot, state));
        }
    });
}

pub(crate) fn take_texture() -> String {
    match TEXTURE.with(|texture| texture.borrow_mut().take()) {
        Some((slot, state)) => format!("texture={slot} state={}", state.name()),
        None => "texture=unknown".to_owned(),
    }
}

struct GfxLog {
    config: Config,
    remaining: u64,
    previous_drawn: u64,
    /// Commands of the current candidate frame, before it is known to qualify.
    lines: Vec<String>,
    frame: Option<u64>,
    overflow: bool,
    emitted: usize,
    limited: bool,
}

impl GfxLog {
    fn new(config: Config) -> Self {
        Self {
            config,
            remaining: config.frames,
            previous_drawn: 0,
            lines: Vec::new(),
            frame: None,
            overflow: false,
            emitted: 0,
            limited: false,
        }
    }

    fn clear_frame(&mut self) {
        self.lines.clear();
        self.frame = None;
        self.overflow = false;
    }

    fn record(&mut self, frame: u64, make: impl FnOnce() -> String) {
        if self.remaining == 0 || self.limited || frame < self.config.from {
            return;
        }
        if self.frame != Some(frame) {
            self.clear_frame();
            self.frame = Some(frame);
        }
        if self.lines.len() < self.config.cap {
            self.lines.push(make());
        } else {
            self.overflow = true;
        }
    }

    fn present(&mut self, frame: u64, drawn: u64) -> Vec<String> {
        let draws = drawn.saturating_sub(self.previous_drawn);
        self.previous_drawn = drawn;
        if self.frame != Some(frame) {
            self.clear_frame();
        }
        let mut out = Vec::new();
        if frame >= self.config.from && self.remaining > 0 && draws > 100 && !self.limited {
            self.remaining -= 1;
            let room = self.config.cap.saturating_sub(self.emitted);
            let take = room.min(self.lines.len());
            for line in self.lines.drain(..take) {
                out.push(format!("frame={frame} {line}"));
                self.emitted += 1;
            }
            if !self.lines.is_empty() || self.overflow || self.emitted >= self.config.cap {
                self.limited = true;
                out.push(format!(
                    "VICTORY_GFX_LOG: {} line limit reached",
                    self.config.cap
                ));
            }
        }
        self.clear_frame();
        out
    }

    fn finished(&self) -> bool {
        self.remaining == 0 || self.limited
    }
}

fn gfx_log() -> &'static Mutex<Option<GfxLog>> {
    static LOG: OnceLock<Mutex<Option<GfxLog>>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(config().map(GfxLog::new)))
}

pub(crate) fn record_command(make: impl FnOnce() -> String) {
    let mut guard = gfx_log().lock().unwrap_or_else(|e| e.into_inner());
    let Some(log) = guard.as_mut() else { return };
    let frame = crate::frame::current();
    if log.remaining == 0 || log.limited || frame < log.config.from {
        return;
    }
    log.record(frame, make);
}

pub(crate) fn gfx_present(frame: u64, drawn: u64) {
    let (lines, stopped) = {
        let mut guard = gfx_log().lock().unwrap_or_else(|e| e.into_inner());
        let Some(log) = guard.as_mut() else { return };
        (log.present(frame, drawn), log.finished())
    };
    for line in lines {
        tracing::info!("{line}");
    }
    if stopped {
        STOPPED.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(from: u64, frames: u64, cap: usize) -> GfxLog {
        GfxLog::new(Config { from, frames, cap })
    }

    #[test]
    fn full_log_needs_all_three_switches() {
        assert!(Config::from_values(None, Some("10"), Some("1"), None).is_none());
        assert!(Config::from_values(Some("1"), None, Some("1"), None).is_none());
        assert!(Config::from_values(Some("1"), Some("10"), None, None).is_none());
        assert!(Config::from_values(Some("0"), Some("10"), Some("1"), None).is_none());
        assert!(Config::from_values(Some("1"), Some("x"), Some("1"), None).is_none());
        assert!(Config::from_values(Some("1"), Some("10"), Some("0"), None).is_none());
        let config = Config::from_values(Some("1"), Some("10"), Some("2"), None).unwrap();
        assert_eq!(
            (config.from, config.frames, config.cap),
            (10, 2, DEFAULT_CAP)
        );
        let config = Config::from_values(Some("1"), Some("0"), Some("2"), Some("7")).unwrap();
        assert_eq!((config.from, config.frames, config.cap), (0, 2, 7));
    }

    #[test]
    fn selects_the_first_frames_with_more_than_one_hundred_draws() {
        let mut log = log(10, 2, 20);
        log.record(9, || "before".to_owned());
        log.record(10, || "too few".to_owned());
        assert!(log.present(10, 100).is_empty());

        log.record(11, || "first".to_owned());
        log.record(11, || "second".to_owned());
        assert_eq!(log.present(11, 201), ["frame=11 first", "frame=11 second"]);

        log.record(12, || "too few again".to_owned());
        assert!(log.present(12, 301).is_empty());
        log.record(13, || "third frame".to_owned());
        assert_eq!(log.present(13, 402), ["frame=13 third frame"]);
        assert!(log.finished());
        assert!(!log.limited);
    }

    #[test]
    fn cap_emits_one_limit_line_and_stops() {
        let mut log = log(1, 3, 2);
        log.record(1, || "one".to_owned());
        log.record(1, || "two".to_owned());
        log.record(1, || "three".to_owned());
        assert_eq!(
            log.present(1, 101),
            [
                "frame=1 one",
                "frame=1 two",
                "VICTORY_GFX_LOG: 2 line limit reached"
            ]
        );
        assert!(log.limited);
        log.record(2, || "after".to_owned());
        assert!(log.present(2, 202).is_empty());
        assert_eq!(log.emitted, 2);
    }

    #[test]
    fn reaching_the_cap_exactly_still_reports_the_limit() {
        let mut log = log(1, 1, 1);
        log.record(1, || "one".to_owned());
        assert_eq!(
            log.present(1, 101),
            ["frame=1 one", "VICTORY_GFX_LOG: 1 line limit reached"]
        );
    }
}
