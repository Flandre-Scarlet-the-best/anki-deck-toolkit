use super::*;
use crate::temp_workspace::TempWorkspace;

fn write(workspace: &TempWorkspace, text: &str) -> PathBuf {
    let path = workspace.path().join("config.toml");
    std::fs::write(&path, text).unwrap();
    path
}

fn with_file(path: &Path) -> ForvoOverrides {
    ForvoOverrides {
        config: Some(path.to_owned()),
        ..ForvoOverrides::default()
    }
}

fn code(error: &AssetError) -> &str {
    error.details["code"].as_str().unwrap()
}

#[test]
fn defaults_apply_without_a_file_and_name_their_source() {
    let workspace = TempWorkspace::create("user-config-default").unwrap();
    let path = write(&workspace, "");
    let settings = forvo(&with_file(&path)).unwrap();
    assert_eq!(settings.mode, ForvoMode::ChromiumFirst);
    assert_eq!(settings.browser_target, None);
    assert_eq!(settings.update_check, UpdateCheck::Weekly);
    assert_eq!(settings.sources[0], ("mode", "default"));
    assert_eq!(settings.describe()["mode"], "chromium_first");
}

#[test]
fn file_values_apply_and_cli_overrides_them() {
    let workspace = TempWorkspace::create("user-config-layers").unwrap();
    let path = write(
        &workspace,
        "schema_version = 1\n[forvo]\nmode = \"extension_only\"\nbrowser_target = \"brave\"\nupdate_check = \"off\"\n",
    );
    let settings = forvo(&with_file(&path)).unwrap();
    assert_eq!(settings.mode, ForvoMode::ExtensionOnly);
    assert_eq!(settings.browser_target.as_deref(), Some("brave"));
    assert_eq!(settings.update_check, UpdateCheck::Off);
    assert_eq!(settings.sources[0], ("mode", "file"));
    assert_eq!(settings.sources[2], ("update_check", "file"));
    let cli = forvo(&ForvoOverrides {
        config: Some(path),
        mode: Some(ForvoMode::ChromiumFirst),
        browser_target: Some("chrome".into()),
    })
    .unwrap();
    assert_eq!(cli.mode, ForvoMode::ChromiumFirst);
    assert_eq!(cli.browser_target.as_deref(), Some("chrome"));
    assert_eq!(cli.sources[0], ("mode", "cli"));
    assert_eq!(cli.sources[1], ("browser_target", "cli"));
    assert_eq!(cli.update_check, UpdateCheck::Off);
    assert_eq!(cli.sources[2], ("update_check", "file"));
}

#[test]
fn unknown_keys_values_and_sections_are_explicit_errors() {
    let workspace = TempWorkspace::create("user-config-strict").unwrap();
    for text in [
        "[forvo]\nmode = \"slow\"\n",
        "[forvo]\nmodee = \"extension_only\"\n",
        "[inference]\nmodel = \"x\"\n",
        "[forvo]\nbrowser_target = \"Brave Browser\"\n",
        "schema_version = 2\n",
        "[forvo\n",
    ] {
        let path = write(&workspace, text);
        let error = forvo(&with_file(&path)).unwrap_err();
        assert_eq!(code(&error), "config_invalid", "{text}");
    }
    let missing = workspace.path().join("absent.toml");
    assert_eq!(
        code(&forvo(&with_file(&missing)).unwrap_err()),
        "config_unreadable"
    );
    let bad_cli = ForvoOverrides {
        browser_target: Some("../x".into()),
        ..with_file(&write(&workspace, ""))
    };
    assert_eq!(code(&forvo(&bad_cli).unwrap_err()), "invalid_target");
}
