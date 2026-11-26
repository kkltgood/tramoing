//! Distinguish an explicit preview, first launch, and an unattended update restart.

use std::path::Path;

const LAUNCHED_MARKER: &str = "ui-started";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Launch {
    pub show: bool,
    pub keep_open: bool,
    pub wait_for_tray: bool,
}

impl Launch {
    pub fn resolve(
        args: &[String],
        preview_env: bool,
        install_env: bool,
        first_launch: bool,
    ) -> Self {
        let has = |flag| args.iter().any(|arg| arg == flag);
        let update = has("--after-update");
        let installation = update || has("--after-install") || install_env;
        let preview = has("--show")
            || has("--settings")
            || has("--check-update")
            || (preview_env && !installation);
        // Old update helpers also pass --after-install. Existing user data must
        // take precedence over that flag, and inherited preview env must not
        // make an unattended restart open a window.
        let welcome = first_launch && !update;
        Self {
            show: preview || welcome,
            keep_open: preview,
            wait_for_tray: welcome && !preview,
        }
    }
}

pub fn has_launched(directory: &Path, database: &Path) -> bool {
    directory.join(LAUNCHED_MARKER).exists()
        || directory.join("settings.conf").exists()
        || database.exists()
}

/// Written only after the primary UI instance is constructed successfully.
pub fn record_launch(directory: &Path) -> std::io::Result<()> {
    tm_store::files::atomic_write(&directory.join(LAUNCHED_MARKER), b"1\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(flags: &[&str]) -> Vec<String> {
        flags.iter().map(|flag| (*flag).to_owned()).collect()
    }

    #[test]
    fn current_and_legacy_update_restarts_stay_hidden() {
        for flags in [vec![], vec!["--after-install"], vec!["--after-update"]] {
            assert_eq!(
                Launch::resolve(&args(&flags), false, false, false),
                Launch::default()
            );
        }
        for flag in ["--after-install", "--after-update"] {
            assert_eq!(
                Launch::resolve(&args(&[flag]), true, false, false),
                Launch::default()
            );
        }
        assert_eq!(Launch::resolve(&[], true, true, false), Launch::default());
        // Even if the profile was removed, an update is not a first installation.
        assert_eq!(
            Launch::resolve(&args(&["--after-update"]), true, true, true),
            Launch::default()
        );
    }

    #[test]
    fn first_launch_waits_for_the_menu_bar_but_explicit_previews_still_open() {
        for flags in [vec![], vec!["--after-install"]] {
            assert_eq!(
                Launch::resolve(&args(&flags), false, false, true),
                Launch {
                    show: true,
                    keep_open: false,
                    wait_for_tray: true
                }
            );
        }
        for flag in ["--show", "--settings", "--check-update"] {
            for first in [false, true] {
                assert_eq!(
                    Launch::resolve(&args(&["--after-update", flag]), false, false, first),
                    Launch {
                        show: true,
                        keep_open: true,
                        wait_for_tray: false
                    }
                );
            }
        }
        assert!(Launch::resolve(&[], true, false, false).show);
    }

    #[test]
    fn first_launch_is_recorded_without_requiring_a_settings_change() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("history.db");
        // Logging creates the directory before startup is resolved.
        std::fs::write(dir.path().join("ui.log"), "startup").unwrap();
        assert!(!has_launched(dir.path(), &database));
        record_launch(dir.path()).unwrap();
        assert!(has_launched(dir.path(), &database));
        assert!(!dir.path().join("settings.conf").exists());
    }

    #[test]
    fn existing_profiles_do_not_need_the_new_first_launch_marker() {
        for file in ["settings.conf", "history.db"] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(file), "existing").unwrap();
            assert!(has_launched(dir.path(), &dir.path().join("history.db")));
        }
    }
}
