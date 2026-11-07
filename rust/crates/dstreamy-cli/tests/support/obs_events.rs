use serde_json::Value;

pub fn event(line: &str) -> Option<Value> {
    line.starts_with('{')
        .then(|| serde_json::from_str(line).expect("malformed harness event"))
}

pub fn field(line: &str, name: &str) -> Option<String> {
    let value = event(line)?.get(name)?.clone();
    Some(
        value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_owned),
    )
}

fn selector(text: &str) -> (String, Vec<(String, String)>) {
    let text = text.strip_prefix("HARNESS ").unwrap_or(text);
    for (prefix, event, key) in [
        ("expected_error code=", "expected_error", "code"),
        ("output stopped with code ", "output_stop", "code"),
    ] {
        if let Some(rest) = text.strip_prefix(prefix) {
            let (code, message) = rest.split_once(": ").unwrap_or((rest, ""));
            let mut fields = vec![(key.into(), code.trim().into())];
            if !message.is_empty() {
                fields.push(("message".into(), message.into()));
            }
            return (event.into(), fields);
        }
    }
    for (prefix, event) in [
        ("expected_error start: ", "expected_error_start"),
        ("frontend opened ", "frontend_opened"),
        ("frontend held ", "frontend_held"),
        ("frontend closed ", "frontend_closed"),
        ("tools output_stopped ", "tools_output_stopped"),
        ("tools output=", "tools_output"),
        ("tools stopped ", "tools_stopped"),
        ("tools video=", "tools_stats"),
        ("tools exiting ", "tools_exiting"),
        ("obs_shutdown begins ", "shutdown_begin"),
    ] {
        if let Some(rest) = text.strip_prefix(prefix) {
            if event == "expected_error_start" {
                return (event.into(), vec![("message".into(), rest.into())]);
            }
            let rest = if prefix.ends_with('=') {
                format!(
                    "{}={rest}",
                    prefix
                        .split_whitespace()
                        .last()
                        .unwrap()
                        .trim_end_matches('=')
                )
            } else {
                rest.to_owned()
            };
            return (event.into(), fields(&rest));
        }
    }
    for (prefix, event) in [
        ("tools waiting for the end", "tools_waiting_end"),
        ("restarting the output", "output_restarting"),
        ("frontend opened none", "frontend_opened_none"),
        ("alive after the stop: ", "alive"),
    ] {
        if text.starts_with(prefix) {
            return (event.into(), Vec::new());
        }
    }
    let (head, rest) = text.split_once(' ').unwrap_or((text, ""));
    if let Some((key, value)) = head.split_once('=') {
        let event = if key == "stop_signals" {
            "stop_signals"
        } else {
            key
        };
        let mut values = vec![(
            if key == "result" { "value" } else { key }.into(),
            value.into(),
        )];
        values.extend(fields(rest));
        return (event.into(), values);
    }
    (head.into(), fields(rest))
}

fn fields(text: &str) -> Vec<(String, String)> {
    let bytes = text.as_bytes();
    let mut positions = Vec::new();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'=' {
            continue;
        }
        let mut start = index;
        while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
            start -= 1;
        }
        if start < index && (start == 0 || bytes[start - 1] == b' ') {
            positions.push((start, index));
        }
    }
    positions
        .iter()
        .enumerate()
        .map(|(index, &(start, equals))| {
            let end = positions
                .get(index + 1)
                .map_or(text.len(), |p| p.0.saturating_sub(1));
            (
                text[start..equals].to_owned(),
                text[equals + 1..end].trim_end().to_owned(),
            )
        })
        .collect()
}

pub fn event_matches(line: &str, expected: &str, prefix: bool) -> bool {
    if !expected.starts_with("HARNESS ") {
        return if prefix {
            line.starts_with(expected)
        } else {
            line == expected
        };
    }
    let Some(row) = event(line) else { return false };
    let (name, fields) = selector(expected);
    let Some(actual) = row["event"].as_str() else {
        return false;
    };
    if actual != name && !(prefix && fields.is_empty() && actual.starts_with(&name)) {
        return false;
    }
    let expected_selector_ends_at_field = expected.ends_with(char::is_whitespace);
    fields.iter().enumerate().all(|(index, (key, expected))| {
        let Some(value) = row.get(key) else {
            return false;
        };
        let value = value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_owned);
        if expected.is_empty() && prefix {
            return true;
        }
        let equal = value == *expected
            || expected
                .strip_prefix("0x")
                .and_then(|hex| u64::from_str_radix(hex, 16).ok())
                .zip(value.parse::<u64>().ok())
                .is_some_and(|(a, b)| a == b);
        equal
            || (prefix
                && !expected_selector_ends_at_field
                && index + 1 == fields.len()
                && value.starts_with(expected))
    })
}

pub fn line_starts(line: &str, expected: &str) -> bool {
    event_matches(line, expected, true)
}

pub fn has_event(lines: &[String], expected: &str) -> bool {
    lines
        .iter()
        .any(|line| event_matches(line, expected, false))
}

#[test]
fn fields_are_compared_independently_of_report_wording() {
    let row = r#"{"event":"settings_property","when":"open","name":"start","type":8,"enabled":1,"desc":"Start"}"#;
    assert!(event_matches(
        row,
        "HARNESS settings_property when=open name=start type=8 enabled=1 desc=Start",
        false
    ));
    assert!(!event_matches(
        row,
        "HARNESS settings_property when=open name=start type=8 enabled=0 desc=Start",
        false
    ));
    assert!(line_starts(
        row,
        "HARNESS settings_property when=open name=sta"
    ));
    assert!(!event_matches(
        "HARNESS result=PASS",
        "HARNESS result=PASS",
        false
    ));
}
