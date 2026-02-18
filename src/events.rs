use std::str::FromStr;

use crate::types::Error;
use anyhow::anyhow;
use cached::proc_macro::cached;
use chrono::{DateTime, Datelike, Days, Local, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use icalendar::{
    Calendar, CalendarComponent, CalendarDateTime, Component, DatePerhapsTime, EventLike,
};
use reqwest::StatusCode;
use rrule::RRuleSet;
use serde::Serialize;
use serde_with::skip_serializing_none;
use std::time::Duration;
use warp::{Filter, Reply, filters::BoxedFilter, reject};

async fn fetch_calendar(calendar_url: &str) -> anyhow::Result<String> {
    let calendar_request = reqwest::get(calendar_url).await?;
    let calendar_data = calendar_request.text().await?;
    Ok(calendar_data)
}

fn process_calendar(calendar_data: String) -> anyhow::Result<Calendar> {
    Calendar::from_str(&calendar_data).map_err(|a| anyhow!(a))
}

#[derive(Serialize, Clone, Debug)]
struct Location {
    string: String,
    url: String,
}

#[skip_serializing_none]
#[derive(Serialize, Clone, Debug)]
struct Event {
    summary: String,
    date: String,
    start_iso8601: String,
    end_iso8601: String,
    location: Option<Location>,
    description: Option<String>,
}

#[derive(Debug)]
enum EventDate {
    Date(NaiveDate),
    DateTimeUtc(DateTime<Utc>),
}

fn to_event_date(datetime: DatePerhapsTime) -> Option<EventDate> {
    match datetime {
        DatePerhapsTime::Date(naive_date) => Some(EventDate::Date(naive_date)),
        DatePerhapsTime::DateTime(CalendarDateTime::Utc(date_time)) => {
            Some(EventDate::DateTimeUtc(date_time))
        }
        DatePerhapsTime::DateTime(CalendarDateTime::WithTimezone {
            date_time: naive_date_time,
            tzid,
        }) => {
            let tz = match tzid.parse::<Tz>() {
                Ok(tz) => tz,
                // Skip if timezone is not found
                _ => return None,
            };
            // TODO: Remove unwraps
            let date_time: DateTime<Tz> = tz.from_local_datetime(&naive_date_time).unwrap();
            let date_time_utc: DateTime<Utc> =
                DateTime::<Utc>::from_timestamp(date_time.timestamp(), 0).unwrap();
            Some(EventDate::DateTimeUtc(date_time_utc))
        }
        date_perhaps_time => {
            eprintln!("Unhandled timestamp type: {:?}", date_perhaps_time);
            None
        }
    }
}

#[derive(Clone)]
struct Space {
    space_label: String,
    id: String,
}

async fn fetch_spaces() -> anyhow::Result<String> {
    let url: &'static str = "https://navi.jyu.fi/api/spaces";
    let request = reqwest::get(url).await?;
    let text_content = request.text().await?;
    Ok(text_content)
}

fn parse_spaces(string: String) -> anyhow::Result<Vec<Space>> {
    let json: serde_json::Value = serde_json::from_str(&string)?;
    let spaces = json["items"]
        .as_array()
        .ok_or_else(|| anyhow!("spaces are expressed in an unrecognized format"))?;
    let parsed_spaces = spaces
        .iter()
        .flat_map(|value| {
            value
                .as_object()
                .map(|dict| match (dict.get("spaceLabel"), dict.get("id")) {
                    (
                        Some(serde_json::Value::String(space_label)),
                        Some(serde_json::Value::String(id)),
                    ) if !space_label.is_empty() => vec![Space {
                        space_label: space_label.to_string(),
                        id: id.to_string(),
                    }],
                    _ => vec![],
                })
                .unwrap_or_else(std::vec::Vec::new)
        })
        .collect::<Vec<Space>>();
    Ok(parsed_spaces)
}

fn url_for_location(location: &str, spaces: &Vec<Space>) -> String {
    // navi.jyu.fi links for locations begining with university space codes (case sensitive!)
    for space in spaces {
        if location.starts_with(&space.space_label) {
            return format!(
                "https://navi.jyu.fi/space/{}",
                urlencoding::encode(&space.id)
            );
        }
    }

    // Link to Google Maps by default
    format!(
        "https://www.google.com/maps/search/?api=1&query={}",
        urlencoding::encode(location)
    )
}

fn data_to_events(
    calendar: Calendar,
    spaces: Vec<Space>,
    current_time: DateTime<Utc>,
) -> Result<Vec<Event>, warp::Rejection> {
    let mut event_components: Vec<icalendar::Event> = calendar
        .iter()
        // Filter out components other than of type event
        .flat_map(|component| match component {
            CalendarComponent::Event(event) => vec![event],
            _ => vec![],
        })
        // Populate recurring events
        .flat_map(|event| {
            // Construct a string containing only the recurrence rules of the event
            let rrules = ["DTSTART", "RRULE", "EXRULE", "RDATE", "EXDATE"];
            let mut ruleset_string = "".to_string();
            for rrule in rrules {
                match event.property_value(rrule) {
                    Some(rule) => ruleset_string.push_str(&format!("{rrule}:{rule}\n")),
                    None => {
                        let multi_rules = event.multi_properties().get(rrule);
                        if let Some(props) = multi_rules {
                            for prop in props {
                                ruleset_string.push_str(&format!("{rrule}:{}\n", prop.value()))
                            }
                        }
                    }
                }
            }

            // Parse recurrence rules
            let rrule: RRuleSet = match ruleset_string.parse() {
                // Append only the original event if parsing recurrence fails or recurrence rules don't exist
                Err(_) => return vec![event.to_owned()],
                Ok(rrule) => rrule,
            };

            // Make clones of the original event with new start and end timestamps
            const MAX_RECURRENCES: u16 = 100;
            rrule
                .all(MAX_RECURRENCES)
                .dates
                .iter()
                .flat_map(|date| {
                    let mut event_clone = event.clone();
                    match (
                        // TODO: Invoking to_event_date can be omitted, remove it
                        event.get_start().map(to_event_date),
                        event.get_end().map(to_event_date),
                    ) {
                        // Timestamps without time
                        (
                            Some(Some(EventDate::Date(original_start_date))),
                            Some(Some(EventDate::Date(original_end_date))),
                        ) => {
                            let duration = Days::new(
                                (original_end_date.num_days_from_ce()
                                    - original_start_date.num_days_from_ce())
                                    as u64,
                            );
                            let event_end = date.to_owned() + duration;
                            event_clone.starts(DatePerhapsTime::Date(date.date_naive()));
                            event_clone.ends(DatePerhapsTime::Date(event_end.date_naive()));
                            vec![event_clone]
                        }
                        // Timestamps with time
                        (
                            Some(Some(EventDate::DateTimeUtc(original_start_date))),
                            Some(Some(EventDate::DateTimeUtc(original_end_date))),
                        ) => {
                            let duration =
                                original_end_date.signed_duration_since(original_start_date);
                            let event_end = *date + duration;
                            let event_end_utc =
                                DateTime::<Utc>::from_timestamp(event_end.timestamp(), 0).unwrap();
                            let event_start = date;
                            let event_start_utc =
                                DateTime::<Utc>::from_timestamp(event_start.timestamp(), 0)
                                    .unwrap();
                            event_clone.starts(DatePerhapsTime::DateTime(event_start_utc.into()));
                            event_clone.ends(DatePerhapsTime::DateTime(event_end_utc.into()));
                            vec![event_clone]
                        }
                        _ => {
                            // Skip if event start and end are expressed in differing formats, or when parsing fails
                            println!("warning: skipping event {:?} recurrence", event);
                            vec![]
                        }
                    }
                })
                .collect()
        })
        // Filter past events out
        .filter(|event| {
            //let current_time: DateTime<Local> = Local::now();
            match event.get_end().map(to_event_date) {
                Some(Some(end_time)) => match end_time {
                    EventDate::Date(end_date) => {
                        current_time.num_days_from_ce() < end_date.num_days_from_ce()
                    }
                    EventDate::DateTimeUtc(end_time) => {
                        current_time.timestamp() <= end_time.timestamp()
                    }
                },
                _ => false,
            }
        })
        // Filter out events with start timestamp more than a year in the future
        .filter(|event| {
            let max_time: DateTime<Utc> = current_time + Duration::from_secs(365 * 24 * 60 * 60);
            match event.get_end().map(to_event_date) {
                Some(Some(start_time)) => match start_time {
                    EventDate::Date(start_date) => {
                        max_time.num_days_from_ce() > start_date.num_days_from_ce()
                    }
                    EventDate::DateTimeUtc(start_time) => {
                        max_time.timestamp() > start_time.timestamp()
                    }
                },
                _ => false,
            }
        })
        .collect();

    event_components.sort_by_key(|event| {
        match event.get_end().map(to_event_date) {
            Some(Some(end_time)) => {
                match end_time {
                    EventDate::Date(end_date) => {
                        let end_date_local = Utc
                            .with_ymd_and_hms(
                                end_date.year(),
                                end_date.month(),
                                end_date.day(),
                                0,
                                0,
                                0,
                            )
                            .unwrap(); // TODO: Remove unwrap
                        end_date_local.timestamp()
                    }
                    EventDate::DateTimeUtc(end_time) => end_time.timestamp(),
                }
            }
            _ => unreachable!(),
        }
    });

    let events: Vec<Event> = event_components
        .iter()
        .flat_map(|event| {
            // Extract required values from event
            let (summary, start, end) = match (
                event.get_summary().map(String::from),
                event.get_start().and_then(to_event_date),
                event.get_end().and_then(to_event_date),
            ) {
                (Some(summary), Some(start), Some(end)) => (summary, start, end),
                // Skip event if required values are missing
                _ => return vec![],
            };

            // Extract optional values from events
            let (description, location) = (
                event.get_description().map(String::from),
                event.get_location().map(String::from),
            );

            let start_iso8601;
            let end_iso8601;
            let date_string = match (&start, end) {
                (EventDate::Date(start), EventDate::Date(end)) => {
                    start_iso8601 = format!("{}", start.format("%Y-%m-%d"));
                    end_iso8601 = format!("{}", end.format("%Y-%m-%d"));
                    if end.signed_duration_since(*start).num_days() == 1 {
                        format!("{}", start.format("%d/%m/%Y"))
                    } else {
                        let real_end: NaiveDate = end - Days::new(1);
                        format!("{} - {}", start.format("%d/%m/%Y"), real_end.format("%d/%m/%Y"))
                    }
                }
                (EventDate::DateTimeUtc(start), EventDate::DateTimeUtc(end)) => {
                    start_iso8601 = start.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true);
                    end_iso8601 = end.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true);
                    let local_start = start.with_timezone(&Local);
                    let local_end = end.with_timezone(&Local);
                    if local_end.signed_duration_since(local_start).num_days() < 1 {
                        format!(
                            "{} {} - {}",
                            local_start.format("%d/%m/%Y"),
                            local_start.format("%H:%M"),
                            local_end.format("%H:%M")
                        )
                    } else {
                        format!(
                            "{} - {}",
                            local_start.format("%d/%m/%Y %H:%M"),
                            local_end.format("%d/%m %H:%M")
                        )
                    }
                }
                // Skip if event start and end are expressed in differing formats, or when parsing fails
                _ => return vec![],
            };

            let location_with_link = location.map(|location| Location {
                url: url_for_location(&location, &spaces),
                string: location,
            });

            vec![Event {
                summary,
                description,
                date: date_string,
                start_iso8601,
                end_iso8601,
                location: location_with_link,
            }]
        })
        .collect();

    Ok(events)
}

#[cached(
    time = 600,
    time_refresh = true,
    sync_writes = "default",
    result = true
)]
async fn get_events() -> Result<Vec<Event>, warp::Rejection> {
    let spaces_data = fetch_spaces().await.unwrap_or_default();
    let spaces = parse_spaces(spaces_data).unwrap_or_default();
    let calendar_data = fetch_calendar("https://calendar.google.com/calendar/ical/c_g2eqt2a7u1fc1pahe2o0ecm7as%40group.calendar.google.com/public/basic.ics").await.unwrap_or_default();
    let calendar = match process_calendar(calendar_data) {
        Ok(calendar) => calendar,
        Err(err) => {
            return Err(reject::custom(Error {
                message: "The remote calendar could not be processed.".to_string(),
                details: Some(format! {"{:?}", err}),
            }));
        }
    };
    let now = Utc::now();
    data_to_events(calendar, spaces, now)
}

async fn events() -> Result<impl Reply, warp::Rejection> {
    let events = get_events().await?;
    let json = warp::reply::json(&events);
    Ok(warp::reply::with_status(json, StatusCode::OK))
}

pub fn filter() -> BoxedFilter<(impl Reply,)> {
    warp::path("events").and_then(events).boxed()
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 2, 2, 16, 32, 11).unwrap()
    }

    #[test]
    fn test_event_parsing() {
        let calendar_data: &'static str = include_str!("test-data/basic.ics");
        let now = now();
        let calendar = Calendar::from_str(calendar_data).unwrap();
        let result = data_to_events(calendar, vec![], now).unwrap();
        assert_matches!(&result[..], [Event {
            summary, description: Some(description),
            location: Some(Location{string: location_string, url: _}),
            ..
        }] if summary == "Test Event"
            && description == "Test description"
            && location_string == "Test Location"
        );
    }

    #[test]
    fn test_recurrence_parsing() {
        let calendar_data: &'static str = include_str!("test-data/recurrence.ics");
        let now = now();
        let calendar = Calendar::from_str(calendar_data).unwrap();
        let result = data_to_events(calendar, vec![], now).unwrap();
        //result.iter().for_each(|event| println!("{}", event.date)); // Uncomment for debug print
        assert_matches!(
            &result[..],
            [
                Event {
                    summary,
                    date: date1,
                    location: None,
                    description: None,
                    ..
                },
                Event {
                    date: date2,
                    ..
                },
                Event {
                    date: date3,
                    ..
                },
                Event {
                    date: date4,
                    ..
                },
                Event {
                    date: date5,
                    ..
                }, .. ,
                Event {
                    date: last_date,
                    ..
                }
            ] if date1 == "02/02/2026" // Each event comforms correctly to recurrence rules
                && date2 == "02/03/2026"
                && date3 == "06/04/2026"
                && date4 == "04/05/2026"
                && date5 == "06/07/2026" // Skipped one event because of exclusion rules
                && last_date == "04/01/2027" // The last returned date is not older than a year
        );
    }

    #[test]
    fn test_multi_day_parsing() {
        let calendar_data: &'static str = include_str!("test-data/multi-day.ics");
        let now = now();
        let calendar = Calendar::from_str(calendar_data).unwrap();
        let result = data_to_events(calendar, vec![], now).unwrap();
        assert_matches!(&result[..], [Event {
            date,
            ..
        }] if date == "03/02/2026 - 07/02/2026"
        );
    }
}
