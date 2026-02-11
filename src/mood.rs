use std::sync::RwLock;

use rand::Rng;

use crate::memory::MemoryStore;
use crate::observer::{ObserverCategory, ObserverHandle};

const MODIFIERS: &[&str] = &[
    "curious",
    "focused",
    "playful",
    "contemplative",
    "energetic",
    "calm",
    "creative",
    "analytical",
    "warm",
    "precise",
];

struct MoodInner {
    energy: f32,
    valence: f32,
    active_modifier: String,
    last_interaction: Option<chrono::DateTime<chrono::Utc>>,
}

/// Thread-safe mood state with time-of-day energy curves and random personality drift.
pub struct MoodState {
    inner: RwLock<MoodInner>,
    timezone_offset: i32,
}

impl MoodState {
    /// Create a new mood state with defaults.
    pub fn new(timezone_offset: i32) -> Self {
        Self {
            inner: RwLock::new(MoodInner {
                energy: 0.7,
                valence: 0.0,
                active_modifier: "calm".to_string(),
                last_interaction: None,
            }),
            timezone_offset,
        }
    }

    /// Load mood state from DB, falling back to defaults.
    pub fn load(memory: &MemoryStore, timezone_offset: i32) -> Self {
        let state = Self::new(timezone_offset);
        if let Ok(row) = memory.load_mood_state() {
            if let Ok(mut inner) = state.inner.write() {
                inner.energy = row.0;
                inner.valence = row.1;
                inner.active_modifier = row.2;
            }
        }
        state
    }

    /// Persist current mood to DB.
    pub fn save(&self, memory: &MemoryStore) {
        if let Ok(inner) = self.inner.read() {
            let _ = memory.save_mood_state(inner.energy, inner.valence, &inner.active_modifier);
        }
    }

    /// Called periodically (~15 min) to drift mood with time-of-day energy curves.
    pub fn drift(&self, observer: &ObserverHandle) {
        let mut rng = rand::thread_rng();
        let mut inner = match self.inner.write() {
            Ok(i) => i,
            Err(_) => return,
        };

        let old_modifier = inner.active_modifier.clone();

        // Energy follows time-of-day curve
        let target_energy = self.time_of_day_energy();
        // Smooth exponential approach (80% toward target)
        inner.energy = inner.energy * 0.8 + target_energy * 0.2;
        // Add small random perturbation
        inner.energy = (inner.energy + rng.gen_range(-0.05..0.05)).clamp(0.0, 1.0);

        // Valence drifts toward neutral with perturbation
        inner.valence = inner.valence * 0.9 + rng.gen_range(-0.1..0.1);
        inner.valence = inner.valence.clamp(-1.0, 1.0);

        // 20% chance to shift modifier
        if rng.gen::<f32>() < 0.2 {
            let idx = rng.gen_range(0..MODIFIERS.len());
            inner.active_modifier = MODIFIERS[idx].to_string();
        }

        let energy = inner.energy;
        let valence = inner.valence;
        let modifier = inner.active_modifier.clone();

        if modifier != old_modifier {
            observer.log(
                ObserverCategory::MoodChange,
                format!("Mood shift: {} -> {} (energy: {:.2}, valence: {:.2})", old_modifier, modifier, energy, valence),
            );
        } else {
            observer.log(
                ObserverCategory::EnergyShift,
                format!("Energy: {:.2}, Valence: {:.2}, Modifier: {}", energy, valence, modifier),
            );
        }
    }

    /// Small energy/valence boost when the user interacts.
    pub fn record_interaction(&self) {
        if let Ok(mut inner) = self.inner.write() {
            inner.energy = (inner.energy + 0.05).min(1.0);
            inner.valence = (inner.valence + 0.05).min(1.0);
            inner.last_interaction = Some(chrono::Utc::now());
        }
    }

    /// Generate a description of current mood for system prompt injection.
    pub fn describe(&self) -> String {
        let inner = match self.inner.read() {
            Ok(i) => i,
            Err(_) => return String::new(),
        };

        let energy_desc = match inner.energy {
            e if e > 0.8 => "very energetic",
            e if e > 0.6 => "alert and engaged",
            e if e > 0.4 => "steady",
            e if e > 0.2 => "a bit low-energy",
            _ => "quite tired",
        };

        let valence_desc = match inner.valence {
            v if v > 0.3 => "positive",
            v if v > -0.3 => "neutral",
            _ => "a bit subdued",
        };

        format!(
            "Current mood: feeling {} and {}, in a {} state.",
            energy_desc, valence_desc, inner.active_modifier
        )
    }

    /// Get current energy level.
    pub fn energy(&self) -> f32 {
        self.inner.read().map(|i| i.energy).unwrap_or(0.5)
    }

    /// Get current modifier.
    pub fn modifier(&self) -> String {
        self.inner
            .read()
            .map(|i| i.active_modifier.clone())
            .unwrap_or_else(|_| "calm".to_string())
    }

    /// Compute target energy based on time of day.
    /// Peak: 9-11am, Dip: 2-3pm, Wind-down: evening.
    fn time_of_day_energy(&self) -> f32 {
        let now = chrono::Utc::now();
        let local_hour = ((now.timestamp() / 3600 + self.timezone_offset as i64) % 24 + 24) as f32 % 24.0;
        let local_minute = ((now.timestamp() / 60) % 60) as f32;
        let t = local_hour + local_minute / 60.0;

        // Piecewise energy curve
        match t {
            t if t < 6.0 => 0.2,             // sleeping/very low
            t if t < 9.0 => 0.2 + (t - 6.0) * 0.2,  // waking up: 0.2 -> 0.8
            t if t < 11.0 => 0.8 + (t - 9.0) * 0.05, // peak: 0.8 -> 0.9
            t if t < 14.0 => 0.9 - (t - 11.0) * 0.1, // gentle decline: 0.9 -> 0.6
            t if t < 15.0 => 0.6 - (t - 14.0) * 0.1, // afternoon dip: 0.6 -> 0.5
            t if t < 17.0 => 0.5 + (t - 15.0) * 0.1, // recovery: 0.5 -> 0.7
            t if t < 21.0 => 0.7 - (t - 17.0) * 0.075, // evening decline: 0.7 -> 0.4
            _ => 0.4 - (t - 21.0) * 0.067,    // wind-down: 0.4 -> ~0.2
        }
        .clamp(0.1, 1.0)
    }
}
