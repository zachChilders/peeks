use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadingReading {
    /// Heading relative to magnetic north, in degrees (0-360).
    pub magnetic_heading: f64,
    /// Heading relative to true north, in degrees (0-360). May equal magnetic heading
    /// if true-north correction is unavailable.
    pub true_heading: f64,
    /// Accuracy of the heading in degrees; negative means invalid.
    pub accuracy: f64,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HeadingEvent {
    Reading(HeadingReading),
    Error(String),
}
