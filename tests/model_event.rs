//! Event envelope contract tests — spec §1 (docs/architecture/07-event-protocol.md).

use tendr::model::event::{Event, EventTimestamp, Kind, Uuid7};
use tendr::model::ids::{Namespace, RunId, SessionName, Source};

// --- EventTimestamp: RFC 3339 UTC, exactly 6 fractional digits, Z ---

#[test]
fn timestamp_formats_epoch_zero() {
    let ts = EventTimestamp::from_parts(0, 0);
    assert_eq!(ts.to_string(), "1970-01-01T00:00:00.000000Z");
}

#[test]
fn timestamp_formats_known_epoch() {
    // Verified against `date -u -r 1000000000`
    let ts = EventTimestamp::from_parts(1_000_000_000, 0);
    assert_eq!(ts.to_string(), "2001-09-09T01:46:40.000000Z");
}

#[test]
fn timestamp_formats_leap_day() {
    // Verified against `date -u -r 951782400`
    let ts = EventTimestamp::from_parts(951_782_400, 0);
    assert_eq!(ts.to_string(), "2000-02-29T00:00:00.000000Z");
}

#[test]
fn timestamp_formats_spec_example() {
    // Spec worked example (a): 2026-07-06T03:14:15.926535Z = epoch 1783307655 + 926535µs
    let ts = EventTimestamp::from_parts(1_783_307_655, 926_535);
    assert_eq!(ts.to_string(), "2026-07-06T03:14:15.926535Z");
}

#[test]
fn timestamp_parses_and_round_trips() {
    let s = "2026-07-06T03:14:15.926535Z";
    let ts: EventTimestamp = s.parse().unwrap();
    assert_eq!(ts.to_string(), s);
    assert_eq!(ts.epoch_secs(), 1_783_307_655);
}

#[test]
fn timestamp_always_six_fractional_digits() {
    let ts = EventTimestamp::from_parts(1_000_000_000, 42);
    assert_eq!(ts.to_string(), "2001-09-09T01:46:40.000042Z");
}

#[test]
fn timestamp_rejects_malformed() {
    assert!("2026-07-06T03:14:15Z".parse::<EventTimestamp>().is_err()); // no micros
    assert!(
        "2026-07-06T03:14:15.926535+00:00"
            .parse::<EventTimestamp>()
            .is_err()
    ); // offset form
    assert!(
        "2026-07-06 03:14:15.926535Z"
            .parse::<EventTimestamp>()
            .is_err()
    ); // space sep
    assert!("garbage".parse::<EventTimestamp>().is_err());
}

#[test]
fn timestamp_lexicographic_order_is_chronological() {
    let a = EventTimestamp::from_parts(999_999_999, 999_999);
    let b = EventTimestamp::from_parts(1_000_000_000, 0);
    assert!(a < b);
    assert!(a.to_string() < b.to_string());
}

#[test]
fn timestamp_serde_is_string() {
    let ts = EventTimestamp::from_parts(1_000_000_000, 42);
    let json = serde_json::to_string(&ts).unwrap();
    assert_eq!(json, "\"2001-09-09T01:46:40.000042Z\"");
    let back: EventTimestamp = serde_json::from_str(&json).unwrap();
    assert_eq!(back, ts);
}

// --- Uuid7 ---

#[test]
fn uuid7_new_is_v7() {
    let id = Uuid7::new();
    assert_eq!(id.as_uuid().get_version_num(), 7);
}

#[test]
fn uuid7_deserialize_rejects_v4() {
    let json = "\"01981f2e-9a3b-4c1d-8e4f-0a1b2c3d4e5f\""; // version nibble = 4
    let result: Result<Uuid7, _> = serde_json::from_str(json);
    assert!(result.is_err());
}

#[test]
fn uuid7_parse_from_str() {
    let id = Uuid7::new();
    let parsed: Uuid7 = id.to_string().parse().unwrap();
    assert_eq!(parsed, id);
    assert!(
        "01981f2e-9a3b-4c1d-8e4f-0a1b2c3d4e5f"
            .parse::<Uuid7>()
            .is_err()
    ); // v4
}

// --- Kind: Source grammar; reserved prefixes only for user-supplied kinds ---

#[test]
fn kind_accepts_valid_dotted_strings() {
    assert!(Kind::new("run.exited").is_ok());
    // Spec worked example (b) and the plan's canonical validation example use
    // underscore kinds (hook.post_tool_use), so Kind grammar = Source grammar + '_'.
    assert!(Kind::new("hook.post_tool_use").is_ok());
}

#[test]
fn kind_grammar_matches_source_grammar_plus_underscore() {
    // Shipped Source grammar: ASCII alphanumeric, '.', '-'; at least one dot;
    // no empty segments; ≤128 bytes. Kind additionally allows '_'.
    assert!(Kind::new("build.finished").is_ok());
    assert!(Kind::new("hook.post-tool-use").is_ok());
    assert!(Kind::new("nodot").is_err());
    assert!(Kind::new("has space.x").is_err());
    assert!(Kind::new(".leading").is_err());
    assert!(Kind::new("trailing.").is_err());
    assert!(Kind::new("double..dot").is_err());
    assert!(Kind::new("").is_err());
    let long = format!("{}.x", "a".repeat(130));
    assert!(Kind::new(&long).is_err());
}

#[test]
fn kind_internal_constructor_allows_reserved_prefixes() {
    // Internal call sites (sidecar lifecycle, exec, rotation) write reserved kinds.
    assert!(Kind::new("run.exited").is_ok());
    assert!(Kind::new("segment.opened").is_ok());
    assert!(Kind::new("tendr.internal").is_ok());
}

#[test]
fn kind_user_constructor_rejects_reserved_prefixes() {
    for reserved in [
        "run.x",
        "log.x",
        "exec.x",
        "session.x",
        "pty.x",
        "callback.x",
        "segment.x",
        "cursor.x",
        "tendr.x",
    ] {
        assert!(
            Kind::new_user(reserved).is_err(),
            "user kind {reserved} must be rejected"
        );
    }
}

#[test]
fn kind_user_constructor_allows_hook_prefix() {
    // `hook.` is deliberately unreserved (spec §1).
    assert!(Kind::new_user("hook.post-tool-use").is_ok());
    assert!(Kind::new_user("build.finished").is_ok());
    // Prefix match is on the dotted prefix, not substring: "runner.x" is fine.
    assert!(Kind::new_user("runner.x").is_ok());
}

// --- Source: reservation enforced at user-input boundary only ---

#[test]
fn source_new_still_rejects_tendr_prefix() {
    assert!(Source::new("tendr.sidecar").is_err());
}

#[test]
fn source_trusted_allows_tendr_prefix() {
    let s = Source::trusted("tendr.sidecar").unwrap();
    assert_eq!(s.as_str(), "tendr.sidecar");
    // Grammar still enforced for trusted sources.
    assert!(Source::trusted("nodot").is_err());
}

#[test]
fn source_deserialize_accepts_tendr_prefix() {
    // Events written by the sidecar carry source "tendr.sidecar" and must round-trip.
    let s: Source = serde_json::from_str("\"tendr.sidecar\"").unwrap();
    assert_eq!(s.as_str(), "tendr.sidecar");
    // Grammar violations still rejected on deserialize.
    assert!(serde_json::from_str::<Source>("\"nodot\"").is_err());
}

// --- Event envelope ---

fn spec_example_a() -> &'static str {
    r#"{"v":1,"id":"01981f2e-9a3b-7c1d-8e4f-0a1b2c3d4e5f","ts":"2026-07-06T03:14:15.926535Z","kind":"run.exited","namespace":"default","session":"build","run_id":"01981f2d-1111-7abc-9def-556677889900","gen":3,"writer":"01981f2d-1111-7abc-9def-556677889900","seq":7,"source":"tendr.sidecar","data":{"status":"Exited","reason":"ExitedError","exit_code":3,"provenance":"direct"}}"#
}

#[test]
fn event_deserializes_spec_example() {
    let event: Event = serde_json::from_str(spec_example_a()).unwrap();
    assert_eq!(event.v, 1);
    assert_eq!(event.kind.as_str(), "run.exited");
    assert_eq!(event.namespace.as_str(), "default");
    assert_eq!(event.session.as_str(), "build");
    assert_eq!(event.generation, Some(3));
    assert_eq!(event.seq, 7);
    assert_eq!(event.source.as_str(), "tendr.sidecar");
    assert!(event.block_id.is_none());
    assert!(event.parent_id.is_none());
    assert!(event.data_ref.is_none());
    assert!(event.truncated.is_none());
    assert_eq!(event.data.as_ref().unwrap()["exit_code"], 3);
}

#[test]
fn event_round_trips_spec_example() {
    // Serialize(Deserialize(x)) must be semantically identical to x.
    let event: Event = serde_json::from_str(spec_example_a()).unwrap();
    let value = serde_json::to_value(&event).unwrap();
    let expected: serde_json::Value = serde_json::from_str(spec_example_a()).unwrap();
    assert_eq!(value, expected);
}

#[test]
fn event_optional_fields_omitted_when_none() {
    let event: Event = serde_json::from_str(spec_example_a()).unwrap();
    let json = serde_json::to_string(&event).unwrap();
    assert!(!json.contains("block_id"));
    assert!(!json.contains("parent_id"));
    assert!(!json.contains("data_ref"));
    assert!(!json.contains("truncated"));
}

#[test]
fn event_ignores_unknown_fields() {
    // Consumers MUST ignore unknown fields (spec §1).
    let mut value: serde_json::Value = serde_json::from_str(spec_example_a()).unwrap();
    value["future_field"] = serde_json::json!({"nested": true});
    let event: Result<Event, _> = serde_json::from_value(value);
    assert!(event.is_ok());
}

#[test]
fn event_rejects_wrong_version() {
    let mut value: serde_json::Value = serde_json::from_str(spec_example_a()).unwrap();
    value["v"] = serde_json::json!(2);
    let event: Result<Event, _> = serde_json::from_value(value);
    assert!(event.is_err());
}

#[test]
fn event_rejects_non_v7_id() {
    let mut value: serde_json::Value = serde_json::from_str(spec_example_a()).unwrap();
    value["id"] = serde_json::json!("6fa459ea-ee8a-3ca4-894e-db77e160355e"); // v3
    let event: Result<Event, _> = serde_json::from_value(value);
    assert!(event.is_err());
}

#[test]
fn event_constructs_with_builder_fields() {
    let event = Event {
        v: 1,
        id: Uuid7::new(),
        ts: EventTimestamp::from_parts(1_783_307_655, 926_535),
        kind: Kind::new("hook.post-tool-use").unwrap(),
        namespace: Namespace::new("default").unwrap(),
        session: SessionName::new("agent").unwrap(),
        run_id: RunId::new(),
        generation: None,
        writer: Uuid7::new(),
        seq: 1,
        source: Source::new("claude.hook").unwrap(),
        block_id: None,
        parent_id: None,
        data: Some(serde_json::json!({"ok": true})),
        data_ref: None,
        truncated: None,
    };
    let json = serde_json::to_string(&event).unwrap();
    let back: Event = serde_json::from_str(&json).unwrap();
    assert_eq!(back.kind.as_str(), "hook.post-tool-use");
    assert_eq!(back.seq, 1);
}
