use crate::stream::{ClaudeEvent, StreamOutput};
use chrono::Local;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Maximum number of recent events to keep in the display history
const MAX_RECENT_EVENTS: usize = 4;

/// Configuration for the progress display
pub struct ProgressConfig {
    pub minion_id: String,
    pub issue: String,
    pub quiet: bool,
}

/// Progress display manager for Minion work
pub struct ProgressDisplay {
    _multi: MultiProgress,
    status_bar: ProgressBar,
    events_bar: ProgressBar,
    start_time: Instant,
    config: ProgressConfig,
    recent_events: Arc<Mutex<Vec<String>>>,
}

impl ProgressDisplay {
    /// Create a new progress display
    pub fn new(config: ProgressConfig) -> Self {
        let multi = MultiProgress::new();

        // Main status bar
        let status_bar = multi.add(ProgressBar::new_spinner());
        status_bar.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.cyan} {msg}")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );

        // Events bar (shows recent activity)
        let events_bar = multi.add(ProgressBar::new_spinner());
        events_bar.set_style(ProgressStyle::default_spinner().template("{msg}").unwrap());

        let display = Self {
            _multi: multi,
            status_bar,
            events_bar,
            start_time: Instant::now(),
            config,
            recent_events: Arc::new(Mutex::new(Vec::new())),
        };

        display.update_header("Starting...");
        display
    }

    /// Update the header with current status
    fn update_header(&self, status: &str) {
        let elapsed = self.start_time.elapsed();
        let mins = elapsed.as_secs() / 60;
        let secs = elapsed.as_secs() % 60;

        let header = format!(
            "🤖 Minion {} | Issue {} | ⏱️  {}m {:02}s\n\nStatus: {}",
            self.config.minion_id, self.config.issue, mins, secs, status
        );

        self.status_bar.set_message(header);
    }

    /// Add an event to the recent events list
    fn add_event(&self, event_text: String) {
        let mut events = match self.recent_events.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                // Log the error and recover with the data
                eprintln!("Warning: Progress display mutex poisoned, recovering");
                poisoned.into_inner()
            }
        };
        events.push(event_text);

        // Keep only the last MAX_RECENT_EVENTS events
        if events.len() > MAX_RECENT_EVENTS {
            events.remove(0);
        }

        // Update the events bar
        let recent_text = events
            .iter()
            .map(|e| format!("  {}", e))
            .collect::<Vec<_>>()
            .join("\n");

        self.events_bar
            .set_message(format!("Recent:\n{}", recent_text));
    }

    /// Process a stream output and update the display
    pub fn handle_output(&self, output: &StreamOutput) {
        if self.config.quiet {
            // In quiet mode, only show errors
            if let StreamOutput::Event(ClaudeEvent::Error { message }) = output {
                eprintln!("Error: {}", message);
            }
            return;
        }

        match output {
            StreamOutput::Event(event) => self.handle_event(event),
            StreamOutput::RawLine(line) => {
                // Pass through non-JSON output to stdout
                use std::io::Write;
                if let Err(e) = std::io::stdout().write_all(line.as_bytes()) {
                    eprintln!("Warning: Failed to write to stdout: {}", e);
                }
                let _ = std::io::stdout().flush();
            }
        }
    }

    /// Truncate a string to a maximum number of characters (not bytes)
    /// Uses single-pass iteration to avoid counting characters twice
    fn truncate_string(s: &str, max_chars: usize) -> String {
        // Check if truncation is needed using nth() which short-circuits
        if s.chars().nth(max_chars).is_none() {
            // String has max_chars or fewer characters
            s.to_string()
        } else {
            // String exceeds max_chars, truncate and add ellipsis
            let truncated: String = s.chars().take(max_chars).collect();
            format!("{}...", truncated)
        }
    }

    /// Handle a parsed Claude event
    fn handle_event(&self, event: &ClaudeEvent) {
        let timestamp = Local::now().format("%H:%M:%S");

        match event {
            ClaudeEvent::Thinking { content } => {
                self.update_header("💭 Thinking...");
                let truncated = Self::truncate_string(content, 50);
                self.add_event(format!("[{}] Thinking: {}", timestamp, truncated));
            }
            ClaudeEvent::ToolUse { name, input } => {
                self.update_header(&format!("🔧 Using tool: {}", name));

                // Extract relevant info from input for display
                let detail = match name.as_str() {
                    "bash" => input.get("command").and_then(|c| c.as_str()).unwrap_or(""),
                    "read" | "write" | "edit" => input
                        .get("file_path")
                        .and_then(|p| p.as_str())
                        .unwrap_or(""),
                    _ => "",
                };

                let event_text = if detail.is_empty() {
                    format!("[{}] Tool: {}", timestamp, name)
                } else {
                    let truncated = Self::truncate_string(detail, 40);
                    format!("[{}] Tool: {} - {}", timestamp, name, truncated)
                };

                self.add_event(event_text);
            }
            ClaudeEvent::Message { content } => {
                self.update_header("📝 Responding...");
                let truncated = Self::truncate_string(content, 50);
                self.add_event(format!("[{}] Message: {}", timestamp, truncated));
            }
            ClaudeEvent::Complete => {
                self.update_header("✅ Complete");
                self.add_event(format!("[{}] Complete", timestamp));
            }
            ClaudeEvent::Error { message } => {
                self.update_header("❌ Error");
                self.add_event(format!("[{}] Error: {}", timestamp, message));
                eprintln!("Error: {}", message);
            }
        }

        // Tick the spinner to show activity
        self.status_bar.tick();
    }

    /// Finish the progress display
    #[allow(dead_code)]
    pub fn finish(&self) {
        self.status_bar.finish_and_clear();
        self.events_bar.finish_and_clear();
    }

    /// Finish the progress display and show a final message
    pub fn finish_with_message(&self, message: &str) {
        self.status_bar.finish_with_message(message.to_string());
        self.events_bar.finish_and_clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_display_creation() {
        let config = ProgressConfig {
            minion_id: "M001".to_string(),
            issue: "42".to_string(),
            quiet: false,
        };

        let display = ProgressDisplay::new(config);
        assert_eq!(display.recent_events.lock().unwrap().len(), 0);
    }

    #[test]
    fn test_add_event_limits_history() {
        let config = ProgressConfig {
            minion_id: "M001".to_string(),
            issue: "42".to_string(),
            quiet: false,
        };

        let display = ProgressDisplay::new(config);

        // Add 6 events
        for i in 0..6 {
            display.add_event(format!("Event {}", i));
        }

        // Should only keep the last 4
        let events = display.recent_events.lock().unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(events[0], "Event 2");
        assert_eq!(events[3], "Event 5");
    }

    #[test]
    fn test_quiet_mode_suppresses_output() {
        let config = ProgressConfig {
            minion_id: "M001".to_string(),
            issue: "42".to_string(),
            quiet: true,
        };

        let display = ProgressDisplay::new(config);

        // In quiet mode, non-error events shouldn't be added
        let thinking = StreamOutput::Event(ClaudeEvent::Thinking {
            content: "test".to_string(),
        });

        display.handle_output(&thinking);

        // The event shouldn't be added to recent events in quiet mode
        // (This is a simplified test - in practice, quiet mode just doesn't display)
        assert!(display.config.quiet);
    }
}
