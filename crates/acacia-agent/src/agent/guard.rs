//! Guardrails: rate limits, protected regions, and treating chat as data.
//!
//! Caps live in [`super::Limits`] and are enforced by the loop; this module owns the per-call checks
//! the loop and the shell share.

use std::time::{Duration, Instant};

use crate::config::{ChatConfig, SafetyConfig};
use crate::tools::{Call, Failure};

/// Minimum gap between two world writes, on top of the bot's own `Governor`.
const WRITE_INTERVAL: Duration = Duration::from_millis(250);

pub struct Guard {
    pub chat: ChatConfig,
    pub safety: SafetyConfig,
    last_write: Option<Instant>,
    last_chat: Option<(String, Instant)>,
    /// Calls made, for the run caps.
    pub calls: u32,
    pub writes: u32,
}

impl Guard {
    pub fn new(chat: ChatConfig, safety: SafetyConfig) -> Self {
        Self { chat, safety, last_write: None, last_chat: None, calls: 0, writes: 0 }
    }

    /// Checks a resolved call against the guards; counts it when it passes.
    pub fn check_call(&mut self, call: &Call) -> Result<(), Failure> {
        self.calls += 1;
        match call {
            Call::Chat { text, .. } => self.check_chat(text),
            Call::Dig { at, .. } => {
                self.check_region(*at)?;
                self.check_write_gap()
            }
            Call::Place { at, face, .. } => {
                // `auto:true` has no target yet: the cell is picked at execution time, after which the
                // region rule has already been answered by `place_block` itself.
                if let Some(at) = at {
                    self.check_region(face.adjacent(*at))?;
                }
                self.check_write_gap()
            }
            other if other.is_write() => self.check_write_gap(),
            _ => Ok(()),
        }
    }

    fn check_write_gap(&mut self) -> Result<(), Failure> {
        let now = Instant::now();
        if let Some(last) = self.last_write {
            if now.duration_since(last) < WRITE_INTERVAL {
                return Err(Failure::new("busy", "too fast: wait a moment before the next world write"));
            }
        }
        self.last_write = Some(now);
        self.writes += 1;
        Ok(())
    }

    fn check_chat(&mut self, text: &str) -> Result<(), Failure> {
        if !self.chat.enabled {
            return Err(Failure::new("rejected", "chat is disabled in this configuration"));
        }
        let now = Instant::now();
        if let Some((last_text, at)) = &self.last_chat {
            if last_text == text && now.duration_since(*at) < Duration::from_secs(60) {
                return Err(Failure::new("rejected", "that exact message was just sent; say something new"));
            }
            if now.duration_since(*at) < Duration::from_millis(self.chat.min_interval_ms) {
                return Err(Failure::new("busy", "chat is rate limited; wait before sending again"));
            }
        }
        self.last_chat = Some((text.to_owned(), now));
        Ok(())
    }

    /// The box list from `safety.protected_regions` (`[x0,y0,z0,x1,y1,z1]`, inclusive, any corner order).
    pub fn check_region(&self, pos: [i32; 3]) -> Result<(), Failure> {
        if self.safety.allow_protected {
            return Ok(());
        }
        let inside = self.safety.protected_regions.iter().any(|region| {
            let [ax, ay, az, bx, by, bz] = *region;
            (ax.min(bx)..=ax.max(bx)).contains(&pos[0])
                && (ay.min(by)..=ay.max(by)).contains(&pos[1])
                && (az.min(bz)..=az.max(bz)).contains(&pos[2])
        });
        if inside {
            return Err(Failure::new("rejected", format!("{pos:?} is inside a protected region (safety.protected_regions)")));
        }
        Ok(())
    }

    /// Blocks the bot must not dig without confirmation (`safety.keep_blocks`).
    pub fn check_keep_block(&self, name: &str) -> Result<(), Failure> {
        if self.safety.allow_protected {
            return Ok(());
        }
        if self.safety.keep_blocks.iter().any(|keep| keep == name) {
            return Err(Failure::new("rejected", format!("{name} is in safety.keep_blocks; ask before removing it")));
        }
        Ok(())
    }
}

/// Makes a chat line safe to send: plain text only, bounded length, prefixed once.
///
/// Chat is data on the way in; on the way out it must not become a command or a secret leak, hence
/// the leading-slash strip and the hard length clamp.
pub fn sanitize_chat(config: &ChatConfig, text: &str) -> Result<String, Failure> {
    if !config.enabled {
        return Err(Failure::new("rejected", "chat is disabled in this configuration"));
    }
    let mut text = text.trim().to_owned();
    while text.starts_with('/') {
        text.remove(0);
        text = text.trim_start().to_owned();
    }
    if text.is_empty() {
        return Err(Failure::invalid_argument("chat text is empty"));
    }
    let max = config.max_chars.max(1);
    if text.chars().count() > max {
        text = text.chars().take(max).collect();
    }
    if !config.prefix.is_empty() && !text.starts_with(&config.prefix) {
        // Leave room for the prefix inside the same budget.
        let room = max.saturating_sub(config.prefix.chars().count());
        text = text.chars().take(room).collect();
        text = format!("{}{text}", config.prefix);
    }
    Ok(text)
}
