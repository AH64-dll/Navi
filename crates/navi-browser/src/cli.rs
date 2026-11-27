//! Pure CLI surface for the NAVI browser bridge.
//!
//! Ported from `packages/navi/src/cli/{main,native-setup,diagnostics}.ts`.
//! Argument parsing, native messaging manifest construction and diagnostic
//! command parsing are pure: this module opens no sockets and writes no files.
//! Callers own the I/O and any browser registration.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use navi_types::{NaviError, NaviResult};
use serde_json::{Value, json};

/// Commands advertised by the diagnostic scaffold in `serve.ts`.
pub const DIAGNOSTIC_COMMANDS: &str =
    "status | observe | focus <ref> | scroll <dy> | cancel | detach | quit";

/// Chrome native messaging host name used by the developer setup.
pub const DEFAULT_HOST_NAME: &str = "dev.navi.bridge";

const HOST_DESCRIPTION: &str = "NAVI relay to a user-started local CLI";
const EXTENSION_ID_LEN: usize = 32;
const MAX_DIAGNOSTIC_INPUT_UNITS: usize = 1024;
const SOCKET_FLAGS: [&str; 2] = ["--socket-dir", "--socket-path"];
const BINARY_FLAGS: [&str; 4] = ["--binary", "--binary-path", "--path", "--out"];

/// A parsed browser bridge command. Paths are kept verbatim, not resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserCliCommand {
    /// Serve the local bridge on the given socket path.
    Serve { socket_path: PathBuf },
    /// Produce developer artifacts for the given launcher and Chrome extension.
    NativeSetup {
        binary_path: PathBuf,
        extension_id: String,
    },
    /// Print a diagnostic report for the given socket path.
    Diagnostics { socket_path: PathBuf },
    /// Print help or version information and exit without side effects.
    Version,
}

/// One diagnostic line after parsing, before any browser operation.
#[derive(Debug, Clone, PartialEq)]
pub enum DiagnosticCommand {
    Status,
    Observe,
    Focus { reference: String },
    Scroll { delta_y: f64 },
    Cancel,
    Detach,
    Quit,
}

/// Parses command-line arguments, including the command name at `args[0]`.
///
/// Empty input and the help/version spellings yield [`BrowserCliCommand::Version`].
/// Every other command requires exact, non-duplicated `--flag value` options;
/// `--flag=value` is accepted as well. Socket and launcher paths must be
/// absolute, and Chrome extension ids must be 32 lowercase letters `a`..`p`.
pub fn parse_browser_cli_args(args: &[String]) -> NaviResult<BrowserCliCommand> {
    let Some((command, rest)) = args.split_first() else {
        return Ok(BrowserCliCommand::Version);
    };
    match command.as_str() {
        "help" | "--help" | "-h" | "version" | "--version" | "-V" => {
            if rest.is_empty() {
                Ok(BrowserCliCommand::Version)
            } else {
                Err(NaviError::InvalidInput(format!(
                    "{command} takes no arguments"
                )))
            }
        }
        "serve" => {
            let options = parse_flags(rest, &SOCKET_FLAGS)?;
            Ok(BrowserCliCommand::Serve {
                socket_path: take_socket_path(&options)?,
            })
        }
        "diagnostics" => {
            let options = parse_flags(rest, &SOCKET_FLAGS)?;
            Ok(BrowserCliCommand::Diagnostics {
                socket_path: take_socket_path(&options)?,
            })
        }
        "native-setup" => parse_native_setup(rest),
        other => Err(NaviError::InvalidInput(format!(
            "unknown command '{other}'; use --help for the supported commands"
        ))),
    }
}

/// Builds the Chrome native messaging manifest for one extension origin.
///
/// The returned value contains only data: registration and file writes belong
/// to the caller. Use [`validate_host_name`] and [`validate_extension_id`] on
/// untrusted inputs before calling this function.
pub fn build_chrome_native_manifest(
    host_name: &str,
    binary_path: &Path,
    extension_id: &str,
) -> Value {
    json!({
        "name": host_name,
        "description": HOST_DESCRIPTION,
        "path": binary_path.to_string_lossy().into_owned(),
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{extension_id}/")],
    })
}

/// Builds the JSON diagnostics report for a bridge socket.
///
/// The keys mirror the `ready` event in `serve.ts` and add the explicit
/// `socket_path` key so callers can report either spelling.
pub fn build_diagnostics_report(socket_path: &Path) -> Value {
    let socket = socket_path.to_string_lossy().into_owned();
    json!({
        "type": "diagnostics",
        "socket": socket.clone(),
        "socket_path": socket,
        "mode": "diagnostic-scaffold",
        "commands": DIAGNOSTIC_COMMANDS,
        "ready": true,
    })
}

/// Validates a Chrome native messaging host name.
pub fn validate_host_name(host_name: &str) -> NaviResult<()> {
    let mut characters = host_name.chars();
    let valid = characters
        .clone()
        .next()
        .is_some_and(|character| character.is_ascii_lowercase())
        && characters.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '.' | '_' | '-')
        });
    if valid {
        Ok(())
    } else {
        Err(NaviError::InvalidInput(
            "host name must be a lowercase DNS-style token".to_owned(),
        ))
    }
}

/// Validates a Chrome extension id: exactly 32 lowercase letters `a`..`p`.
pub fn validate_extension_id(extension_id: &str) -> NaviResult<()> {
    let valid = extension_id.len() == EXTENSION_ID_LEN
        && extension_id
            .bytes()
            .all(|byte| (b'a'..=b'p').contains(&byte));
    if valid {
        Ok(())
    } else {
        Err(NaviError::InvalidInput(
            "extension id must be 32 lowercase letters a-p".to_owned(),
        ))
    }
}

/// Parses one untrusted diagnostic input line into a supported command.
pub fn parse_diagnostic_command(line: &str) -> NaviResult<DiagnosticCommand> {
    if line.encode_utf16().count() > MAX_DIAGNOSTIC_INPUT_UNITS {
        return Err(NaviError::InvalidInput(
            "command exceeds the diagnostic input limit".to_owned(),
        ));
    }
    let mut parts = line.split_whitespace();
    let Some(command) = parts.next() else {
        return Err(unsupported_diagnostic_command());
    };
    let arguments: Vec<&str> = parts.collect();
    match (command, arguments.as_slice()) {
        ("status", []) => Ok(DiagnosticCommand::Status),
        ("observe", []) => Ok(DiagnosticCommand::Observe),
        ("cancel", []) => Ok(DiagnosticCommand::Cancel),
        ("detach", []) => Ok(DiagnosticCommand::Detach),
        ("quit", []) => Ok(DiagnosticCommand::Quit),
        ("focus", [reference]) => Ok(DiagnosticCommand::Focus {
            reference: (*reference).to_owned(),
        }),
        ("scroll", [delta]) => {
            let delta_y = delta
                .parse::<f64>()
                .map_err(|_| NaviError::InvalidInput("scroll delta must be finite".to_owned()))?;
            if !delta_y.is_finite() {
                return Err(NaviError::InvalidInput(
                    "scroll delta must be finite".to_owned(),
                ));
            }
            Ok(DiagnosticCommand::Scroll { delta_y })
        }
        _ => Err(unsupported_diagnostic_command()),
    }
}

fn unsupported_diagnostic_command() -> NaviError {
    NaviError::InvalidInput(format!(
        "supported diagnostic commands: {DIAGNOSTIC_COMMANDS}"
    ))
}

fn parse_native_setup(args: &[String]) -> NaviResult<BrowserCliCommand> {
    let mut allowed = Vec::with_capacity(SOCKET_FLAGS.len() + BINARY_FLAGS.len() + 1);
    allowed.extend_from_slice(&SOCKET_FLAGS);
    allowed.extend_from_slice(&BINARY_FLAGS);
    allowed.push("--extension-id");
    let options = parse_flags(args, &allowed)?;
    // The original CLI also accepts a socket directory here; validate it but
    // do not persist it, because setup only emits developer artifacts.
    if options.contains_key("--socket-dir") || options.contains_key("--socket-path") {
        let _ = take_socket_path(&options)?;
    }
    Ok(BrowserCliCommand::NativeSetup {
        binary_path: take_binary_path(&options)?,
        extension_id: take_extension_id(&options)?,
    })
}

/// Parses named options into a map, rejecting missing, duplicate, unknown and
/// flag-shaped values. Both `--flag value` and `--flag=value` are accepted.
fn parse_flags(args: &[String], allowed: &[&str]) -> NaviResult<BTreeMap<String, String>> {
    let mut parsed = BTreeMap::new();
    let mut index = 0;
    while index < args.len() {
        let token = &args[index];
        let (name, inline_value) = match token.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (token.as_str(), None),
        };
        if !allowed.contains(&name) {
            return Err(NaviError::InvalidInput(format!("unknown option: {name}")));
        }
        let value = match inline_value {
            Some(value) => value.to_owned(),
            None => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| NaviError::InvalidInput(format!("missing value for {name}")))?;
                index += 1;
                value.clone()
            }
        };
        if value.is_empty() || value.starts_with("--") {
            return Err(NaviError::InvalidInput(format!(
                "missing or invalid value for {name}"
            )));
        }
        if parsed.insert(name.to_owned(), value).is_some() {
            return Err(NaviError::InvalidInput(format!("duplicate option: {name}")));
        }
        index += 1;
    }
    Ok(parsed)
}

fn take_socket_path(options: &BTreeMap<String, String>) -> NaviResult<PathBuf> {
    let raw = match (options.get("--socket-dir"), options.get("--socket-path")) {
        (Some(socket_dir), None) => socket_dir,
        (None, Some(socket_path)) => socket_path,
        (Some(_), Some(_)) => {
            return Err(NaviError::InvalidInput(
                "--socket-dir and --socket-path are mutually exclusive".to_owned(),
            ));
        }
        (None, None) => {
            return Err(NaviError::InvalidInput(
                "missing required option: --socket-dir".to_owned(),
            ));
        }
    };
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(NaviError::InvalidInput(
            "--socket-dir must be an absolute private directory".to_owned(),
        ));
    }
    Ok(path)
}

fn take_binary_path(options: &BTreeMap<String, String>) -> NaviResult<PathBuf> {
    let mut selected = None;
    for flag in BINARY_FLAGS {
        if let Some(value) = options.get(flag) {
            if selected.is_some() {
                return Err(NaviError::InvalidInput(
                    "native setup accepts a single binary path option".to_owned(),
                ));
            }
            selected = Some(value);
        }
    }
    let raw = selected
        .ok_or_else(|| NaviError::InvalidInput("missing required option: --binary".to_owned()))?;
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(NaviError::InvalidInput(
            "--binary must be an absolute launcher path".to_owned(),
        ));
    }
    Ok(path)
}

fn take_extension_id(options: &BTreeMap<String, String>) -> NaviResult<String> {
    let extension_id = options.get("--extension-id").ok_or_else(|| {
        NaviError::InvalidInput("missing required option: --extension-id".to_owned())
    })?;
    validate_extension_id(extension_id)?;
    Ok(extension_id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|token| (*token).to_owned()).collect()
    }

    #[test]
    fn serve_parsing_accepts_absolute_socket_paths_and_rejects_bad_options() {
        let command = parse_browser_cli_args(&args(&["serve", "--socket-dir", "/tmp/navi.sock"]))
            .expect("valid serve command");
        assert_eq!(
            command,
            BrowserCliCommand::Serve {
                socket_path: PathBuf::from("/tmp/navi.sock")
            }
        );
        let inline = parse_browser_cli_args(&args(&["serve", "--socket-path=/tmp/other.sock"]))
            .expect("inline flag");
        assert_eq!(
            inline,
            BrowserCliCommand::Serve {
                socket_path: PathBuf::from("/tmp/other.sock")
            }
        );

        assert!(parse_browser_cli_args(&args(&["serve"])).is_err());
        assert!(parse_browser_cli_args(&args(&["serve", "--socket-dir"])).is_err());
        assert!(
            parse_browser_cli_args(&args(&["serve", "--socket-dir", "relative.sock"])).is_err()
        );
        assert!(
            parse_browser_cli_args(&args(&["serve", "--socket-dir", "--extension-id"])).is_err()
        );
        assert!(
            parse_browser_cli_args(&args(&[
                "serve",
                "--socket-dir",
                "/tmp/a.sock",
                "--socket-dir",
                "/tmp/b.sock"
            ]))
            .is_err()
        );
        assert!(
            parse_browser_cli_args(&args(&[
                "serve",
                "--socket-dir",
                "/tmp/a.sock",
                "--socket-path",
                "/tmp/b.sock"
            ]))
            .is_err()
        );
        assert!(
            parse_browser_cli_args(&args(&[
                "serve",
                "--extension-id",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "--socket-dir",
                "/tmp/a.sock"
            ]))
            .is_err()
        );
    }

    #[test]
    fn diagnostics_and_version_commands_are_parsed_without_side_effects() {
        let command =
            parse_browser_cli_args(&args(&["diagnostics", "--socket-dir", "/tmp/navi.sock"]))
                .expect("valid diagnostics command");
        assert_eq!(
            command,
            BrowserCliCommand::Diagnostics {
                socket_path: PathBuf::from("/tmp/navi.sock")
            }
        );

        for spelling in ["help", "--help", "-h", "version", "--version", "-V"] {
            assert_eq!(
                parse_browser_cli_args(&args(&[spelling])).expect("version command"),
                BrowserCliCommand::Version
            );
        }
        assert_eq!(
            parse_browser_cli_args(&[]).expect("empty arguments"),
            BrowserCliCommand::Version
        );
        assert!(parse_browser_cli_args(&args(&["bogus"])).is_err());
        assert!(parse_browser_cli_args(&args(&["--help", "serve"])).is_err());
        assert!(
            parse_browser_cli_args(&args(&["diagnostics", "--socket-dir", "./relative"])).is_err()
        );
    }

    #[test]
    fn native_setup_parses_both_flag_styles_and_validates_its_inputs() {
        let extension_id = "a".repeat(EXTENSION_ID_LEN);
        let command = parse_browser_cli_args(&args(&[
            "native-setup",
            "--binary",
            "/opt/navi/navi-connector",
            "--extension-id",
            &extension_id,
        ]))
        .expect("valid native setup");
        assert_eq!(
            command,
            BrowserCliCommand::NativeSetup {
                binary_path: PathBuf::from("/opt/navi/navi-connector"),
                extension_id: extension_id.clone(),
            }
        );

        // The original TypeScript CLI used `--socket-dir` and `--out`.
        let legacy = parse_browser_cli_args(&args(&[
            "native-setup",
            "--socket-dir",
            "/tmp/navi-private",
            "--extension-id",
            &extension_id,
            "--out=/tmp/navi-private/navi-connector",
        ]))
        .expect("legacy flags");
        assert_eq!(
            legacy,
            BrowserCliCommand::NativeSetup {
                binary_path: PathBuf::from("/tmp/navi-private/navi-connector"),
                extension_id: extension_id.clone(),
            }
        );

        let uppercase = "A".repeat(EXTENSION_ID_LEN);
        let wrong_length = "a".repeat(EXTENSION_ID_LEN - 1);
        let out_of_range = "q".repeat(EXTENSION_ID_LEN);
        let injected = format!("{}'", "a".repeat(EXTENSION_ID_LEN - 1));
        let bad_ids: [&str; 5] = [&uppercase, &wrong_length, &out_of_range, &injected, "*"];
        for bad_id in bad_ids {
            assert!(
                parse_browser_cli_args(&args(&[
                    "native-setup",
                    "--binary",
                    "/opt/navi/navi-connector",
                    "--extension-id",
                    bad_id,
                ]))
                .is_err(),
                "accepted bad extension id {bad_id:?}"
            );
        }
        assert!(
            parse_browser_cli_args(&args(&["native-setup", "--extension-id", &extension_id]))
                .is_err()
        );
        assert!(
            parse_browser_cli_args(&args(&[
                "native-setup",
                "--binary",
                "/opt/navi/navi-connector"
            ]))
            .is_err()
        );
        assert!(
            parse_browser_cli_args(&args(&[
                "native-setup",
                "--binary",
                "relative/launcher",
                "--extension-id",
                &extension_id
            ]))
            .is_err()
        );
        assert!(
            parse_browser_cli_args(&args(&[
                "native-setup",
                "--binary",
                "/opt/navi/launcher",
                "--out",
                "/opt/navi/other",
                "--extension-id",
                &extension_id
            ]))
            .is_err()
        );
        assert!(
            parse_browser_cli_args(&args(&[
                "native-setup",
                "--socket-dir",
                "relative",
                "--binary",
                "/opt/navi/launcher",
                "--extension-id",
                &extension_id
            ]))
            .is_err()
        );
    }

    #[test]
    fn chrome_manifest_pins_one_origin_and_validates_its_identifiers() {
        let extension_id = "p".repeat(EXTENSION_ID_LEN);
        let manifest = build_chrome_native_manifest(
            DEFAULT_HOST_NAME,
            Path::new("/opt/navi/navi-connector"),
            &extension_id,
        );
        assert_eq!(
            manifest,
            json!({
                "name": "dev.navi.bridge",
                "description": "NAVI relay to a user-started local CLI",
                "path": "/opt/navi/navi-connector",
                "type": "stdio",
                "allowed_origins": [format!("chrome-extension://{extension_id}/")],
            })
        );
        assert_eq!(
            manifest["allowed_origins"].as_array().map(Vec::len),
            Some(1)
        );
        assert!(validate_extension_id(&extension_id).is_ok());
        assert!(validate_extension_id("q".repeat(EXTENSION_ID_LEN).as_str()).is_err());
        assert!(validate_extension_id(&format!("{extension_id}/..")).is_err());
        assert!(validate_host_name(DEFAULT_HOST_NAME).is_ok());
        assert!(validate_host_name("dev.navi.bridge-1").is_ok());
        assert!(validate_host_name("Dev.Navi").is_err());
        assert!(validate_host_name("").is_err());
        assert!(validate_host_name("dev navi").is_err());
    }

    #[test]
    fn diagnostics_report_exposes_the_socket_and_supported_commands() {
        let report = build_diagnostics_report(Path::new("/tmp/navi.sock"));
        assert_eq!(report["type"], json!("diagnostics"));
        assert_eq!(report["socket"], json!("/tmp/navi.sock"));
        assert_eq!(report["socket_path"], json!("/tmp/navi.sock"));
        assert_eq!(report["mode"], json!("diagnostic-scaffold"));
        assert_eq!(report["commands"], json!(DIAGNOSTIC_COMMANDS));
        assert_eq!(report["ready"], json!(true));
    }

    #[test]
    fn diagnostic_command_parsing_enforces_arity_and_finite_deltas() {
        assert_eq!(
            parse_diagnostic_command("  status   ").expect("status"),
            DiagnosticCommand::Status
        );
        assert_eq!(
            parse_diagnostic_command("observe").expect("observe"),
            DiagnosticCommand::Observe
        );
        assert_eq!(
            parse_diagnostic_command("cancel").expect("cancel"),
            DiagnosticCommand::Cancel
        );
        assert_eq!(
            parse_diagnostic_command("detach").expect("detach"),
            DiagnosticCommand::Detach
        );
        assert_eq!(
            parse_diagnostic_command("quit").expect("quit"),
            DiagnosticCommand::Quit
        );
        assert_eq!(
            parse_diagnostic_command("focus e12").expect("focus"),
            DiagnosticCommand::Focus {
                reference: "e12".to_owned()
            }
        );
        let scroll = parse_diagnostic_command("scroll -12.5").expect("scroll");
        let DiagnosticCommand::Scroll { delta_y } = scroll else {
            panic!("expected a scroll command");
        };
        assert!((delta_y + 12.5).abs() < f64::EPSILON);

        let too_long = "a".repeat(MAX_DIAGNOSTIC_INPUT_UNITS + 1);
        let invalid_lines: [&str; 10] = [
            "",
            "   ",
            "status now",
            "focus",
            "focus e1 e2",
            "scroll",
            "scroll NaN",
            "scroll infinity",
            "click",
            &too_long,
        ];
        for invalid in invalid_lines {
            assert!(
                parse_diagnostic_command(invalid).is_err(),
                "accepted invalid diagnostic line {invalid:?}"
            );
        }
    }
}
