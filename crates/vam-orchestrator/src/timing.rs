//! Cold-start phase timing harness, gated on `VAM_TIMING=1`.
//!
//! When disabled (default), `mark` and `finish` are no-ops with no allocations
//! beyond the struct itself on the stack.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub struct PhaseTimer {
    enabled: bool,
    run_id: String,
    session_id: String,
    workspace_id: String,
    kernel_path: Option<String>,
    start: Instant,
    last: Instant,
    steps: Vec<(&'static str, u128)>,
    finished: bool,
}

impl PhaseTimer {
    pub fn start(run_id: String, session_id: String, workspace_id: String) -> Self {
        let enabled = std::env::var("VAM_TIMING").ok().as_deref() == Some("1");
        let now = Instant::now();
        Self {
            enabled,
            run_id,
            session_id,
            workspace_id,
            kernel_path: None,
            start: now,
            last: now,
            steps: Vec::new(),
            finished: false,
        }
    }

    pub fn shared(run_id: String, session_id: String, workspace_id: String) -> SharedPhaseTimer {
        Arc::new(Mutex::new(Self::start(run_id, session_id, workspace_id)))
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn step_labels(&self) -> Vec<&'static str> {
        self.steps.iter().map(|(label, _)| *label).collect()
    }

    pub fn set_kernel_path(&mut self, path: impl AsRef<Path>) {
        if self.enabled {
            self.kernel_path = Some(path.as_ref().display().to_string());
        }
    }

    pub fn mark(&mut self, label: &'static str) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        let micros = now.duration_since(self.last).as_micros();
        self.steps.push((label, micros));
        self.last = now;

        tracing::info!(
            target: "vam.timing",
            run_id = %self.run_id,
            session_id = %self.session_id,
            step = label,
            duration_us = micros as u64,
            "Phase step completed: {} in {} us",
            label,
            micros
        );
    }

    pub fn merge_steps(&mut self, sub: Vec<(&'static str, u128)>) {
        if !self.enabled || sub.is_empty() {
            return;
        }
        for (label, micros) in &sub {
            tracing::info!(
                target: "vam.timing",
                run_id = %self.run_id,
                session_id = %self.session_id,
                step = *label,
                duration_us = *micros as u64,
                "Merged sub-step: {} in {} us",
                label,
                micros
            );
        }
        self.steps.extend(sub);
    }

    pub fn finish(&mut self) {
        if !self.enabled || self.finished {
            return;
        }
        self.finished = true;
        let total_us = self.start.elapsed().as_micros() as u64;

        tracing::info!(
            target: "vam.timing",
            run_id = %self.run_id,
            session_id = %self.session_id,
            workspace_id = %self.workspace_id,
            total_us = total_us,
            step_count = self.steps.len(),
            "Phase timing completed in {} us across {} steps",
            total_us,
            self.steps.len()
        );

        // Optional legacy stderr JSON fallback if explicitly requested
        if std::env::var("VAM_TIMING_STDERR_JSON").ok().as_deref() == Some("1") {
            let mut steps_obj = serde_json::Map::new();
            for (label, micros) in &self.steps {
                steps_obj.insert(
                    (*label).to_string(),
                    serde_json::Value::Number(serde_json::Number::from(*micros as u64)),
                );
            }
            let line = serde_json::json!({
                "type": "vam.phase_timing",
                "run_id": self.run_id,
                "session_id": self.session_id,
                "workspace_id": self.workspace_id,
                "kernel_path": self.kernel_path.as_deref().unwrap_or(""),
                "total_us": total_us,
                "steps": steps_obj,
            });
            eprintln!("{}", line);
        }
    }
}

impl Drop for PhaseTimer {
    fn drop(&mut self) {
        self.finish();
    }
}

pub type SharedPhaseTimer = Arc<Mutex<PhaseTimer>>;

pub fn mark_shared(timer: &Option<SharedPhaseTimer>, label: &'static str) {
    if let Some(t) = timer {
        if let Ok(mut guard) = t.lock() {
            guard.mark(label);
        }
    }
}

pub fn merge_shared(timer: &Option<SharedPhaseTimer>, sub: Vec<(&'static str, u128)>) {
    if let Some(t) = timer {
        if let Ok(mut guard) = t.lock() {
            guard.merge_steps(sub);
        }
    }
}

pub fn set_kernel_shared(timer: &Option<SharedPhaseTimer>, path: impl AsRef<Path>) {
    if let Some(t) = timer {
        if let Ok(mut guard) = t.lock() {
            guard.set_kernel_path(path);
        }
    }
}

pub fn finish_shared(timer: &Option<SharedPhaseTimer>) {
    if let Some(t) = timer {
        if let Ok(mut guard) = t.lock() {
            guard.finish();
        }
    }
}
