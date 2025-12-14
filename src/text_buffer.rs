use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Maximum buffer size in characters before forcing a flush.
/// This prevents unbounded memory growth from streaming text without newlines.
/// 1000 characters is roughly 15-20 lines of typical code or prose.
const MAX_BUFFER_SIZE: usize = 1000;

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
        let mut state = self
            .buffer
            .lock()
            .expect("TextBuffer mutex poisoned - indicates a panic in buffer code");

        // Check if we should flush due to timeout
        let should_flush_timeout = if let Some(last_update) = state.last_update {
            last_update.elapsed() >= state.flush_interval
        } else {
            false
        };

        // If timeout expired and we have buffered text, flush it before adding new text
        if should_flush_timeout && !state.text.is_empty() {
            let flushed = std::mem::take(&mut state.text);
            state.text.push_str(text);
            // Start timing new buffer with the just-added text
            state.last_update = Some(Instant::now());
            return Some(flushed);
        }

        // Add new text to buffer
        state.text.push_str(text);
        // Update or initialize the timestamp
        if state.last_update.is_none() {
            state.last_update = Some(Instant::now());
        }

        // Check if newly added text contains newline or ends with sentence boundary
        // (more efficient than scanning entire buffer)
        let has_newline = text.contains('\n');
        let has_sentence_end = text.ends_with(". ") || text.ends_with("! ") || text.ends_with("? ");

        // Flush if newline, sentence boundary, or buffer is getting full
        if has_newline || has_sentence_end || state.text.len() > MAX_BUFFER_SIZE {
            let flushed = std::mem::take(&mut state.text);
            // Reset timestamp since buffer is now empty
            state.last_update = None;
            Some(flushed)
        } else {
            None
        }
    }

    /// Force flush the buffer, returning any accumulated text
    pub fn flush(&self) -> Option<String> {
        let mut state = self
            .buffer
            .lock()
            .expect("TextBuffer mutex poisoned - indicates a panic in buffer code");

        if state.text.is_empty() {
            None
        } else {
            let flushed = std::mem::take(&mut state.text);
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

        // Add text that exceeds MAX_BUFFER_SIZE
        let long_text = "a".repeat(MAX_BUFFER_SIZE + 1);
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

    #[test]
    fn test_buffer_flushes_on_period_space() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        buffer.add("Hello");
        let result = buffer.add(" world. ");

        assert_eq!(result, Some("Hello world. ".to_string()));
    }

    #[test]
    fn test_buffer_flushes_on_question_space() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        buffer.add("Is this working");
        let result = buffer.add("? ");

        assert_eq!(result, Some("Is this working? ".to_string()));
    }

    #[test]
    fn test_buffer_flushes_on_exclamation_space() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        buffer.add("Great work");
        let result = buffer.add("! ");

        assert_eq!(result, Some("Great work! ".to_string()));
    }

    #[test]
    fn test_url_does_not_trigger_flush() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        // URLs like "example.com" should not trigger flush (no space after period)
        let result = buffer.add("Visit example.com");
        assert!(result.is_none());

        // Verify buffer still contains the text
        let flushed = buffer.flush();
        assert_eq!(flushed, Some("Visit example.com".to_string()));
    }

    #[test]
    fn test_abbreviation_does_not_trigger_flush() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        // "Dr." followed by space but part of a name should not trigger flush
        // However, this will trigger flush - that's acceptable per issue notes
        let result = buffer.add("Dr. ");
        assert_eq!(result, Some("Dr. ".to_string()));
    }

    #[test]
    fn test_multiple_sentence_boundaries() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        // First sentence
        let result = buffer.add("First sentence. ");
        assert_eq!(result, Some("First sentence. ".to_string()));

        // Second sentence
        let result = buffer.add("Second sentence. ");
        assert_eq!(result, Some("Second sentence. ".to_string()));

        // Buffer should be empty now
        let result = buffer.flush();
        assert!(result.is_none());
    }

    #[test]
    fn test_sentence_boundary_without_trailing_space() {
        let buffer = TextBuffer::new(Duration::from_millis(150));

        // Period without space should not trigger flush
        let result = buffer.add("Hello world.");
        assert!(result.is_none());

        // Verify buffer still contains the text
        let flushed = buffer.flush();
        assert_eq!(flushed, Some("Hello world.".to_string()));
    }
}
