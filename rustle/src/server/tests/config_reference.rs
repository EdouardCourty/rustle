//! `docs/config.json` (read by the website at the release tag) is the
//! reference of every config key. It is generated from the serde structs and
//! the reload code, so it cannot drift:
//!
//!     UPDATE_CONFIG_DOCS=1 cargo test -p rustle config_reference
//!
//! regenerates it after a key, a default or a reload rule changed.

use std::path::Path;

use super::live::{effective, restart_required};
use crate::config::reference::{render, with_changed, KEYS};
use crate::config::Config;

/// Does a change to `path` need a restart? Derived from the reload code:
/// `restart_required` must report exactly that key when `effective` keeps the
/// running value, and nothing when the new value is applied.
fn restart_only(path: &str) -> bool {
    let (old, new) = (Config::default(), with_changed(path));
    let reported = restart_required(&old, &new);
    let ignored = format!("{:?}", effective(&old, new.clone())) == format!("{old:?}");
    if ignored {
        assert_eq!(
            reported,
            [path],
            "{path}: ignored by a reload (`effective`) but not reported by `restart_required`"
        );
    } else {
        assert!(
            reported.is_empty(),
            "{path}: applied by a reload but reported as restart-only: {reported:?}"
        );
    }
    ignored
}

#[test]
fn every_key_has_a_reload_rule() {
    // restart_only() asserts the two reload functions agree for each key.
    assert!(KEYS.iter().any(|k| restart_only(k.0)));
    assert!(KEYS.iter().any(|k| !restart_only(k.0)));
}

#[test]
fn config_reference_is_current() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/config.json");
    let fresh = render(&restart_only);
    if std::env::var_os("UPDATE_CONFIG_DOCS").is_some() {
        std::fs::write(&path, &fresh).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == fresh,
        "docs/config.json is stale: run `UPDATE_CONFIG_DOCS=1 cargo test -p rustle config_reference` and commit it"
    );
}
