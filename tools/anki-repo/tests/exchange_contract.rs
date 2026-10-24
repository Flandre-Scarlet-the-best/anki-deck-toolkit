//! Все экспорты и служебное состояние синтетические, во временном каталоге вне рабочей копии.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

use anki_repo::exchange::{
    Confirmation, DeckIdentity, Disposition, ExchangeError, ExchangeStore, PreparationAction,
    ProfileId, Snapshot, compare, export_name,
};
use serde_json::{Value, json};

use crate::common::{TempDir, base_export};

fn profile() -> ProfileId {
    ProfileId::new("synthetic-profile").unwrap()
}
fn media() -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([("a.mp3".into(), b"audio-one".to_vec())])
}
fn snapshot(value: &Value) -> Snapshot {
    snapshot_media(value, &media())
}
fn snapshot_media(value: &Value, media: &BTreeMap<String, Vec<u8>>) -> Snapshot {
    Snapshot::from_bytes(profile(), &serde_json::to_vec(value).unwrap(), media).unwrap()
}
fn confirm(snapshot: Snapshot, sequence: u64) -> Confirmation {
    Confirmation::from_trusted_owner(
        snapshot,
        sequence,
        format!("synthetic-confirmation-{sequence}"),
    )
    .unwrap()
}

struct Fixture {
    temp: TempDir,
    store: ExchangeStore,
    identity: DeckIdentity,
}
impl Fixture {
    fn new() -> Self {
        Self::with_export(&base_export())
    }
    fn with_export(value: &Value) -> Self {
        let temp = TempDir::new("exchange");
        let store = ExchangeStore::initialize(temp.path()).unwrap();
        let initial = snapshot(value);
        let identity = initial.identity().clone();
        store
            .record_input(&store.read().unwrap().token, confirm(initial, 1))
            .unwrap();
        Self {
            temp,
            store,
            identity,
        }
    }
    fn output(&self) -> PathBuf {
        self.temp
            .path()
            .join("decks/output")
            .join(&self.store.read().unwrap().streams[&self.identity.key()].output_directory)
    }
    fn write(&self, value: &Value) {
        write_export(&self.output(), value, &media());
    }
    fn prepare(&self, value: &Value) {
        let plan = self.store.plan_preparation(&self.identity).unwrap();
        assert!(matches!(
            plan.action(),
            PreparationAction::PrepareFirst | PreparationAction::RefreshUnchanged
        ));
        self.write(value);
        self.store.record_prepared(&plan).unwrap();
    }
    fn input(&self, value: &Value, sequence: u64) {
        self.store
            .record_input(
                &self.store.read().unwrap().token,
                confirm(snapshot(value), sequence),
            )
            .unwrap();
    }
    fn state_file(&self) -> PathBuf {
        self.temp.path().join("decks/.exchange/revisions.json")
    }
}

fn write_export(directory: &Path, value: &Value, media: &BTreeMap<String, Vec<u8>>) {
    fs::create_dir_all(directory.join("media")).unwrap();
    fs::write(
        directory.join("deck.json"),
        serde_json::to_vec_pretty(value).unwrap(),
    )
    .unwrap();
    for (name, bytes) in media {
        fs::write(directory.join("media").join(name), bytes).unwrap();
    }
}

#[test]
fn formatting_collection_order_and_field_definitions_by_ord_are_semantic_noops() {
    let mut original = base_export();
    original["notes"][0]["tags"] = json!(["Б", "А"]);
    let before = snapshot(&original);
    let mut reordered = original.clone();
    reordered["notes"].as_array_mut().unwrap().reverse();
    reordered["notes"][1]["tags"] = json!(["а", "б", "А"]);
    reordered["note_models"][0]["flds"]
        .as_array_mut()
        .unwrap()
        .reverse();
    let compact = snapshot(&reordered);
    let pretty = Snapshot::from_bytes(
        profile(),
        &serde_json::to_vec_pretty(&reordered).unwrap(),
        &media(),
    )
    .unwrap();
    assert_eq!(before.revision(), compact.revision());
    assert_eq!(compact, pretty);
    assert_eq!(
        compare(&before, &compact, &pretty).unwrap().disposition,
        Disposition::Unchanged
    );
}

#[test]
fn all_two_field_change_combinations_classify_and_never_merge() {
    let a = snapshot(&base_export());
    // Исчерпывающий небольшой домен: две независимые правки, каждая имеет два
    // разных новых значения. Ожидание — по множествам фактически затронутых полей.
    for input_mask in 0..4 {
        for working_mask in 0..4 {
            for same_values in [true, false] {
                let mut input = base_export();
                let mut working = base_export();
                for field in 0..2 {
                    if input_mask & (1 << field) != 0 {
                        input["notes"][0]["fields"][field] = json!("input");
                    }
                    if working_mask & (1 << field) != 0 {
                        working["notes"][0]["fields"][field] =
                            json!(if same_values { "input" } else { "working" });
                    }
                }
                let input = snapshot(&input);
                let working = snapshot(&working);
                let report = compare(&a, &input, &working).unwrap();
                let expected = match (input_mask, working_mask) {
                    (0, 0) => Disposition::Unchanged,
                    (_, 0) => Disposition::InputOnly,
                    (0, _) => Disposition::WorkingOnly,
                    (c, b) if c == b && same_values => Disposition::Converged,
                    (c, b) if c & b != 0 && !same_values => Disposition::Conflict,
                    _ => Disposition::Diverged,
                };
                assert_eq!(
                    report.disposition, expected,
                    "{input_mask}/{working_mask}/{same_values}"
                );
                assert_eq!(report, compare(&a, &input, &working).unwrap());
                let reversed = compare(&a, &working, &input).unwrap();
                assert_eq!(report.differences.len(), reversed.differences.len());
                assert_eq!(
                    report.differences.iter().filter(|d| d.conflict).count(),
                    reversed.differences.iter().filter(|d| d.conflict).count()
                );
            }
        }
    }
}

#[test]
fn notes_deletions_moves_and_tags_are_addressed_by_guid() {
    let a = snapshot(&base_export());
    let mut deleted = base_export();
    deleted["notes"].as_array_mut().unwrap().remove(0);
    let c = snapshot(&deleted);
    let mut edited = base_export();
    edited["notes"][0]["fields"][1] = json!("правка удалённой заметки");
    let report = compare(&a, &c, &snapshot(&edited)).unwrap();
    assert_eq!(report.disposition, Disposition::Conflict);
    assert!(
        report
            .differences
            .iter()
            .any(|d| d.path == "/notes/guid-1/fields/1" && d.conflict)
    );
    edited = base_export();
    edited["notes"][1]["tags"] = json!(["новый"]);
    assert_eq!(
        compare(&a, &c, &snapshot(&edited)).unwrap().disposition,
        Disposition::Diverged
    );
    let report = compare(&a, &c, &a).unwrap();
    assert_eq!(report.disposition, Disposition::InputOnly);
    assert!(
        report
            .differences
            .iter()
            .any(|d| d.base.is_some() && d.input.is_none())
    );
}

#[test]
fn deletion_conflicts_with_note_moves_and_deck_reparenting_on_either_side() {
    let empty_deck = |id: &str, name: &str| {
        let mut node = base_export();
        node["crowdanki_uuid"] = json!(id);
        node["name"] = json!(name);
        for key in ["notes", "note_models", "deck_configurations", "media_files"] {
            node[key] = json!([]);
        }
        node
    };
    let mut original = base_export();
    let mut left = empty_deck("left", "Test::Left");
    left["children"] = json!([empty_deck("nested", "Test::Nested")]);
    original["children"] = json!([left, empty_deck("right", "Test::Right")]);
    let base = snapshot(&original);

    for move_note in [true, false] {
        let mut deleted = original.clone();
        let mut moved = original.clone();
        let expected_path = if move_note {
            deleted["notes"].as_array_mut().unwrap().remove(0);
            let note = moved["notes"].as_array_mut().unwrap().remove(0);
            moved["children"][0]["notes"] = json!([note]);
            "/membership/guid-1/deck"
        } else {
            deleted["children"][0]["children"] = json!([]);
            let child = moved["children"][0]["children"]
                .as_array_mut()
                .unwrap()
                .remove(0);
            moved["children"][1]["children"] = json!([child]);
            "/hierarchy/nested/parent"
        };
        let deleted = snapshot(&deleted);
        let moved = snapshot(&moved);
        for (input, working) in [(&deleted, &moved), (&moved, &deleted)] {
            let report = compare(&base, input, working).unwrap();
            assert_eq!(report.disposition, Disposition::Conflict, "{expected_path}");
            assert!(
                report
                    .differences
                    .iter()
                    .any(|difference| difference.path == expected_path && difference.conflict),
                "{expected_path}"
            );
        }
    }
}

#[test]
fn invalid_note_field_values_return_errors_before_snapshot_indexing() {
    for fields in [json!([]), json!(["one"]), json!([1, "two"])] {
        let mut value = base_export();
        value["notes"][0]["fields"] = fields;
        assert!(matches!(
            Snapshot::from_bytes(profile(), &serde_json::to_vec(&value).unwrap(), &media()),
            Err(ExchangeError::InvalidSnapshot(_))
        ));
    }
}

#[test]
fn deleting_a_parent_conflicts_with_new_dependents_on_either_side() {
    let mut original = base_export();
    original["children"] = json!([
        {"__type__":"Deck", "crowdanki_uuid":"parent/~", "name":"Test::Parent", "notes":[], "children":[]},
        {"__type__":"Deck", "crowdanki_uuid":"other", "name":"Test::Other", "notes":[], "children":[]}
    ]);
    let base = snapshot(&original);
    let mut deleted = original.clone();
    deleted["children"].as_array_mut().unwrap().remove(0);
    let removed = snapshot(&deleted);
    assert_eq!(
        compare(&base, &removed, &base).unwrap().disposition,
        Disposition::InputOnly
    );
    assert_eq!(
        compare(&base, &removed, &removed).unwrap().disposition,
        Disposition::Converged
    );

    for add_note in [true, false] {
        let mut added = original.clone();
        let mut independent = original.clone();
        let expected_path = if add_note {
            let mut note = original["notes"][0].clone();
            note["guid"] = json!("new-note");
            added["children"][0]["notes"] = json!([note.clone()]);
            independent["children"][1]["notes"] = json!([note]);
            "/membership/new-note/deck"
        } else {
            let child = json!({"__type__":"Deck", "crowdanki_uuid":"new-child", "name":"Test::New", "notes":[], "children":[]});
            added["children"][0]["children"] = json!([child.clone()]);
            independent["children"][1]["children"] = json!([child]);
            "/hierarchy/new-child/parent"
        };
        let added_snapshot = snapshot(&added);
        let independent_snapshot = snapshot(&independent);
        for (input, working) in [(&removed, &added_snapshot), (&added_snapshot, &removed)] {
            let report = compare(&base, input, working).unwrap();
            assert_eq!(report.disposition, Disposition::Conflict, "{expected_path}");
            assert!(
                report
                    .differences
                    .iter()
                    .any(|d| d.path == expected_path && d.conflict)
            );
        }
        for (input, working) in [
            (&removed, &independent_snapshot),
            (&independent_snapshot, &removed),
        ] {
            let report = compare(&base, input, working).unwrap();
            assert_eq!(report.disposition, Disposition::Diverged, "{expected_path}");
            assert!(report.differences.iter().all(|d| !d.conflict));
        }
        for (input, working) in [(&deleted, &added), (&added, &deleted)] {
            let f = Fixture::with_export(&original);
            f.prepare(&original);
            f.input(input, 2);
            f.write(working);
            let before = fs::read(f.state_file()).unwrap();
            let plan = f.store.plan_preparation(&f.identity).unwrap();
            assert_eq!(plan.action(), PreparationAction::Blocked);
            assert_eq!(
                plan.comparison.as_ref().unwrap().disposition,
                Disposition::Conflict
            );
            assert!(f.store.record_prepared(&plan).is_err());
            assert!(f.store.plan_publication(&f.identity).is_err());
            assert_eq!(fs::read(f.state_file()).unwrap(), before);
        }
    }
}

#[test]
fn physical_media_and_declarations_are_independent_units() {
    let original = base_export();
    let a = snapshot(&original);
    let mut bytes = media();
    bytes.insert("a.mp3".into(), b"audio-two".to_vec());
    let c = snapshot_media(&original, &bytes);
    let report = compare(&a, &c, &a).unwrap();
    assert_eq!(report.differences.len(), 1);
    assert_eq!(report.differences[0].path, "/media/a.mp3");
    bytes.insert("a.mp3".into(), b"audio-three".to_vec());
    assert_eq!(
        compare(&a, &c, &snapshot_media(&original, &bytes))
            .unwrap()
            .disposition,
        Disposition::Conflict
    );
    let missing = snapshot_media(&original, &BTreeMap::new());
    assert!(missing.unproven().contains("missing_media:a.mp3"));
    assert!(!compare(&a, &missing, &a).unwrap().proven_compatible());
    let mut removed = original;
    removed["media_files"] = json!([]);
    removed["notes"][0]["fields"][0] = json!("без звука");
    let removed = snapshot_media(&removed, &BTreeMap::new());
    assert!(removed.unproven().is_empty());
    let report = compare(&a, &removed, &a).unwrap();
    assert!(
        report
            .differences
            .iter()
            .any(|d| d.path.ends_with("/media_files"))
    );
    assert!(
        report
            .differences
            .iter()
            .any(|d| d.path == "/media/a.mp3" && d.input.is_none())
    );
    let mut extra = media();
    extra.insert("unlisted.bin".into(), vec![1]);
    assert_ne!(
        a.revision(),
        snapshot_media(&base_export(), &extra).revision()
    );
}

#[test]
fn models_templates_configs_and_unknown_attributes_are_not_lost() {
    let a = snapshot(&base_export());
    for pointer in [
        "/note_models/0/css",
        "/note_models/0/tmpls/0/qfmt",
        "/deck_configurations/0/name",
    ] {
        let mut changed = base_export();
        *changed.pointer_mut(pointer).unwrap() = json!("новое значение");
        let report = compare(&a, &snapshot(&changed), &a).unwrap();
        assert_eq!(report.disposition, Disposition::InputOnly, "{pointer}");
        assert_eq!(report.differences.len(), 1);
        assert!(report.proven_compatible(), "{pointer}");
    }
    let mut unknown = base_export();
    unknown["future_property"] = json!({"nested": 1});
    let report = compare(&a, &snapshot(&unknown), &a).unwrap();
    assert!(report.unproven.iter().any(|p| p.contains("opaque_change")));
    let mut layout = base_export();
    layout["note_models"][0]["flds"][0]["name"] = json!("Иное");
    layout["note_models"][0]["tmpls"][0]["qfmt"] = json!("{{Иное}}");
    let report = compare(&a, &snapshot(&layout), &a).unwrap();
    assert_eq!(report.incompatible, vec!["model_layout:model-1"]);
}

#[test]
fn nested_unknown_changes_are_unproven_and_block_exchange_transitions() {
    for (container, opaque) in [
        (
            "/note_models/0/tmpls/0",
            "/opaque/models/model-1/tmpls/0/future~1setting~0",
        ),
        (
            "/note_models/0/flds/0",
            "/opaque/models/model-1/flds/0/future~1setting~0",
        ),
        ("/note_models/0/req/0", "/opaque/models/model-1/req/0"),
        (
            "/note_models/0/flds/0/description",
            "/opaque/models/model-1/flds/0/description",
        ),
        ("/deck_configurations/0/new", "/opaque/configs/cfg-1/new"),
        ("/deck_configurations/0/rev", "/opaque/configs/cfg-1/rev"),
        (
            "/deck_configurations/0/lapse",
            "/opaque/configs/cfg-1/lapse",
        ),
    ] {
        let mut without = base_export();
        if container.starts_with("/deck_configurations/") {
            without["deck_configurations"][0][container.rsplit('/').next().unwrap()] =
                json!({"perDay": 20});
        } else if container.ends_with("/req/0") {
            without["note_models"][0]["req"] = json!([{}]);
        } else if container.ends_with("/description") {
            without["note_models"][0]["flds"][0]["description"] = json!({});
        }
        for (initial, updated) in [
            (json!("first"), json!("second")),
            (
                json!({"nested": [1, {"mode": "first"}]}),
                json!({"nested": [1, {"mode": "second"}]}),
            ),
        ] {
            let mut original = without.clone();
            original.pointer_mut(container).unwrap()["future/setting~"] = initial;
            let mut changed = original.clone();
            changed.pointer_mut(container).unwrap()["future/setting~"] = updated;
            // Добавление, изменение и удаление неизвестного свойства должны блокироваться одинаково.
            for (old, new) in [
                (&without, &original),
                (&original, &changed),
                (&original, &without),
            ] {
                let a = snapshot(old);
                let next = snapshot(new);
                assert_ne!(a.revision(), next.revision(), "{container}");
                for (input, working, disposition) in [
                    (&next, &a, Disposition::InputOnly),
                    (&a, &next, Disposition::WorkingOnly),
                    (&next, &next, Disposition::Converged),
                ] {
                    let report = compare(&a, input, working).unwrap();
                    assert_eq!(report.disposition, disposition, "{container}");
                    assert!(
                        report.unproven.contains(&format!("opaque_change:{opaque}")),
                        "{:?}",
                        report.unproven
                    );
                    assert!(!report.proven_compatible(), "{container}");
                }
                for incoming in [true, false] {
                    let f = Fixture::with_export(old);
                    f.prepare(old);
                    if incoming {
                        f.input(new, 2);
                    } else {
                        f.write(new);
                    }
                    let before = fs::read(f.state_file()).unwrap();
                    let working_bytes = fs::read(f.output().join("deck.json")).unwrap();
                    let plan = f.store.plan_preparation(&f.identity).unwrap();
                    assert_eq!(plan.action(), PreparationAction::Blocked, "{container}");
                    assert!(!plan.comparison.as_ref().unwrap().proven_compatible());
                    assert!(f.store.record_prepared(&plan).is_err());
                    assert!(matches!(
                        f.store.plan_publication(&f.identity),
                        Err(ExchangeError::Blocked(_))
                    ));
                    assert_eq!(fs::read(f.state_file()).unwrap(), before);
                    assert_eq!(
                        fs::read(f.output().join("deck.json")).unwrap(),
                        working_bytes
                    );
                }
            }
        }
    }
}

#[test]
fn unchanged_nested_unknown_properties_allow_known_edits_and_ord_reordering() {
    let mut original = base_export();
    original["note_models"][0]["flds"][0]["future_field"] = json!({"nested": 1});
    original["note_models"][0]["tmpls"][0]["future_rendering"] = json!({"nested": 1});
    let mut second_template = original["note_models"][0]["tmpls"][0].clone();
    second_template["ord"] = json!(1);
    second_template["future_rendering"] = json!({"nested": 2});
    original["note_models"][0]["tmpls"]
        .as_array_mut()
        .unwrap()
        .push(second_template);
    original["deck_configurations"][0]["new"] = json!({"future_setting": {"nested": 1}});
    let a = snapshot(&original);
    let mut reordered = original.clone();
    for key in ["flds", "tmpls"] {
        reordered["note_models"][0][key]
            .as_array_mut()
            .unwrap()
            .reverse();
    }
    let b = snapshot(&reordered);
    assert_eq!(a.revision(), b.revision());
    let report = compare(&a, &b, &a).unwrap();
    assert_eq!(report.disposition, Disposition::Unchanged);
    assert!(report.differences.is_empty());
    assert!(report.proven_compatible());

    let f = Fixture::with_export(&original);
    f.prepare(&original);
    assert_eq!(
        f.store.plan_preparation(&f.identity).unwrap().action(),
        PreparationAction::Keep
    );
    assert!(f.store.plan_publication(&f.identity).is_ok());
    reordered["notes"][0]["fields"][1] = json!("известная правка");
    f.input(&reordered, 2);
    assert_eq!(
        f.store.plan_preparation(&f.identity).unwrap().action(),
        PreparationAction::RefreshUnchanged
    );
    f.prepare(&reordered);
    reordered["notes"][1]["fields"][1] = json!("рабочая правка");
    f.write(&reordered);
    assert_eq!(
        f.store.plan_preparation(&f.identity).unwrap().action(),
        PreparationAction::Keep
    );
    assert!(f.store.plan_publication(&f.identity).is_ok());
}

#[test]
fn invalid_models_guids_deck_identities_and_paths_fail_closed() {
    for mutation in 0..7 {
        let mut value = base_export();
        match mutation {
            0 => value["notes"][0]["note_model_uuid"] = json!("missing"),
            1 => value["note_models"][0]["flds"][1]["ord"] = json!(0),
            2 => value["notes"][1]["guid"] = json!("guid-1"),
            3 => {
                value.as_object_mut().unwrap().remove("crowdanki_uuid");
            }
            4 => value["name"] = json!("../outside"),
            5 => value["media_files"] = json!(["../escape.bin"]),
            6 => value["note_models"][0]["tmpls"][0]["ord"] = json!(-1),
            _ => unreachable!(),
        }
        assert!(
            Snapshot::from_bytes(profile(), &serde_json::to_vec(&value).unwrap(), &media())
                .is_err(),
            "{mutation}"
        );
    }
}

#[test]
fn nested_decks_models_and_media_declarations_are_preserved() {
    let mut a = base_export();
    let mut child = base_export();
    child["crowdanki_uuid"] = json!("child-id");
    child["name"] = json!("Test::Deck::Child");
    child["notes"] = json!([]);
    child["note_models"] = json!([]);
    child["deck_configurations"] = json!([]);
    a["children"] = json!([child]);
    let base = snapshot(&a);
    let note = a["notes"].as_array_mut().unwrap().remove(0);
    a["children"][0]["notes"] = json!([note]);
    let moved = snapshot(&a);
    let report = compare(&base, &moved, &base).unwrap();
    assert_eq!(report.differences.len(), 1);
    assert_eq!(report.differences[0].path, "/membership/guid-1/deck");
    a["children"][0]["name"] = json!("Test::Deck::Renamed");
    assert_ne!(moved.revision(), snapshot(&a).revision());
    a["children"][0]["crowdanki_uuid"] = a["crowdanki_uuid"].clone();
    assert!(Snapshot::from_bytes(profile(), &serde_json::to_vec(&a).unwrap(), &media()).is_err());
}

#[test]
fn identity_is_profile_plus_uuid_and_normalization_never_conflates_streams() {
    assert_eq!(export_name("Parent::Child").unwrap(), "Parent__Child");
    assert_eq!(export_name("Parent__Child").unwrap(), "Parent__Child");
    let a = DeckIdentity::new(profile(), "id-a").unwrap();
    let b = DeckIdentity::new(profile(), "id-b").unwrap();
    assert_ne!(
        a.output_directory("Parent::Child").unwrap(),
        b.output_directory("Parent__Child").unwrap()
    );
    let other = DeckIdentity::new(ProfileId::new("other-profile").unwrap(), "id-a").unwrap();
    assert_ne!(
        a.output_directory("Same").unwrap(),
        other.output_directory("Same").unwrap()
    );
    assert_eq!(
        a.output_directory("é").unwrap(),
        a.output_directory("e\u{301}").unwrap()
    );
    for name in [
        "",
        "a::::b",
        "a/../b",
        "a\\b",
        "a::..",
        "a:: x",
        "a.",
        "a:b",
        "\n",
        "/absolute",
    ] {
        assert!(export_name(name).is_err(), "{name:?}");
    }
    assert!(a.output_directory(&"界".repeat(1000)).unwrap().len() <= 166);
    assert!(ProfileId::new(" ").is_err());
    let other_snapshot = Snapshot::from_bytes(
        other.profile,
        &serde_json::to_vec(&base_export()).unwrap(),
        &media(),
    )
    .unwrap();
    assert!(matches!(
        compare(
            &snapshot(&base_export()),
            &other_snapshot,
            &snapshot(&base_export())
        ),
        Err(ExchangeError::IdentityMismatch)
    ));
}

#[test]
fn v1_v2_v3_and_restart_preserve_base_and_do_not_overwrite_working_data() {
    let f = Fixture::new();
    let v1 = base_export();
    assert_eq!(
        f.store.plan_preparation(&f.identity).unwrap().action(),
        PreparationAction::PrepareFirst
    );
    f.prepare(&v1);
    let initial = f.store.read().unwrap();
    assert_eq!(
        initial.streams[&f.identity.key()]
            .base
            .as_ref()
            .unwrap()
            .source_sequence,
        1
    );
    let repeated = f
        .store
        .record_input(&initial.token, confirm(snapshot(&v1), 1))
        .unwrap();
    assert_eq!(initial.token, repeated.token);
    let mut v2 = v1.clone();
    v2["notes"][0]["fields"][1] = json!("V2");
    f.input(&v2, 2);
    assert_eq!(
        f.store.plan_preparation(&f.identity).unwrap().action(),
        PreparationAction::RefreshUnchanged
    );
    f.prepare(&v2);
    let mut b = v2.clone();
    b["notes"][1]["fields"][1] = json!("working");
    f.write(&b);
    assert_eq!(
        f.store.plan_preparation(&f.identity).unwrap().action(),
        PreparationAction::Keep
    );
    let mut v3 = v2;
    v3["notes"][0]["fields"][1] = json!("V3");
    f.input(&v3, 3);
    let bytes = fs::read(f.output().join("deck.json")).unwrap();
    let plan = f.store.plan_preparation(&f.identity).unwrap();
    assert_eq!(plan.action(), PreparationAction::Blocked);
    assert_eq!(plan.comparison.unwrap().disposition, Disposition::Diverged);
    assert!(f.store.plan_publication(&f.identity).is_err());
    let reopened = ExchangeStore::open(f.temp.path()).unwrap();
    let state = reopened.read().unwrap();
    assert_eq!(
        state.streams[&f.identity.key()]
            .base
            .as_ref()
            .unwrap()
            .source_sequence,
        2
    );
    assert_eq!(state.streams[&f.identity.key()].current.source_sequence, 3);
    assert_eq!(fs::read(f.output().join("deck.json")).unwrap(), bytes);
}

#[test]
fn preparation_rejects_existing_untracked_missing_and_changed_outputs() {
    let f = Fixture::new();
    f.write(&base_export());
    assert_eq!(
        f.store
            .plan_preparation(&f.identity)
            .unwrap()
            .reason
            .as_deref(),
        Some("untracked_output")
    );
    fs::remove_dir_all(f.output()).unwrap();
    let plan = f.store.plan_preparation(&f.identity).unwrap();
    let mut wrong = base_export();
    wrong["notes"][0]["fields"][1] = json!("не C");
    f.write(&wrong);
    assert!(f.store.record_prepared(&plan).is_err());
    f.write(&base_export());
    f.store.record_prepared(&plan).unwrap();
    fs::remove_dir_all(f.output()).unwrap();
    assert_eq!(
        f.store
            .plan_preparation(&f.identity)
            .unwrap()
            .reason
            .as_deref(),
        Some("working_missing")
    );
}

#[test]
fn publication_needs_fresh_base_unchanged_attempt_and_confirmed_readback() {
    let f = Fixture::new();
    f.prepare(&base_export());
    let mut b = base_export();
    b["notes"][0]["fields"][1] = json!("опубликовано");
    f.write(&b);
    let permit = f.store.plan_publication(&f.identity).unwrap();
    let before = f.store.read().unwrap();
    assert!(before.streams[&f.identity.key()].published.is_none());
    assert!(
        f.store
            .record_published(&permit, confirm(snapshot(&base_export()), 2))
            .is_err()
    );
    assert_eq!(f.store.read().unwrap().token, before.token);
    let result = f
        .store
        .record_published(&permit, confirm(snapshot(&b), 2))
        .unwrap();
    let stream = &result.streams[&f.identity.key()];
    assert_eq!(stream.base, Some(stream.current.clone()));
    assert_eq!(
        stream.published.as_ref().unwrap().working_revision,
        snapshot(&b).revision()
    );
    assert_eq!(stream.published.as_ref().unwrap().confirmed_sequence, 2);
    let next = f.store.plan_publication(&f.identity).unwrap();
    b["notes"][0]["fields"][1] = json!("новое изменение после разрешения публикации");
    f.write(&b);
    assert!(
        f.store
            .record_published(&next, confirm(snapshot(&b), 3))
            .is_err()
    );
    assert!(Confirmation::from_trusted_owner(snapshot(&b), 0, "no").is_err());
    assert!(Confirmation::from_trusted_owner(snapshot(&b), 3, "").is_err());
}

#[test]
fn renamed_deck_keeps_binding_and_another_profile_has_separate_state() {
    let f = Fixture::new();
    f.prepare(&base_export());
    let path = f.output();
    let mut renamed = base_export();
    renamed["name"] = json!("Renamed::Hierarchy");
    f.input(&renamed, 2);
    assert_eq!(f.output(), path);
    assert_eq!(
        f.store.plan_preparation(&f.identity).unwrap().action(),
        PreparationAction::RefreshUnchanged
    );
    f.prepare(&renamed);
    let other = Snapshot::from_bytes(
        ProfileId::new("other").unwrap(),
        &serde_json::to_vec(&renamed).unwrap(),
        &media(),
    )
    .unwrap();
    let other_id = other.identity().clone();
    f.store
        .record_input(&f.store.read().unwrap().token, confirm(other, 1))
        .unwrap();
    let state = f.store.read().unwrap();
    assert_eq!(state.streams.len(), 2);
    assert_ne!(
        state.streams[&f.identity.key()].output_directory,
        state.streams[&other_id.key()].output_directory
    );
}

#[test]
fn stale_sequence_and_cas_reject_without_changing_metadata() {
    let f = Fixture::new();
    let initial = f.store.read().unwrap();
    f.input(&base_export(), 3);
    let bytes = fs::read(f.state_file()).unwrap();
    assert!(matches!(
        f.store
            .record_input(&initial.token, confirm(snapshot(&base_export()), 4)),
        Err(ExchangeError::StaleState)
    ));
    assert!(matches!(
        f.store.record_input(
            &f.store.read().unwrap().token,
            confirm(snapshot(&base_export()), 2)
        ),
        Err(ExchangeError::StaleState)
    ));
    let mut changed = base_export();
    changed["name"] = json!("other");
    assert!(
        f.store
            .record_input(
                &f.store.read().unwrap().token,
                confirm(snapshot(&changed), 3)
            )
            .is_err()
    );
    assert_eq!(fs::read(f.state_file()).unwrap(), bytes);
}

#[test]
fn concurrent_calls_with_same_token_have_exactly_one_winner() {
    let f = Fixture::new();
    let token = f.store.read().unwrap().token;
    let barrier = Arc::new(Barrier::new(3));
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|n| {
                let barrier = barrier.clone();
                let token = token.clone();
                let root = f.temp.path();
                scope.spawn(move || {
                    let store = ExchangeStore::open(root).unwrap();
                    let mut value = base_export();
                    value["name"] = json!(format!("Name-{n}"));
                    barrier.wait();
                    store.record_input(&token, confirm(snapshot(&value), 2))
                })
            })
            .collect();
        barrier.wait();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Err(ExchangeError::StaleState)))
                .count(),
            1
        );
    });
    assert_eq!(
        ExchangeStore::open(f.temp.path())
            .unwrap()
            .read()
            .unwrap()
            .generation,
        2
    );
}

#[test]
fn corruption_unknown_version_and_incomplete_state_never_reset() {
    let f = Fixture::new();
    let original = fs::read(f.state_file()).unwrap();
    for bytes in [
        b"{".to_vec(),
        b"{}".to_vec(),
        {
            let mut value: Value = serde_json::from_slice(&original).unwrap();
            value["state"]["version"] = json!(999);
            serde_json::to_vec(&value).unwrap()
        },
        {
            let mut value: Value = serde_json::from_slice(&original).unwrap();
            value["state"]["streams"][f.identity.key()]["output_directory"] = json!("../escape");
            serde_json::to_vec(&value).unwrap()
        },
    ] {
        fs::write(f.state_file(), &bytes).unwrap();
        assert!(ExchangeStore::open(f.temp.path()).is_err());
        assert!(ExchangeStore::initialize(f.temp.path()).is_err());
        assert_eq!(fs::read(f.state_file()).unwrap(), bytes);
    }
    fs::remove_file(f.state_file()).unwrap();
    assert!(ExchangeStore::initialize(f.temp.path()).is_err());
    assert!(!f.state_file().exists());
}

#[test]
fn symlinks_hardlinks_and_special_files_are_not_snapshots_or_state() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    f.prepare(&base_export());
    let output = f.output();
    let outside = f.temp.path().join("outside");
    fs::write(&outside, b"outside bytes").unwrap();
    let deck = f.output().join("deck.json");
    fs::remove_file(&deck).unwrap();
    symlink(&outside, &deck).unwrap();
    assert!(f.store.plan_preparation(&f.identity).is_err());
    fs::remove_file(&deck).unwrap();
    fs::hard_link(&outside, &deck).unwrap();
    assert!(f.store.plan_preparation(&f.identity).is_err());
    fs::remove_file(&deck).unwrap();
    f.write(&base_export());
    let media_path = f.output().join("media/a.mp3");
    fs::remove_file(&media_path).unwrap();
    symlink(&outside, &media_path).unwrap();
    assert!(f.store.plan_preparation(&f.identity).is_err());
    fs::remove_file(&media_path).unwrap();
    rustix::fs::mkfifoat(rustix::fs::CWD, &media_path, rustix::fs::Mode::RUSR).unwrap();
    assert!(f.store.plan_preparation(&f.identity).is_err());
    fs::remove_file(f.state_file()).unwrap();
    symlink(&outside, f.state_file()).unwrap();
    assert!(f.store.read().is_err());
    assert_eq!(fs::read(&outside).unwrap(), b"outside bytes");
    let alias = f.temp.path().join("alias");
    symlink(f.temp.path().join("decks"), &alias).unwrap();
    assert!(
        Snapshot::read_working(
            profile(),
            &alias.join("output").join(output.file_name().unwrap())
        )
        .is_err()
    );
}

#[test]
fn ordinary_snapshot_reader_has_no_input_bypass_even_without_existing_path() {
    let temp = TempDir::new("exchange-input-guard");
    // Ни одного файла в input не создаём: отказ должен предшествовать чтению.
    let forbidden = temp.path().join("decks/input/synthetic");
    assert!(Snapshot::read_working(profile(), &forbidden).is_err());
    assert!(ExchangeStore::initialize(&forbidden).is_err());
    assert!(!temp.path().join("decks").exists());
}

#[test]
fn template_css_and_external_or_dynamic_media_stay_unproven() {
    for (property, value, expected) in [
        (
            "css",
            ".card {background:url(missing.png)}",
            "unresolved_media:missing.png",
        ),
        (
            "css",
            "@import 'https://example.invalid/style.css';",
            "unproven_media_reference:https://example.invalid/style.css",
        ),
        (
            "qfmt",
            "<img src='{{Заголовок}}'>",
            "unproven_media_reference:{{Заголовок}}",
        ),
        (
            "qfmt",
            "<img src='../outside.png'>",
            "unproven_media_reference:../outside.png",
        ),
    ] {
        let mut value_json = base_export();
        if property == "css" {
            value_json["note_models"][0]["css"] = json!(value);
        } else {
            value_json["note_models"][0]["tmpls"][0][property] = json!(value);
        }
        let result = snapshot(&value_json);
        assert!(
            result.unproven().contains(expected),
            "{:?}",
            result.unproven()
        );
    }
}

#[test]
fn diverged_conflicted_and_converged_outputs_cannot_advance_base() {
    for same in [true, false] {
        let f = Fixture::new();
        f.prepare(&base_export());
        let mut incoming = base_export();
        incoming["notes"][0]["fields"][1] = json!("input");
        let mut working = incoming.clone();
        if !same {
            working["notes"][0]["fields"][1] = json!("working");
        }
        f.write(&working);
        f.input(&incoming, 2);
        let before = fs::read(f.state_file()).unwrap();
        let plan = f.store.plan_preparation(&f.identity).unwrap();
        assert_eq!(plan.action(), PreparationAction::Blocked);
        assert_eq!(
            plan.comparison.as_ref().unwrap().disposition,
            if same {
                Disposition::Converged
            } else {
                Disposition::Conflict
            }
        );
        assert!(f.store.record_prepared(&plan).is_err());
        assert!(f.store.plan_publication(&f.identity).is_err());
        assert_eq!(fs::read(f.state_file()).unwrap(), before);
    }
}

#[test]
fn preparation_and_publication_permits_are_invalidated_by_new_input() {
    let f = Fixture::new();
    let preparation = f.store.plan_preparation(&f.identity).unwrap();
    f.write(&base_export());
    f.input(&base_export(), 2);
    assert!(matches!(
        f.store.record_prepared(&preparation),
        Err(ExchangeError::StaleState)
    ));
    fs::remove_dir_all(f.output()).unwrap();
    f.prepare(&base_export());
    let publication = f.store.plan_publication(&f.identity).unwrap();
    f.input(&base_export(), 3);
    assert!(matches!(
        f.store
            .record_published(&publication, confirm(snapshot(&base_export()), 4)),
        Err(ExchangeError::StaleState)
    ));
}

#[test]
fn moved_metadata_directory_invalidates_already_open_store() {
    let f = Fixture::new();
    let before = f.store.read().unwrap();
    let original = f.temp.path().join("decks/.exchange");
    let moved = f.temp.path().join("decks/old-exchange");
    fs::rename(&original, &moved).unwrap();
    assert!(f.store.read().is_err());
    assert!(
        f.store
            .record_input(&before.token, confirm(snapshot(&base_export()), 2))
            .is_err()
    );
    assert!(!original.exists());
    fs::rename(moved, original).unwrap();
    assert_eq!(f.store.read().unwrap().token, before.token);
}

#[test]
fn readonly_metadata_write_failure_leaves_previous_state_and_output_intact() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    f.prepare(&base_export());
    let state = f.store.read().unwrap();
    let state_bytes = fs::read(f.state_file()).unwrap();
    let working_bytes = fs::read(f.output().join("deck.json")).unwrap();
    let directory = f.state_file().parent().unwrap().to_path_buf();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o500)).unwrap();
    // Как в write.rs: суперпользователь игнорирует права каталога, такой запуск не проверяет отказ.
    let probe = directory.join("write-probe");
    let rejects_write = fs::write(&probe, b"probe").is_err();
    let _ = fs::remove_file(probe);
    if rejects_write {
        assert!(
            f.store
                .record_input(&state.token, confirm(snapshot(&base_export()), 2))
                .is_err()
        );
    }
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(fs::read(f.state_file()).unwrap(), state_bytes);
    assert_eq!(
        fs::read(f.output().join("deck.json")).unwrap(),
        working_bytes
    );
    assert_eq!(f.store.read().unwrap().token, state.token);
    assert_eq!(fs::read_dir(directory).unwrap().count(), 1);
}

#[test]
fn metadata_internal_invariants_are_checked_even_with_recomputed_checksum() {
    let f = Fixture::new();
    f.prepare(&base_export());
    let original: Value = serde_json::from_slice(&fs::read(f.state_file()).unwrap()).unwrap();
    for mutation in 0..6 {
        let mut value = original.clone();
        let key = f.identity.key();
        match mutation {
            0 => value["state"]["version"] = json!(999),
            1 => value["state"]["streams"][&key]["output_directory"] = json!("../outside"),
            2 => value["state"]["streams"][&key]["base"]["source_sequence"] = json!(2),
            3 => {
                value["state"]["streams"][&key]["current"]["snapshot"]["revision"] =
                    json!("0".repeat(64))
            }
            4 => {
                let duplicate = value["state"]["streams"][&key].clone();
                value["state"]["streams"]["foreign-key"] = duplicate;
            }
            5 => {
                value["state"]["streams"][&key]["published"] = json!({"identity": f.identity, "working_revision": "0".repeat(64), "based_on_sequence": 1, "checked_input_sequence": 1,
                "confirmed_sequence": 2, "evidence": "invalid-future-publication"})
            }
            _ => unreachable!(),
        }
        value["checksum"] = json!(asset_store::hashing::sha256_hex(
            serde_json::to_vec(&value["state"]).unwrap()
        ));
        let bytes = serde_json::to_vec(&value).unwrap();
        fs::write(f.state_file(), &bytes).unwrap();
        let error = f.store.read().unwrap_err();
        assert!(
            !error.to_string().contains("checksum"),
            "должна проверяться внутренняя согласованность: {error}"
        );
        assert_eq!(fs::read(f.state_file()).unwrap(), bytes);
    }
}

#[test]
fn working_file_field_report_keeps_ord_and_model_name_resolution() {
    let a = snapshot(&base_export());
    let mut c = base_export();
    c["notes"][0]["fields"][1] = json!("новый текст");
    c["note_models"][0]["flds"]
        .as_array_mut()
        .unwrap()
        .reverse();
    let diff = compare(&a, &snapshot(&c), &a).unwrap();
    let field = diff.differences[0].field.as_ref().unwrap();
    assert_eq!(field.model_uuid, "model-1");
    assert_eq!(field.ord, 1);
    assert_eq!(field.name, "Толкование");
}

#[test]
fn revision_writer_process() {
    use std::io::{Read, Write};
    let Some(root) = std::env::var_os("ANKI_EXCHANGE_TEST_ROOT") else {
        return;
    };
    let role = std::env::var("ANKI_EXCHANGE_TEST_ROLE").unwrap();
    let store = ExchangeStore::open(Path::new(&root)).unwrap();
    let token = store.read().unwrap().token;
    println!("EXCHANGE_READY");
    std::io::stdout().flush().unwrap();
    std::io::stdin().read_exact(&mut [0_u8]).unwrap();
    let mut value = base_export();
    value["name"] = json!(format!("Process-{role}"));
    match store.record_input(&token, confirm(snapshot(&value), 2)) {
        Ok(_) => println!("EXCHANGE_PUBLISHED"),
        Err(ExchangeError::StaleState) => println!("EXCHANGE_STALE"),
        other => panic!("неожиданный результат: {other:?}"),
    }
}

#[test]
fn independent_processes_cannot_overwrite_each_others_revision() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Command, Stdio};
    let f = Fixture::new();
    let mut children = Vec::new();
    for role in ["left", "right"] {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "exchange_contract::revision_writer_process",
                "--nocapture",
            ])
            .env("ANKI_EXCHANGE_TEST_ROOT", f.temp.path())
            .env("ANKI_EXCHANGE_TEST_ROLE", role)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            assert_ne!(
                reader.read_line(&mut line).unwrap(),
                0,
                "процесс завершился до READY"
            );
            if line.contains("EXCHANGE_READY") {
                break;
            }
            line.clear();
        }
        children.push((child, reader));
    }
    // Оба процесса уже прочли один токен; теперь общая штатная блокировка
    // сериализует их вызовы record_input, CAS пропустит только одного.
    let lock = fs::File::open(f.state_file().parent().unwrap()).unwrap();
    lock.lock().unwrap();
    for (child, _) in &mut children {
        child.stdin.take().unwrap().write_all(b"g").unwrap();
    }
    drop(lock);
    let mut published = 0;
    let mut stale = 0;
    for (child, mut reader) in children {
        let result = child.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let mut output = String::new();
        reader.read_to_string(&mut output).unwrap();
        published += usize::from(output.contains("EXCHANGE_PUBLISHED"));
        stale += usize::from(output.contains("EXCHANGE_STALE"));
    }
    assert_eq!((published, stale), (1, 1));
    assert_eq!(
        ExchangeStore::open(f.temp.path())
            .unwrap()
            .read()
            .unwrap()
            .generation,
        2
    );
}

#[test]
fn normalized_name_and_registered_folder_collisions_are_diagnosed() {
    let mut value = base_export();
    value["name"] = json!("Parent::Child");
    value["children"] = json!([{"__type__": "Deck", "crowdanki_uuid": "child",
        "name": "Parent__Child", "notes": [], "children": []}]);
    assert!(
        Snapshot::from_bytes(profile(), &serde_json::to_vec(&value).unwrap(), &media()).is_err()
    );
    let f = Fixture::new();
    let other = Snapshot::from_bytes(
        ProfileId::new("other").unwrap(),
        &serde_json::to_vec(&base_export()).unwrap(),
        &media(),
    )
    .unwrap();
    let other_key = other.identity().key();
    f.store
        .record_input(&f.store.read().unwrap().token, confirm(other, 1))
        .unwrap();
    let mut value: Value = serde_json::from_slice(&fs::read(f.state_file()).unwrap()).unwrap();
    value["state"]["streams"][&other_key]["output_directory"] =
        value["state"]["streams"][f.identity.key()]["output_directory"].clone();
    value["checksum"] = json!(asset_store::hashing::sha256_hex(
        serde_json::to_vec(&value["state"]).unwrap()
    ));
    fs::write(f.state_file(), serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(f.store.read().is_err());
}

#[test]
fn publication_keeps_original_base_sequence_when_identical_input_is_reconfirmed() {
    let f = Fixture::new();
    f.prepare(&base_export());
    f.input(&base_export(), 2);
    let permit = f.store.plan_publication(&f.identity).unwrap();
    let view = f
        .store
        .record_published(&permit, confirm(snapshot(&base_export()), 3))
        .unwrap();
    let published = view.streams[&f.identity.key()].published.as_ref().unwrap();
    assert_eq!(published.based_on_sequence, 1);
    assert_eq!(published.checked_input_sequence, 2);
    assert_eq!(published.confirmed_sequence, 3);
}

#[test]
fn dynamic_html_and_model_type_changes_are_not_silently_proven() {
    for html in [
        "<script>loadMedia()</script>",
        "<b onclick='change()'>x</b>",
        "<img src='a.mp3'",
    ] {
        let mut value = base_export();
        value["notes"][0]["fields"][1] = json!(html);
        let b = snapshot(&value);
        assert!(
            b.unproven()
                .iter()
                .any(|p| p.starts_with("dynamic_html:") || p.starts_with("incomplete_html:")),
            "{html:?}: {:?}",
            b.unproven()
        );
    }
    let a = snapshot(&base_export());
    let mut cloze = base_export();
    cloze["note_models"][0]["type"] = json!(1);
    assert!(
        !compare(&a, &snapshot(&cloze), &a)
            .unwrap()
            .incompatible
            .is_empty()
    );
}

#[test]
fn reordering_unchecked_templates_does_not_change_semantic_revision() {
    let mut value = base_export();
    let mut other = value["note_models"][0]["tmpls"][0].clone();
    other["ord"] = json!(1);
    value["note_models"][0]["tmpls"][0]["qfmt"] = json!("{{");
    value["note_models"][0]["tmpls"]
        .as_array_mut()
        .unwrap()
        .push(other);
    let a = snapshot(&value);
    assert!(!a.unproven().is_empty());
    value["note_models"][0]["tmpls"]
        .as_array_mut()
        .unwrap()
        .reverse();
    assert_eq!(snapshot(&value), a);
}

#[test]
fn unknown_properties_cannot_shadow_hierarchy_or_note_membership() {
    let mut value = base_export();
    value["notes"][0]["deck"] = json!("opaque-original-property");
    value["children"] = json!([
        {"__type__":"Deck", "crowdanki_uuid":"child-one", "name":"ChildOne", "parent":"opaque-parent", "notes":[], "children":[]},
        {"__type__":"Deck", "crowdanki_uuid":"child-two", "name":"ChildTwo", "parent":"opaque-parent", "notes":[], "children":[]}
    ]);
    let a = snapshot(&value);
    let note = value["notes"].as_array_mut().unwrap().remove(0);
    value["children"][0]["notes"] = json!([note]);
    let b = snapshot(&value);
    let report = compare(&a, &a, &b).unwrap();
    assert_eq!(report.differences.len(), 1);
    assert_eq!(report.differences[0].path, "/membership/guid-1/deck");
    let child = value["children"].as_array_mut().unwrap().remove(1);
    value["children"][0]["children"] = json!([child]);
    let report = compare(&b, &snapshot(&value), &b).unwrap();
    assert_eq!(report.differences.len(), 1);
    assert_eq!(report.differences[0].path, "/hierarchy/child-two/parent");
}
