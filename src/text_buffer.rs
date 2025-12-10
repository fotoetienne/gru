use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Time-based text buffer for grouping streaming text fragments
/// Flushes after a timeout or on newline characters
pub struct TextBuffer {
    buffer: Arc<Mutex<BufferState>>,
}

struct BufferState {
    text: String,
    last_update: Option<Instant>,
    flush_interval: Duration,
}

impl TextBuffer {
    /// Create a new TextBuffer with the specified flush interval
    pub fn new(flush_interval: Duration) -> Self {
        Self {
            buffer: Arc::new(Mutex::new(BufferState {
                text: String::new(),
                last_update: None,
                flush_interval,
            })),
        }
    }

    /// Add text to the buffer
    /// Returns Some(flushed_text) if the buffer should be flushed, None otherwise
    pub fn add(&self, text: &str) -> Option<String> {
        let mut state = match self.buffer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                eprintln!("Warning: TextBuffer mutex poisoned, recovering");
                poisoned.into_inner()
            }
        };

        // Check if we should flush due to timeout
        let should_flush_timeout = if let Some(last_update) = state.last_update {
            last_update.elapsed() >= state.flush_interval
        } else {
            false
        };

        // If timeout expired and we have buffered text, flush it before adding new text
        if should_flush_timeout && !state.text.is_empty() {
            let flushed = state.text.clone();
            state.text.clear();
            state.text.push_str(text);
            state.last_update = Some(Instant::now());
            return Some(flushed);
        }

        // Add new text to buffer
        state.text.push_str(text);
        state.last_update = Some(Instant::now());

        // Check if buffer contains newline
        let has_newline = state.text.contains('\n');

        // Flush if newline detected or buffer is getting full
        if has_newline || state.text.len() > 1000 {
            let flushed = state.text.clone();
            state.text.clear();
            state.last_update = None;
            Some(flushed)
        } else {
            None
        }
    }

    /// Force flush the buffer, returning any accumulated text
    pub fn flush(&self) -> Option<String> {
        let mut state = match self.buffer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                eprintln!("Warning: TextBuffer mutex poisoned, recovering");
                poisoned.into_inner()
            }
        };

        if state.text.is_empty() {
            None
        } else {
            let flushed = state.text.clone();
            state.text.clear();
            state.last_update = None;
            Some(flushed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_buffer_accumulates_text() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        // Add text without triggering flush
        let result = buffer.add("Hello");
        assert!(result.is_none());

        let result = buffer.add(" world");
        assert!(result.is_none());
    }

    #[test]
    fn test_buffer_flushes_on_newline() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        buffer.add("Hello");
        let result = buffer.add(" world\n");

        assert_eq!(result, Some("Hello world\n".to_string()));
    }

    #[test]
    fn test_buffer_flushes_on_timeout() {
        let buffer = TextBuffer::new(Duration::from_millis(50));

        // Add text
        let result = buffer.add("Hello");
        assert!(result.is_none());

        // Wait for timeout
        thread::sleep(Duration::from_millis(60));

        // Next add should flush due to timeout
        let result = buffer.add(" world");
        assert_eq!(result, Some("Hello".to_string()));
    }

    #[test]
    fn test_manual_flush() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        buffer.add("Hello");
        buffer.add(" world");

        let result = buffer.flush();
        assert_eq!(result, Some("Hello world".to_string()));

        // Second flush should return None
        let result = buffer.flush();
        assert!(result.is_none());
    }

    #[test]
    fn test_buffer_flushes_when_full() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        // Add text that exceeds 1000 chars
        let long_text = "a".repeat(1001);
        let result = buffer.add(&long_text);

        // Should flush immediately when buffer is too full
        assert!(result.is_some());
    }

    #[test]
    fn test_multiple_newlines() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        let result = buffer.add("Line 1\nLine 2\n");
        assert_eq!(result, Some("Line 1\nLine 2\n".to_string()));

        // Buffer should be empty now
        let result = buffer.flush();
        assert!(result.is_none());
    }
}
