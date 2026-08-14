fn main() -> glib::ExitCode {
    sanitize_env();
    openlook::app::run()
}

/// Undo snap environment pollution (for example when launched from a snap
/// VS Code terminal): a foreign snap's library paths make WebKit's helper
/// processes die with GLIBC symbol errors.
fn sanitize_env() {
    let snap_originals: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| k.ends_with("_VSCODE_SNAP_ORIG"))
        .collect();
    for key in snap_originals {
        let original = key.trim_end_matches("_VSCODE_SNAP_ORIG").to_string();
        match std::env::var(&key) {
            Ok(value) if !value.is_empty() => std::env::set_var(&original, value),
            _ => std::env::remove_var(&original),
        }
        std::env::remove_var(&key);
    }
    for key in [
        "LD_LIBRARY_PATH",
        "GTK_PATH",
        "GTK_EXE_PREFIX",
        "GTK_IM_MODULE_FILE",
        "GIO_MODULE_DIR",
        "GDK_PIXBUF_MODULE_FILE",
        "GSETTINGS_SCHEMA_DIR",
        "LOCPATH",
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "PYTHONHOME",
    ] {
        let Ok(value) = std::env::var(key) else { continue };
        if !value.contains("/snap/") {
            continue;
        }
        let kept: Vec<&str> = value.split(':').filter(|p| !p.contains("/snap/")).collect();
        if kept.is_empty() {
            std::env::remove_var(key);
        } else {
            std::env::set_var(key, kept.join(":"));
        }
    }
}
