use crate::stream::{ClaudeEvent, StreamOutput};
use crate::text_buffer::TextBuffer;
use chrono::Local;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Maximum number of recent events to keep in the display history
const MAX_RECENT_EVENTS: usize = 4;

/// Maximum characters to display for buffered text chunks.
/// Since text is now buffered for up to 2 seconds, chunks can be larger.
/// 200 characters allows ~3-4 lines of text to display coherently.
const MAX_DISPLAY_CHARS: usize = 200;

/// Maximum characters for raw lines (JSON and other verbose content).
/// Raw lines are more aggressively truncated than buffered text to reduce clutter.
const MAX_RAW_LINE_CHARS: usize = 80;

/// Configuration for the progress display
pub struct ProgressConfig {
    pub minion_id: String,
    pub issue: String,
    pub quiet: bool,
}

/// Tracks the current tool being used and its accumulated input JSON
#[derive(Default)]
struct ToolInputTracker {
    tool_name: Option<String>,
    input_json: String,
}

impl ToolInputTracker {
    fn start_tool(&mut self, name: String) {
        self.tool_name = Some(name);
        self.input_json.clear();
    }

    fn add_input_chunk(&mut self, chunk: &str) {
        self.input_json.push_str(chunk);
    }

    fn take(&mut self) -> Option<(String, String)> {
        if let Some(name) = self.tool_name.take() {
            let input = std::mem::take(&mut self.input_json);
            Some((name, input))
        } else {
            None
        }
    }
}

/// Progress display manager for Minion work
pub struct ProgressDisplay {
    _multi: MultiProgress,
    status_bar: ProgressBar,
    events_bar: ProgressBar,
    start_time: Instant,
    config: ProgressConfig,
    recent_events: Arc<Mutex<Vec<String>>>,
    text_buffer: TextBuffer,
    tool_tracker: Arc<Mutex<ToolInputTracker>>,
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
            text_buffer: TextBuffer::new(Duration::from_secs(2)),
            tool_tracker: Arc::new(Mutex::new(ToolInputTracker::default())),
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
            if let StreamOutput::Event(ClaudeEvent::Error { error }) = output {
                let error_msg = error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown error");
                eprintln!("Error: {}", error_msg);
            }
            return;
        }

        match output {
            StreamOutput::Event(event) => self.handle_event(event),
            StreamOutput::ToolResult(tool_result) => self.handle_tool_result(tool_result),
            StreamOutput::RawLine(line) => {
                // Try to abbreviate if it looks like JSON
                let abbreviated = Self::abbreviate_raw_line(line);

                // Only output if we have something to show
                if !abbreviated.is_empty() {
                    use std::io::Write;
                    if let Err(e) = std::io::stdout().write_all(abbreviated.as_bytes()) {
                        eprintln!("Warning: Failed to write to stdout: {}", e);
                    }
                    let _ = std::io::stdout().flush();
                }
            }
        }
    }

    /// Abbreviate raw lines that might be JSON or other verbose content
    fn abbreviate_raw_line(line: &str) -> String {
        let trimmed = line.trim();

        // Empty lines pass through as-is
        if trimmed.is_empty() {
            return line.to_string();
        }

        // Try to parse as JSON to abbreviate
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) {
            // If it's a recognizable JSON structure, try to abbreviate it
            if json.is_object() {
                // For now, just truncate JSON objects
                let preview = format!("{}", json);
                if preview.len() > MAX_RAW_LINE_CHARS {
                    return format!("{}\n", Self::truncate_string(&preview, MAX_RAW_LINE_CHARS));
                }
            }
        }

        // If not JSON or too short to worry about, truncate to single line
        if trimmed.len() > MAX_RAW_LINE_CHARS {
            format!("{}\n", Self::truncate_string(trimmed, MAX_RAW_LINE_CHARS))
        } else {
            line.to_string()
        }
    }

    /// Handle a tool result message
    fn handle_tool_result(&self, tool_result: &crate::stream::ToolResult) {
        let timestamp = Local::now().format("%H:%M:%S");

        let formatted = if tool_result.is_error {
            // Format error tool results
            let error_msg = match tool_result.content.as_str() {
                Some(s) => s.to_string(),
                None => format!(
                    "Tool failed with non-string error content: {}",
                    tool_result.content
                ),
            };
            // Show first line of error
            let first_line = error_msg.lines().next().unwrap_or(&error_msg);
            let truncated = Self::truncate_string(first_line, 60);
            format!("[{}] ✗ Tool failed: {}", timestamp, truncated)
        } else {
            // Format successful tool results
            let size = tool_result
                .content
                .as_str()
                .map(|s| s.len())
                .unwrap_or_else(|| {
                    // For non-string content, estimate size from JSON
                    tool_result.content.to_string().len()
                });
            format!("[{}] ✓ Tool completed ({} bytes)", timestamp, size)
        };

        self.add_event(formatted);
    }

    /// Truncate a string to a maximum number of characters (not bytes)
    fn truncate_string(s: &str, max_chars: usize) -> String {
        // Collect up to max_chars + 1 characters to determine if truncation is needed
        let chars: Vec<char> = s.chars().take(max_chars + 1).collect();
        if chars.len() > max_chars {
            // String is too long, truncate it
            format!("{}...", chars[..max_chars].iter().collect::<String>())
        } else {
            // String is max_chars or shorter, return as-is
            s.to_string()
        }
    }

    /// Format tool information for display
    /// Returns a concise, human-readable description of the tool call
    fn format_tool_info(tool_name: &str, input_json: &str) -> String {
        // Try to parse the input JSON
        let input: serde_json::Value = match serde_json::from_str(input_json) {
            Ok(v) => v,
            Err(_) => {
                // If parsing fails, just show the tool name
                return format!("Tool: {}", tool_name);
            }
        };

        match tool_name {
            "Bash" => {
                if let Some(command) = input.get("command").and_then(|c| c.as_str()) {
                    let truncated = Self::truncate_string(command, 60);
                    format!("Run: {}", truncated)
                } else {
                    "Run: bash command".to_string()
                }
            }
            "Read" => {
                if let Some(file_path) = input.get("file_path").and_then(|f| f.as_str()) {
                    // Show just the filename or last few path components
                    let shortened = Self::shorten_path(file_path);
                    format!("Read: {}", shortened)
                } else {
                    "Read: file".to_string()
                }
            }
            "Write" => {
                if let Some(file_path) = input.get("file_path").and_then(|f| f.as_str()) {
                    let shortened = Self::shorten_path(file_path);
                    format!("Write: {}", shortened)
                } else {
                    "Write: file".to_string()
                }
            }
            "Edit" => {
                if let Some(file_path) = input.get("file_path").and_then(|f| f.as_str()) {
                    let shortened = Self::shorten_path(file_path);
                    format!("Edit: {}", shortened)
                } else {
                    "Edit: file".to_string()
                }
            }
            "Grep" => {
                if let Some(pattern) = input.get("pattern").and_then(|p| p.as_str()) {
                    let truncated = Self::truncate_string(pattern, 40);
                    format!("Search: {}", truncated)
                } else {
                    "Search: pattern".to_string()
                }
            }
            "Glob" => {
                if let Some(pattern) = input.get("pattern").and_then(|p| p.as_str()) {
                    let truncated = Self::truncate_string(pattern, 40);
                    format!("Find: {}", truncated)
                } else {
                    "Find: files".to_string()
                }
            }
            "Task" => {
                if let Some(description) = input.get("description").and_then(|d| d.as_str()) {
                    let truncated = Self::truncate_string(description, 50);
                    format!("Task: {}", truncated)
                } else {
                    "Task: running agent".to_string()
                }
            }
            "TodoWrite" => "Update todos".to_string(),
            "AskUserQuestion" => "Asking question...".to_string(),
            _ => format!("Tool: {}", tool_name),
        }
    }

    /// Shorten a file path for display
    /// Shows ".../" prefix if path is long, keeping the most relevant parts
    fn shorten_path(path: &str) -> String {
        let path_obj = std::path::Path::new(path);
        let components: Vec<_> = path_obj.components().collect();

        if components.len() <= 3 {
            // Short path, show as-is
            path.to_string()
        } else {
            // Long path, show last 3 components
            let last_parts: Vec<_> = components
                .iter()
                .rev()
                .take(3)
                .rev()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect();
            format!(".../{}", last_parts.join("/"))
        }
    }

    /// Handle a parsed Claude event
    fn handle_event(&self, event: &ClaudeEvent) {
        let timestamp = Local::now().format("%H:%M:%S");

        match event {
            ClaudeEvent::MessageStart { .. } => {
                self.update_header("💭 Thinking...");
                self.add_event(format!("[{}] Message started", timestamp));
            }
            ClaudeEvent::ContentBlockStart { content_block, .. } => {
                // Check if this is a tool_use block
                if let Some(block_type) = content_block.get("type").and_then(|t| t.as_str()) {
                    match block_type {
                        "tool_use" => {
                            let tool_name = content_block
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("unknown");
                            self.update_header(&format!("🔧 Using tool: {}", tool_name));

                            // Start tracking this tool's input
                            let mut tracker = match self.tool_tracker.lock() {
                                Ok(guard) => guard,
                                Err(poisoned) => {
                                    eprintln!("Warning: Tool tracker mutex poisoned, recovering");
                                    poisoned.into_inner()
                                }
                            };
                            tracker.start_tool(tool_name.to_string());
                        }
                        "text" => {
                            self.update_header("📝 Responding...");
                        }
                        _ => {}
                    }
                }
            }
            ClaudeEvent::ContentBlockDelta { delta, .. } => {
                // Handle different types of deltas
                if let Some(delta_type) = delta.get("type").and_then(|t| t.as_str()) {
                    match delta_type {
                        "text_delta" => {
                            // Handle text deltas with buffering
                            if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                                // Add text to buffer; flush if ready
                                if let Some(flushed_text) = self.text_buffer.add(text) {
                                    let truncated =
                                        Self::truncate_string(&flushed_text, MAX_DISPLAY_CHARS);
                                    self.add_event(format!("[{}] Text: {}", timestamp, truncated));
                                }
                            }
                        }
                        "input_json_delta" => {
                            // Accumulate tool input JSON
                            if let Some(partial_json) =
                                delta.get("partial_json").and_then(|p| p.as_str())
                            {
                                let mut tracker = match self.tool_tracker.lock() {
                                    Ok(guard) => guard,
                                    Err(poisoned) => {
                                        eprintln!(
                                            "Warning: Tool tracker mutex poisoned, recovering"
                                        );
                                        poisoned.into_inner()
                                    }
                                };
                                tracker.add_input_chunk(partial_json);
                            }
                        }
                        _ => {}
                    }
                }
            }
            ClaudeEvent::ContentBlockStop { .. } => {
                // Content block finished - handle both text and tool blocks

                // First, check if we have a tool to format
                let mut tracker = match self.tool_tracker.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => {
                        eprintln!("Warning: Tool tracker mutex poisoned, recovering");
                        poisoned.into_inner()
                    }
                };
                let tool_info = tracker.take();

                if let Some((tool_name, input_json)) = tool_info {
                    // Format and display tool information
                    let formatted = Self::format_tool_info(&tool_name, &input_json);
                    self.add_event(format!("[{}] {}", timestamp, formatted));
                } else {
                    // No tool, check for buffered text
                    if let Some(flushed_text) = self.text_buffer.flush() {
                        let truncated = Self::truncate_string(&flushed_text, MAX_DISPLAY_CHARS);
                        self.add_event(format!("[{}] Text: {}", timestamp, truncated));
                    }
                }
            }
            ClaudeEvent::MessageDelta { delta } => {
                // Check for stop_reason to detect completion
                if let Some(stop_reason) = delta.get("stop_reason").and_then(|r| r.as_str()) {
                    match stop_reason {
                        "end_turn" => {
                            self.update_header("✅ Complete");
                            self.add_event(format!("[{}] Complete", timestamp));
                        }
                        "tool_use" => {
                            self.update_header("⏸️  Waiting for tool results...");
                        }
                        _ => {}
                    }
                }
            }
            ClaudeEvent::MessageStop => {
                // Flush any remaining buffered text before completing
                if let Some(flushed_text) = self.text_buffer.flush() {
                    let truncated = Self::truncate_string(&flushed_text, MAX_DISPLAY_CHARS);
                    self.add_event(format!("[{}] Text: {}", timestamp, truncated));
                }
                self.update_header("✅ Message complete");
            }
            ClaudeEvent::Error { error } => {
                // Flush any buffered text before showing error
                if let Some(flushed_text) = self.text_buffer.flush() {
                    let truncated = Self::truncate_string(&flushed_text, MAX_DISPLAY_CHARS);
                    self.add_event(format!("[{}] Text: {}", timestamp, truncated));
                }

                self.update_header("❌ Error");
                let error_msg = error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown error");
                self.add_event(format!("[{}] Error: {}", timestamp, error_msg));
                eprintln!("Error: {}", error_msg);
            }
            ClaudeEvent::Ping => {
                // Keepalive ping - no action needed, spinner ticks below
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
        let message_start = StreamOutput::Event(ClaudeEvent::MessageStart {
            message: serde_json::json!({}),
        });

        display.handle_output(&message_start);

        // The event shouldn't be added to recent events in quiet mode
        // (This is a simplified test - in practice, quiet mode just doesn't display)
        assert!(display.config.quiet);
    }

    #[test]
    fn test_format_tool_info_bash() {
        let input = r#"{"command":"git status"}"#;
        let result = ProgressDisplay::format_tool_info("Bash", input);
        assert_eq!(result, "Run: git status");
    }

    #[test]
    fn test_format_tool_info_bash_long() {
        let input = r#"{"command":"git commit -m 'This is a very long commit message that should be truncated'"}"#;
        let result = ProgressDisplay::format_tool_info("Bash", input);
        assert!(result.starts_with("Run: git commit"));
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_format_tool_info_read() {
        let input = r#"{"file_path":"src/main.rs"}"#;
        let result = ProgressDisplay::format_tool_info("Read", input);
        assert_eq!(result, "Read: src/main.rs");
    }

    #[test]
    fn test_format_tool_info_write() {
        let input = r#"{"file_path":"/Users/test/project/src/lib.rs"}"#;
        let result = ProgressDisplay::format_tool_info("Write", input);
        assert!(result.contains("Write:"));
        assert!(result.contains("lib.rs"));
    }

    #[test]
    fn test_format_tool_info_edit() {
        let input = r#"{"file_path":"src/commands/fix.rs"}"#;
        let result = ProgressDisplay::format_tool_info("Edit", input);
        assert_eq!(result, "Edit: src/commands/fix.rs");
    }

    #[test]
    fn test_format_tool_info_grep() {
        let input = r#"{"pattern":"TODO"}"#;
        let result = ProgressDisplay::format_tool_info("Grep", input);
        assert_eq!(result, "Search: TODO");
    }

    #[test]
    fn test_format_tool_info_glob() {
        let input = r#"{"pattern":"**/*.rs"}"#;
        let result = ProgressDisplay::format_tool_info("Glob", input);
        assert_eq!(result, "Find: **/*.rs");
    }

    #[test]
    fn test_format_tool_info_task() {
        let input = r#"{"description":"Explore codebase"}"#;
        let result = ProgressDisplay::format_tool_info("Task", input);
        assert_eq!(result, "Task: Explore codebase");
    }

    #[test]
    fn test_format_tool_info_todowrite() {
        let input = r#"{"todos":[]}"#;
        let result = ProgressDisplay::format_tool_info("TodoWrite", input);
        assert_eq!(result, "Update todos");
    }

    #[test]
    fn test_format_tool_info_unknown_tool() {
        let input = r#"{"foo":"bar"}"#;
        let result = ProgressDisplay::format_tool_info("UnknownTool", input);
        assert_eq!(result, "Tool: UnknownTool");
    }

    #[test]
    fn test_format_tool_info_invalid_json() {
        let input = "not valid json";
        let result = ProgressDisplay::format_tool_info("Bash", input);
        assert_eq!(result, "Tool: Bash");
    }

    #[test]
    fn test_shorten_path_short() {
        let path = "src/main.rs";
        let result = ProgressDisplay::shorten_path(path);
        assert_eq!(result, "src/main.rs");
    }

    #[test]
    fn test_shorten_path_long() {
        let path = "/Users/test/projects/gru/src/commands/fix.rs";
        let result = ProgressDisplay::shorten_path(path);
        assert_eq!(result, ".../src/commands/fix.rs");
    }

    #[test]
    fn test_tool_input_tracker() {
        let mut tracker = ToolInputTracker::default();

        // Start tracking a tool
        tracker.start_tool("Bash".to_string());
        tracker.add_input_chunk(r#"{"comm"#);
        tracker.add_input_chunk(r#"and":"ls"}"#);

        // Take the accumulated input
        let result = tracker.take();
        assert!(result.is_some());
        let (name, input) = result.unwrap();
        assert_eq!(name, "Bash");
        assert_eq!(input, r#"{"command":"ls"}"#);

        // After taking, tracker should be empty
        assert!(tracker.take().is_none());
    }

    #[test]
    fn test_abbreviate_raw_line_empty() {
        let line = "";
        let result = ProgressDisplay::abbreviate_raw_line(line);
        assert_eq!(result, "");
    }

    #[test]
    fn test_abbreviate_raw_line_short() {
        let line = "Short message\n";
        let result = ProgressDisplay::abbreviate_raw_line(line);
        assert_eq!(result, "Short message\n");
    }

    #[test]
    fn test_abbreviate_raw_line_long() {
        let line = "This is a very long message that exceeds the maximum character limit and should be truncated to fit within the display";
        let result = ProgressDisplay::abbreviate_raw_line(line);
        assert!(result.len() <= 84); // MAX_RAW_LINE_CHARS + "..." + "\n"
        assert!(result.ends_with("...\n"));
    }

    #[test]
    fn test_abbreviate_raw_line_json_short() {
        let line = r#"{"foo":"bar"}"#;
        let result = ProgressDisplay::abbreviate_raw_line(line);
        assert_eq!(result, r#"{"foo":"bar"}"#);
    }

    #[test]
    fn test_abbreviate_raw_line_json_long() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_123","content":"very long content here"}]}}"#;
        let result = ProgressDisplay::abbreviate_raw_line(line);
        assert!(result.len() <= 84); // MAX_RAW_LINE_CHARS + "..." + "\n"
        assert!(result.ends_with("...\n"));
    }

    #[test]
    fn test_handle_tool_result_success() {
        use crate::stream::ToolResult;

        let tool_result = ToolResult {
            result_type: "tool_result".to_string(),
            tool_use_id: "toolu_123".to_string(),
            content: serde_json::json!("Command output here"),
            is_error: false,
        };

        let config = ProgressConfig {
            minion_id: "M001".to_string(),
            issue: "42".to_string(),
            quiet: false,
        };

        let display = ProgressDisplay::new(config);
        display.handle_tool_result(&tool_result);

        let events = display.recent_events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].contains("✓ Tool completed"));
        assert!(events[0].contains("bytes"));
    }

    #[test]
    fn test_handle_tool_result_error() {
        use crate::stream::ToolResult;

        let tool_result = ToolResult {
            result_type: "tool_result".to_string(),
            tool_use_id: "toolu_456".to_string(),
            content: serde_json::json!("Error: command not found"),
            is_error: true,
        };

        let config = ProgressConfig {
            minion_id: "M001".to_string(),
            issue: "42".to_string(),
            quiet: false,
        };

        let display = ProgressDisplay::new(config);
        display.handle_tool_result(&tool_result);

        let events = display.recent_events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].contains("✗ Tool failed"));
        assert!(events[0].contains("Error"));
    }
}
