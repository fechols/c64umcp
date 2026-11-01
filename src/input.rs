//! REST input validation and paced C64 keyboard entry.
use std::{collections::HashSet, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, bail, ensure};
use reqwest::Method;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::device::Device;

const EMERGENCY_CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Transition {
    Press,
    Release,
    Tap,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputEvent {
    Keyboard {
        /// One to eight unique physical C64 key names. RESTORE must be tapped alone.
        inputs: Vec<String>,
        transition: Transition,
    },
    Joystick {
        /// Joystick port 1 or 2.
        port: u8,
        inputs: Vec<String>,
        transition: Transition,
    },
    ReleaseAll {},
}

const KEY_NAMES: &[&str] = &[
    "inst_del",
    "return",
    "cursor_left_right",
    "f7",
    "f1",
    "f3",
    "f5",
    "cursor_up_down",
    "left_shift",
    "right_shift",
    "plus",
    "minus",
    "period",
    "colon",
    "at",
    "comma",
    "pound",
    "star",
    "semicolon",
    "clr_home",
    "equals",
    "arrow_up",
    "slash",
    "arrow_left",
    "ctrl",
    "space",
    "commodore",
    "run_stop",
    "restore",
];
const JOYSTICK_NAMES: &[&str] = &["up", "down", "left", "right", "fire", "fire2", "fire3"];

fn valid_key(key: &str) -> bool {
    (key.len() == 1
        && key
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()))
        || KEY_NAMES.contains(&key)
}

fn unique_inputs(inputs: &[String], max: usize) -> Result<()> {
    ensure!(
        (1..=max).contains(&inputs.len()),
        "inputs must contain 1 to {max} names"
    );
    let unique: HashSet<_> = inputs.iter().collect();
    ensure!(unique.len() == inputs.len(), "input names must be unique");
    Ok(())
}

/// Validate both firmware limits before returning the exact JSON request body.
pub fn validate_events(events: &[InputEvent]) -> Result<Value> {
    ensure!(
        (1..=64).contains(&events.len()),
        "input batch must contain 1 to 64 events"
    );
    for event in events {
        match event {
            InputEvent::Keyboard { inputs, transition } => {
                unique_inputs(inputs, 8)?;
                for key in inputs {
                    ensure!(valid_key(key), "unknown keyboard input: {key}");
                }
                if inputs.iter().any(|key| key == "restore") {
                    ensure!(
                        inputs.len() == 1 && *transition == Transition::Tap,
                        "restore must appear alone with transition tap"
                    );
                }
            }
            InputEvent::Joystick { port, inputs, .. } => {
                ensure!((1..=2).contains(port), "joystick port must be 1 or 2");
                unique_inputs(inputs, 7)?;
                for key in inputs {
                    ensure!(
                        JOYSTICK_NAMES.contains(&key.as_str()),
                        "unknown joystick input: {key}"
                    );
                }
            }
            InputEvent::ReleaseAll {} => {}
        }
    }
    let body = json!({ "events": events });
    ensure!(
        serde_json::to_vec(&body)?.len() <= 4096,
        "serialized input batch exceeds the firmware's 4096-byte limit"
    );
    Ok(body)
}

/// Map the supported ASCII subset to physical C64 chords. Letter case folds to
/// unshifted keys: these produce uppercase on the default uppercase/graphics screen.
/// CR, LF and CRLF map to RETURN. Unsupported characters are rejected up front.
pub fn text_to_keys(text: &str) -> Result<Vec<Vec<String>>> {
    ensure!(
        text.chars().count() <= 1000,
        "text is limited to 1000 characters per call"
    );
    let mut keys = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let (shift, key) = match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' => {
                keys.push(vec![c.to_ascii_lowercase().to_string()]);
                continue;
            }
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                (false, "return")
            }
            '\n' => (false, "return"),
            ' ' => (false, "space"),
            '+' => (false, "plus"),
            '-' => (false, "minus"),
            '.' => (false, "period"),
            ':' => (false, "colon"),
            '@' => (false, "at"),
            ',' => (false, "comma"),
            '*' => (false, "star"),
            ';' => (false, "semicolon"),
            '=' => (false, "equals"),
            '/' => (false, "slash"),
            '!' => (true, "1"),
            '"' => (true, "2"),
            '#' => (true, "3"),
            '$' => (true, "4"),
            '%' => (true, "5"),
            '&' => (true, "6"),
            '\'' => (true, "7"),
            '(' => (true, "8"),
            ')' => (true, "9"),
            '?' => (true, "slash"),
            '<' => (true, "comma"),
            '>' => (true, "period"),
            '[' => (true, "colon"),
            ']' => (true, "semicolon"),
            _ => bail!(
                "unsupported typing character {c:?}; use raw keyboard events for other C64 keys"
            ),
        };
        let mut chord = Vec::with_capacity(2);
        if shift {
            chord.push("left_shift".into());
        }
        chord.push(key.into());
        keys.push(chord);
    }
    Ok(keys)
}

async fn send_chord(device: &Device, inputs: &[String], transition: Transition) -> Result<()> {
    let body = validate_events(&[InputEvent::Keyboard {
        inputs: inputs.to_vec(),
        transition,
    }])?;
    device
        .json(Method::POST, "machine:input", &[], Some(body))
        .await?;
    Ok(())
}

/// If a task is cancelled while a chord might be pressed, release that chord.
/// Cleanup cannot be guaranteed after process termination or lost connectivity.
struct KeyReleaseGuard {
    device: Device,
    active: Option<Vec<String>>,
    operation_guard: Option<Arc<tokio::sync::OwnedMutexGuard<()>>>,
    cleanup_deadline: Option<tokio::time::Instant>,
}

impl KeyReleaseGuard {
    async fn release(&mut self) -> Result<()> {
        if let Some(inputs) = &self.active {
            send_chord(&self.device, inputs, Transition::Release).await?;
            self.active = None;
        }
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<()> {
        if let Some(inputs) = &self.active {
            let deadline = *self
                .cleanup_deadline
                .get_or_insert_with(|| tokio::time::Instant::now() + EMERGENCY_CLEANUP_TIMEOUT);
            emergency_release(&self.device, inputs, deadline).await?;
            self.active = None;
        }
        Ok(())
    }
}

async fn emergency_release(
    device: &Device,
    inputs: &[String],
    deadline: tokio::time::Instant,
) -> Result<()> {
    tokio::time::timeout_at(deadline, send_chord(device, inputs, Transition::Release))
        .await
        .context("emergency key release exceeded its 3-second deadline; injected keys may remain pressed")?
}

impl Drop for KeyReleaseGuard {
    fn drop(&mut self) {
        if let Some(inputs) = self.active.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let device = self.device.clone();
            let operation_guard = self.operation_guard.take();
            // Cancellation during cleanup keeps the original deadline instead
            // of allowing another full HTTP timeout while holding the lock.
            let deadline = self
                .cleanup_deadline
                .unwrap_or_else(|| tokio::time::Instant::now() + EMERGENCY_CLEANUP_TIMEOUT);
            runtime.spawn(async move {
                let _operation_guard = operation_guard;
                if let Err(error) = emergency_release(&device, &inputs, deadline).await {
                    tracing::warn!("C64U keyboard cancellation cleanup failed: {error:#}");
                }
            });
        }
    }
}

/// Caller must serialize the entire operation against other machine operations.
pub async fn type_text(
    device: &Device,
    text: &str,
    hold_ms: u64,
    gap_ms: u64,
    operation_guard: Option<Arc<tokio::sync::OwnedMutexGuard<()>>>,
) -> Result<Value> {
    let chords = text_to_keys(text)?;
    ensure!(
        (1..=1000).contains(&hold_ms),
        "hold_ms must be between 1 and 1000"
    );
    ensure!(
        (1..=1000).contains(&gap_ms),
        "gap_ms must be between 1 and 1000"
    );
    let mut guard = KeyReleaseGuard {
        device: device.clone(),
        active: None,
        operation_guard,
        cleanup_deadline: None,
    };
    let mut completed = 0;
    for chord in &chords {
        // Mark before sending: even a failed response may have applied the press.
        guard.active = Some(chord.clone());
        let result = async {
            send_chord(device, chord, Transition::Press).await?;
            tokio::time::sleep(Duration::from_millis(hold_ms)).await;
            guard.release().await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            let cleanup = guard.cleanup().await;
            // Do not retry a failed explicit cleanup again in Drop.
            guard.active = None;
            let cleanup_detail = match cleanup {
                Ok(()) => "key release cleanup completed".to_string(),
                Err(cleanup_error) => format!("key release cleanup failed: {cleanup_error:#}"),
            };
            return Err(anyhow!(
                "typing stopped after {completed}/{} characters; current character may have been applied: {error:#}; {cleanup_detail}",
                chords.len()
            ));
        }
        completed += 1;
        tokio::time::sleep(Duration::from_millis(gap_ms)).await;
    }
    Ok(
        json!({ "characters_typed": completed, "hold_ms": hold_ms, "gap_ms": gap_ms,
        "letter_case": "ASCII letters use unshifted C64 keys; appearance depends on the active character set" }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyboard(keys: &[&str], transition: Transition) -> InputEvent {
        InputEvent::Keyboard {
            inputs: keys.iter().map(|s| (*s).into()).collect(),
            transition,
        }
    }

    #[test]
    fn batch_validation_matches_wire_contract() {
        let body = validate_events(&[
            keyboard(&["left_shift", "a"], Transition::Tap),
            InputEvent::Joystick {
                port: 2,
                inputs: vec!["up".into(), "fire".into()],
                transition: Transition::Press,
            },
            InputEvent::ReleaseAll {},
        ])
        .unwrap();
        assert_eq!(
            body["events"][0],
            json!({"kind":"keyboard", "inputs":["left_shift","a"], "transition":"tap"})
        );
        assert_eq!(body["events"][2], json!({"kind":"release_all"}));
    }

    #[test]
    fn batch_count_and_byte_limit_are_independent() {
        assert!(validate_events(&[]).is_err());
        assert!(validate_events(&vec![InputEvent::ReleaseAll {}; 64]).is_ok());
        assert!(validate_events(&vec![InputEvent::ReleaseAll {}; 65]).is_err());
        let big = keyboard(
            &[
                "cursor_left_right",
                "cursor_up_down",
                "left_shift",
                "right_shift",
            ],
            Transition::Tap,
        );
        assert!(
            validate_events(&vec![big; 64])
                .unwrap_err()
                .to_string()
                .contains("4096")
        );
    }

    #[test]
    fn validates_keys_ports_duplicates_and_restore() {
        for event in [
            keyboard(&[], Transition::Tap),
            keyboard(&["a", "a"], Transition::Press),
            keyboard(&["A"], Transition::Tap),
            keyboard(&["not_a_key"], Transition::Tap),
            keyboard(&["restore"], Transition::Press),
            keyboard(&["restore", "a"], Transition::Tap),
            InputEvent::Joystick {
                port: 0,
                inputs: vec!["up".into()],
                transition: Transition::Tap,
            },
            InputEvent::Joystick {
                port: 1,
                inputs: vec!["up", "up"].into_iter().map(String::from).collect(),
                transition: Transition::Tap,
            },
        ] {
            assert!(validate_events(&[event]).is_err());
        }
        assert!(validate_events(&[keyboard(&["restore"], Transition::Tap)]).is_ok());
        assert!(
            serde_json::from_value::<InputEvent>(json!({"kind":"release_all", "inputs":["a"]}))
                .is_err()
        );
    }

    #[test]
    fn text_mapping_preserves_c64_chords_and_normalizes_newlines() {
        assert_eq!(
            text_to_keys("Aa\r\n\n").unwrap(),
            vec![vec!["a"], vec!["a"], vec!["return"], vec!["return"]]
        );
        assert_eq!(
            text_to_keys("\"?[").unwrap(),
            vec![
                vec!["left_shift", "2"],
                vec!["left_shift", "slash"],
                vec!["left_shift", "colon"]
            ]
        );
        assert!(text_to_keys("PRINT \"HI\"\n").is_ok());
        assert!(text_to_keys("hello🙂").is_err());
        assert!(text_to_keys("\\").is_err());
        assert!(text_to_keys(&"a".repeat(1001)).is_err());
        assert_eq!(text_to_keys("").unwrap().len(), 0);
    }

    #[tokio::test]
    async fn typing_sends_sequential_press_release_and_validates_before_network() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/machine:input"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"errors":[]})))
            .expect(4)
            .mount(&server)
            .await;
        let device = Device::new(Some(&server.uri()), None, Duration::from_secs(2)).unwrap();
        assert!(type_text(&device, "A🙂", 1, 1, None).await.is_err());
        let result = type_text(&device, "a?", 1, 1, None).await.unwrap();
        assert_eq!(result["characters_typed"], 2);
        let requests = server.received_requests().await.unwrap();
        let events: Vec<Value> = requests
            .iter()
            .map(|r| r.body_json::<Value>().unwrap()["events"][0].clone())
            .collect();
        assert_eq!(
            events
                .iter()
                .map(|e| e["transition"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["press", "release", "press", "release"]
        );
        assert_eq!(events[0]["inputs"], json!(["a"]));
        assert_eq!(events[2]["inputs"], json!(["left_shift", "slash"]));
    }

    #[tokio::test]
    async fn typing_failure_reports_progress_and_releases_uncertain_press() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(|request: &wiremock::Request| {
                let event = &request.body_json::<Value>().unwrap()["events"][0];
                if event["inputs"] == json!(["b"]) && event["transition"] == "press" {
                    ResponseTemplate::new(500).set_body_json(json!({"errors":["injected failure"]}))
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({"errors":[]}))
                }
            })
            .expect(4)
            .mount(&server)
            .await;
        let device = Device::new(Some(&server.uri()), None, Duration::from_secs(2)).unwrap();
        let error = type_text(&device, "abc", 1, 1, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("1/3 characters"));
        assert!(error.contains("key release cleanup completed"));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.last().unwrap().body_json::<Value>().unwrap()["events"][0],
            json!({"kind":"keyboard", "inputs":["b"], "transition":"release"})
        );
    }

    #[tokio::test]
    async fn cancellation_releases_keys_while_retaining_operation_lock() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
        let server = MockServer::start().await;
        let pressed = Arc::new(tokio::sync::Notify::new());
        let released = Arc::new(tokio::sync::Notify::new());
        let press_signal = pressed.clone();
        let release_signal = released.clone();
        Mock::given(method("POST"))
            .respond_with(move |request: &wiremock::Request| {
                let body = request.body_json::<Value>().unwrap();
                let response = ResponseTemplate::new(200).set_body_json(json!({"errors":[]}));
                if body["events"][0]["transition"] == "press" {
                    press_signal.notify_one();
                    response
                } else {
                    release_signal.notify_one();
                    response.set_delay(Duration::from_millis(80))
                }
            })
            .expect(2)
            .mount(&server)
            .await;
        let device = Device::new(Some(&server.uri()), None, Duration::from_secs(2)).unwrap();
        let operation_lock = Arc::new(tokio::sync::Mutex::new(()));
        let guard = Arc::new(operation_lock.clone().lock_owned().await);
        let typing =
            tokio::spawn(async move { type_text(&device, "ab", 1000, 1, Some(guard)).await });
        tokio::time::timeout(Duration::from_secs(2), pressed.notified())
            .await
            .unwrap();
        typing.abort();
        assert!(typing.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), released.notified())
            .await
            .unwrap();
        assert!(operation_lock.try_lock().is_err());
        let _guard = tokio::time::timeout(Duration::from_secs(2), operation_lock.lock())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn emergency_cleanup_failure_preserves_both_errors_and_does_not_retry() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(|request: &wiremock::Request| {
                let body = request.body_json::<Value>().unwrap();
                let message = if body["events"][0]["transition"] == "press" {
                    "press response failed"
                } else {
                    "release response failed"
                };
                ResponseTemplate::new(500).set_body_json(json!({"errors":[message]}))
            })
            .expect(2)
            .mount(&server)
            .await;
        let device = Device::new(Some(&server.uri()), None, Duration::from_secs(10)).unwrap();
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let guard = Arc::new(lock.clone().lock_owned().await);
        let error = type_text(&device, "A", 1, 1, Some(guard))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("0/1 characters"));
        assert!(error.contains("press response failed"));
        assert!(error.contains("key release cleanup failed"));
        assert!(error.contains("release response failed"));
        assert!(lock.try_lock().is_ok());
        server.verify().await;
    }

    #[tokio::test]
    async fn emergency_cleanup_timeout_releases_operation_lock() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(|request: &wiremock::Request| {
                let body = request.body_json::<Value>().unwrap();
                if body["events"][0]["transition"] == "press" {
                    ResponseTemplate::new(500).set_body_json(json!({"errors":["uncertain press"]}))
                } else {
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"errors":[]}))
                        .set_delay(Duration::from_secs(10))
                }
            })
            .expect(2)
            .mount(&server)
            .await;
        let device = Device::new(Some(&server.uri()), None, Duration::from_secs(15)).unwrap();
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let guard = Arc::new(lock.clone().lock_owned().await);
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            type_text(&device, "A", 1, 1, Some(guard)),
        )
        .await
        .expect("cleanup must finish before the HTTP timeout")
        .unwrap_err()
        .to_string();
        assert!(error.contains("3-second deadline"));
        assert!(error.contains("may remain pressed"));
        assert!(lock.try_lock().is_ok());
        server.verify().await;
    }

    #[tokio::test]
    async fn cancellation_during_emergency_cleanup_keeps_lock_until_cleanup_deadline() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
        let server = MockServer::start().await;
        let cleanup_started = Arc::new(tokio::sync::Notify::new());
        let signal = cleanup_started.clone();
        Mock::given(method("POST"))
            .respond_with(move |request: &wiremock::Request| {
                let body = request.body_json::<Value>().unwrap();
                if body["events"][0]["transition"] == "press" {
                    ResponseTemplate::new(500).set_body_json(json!({"errors":["uncertain press"]}))
                } else {
                    signal.notify_one();
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"errors":[]}))
                        .set_delay(Duration::from_secs(10))
                }
            })
            .expect(3)
            .mount(&server)
            .await;
        let device = Device::new(Some(&server.uri()), None, Duration::from_secs(15)).unwrap();
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let guard = Arc::new(lock.clone().lock_owned().await);
        let typing = tokio::spawn(async move { type_text(&device, "A", 1, 1, Some(guard)).await });
        tokio::time::timeout(Duration::from_secs(2), cleanup_started.notified())
            .await
            .unwrap();
        typing.abort();
        assert!(typing.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), cleanup_started.notified())
            .await
            .unwrap();
        assert!(
            lock.try_lock().is_err(),
            "cancelled cleanup must retain the operation lock"
        );
        let _guard = tokio::time::timeout(Duration::from_secs(5), lock.lock())
            .await
            .expect("emergency cleanup must release the operation lock after its deadline");
        server.verify().await;
    }
}
