use std::sync::{LazyLock, Mutex};
use chrono::prelude::*;

pub static LAST_SEEN: LazyLock<Mutex<Option<DateTime<Utc>>>> = LazyLock::new(|| Mutex::new(None));
