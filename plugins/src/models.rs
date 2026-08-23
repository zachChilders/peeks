use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AltitudeReading {
    /// Change in altitude in meters since updates started (positive = up).
    pub relative_altitude: f64,
    /// Atmospheric pressure in kilopascals.
    pub pressure: f64,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AltitudeEvent {
    Reading(AltitudeReading),
    Error(String),
}
