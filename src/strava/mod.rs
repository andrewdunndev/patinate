// module: strava — Strava API client, OAuth, paginated sync
//
// Types here describe the subset of Strava activity data we need for
// rendering. The polyline trick: `summary_polyline` from the activity
// list endpoint is sufficient for poster-resolution heatmaps; we never
// need to call the streams endpoint.

pub mod auth;
pub mod client;
pub mod sync;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Activity {
    pub id: i64,
    pub name: String,
    #[serde(rename = "type")]
    pub activity_type: ActivityType,
    pub start_date: DateTime<Utc>,
    pub start_lat: f64,
    pub start_lng: f64,
    pub distance_m: f64,
    pub moving_time_s: i64,
    /// Google-encoded polyline. Empty string if Strava returned no map.
    pub summary_polyline: String,
    pub athlete_id: i64,
    pub gear_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub enum ActivityType {
    Ride,
    EBikeRide,
    VirtualRide,
    Run,
    VirtualRun,
    Walk,
    Hike,
    #[serde(other)]
    Other,
}

impl ActivityType {
    pub fn as_data_attr(self) -> &'static str {
        match self {
            ActivityType::Ride => "Ride",
            ActivityType::EBikeRide => "EBikeRide",
            ActivityType::VirtualRide => "VirtualRide",
            ActivityType::Run => "Run",
            ActivityType::VirtualRun => "VirtualRun",
            ActivityType::Walk => "Walk",
            ActivityType::Hike => "Hike",
            ActivityType::Other => "Other",
        }
    }
}

impl std::str::FromStr for ActivityType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Ride" => Ok(ActivityType::Ride),
            "EBikeRide" => Ok(ActivityType::EBikeRide),
            "VirtualRide" => Ok(ActivityType::VirtualRide),
            "Run" => Ok(ActivityType::Run),
            "VirtualRun" => Ok(ActivityType::VirtualRun),
            "Walk" => Ok(ActivityType::Walk),
            "Hike" => Ok(ActivityType::Hike),
            "Other" => Ok(ActivityType::Other),
            _ => Err(format!(
                "unknown activity type {s:?}. Expected one of: \
                 Ride, EBikeRide, VirtualRide, Run, VirtualRun, Walk, Hike, Other."
            )),
        }
    }
}
