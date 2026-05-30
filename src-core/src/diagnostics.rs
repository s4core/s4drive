use std::collections::VecDeque;

/// Diagnostics and health monitoring for S4Drive Core.
#[derive(Default)]
pub struct Diagnostics {
    events: VecDeque<DiagnosticEvent>,
}

/// A single diagnostic event (log entry).
#[derive(Debug, Clone)]
pub struct DiagnosticEvent {
    pub message: String,
    pub timestamp: String,
    pub level: String,
}

impl Diagnostics {
    pub fn new() -> Self {
        Self {
            events: VecDeque::new(),
        }
    }

    /// Log a diagnostic event.
    pub fn log(&mut self, message: &str) {
        let event = DiagnosticEvent {
            message: message.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            level: "info".into(),
        };
        tracing::info!("{}", message);
        self.events.push_back(event);
        if self.events.len() > 1000 {
            self.events.pop_front();
        }
    }

    /// Log an error event.
    pub fn error(&mut self, message: &str) {
        let event = DiagnosticEvent {
            message: message.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            level: "error".into(),
        };
        tracing::error!("{}", message);
        self.events.push_back(event);
    }

    /// Get recent events.
    pub fn recent_events(&self, count: usize) -> Vec<DiagnosticEvent> {
        self.events.iter().rev().take(count).cloned().collect()
    }

    /// Export diagnostics as JSON bundle.
    pub fn export_bundle(&self) -> String {
        let events: Vec<serde_json::Value> = self
            .events
            .iter()
            .map(|e| {
                serde_json::json!({
                    "message": e.message,
                    "timestamp": e.timestamp,
                    "level": e.level,
                })
            })
            .collect();

        serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "events": events,
            "event_count": events.len(),
        })
        .to_string()
    }
}
