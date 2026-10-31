//! Boundary descriptor model — parsing (`kind:label`), serde round-trip,
//! and the `BoundaryContext` ancestry shape. See docs/guide.md
//! ("Record where a session runs").

use tendr::model::boundary::{Boundary, BoundaryContext, BoundaryKind};

#[test]
fn parses_kind_and_label() {
    let b: Boundary = "host:data-box".parse().unwrap();
    assert_eq!(b.kind, BoundaryKind::Host);
    assert_eq!(b.label, "data-box");
}

#[test]
fn parses_all_known_kinds() {
    assert_eq!(
        "container:x".parse::<Boundary>().unwrap().kind,
        BoundaryKind::Container
    );
    assert_eq!("vm:x".parse::<Boundary>().unwrap().kind, BoundaryKind::Vm);
    assert_eq!("pod:x".parse::<Boundary>().unwrap().kind, BoundaryKind::Pod);
}

#[test]
fn label_may_contain_colons() {
    // Only the first colon separates kind from label — image tags survive.
    let b: Boundary = "container:my-image:latest".parse().unwrap();
    assert_eq!(b.kind, BoundaryKind::Container);
    assert_eq!(b.label, "my-image:latest");
}

#[test]
fn rejects_missing_colon() {
    assert!("host".parse::<Boundary>().is_err());
}

#[test]
fn rejects_empty_label() {
    assert!("host:".parse::<Boundary>().is_err());
}

#[test]
fn rejects_unknown_kind() {
    assert!("wormhole:x".parse::<Boundary>().is_err());
}

#[test]
fn display_round_trips_through_parse() {
    for s in [
        "host:data-box",
        "container:my-image:latest",
        "vm:builder",
        "pod:web-7",
    ] {
        let b: Boundary = s.parse().unwrap();
        assert_eq!(b.to_string(), s, "Display must reconstruct the parse input");
        // and Display output re-parses to the same value
        assert_eq!(b.to_string().parse::<Boundary>().unwrap(), b);
    }
}

#[test]
fn kind_serializes_snake_case() {
    let b = Boundary {
        kind: BoundaryKind::Host,
        label: "data-box".to_owned(),
    };
    let json = serde_json::to_string(&b).unwrap();
    assert_eq!(json, r#"{"kind":"host","label":"data-box"}"#);
}

#[test]
fn context_round_trips_through_json() {
    let ctx = BoundaryContext {
        current: Boundary {
            kind: BoundaryKind::Container,
            label: "my-image:latest".to_owned(),
        },
        parents: vec![Boundary {
            kind: BoundaryKind::Host,
            label: "data-box".to_owned(),
        }],
    };
    let json = serde_json::to_string(&ctx).unwrap();
    let back: BoundaryContext = serde_json::from_str(&json).unwrap();
    assert_eq!(back, ctx);
}

#[test]
fn context_with_no_parents_serializes_empty_vec() {
    let ctx = BoundaryContext {
        current: Boundary {
            kind: BoundaryKind::Host,
            label: "solo".to_owned(),
        },
        parents: vec![],
    };
    let json = serde_json::to_string(&ctx).unwrap();
    assert_eq!(
        json,
        r#"{"current":{"kind":"host","label":"solo"},"parents":[]}"#
    );
}
