// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use livi_core_proto::icon::{DEFAULT_180, SIZES};
use serde_json::{Map, Value};

use crate::i18n::Strings;
use crate::settings::custom::files::Wanted;
use crate::settings::view::{Change, Local, Row, RowKind};

pub fn own(state: &Value) -> Option<String> {
    SIZES
        .iter()
        .map(|(key, _)| state["config"][key].as_str().unwrap_or("").trim())
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

pub fn shown(state: &Value) -> String {
    own(state).unwrap_or_else(|| DEFAULT_180.trim().to_string())
}

pub fn rows(state: &Value, t: &Strings) -> Vec<Row> {
    let mut rows = vec![
        Row {
            kind: RowKind::Button,
            label: t.t("settings.importPng", "Import PNG"),
            button: t.t("settings.import", "Import"),
            outlined: true,
            enabled: true,
            local: Some(Local::Browse(Wanted::Picture)),
            ..Row::default()
        },
        Row {
            kind: RowKind::Button,
            label: t.t("settings.uiIcon", "UI Icon"),
            button: t.t("settings.reset", "Reset"),
            outlined: true,
            enabled: own(state).is_some(),
            change: Some(Change::Config(icons(["", "", ""].map(String::from)))),
            ..Row::default()
        },
        Row {
            kind: RowKind::Picture,
            value: shown(state),
            enabled: true,
            local: Some(Local::Browse(Wanted::Picture)),
            ..Row::default()
        },
    ];
    if let Some(splash) = state["system"]["bootSplash"].as_object() {
        let error = splash.get("error").and_then(Value::as_str);
        let value = if splash.get("updating").and_then(Value::as_bool) == Some(true) {
            "Updating boot splash… Wait before rebooting.".to_string()
        } else if let Some(error) = error {
            error.to_string()
        } else {
            "Updated. The new splash appears after reboot.".to_string()
        };
        rows.push(Row {
            kind: RowKind::Info,
            label: "Boot Splash".into(),
            value,
            alert: error.is_some(),
            enabled: true,
            ..Row::default()
        });
    }
    rows
}

pub fn icons(pngs: [String; 3]) -> Value {
    let mut m = Map::new();
    for ((key, _), png) in SIZES.iter().zip(pngs) {
        m.insert((*key).to_string(), Value::from(png));
    }
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_own_icon_wins_over_the_default() {
        let none = json!({ "config": {} });
        assert_eq!(own(&none), None);
        assert_eq!(shown(&none), DEFAULT_180.trim());
        let set = json!({ "config": { "carplayIcon120": " abc " } });
        assert_eq!(own(&set).as_deref(), Some("abc"));
        assert_eq!(shown(&set), "abc");
    }

    #[test]
    fn reset_is_only_offered_for_an_own_icon() {
        let t = Strings::default();
        let rows_default = rows(&json!({ "config": {} }), &t);
        assert!(!rows_default[1].enabled);
        let rows_own = rows(&json!({ "config": { "carplayIcon180": "abc" } }), &t);
        assert!(rows_own[1].enabled);
        assert_eq!(
            rows_own[1].change,
            Some(Change::Config(json!({
                "carplayIcon120": "",
                "carplayIcon180": "",
                "carplayIcon256": ""
            })))
        );
        assert_eq!(rows_own[2].kind, RowKind::Picture);
        assert_eq!(rows_own[2].value, "abc");
    }

    #[test]
    fn icons_fill_all_three_sizes_in_order() {
        let v = icons(["a", "b", "c"].map(String::from));
        assert_eq!(
            v,
            json!({ "carplayIcon120": "a", "carplayIcon180": "b", "carplayIcon256": "c" })
        );
    }

    #[test]
    fn boot_splash_progress_and_failures_are_visible_on_the_icon_page() {
        let t = Strings::default();
        let updating = json!({ "system": { "bootSplash": { "updating": true, "error": null } } });
        let row = rows(&updating, &t).pop().unwrap();
        assert_eq!(row.label, "Boot Splash");
        assert!(row.value.contains("Wait before rebooting"));
        let ready = json!({ "system": { "bootSplash": { "updating": false, "error": null } } });
        assert!(
            rows(&ready, &t)
                .last()
                .unwrap()
                .value
                .contains("after reboot")
        );
        let failed =
            json!({ "system": { "bootSplash": { "updating": false, "error": "disk full" } } });
        let row = rows(&failed, &t).pop().unwrap();
        assert!(row.alert);
        assert_eq!(row.value, "disk full");
        assert_eq!(rows(&json!({}), &t).len(), 3);
    }
}
