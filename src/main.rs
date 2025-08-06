mod models;
mod user_status;

use actix_web::web::Buf;
use chrono::NaiveDateTime;
use chrono::NaiveTime;
use chrono::TimeDelta;
use chrono::TimeZone;
use icalendar::DatePerhapsTime;
use rodio::source::Repeat;
use rodio::Sink;
use url::Url;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::thread;
use std::thread::sleep;
use serde::Deserialize;
use serde::Serialize;
use actix_web::{web, App, HttpServer};
use reqwest::get;
use actix_web::HttpResponse;
use serde_json::json;
use chrono::Utc;
use std::fs::File;
use rodio::{Decoder, OutputStream, source::Source};

static DATES: LazyLock<Mutex<Vec<NaiveDateTime>>> = LazyLock::new(|| Mutex::new(vec![]));
static ACTIVE_NOTIF: LazyLock<Mutex<Vec<NaiveDateTime>>> = LazyLock::new(|| Mutex::new(vec![]));
static SILENCED: LazyLock<Mutex<Vec<NaiveDateTime>>> = LazyLock::new(|| Mutex::new(vec![]));
static AUDIO_STREAM: LazyLock<async_std::sync::Mutex<OutputStream>> = LazyLock::new(|| async_std::sync::Mutex::new(rodio::OutputStreamBuilder::open_default_stream().expect("Could not open default audio stream")));
static NOTIFICATION_SOUND: LazyLock<Mutex<Option<File>>> = LazyLock::new(|| Mutex::new(None));
static ALARM_SINK: LazyLock<Mutex<Option<rodio::Sink>>> = LazyLock::new(|| Mutex::new(None));
static NOTIFICATION_SINK: LazyLock<Mutex<Option<rodio::Sink>>> = LazyLock::new(|| Mutex::new(None));

#[derive(Serialize, Deserialize)]
struct Config {
    route: String,
    email: String,
    notification_sound_path: String,
    alarm_sound_path: String,
    alarm_check_interval_seconds: u32,
    alarm_silence_interval_seconds: u32,
    last_activity_check_minutes: u16,
    smtp_config: SmtpConfig,
    calendar_config: CalendarConfig
}

#[derive(Serialize, Deserialize, Default)]
struct SmtpConfig {
    endpoint: String,
    port: u16,
    username: String,
    password: String
}

#[derive(Serialize, Deserialize, Default)]
struct CalendarConfig {
    link: String,
    notify_before_seconds: u32,
    calendar_check_interval_minutes: u16
}

impl Default for Config {
    fn default() -> Config {
        Config {
            route: "/checkin".to_string(),
            email: "jackham800@gmail.com".to_string(),
            notification_sound_path: "~/.config/awaken/notification.mp3".to_string(),
            alarm_sound_path: "~/.config/awaken/alarm.mp3".to_string(),
            alarm_check_interval_seconds: 15,
            alarm_silence_interval_seconds: 1,
            last_activity_check_minutes: 15,
            smtp_config: SmtpConfig {
                endpoint: "smtp.gmail.com".to_string(),
                port: 587,
                username: "username".to_string(),
                password: "password".to_string()
            },
            calendar_config: CalendarConfig {
                link: "foo.com/basic.ical".to_string(),
                notify_before_seconds: 300,
                calendar_check_interval_minutes: 30,
            },
        }
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let config = toml_configurator::get_config::<Config>("awaken".to_string());
    *NOTIFICATION_SOUND.lock().expect("Could not lock notification sound file") = Some(File::open(shellexpand::tilde(&config.notification_sound_path).to_mut()).unwrap());
    let alarm_sound = File::open(shellexpand::tilde(&config.alarm_sound_path).to_mut()).unwrap();
    let alarm_noise = alarm_sound;
    let stream = AUDIO_STREAM.lock().await;
    let alarm_sink = Sink::connect_new(stream.mixer());
    let sound = Decoder::try_from(alarm_noise).unwrap();
    alarm_sink.pause();
    alarm_sink.append(sound.repeat_infinite());
    *ALARM_SINK.lock().expect("Failed to lock alarm sink") = Some(alarm_sink);
    *NOTIFICATION_SINK.lock().expect("Failed to lock alarm sink") = Some(Sink::connect_new(stream.mixer()));

    thread::spawn(move || {
        // Calendar check thread
        let runtime = tokio::runtime::Runtime::new().expect("Failed to setup tokio runtime");
        runtime.block_on(async {
            let mut interval_timer = tokio::time::interval(chrono::Duration::minutes(config.calendar_config.calendar_check_interval_minutes.into()).to_std().unwrap());
            loop {
                interval_timer.tick().await;
                fetch_calendar().await;
            }
        });
    });
    thread::spawn(move || {
        // Alarm parsing thread
        let runtime = tokio::runtime::Runtime::new().expect("Failed to setup tokio runtime");
        let notify_timer = &config.calendar_config.notify_before_seconds;
        runtime.block_on(async {
            let mut interval_timer = tokio::time::interval(chrono::Duration::seconds(config.alarm_check_interval_seconds.into()).to_std().unwrap());
            loop {
                interval_timer.tick().await;
                next_meeting(notify_timer, TimeDelta::minutes(config.last_activity_check_minutes.into()));
            }
        });
    });
    thread::spawn(move || {
        // Silencing thread
        let runtime = tokio::runtime::Runtime::new().expect("Failed to setup tokio runtime");
        runtime.block_on(async {
            let mut interval_timer = tokio::time::interval(chrono::Duration::seconds(config.alarm_silence_interval_seconds.into()).to_std().unwrap());
            loop {
                interval_timer.tick().await;
                remove_passed();
                update_alarm_state();
            }
        });
    });
    HttpServer::new(move || {
        App::new()
            .route(&config.route, web::get().to(update_activity))
    })
    .bind("127.0.0.1:8080")?
    .run()
    .await
}

fn next_meeting(notify_before_seconds: &u32, time_delta_since_last_seen: TimeDelta) {
    let time = NaiveDateTime::new(chrono::Utc::now().date_naive(), chrono::Utc::now().time());
    let dates = DATES.lock().expect("Could not lock dates").clone();
    let mut upcoming: Vec<NaiveDateTime> = dates.into_iter().filter(|item| item.cmp(&time).is_ge()).collect();
    upcoming.sort();
    if !upcoming.is_empty() {
        println!("{} upcoming events. Next: {:?}, in {}", upcoming.len(), upcoming.first(), *upcoming.first().expect("No event found") - time);
        if *upcoming.first().expect("Event expected, none found.") - time < TimeDelta::seconds((*notify_before_seconds).into()) {
            conditional_alarm(*upcoming.first().expect("Event expected, none found."), time_delta_since_last_seen);
        }
    }
}

fn remove_passed() {
    let mut active = ACTIVE_NOTIF.lock().expect("Failed to lock active notifications");
    let time = NaiveDateTime::new(chrono::Utc::now().date_naive(), chrono::Utc::now().time());
    *active = active.clone().into_iter().filter(|item| item.cmp(&time).is_ge()).collect();
    let mut silenced = SILENCED.lock().expect("Failed to lock active notifications");
    *silenced = silenced.clone().into_iter().filter(|item| item.cmp(&time).is_ge()).collect();
}

pub async fn update_activity() -> HttpResponse {
    match user_status::LAST_SEEN.lock() {
        Ok(mut last_seen) => {
            *last_seen = Some(Utc::now());
            println!("Activity seen {}", last_seen.expect("Error: expected date."));
            // Silence all active notifications
            let mut active = ACTIVE_NOTIF.lock().expect("Failed to lock active notifications");
            let mut silenced = SILENCED.lock().expect("Failed to lock silenced notifications");

            active.clone().into_iter().for_each(|item| {
                silenced.push(item);
            });
            active.clear();
        },
        Err(_) => return HttpResponse::InternalServerError().json(json!("Error: Failed to lock shared state.")),
    }
    HttpResponse::Ok().finish()
}

fn conditional_alarm(date: NaiveDateTime, time_delta_since_last_seen: TimeDelta) {
    if (user_status::LAST_SEEN.lock().expect("Could not lock last seen").or(Some(Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap())).expect("Unknown error") + time_delta_since_last_seen).cmp(&Utc::now()).is_ge() {
        if !SILENCED.lock().expect("Could not lock silenced").contains(&date) {
            // User has been seen recently, notify instead of alarm
            println!("Firing notification");
            SILENCED.lock().expect("Could not lock silenced").push(date);
            play_notification();
        }
        return
    }
    if !ACTIVE_NOTIF.lock().expect("Failed to lock active notifications").contains(&date) && !SILENCED.lock().expect("Failed to lock silenced").contains(&date) {
        ACTIVE_NOTIF.lock().expect("Failed to lock active notifications").push(date);
        println!("Firing alarm");
        thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().expect("Failed to setup tokio runtime");
            runtime.block_on(async {
                sleep(chrono::Duration::minutes(10).to_std().unwrap());
                let mut active = ACTIVE_NOTIF.lock().expect("Failed to lock active notifications");
                let result = active.binary_search(&date);
                println!("Expired, removing active alarm");
                result.map(|idx| active.remove(idx)).expect("Could not remove active alarm");
            });
        });
    }
}

fn update_alarm_state() {
    if !ACTIVE_NOTIF.lock().expect("Could not lock active notifications").is_empty() {
        play_alarm_looping();
    } else {
        stop_alarm();
    }
}

fn play_notification() {
    println!("Playing notification");
    let mut binding = NOTIFICATION_SINK.lock().expect("Could not lock sink");
    let notification_sink = binding.as_mut().unwrap();
    let mut notification_sound_lock = NOTIFICATION_SOUND.lock().expect("Could not lock notification sound file");
    let notification_sound = notification_sound_lock.as_mut().unwrap();
    let sound = Decoder::try_from(notification_sound.try_clone().unwrap()).unwrap();
    notification_sink.append(sound);
    notification_sink.play();
}

fn play_alarm_looping() {
    ALARM_SINK.lock().expect("Could not lock sink").as_mut().unwrap().play();
}

fn stop_alarm() {
    ALARM_SINK.lock().expect("Could not lock sink").as_mut().unwrap().pause();
}

async fn fetch_calendar() {
    println!("Fetching calendar.");
    *DATES.lock().unwrap() = vec![];
    let config = toml_configurator::get_config::<Config>("awaken".to_string());
    let url = Url::parse(&config.calendar_config.link).unwrap();
    let response = get(url).await.expect("Invalid URL");
    let contents = response.bytes().await.expect("Could not read bytes");
    let calendar = ical::IcalParser::new(contents.reader());
    for item in calendar {
        item.inspect(process_events).expect("Invalid calendar contained");
    }
}

fn process_events(calendar: &ical::parser::ical::component::IcalCalendar) {
    for event in &calendar.events {
        for property in &event.properties {
            if property.name == "DTSTART" {
                let icalendar_prop = icalendar::Property::new("DTSTART", property.clone().value.expect("Could not read property"));
                let date_time = DatePerhapsTime::from_property(&icalendar_prop);

                match date_time {
                    Some(DatePerhapsTime::DateTime(date)) => {
                        match date {
                            icalendar::CalendarDateTime::Floating(date_unw) => add_date(date_unw),
                            icalendar::CalendarDateTime::Utc(date_unw) => add_date(date_unw.naive_utc()),
                            icalendar::CalendarDateTime::WithTimezone { date_time, tzid: _ } => add_date(date_time),

                        }
                    },
                    Some(DatePerhapsTime::Date(date)) => add_date(NaiveDateTime::new(date, NaiveTime::from_hms_opt(15, 30, 0).expect("Invalid anchor time for nonspecified time"))),
                    _ => ()
                }
            }
        }
    }
}

fn add_date(date: NaiveDateTime) {
    DATES.lock().unwrap().push(date);
}
