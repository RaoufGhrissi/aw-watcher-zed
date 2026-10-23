// Heartbeat model:
// - while Zed is in use, a heartbeat is sent every minute with a pulsetime of 80s,
//   so consecutive heartbeats with the same data merge into one event;
// - when the data changes, the previous data is sent once more 1ms before the new
//   one, so the previous event ends exactly where the new one starts;
// - when Zed is no longer in use, the previous data is sent once more, then the same
//   data with `focused: false`, which makes a 0s event that closes the previous one.

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Map, Value};

pub type Data = Map<String, Value>;

pub trait Sender {
    async fn send(&self, data: &Data, timestamp: DateTime<Utc>);
}

pub struct Heartbeats<S> {
    sender: S,
    last: Option<Data>,
}

impl<S: Sender> Heartbeats<S> {
    pub fn new(sender: S) -> Self {
        Self { sender, last: None }
    }

    // Extends the current event, or starts a new one if the data changed
    pub async fn beat(&mut self, data: Data, now: DateTime<Utc>) {
        if let Some(last) = self.last.as_ref().filter(|last| **last != data) {
            self.sender
                .send(last, now - TimeDelta::milliseconds(1))
                .await;
        }
        self.sender.send(&data, now).await;
        self.last = Some(data);
    }

    // Starts a new event only if the data changed; otherwise the next beat extends it
    pub async fn beat_if_changed(&mut self, data: Data, now: DateTime<Utc>) {
        if self.last.as_ref() != Some(&data) {
            self.beat(data, now).await;
        }
    }

    // Ends the current event at `now`
    pub async fn close(&mut self, now: DateTime<Utc>) {
        let Some(mut last) = self.last.take() else {
            return;
        };
        self.sender
            .send(&last, now - TimeDelta::milliseconds(1))
            .await;
        last.insert("focused".to_string(), Value::Bool(false));
        self.sender.send(&last, now).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(Value, i64)>>);

    impl Sender for &Recorder {
        async fn send(&self, data: &Data, timestamp: DateTime<Utc>) {
            let data = Value::Object(data.clone());
            self.0
                .lock()
                .unwrap()
                .push((data, timestamp.timestamp_millis()));
        }
    }

    fn data(branch: &str) -> Data {
        json!({ "branch": branch }).as_object().unwrap().clone()
    }

    fn at(ms: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(ms).unwrap()
    }

    #[tokio::test]
    async fn same_data_extends_the_event() {
        let recorder = Recorder::default();
        let mut heartbeats = Heartbeats::new(&recorder);
        heartbeats.beat(data("a"), at(1000)).await;
        heartbeats.beat(data("a"), at(61000)).await;
        assert_eq!(
            *recorder.0.lock().unwrap(),
            [
                (json!({"branch": "a"}), 1000),
                (json!({"branch": "a"}), 61000)
            ]
        );
    }

    #[tokio::test]
    async fn new_data_ends_the_previous_event_1ms_before() {
        let recorder = Recorder::default();
        let mut heartbeats = Heartbeats::new(&recorder);
        heartbeats.beat(data("a"), at(1000)).await;
        heartbeats.beat(data("b"), at(5000)).await;
        assert_eq!(
            *recorder.0.lock().unwrap(),
            [
                (json!({"branch": "a"}), 1000),
                (json!({"branch": "a"}), 4999),
                (json!({"branch": "b"}), 5000),
            ]
        );
    }

    #[tokio::test]
    async fn beat_if_changed_sends_only_new_data() {
        let recorder = Recorder::default();
        let mut heartbeats = Heartbeats::new(&recorder);
        heartbeats.beat_if_changed(data("a"), at(1000)).await;
        heartbeats.beat_if_changed(data("a"), at(2000)).await;
        heartbeats.beat_if_changed(data("b"), at(3000)).await;
        assert_eq!(
            *recorder.0.lock().unwrap(),
            [
                (json!({"branch": "a"}), 1000),
                (json!({"branch": "a"}), 2999),
                (json!({"branch": "b"}), 3000),
            ]
        );
    }

    #[tokio::test]
    async fn close_ends_the_event_and_the_next_beat_starts_one() {
        let recorder = Recorder::default();
        let mut heartbeats = Heartbeats::new(&recorder);
        heartbeats.beat(data("a"), at(1000)).await;
        heartbeats.close(at(30000)).await;
        heartbeats.close(at(40000)).await;
        heartbeats.beat_if_changed(data("a"), at(50000)).await;
        assert_eq!(
            *recorder.0.lock().unwrap(),
            [
                (json!({"branch": "a"}), 1000),
                (json!({"branch": "a"}), 29999),
                (json!({"branch": "a", "focused": false}), 30000),
                (json!({"branch": "a"}), 50000),
            ]
        );
    }
}
