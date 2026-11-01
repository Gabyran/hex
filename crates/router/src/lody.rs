//! Sending a capture to Lody: one new session per capture, for the Agent Role the host
//! configured, through Lody's own CLI (`lody session create`). The CLI is found, never started,
//! and Lody's files and ports are never touched.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};

use crate::bundle::load_bundle;
use crate::config::LodyConfig;
use crate::model::ScrawlBundle;

/// The most files one `session create` takes.
pub const MAX_ATTACHMENTS: usize = 8;
/// The biggest file one `--attach` takes.
const MAX_ATTACHMENT_BYTES: u64 = 100 << 20;
/// How long the CLI may take to create a session.
const CLI_TIMEOUT: Duration = Duration::from_secs(60);
/// How much of the CLI's output is kept.
const OUTPUT_LIMIT: u64 = 64 * 1024;

const APP_NODE: &str = "/Applications/Lody.app/Contents/MacOS/Lody";
const APP_CLI: &str =
    "/Applications/Lody.app/Contents/Resources/app.asar.unpacked/resources/cli/index.js";

/// What Lody made of a capture.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Created {
    pub session_id: String,
}

/// A way to run Lody's CLI.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Cli {
    program: PathBuf,
    /// Before the CLI's own arguments: Lody.app's `index.js`.
    prefix: Vec<OsString>,
    /// Lody.app's own Electron runs the script as Node.
    electron_as_node: bool,
    /// Where it was found, for errors.
    origin: &'static str,
}

/// Where to look for the CLI, besides the host's override.
#[derive(Clone, Debug)]
struct Places {
    path: Vec<PathBuf>,
    app_node: PathBuf,
    app_cli: PathBuf,
}

impl Places {
    fn system() -> Self {
        Self {
            path: std::env::var_os("PATH")
                .map(|paths| std::env::split_paths(&paths).collect())
                .unwrap_or_default(),
            app_node: PathBuf::from(APP_NODE),
            app_cli: PathBuf::from(APP_CLI),
        }
    }
}

fn executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn expand_home(text: &str) -> PathBuf {
    match text.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => PathBuf::from(text),
    }
}

/// The CLIs to try, in order: the configured one alone, else `lody` on `PATH`, then Lody.app's
/// own.
fn candidates(config: &LodyConfig, places: &Places) -> Result<Vec<Cli>> {
    if let Some(cli) = config.cli() {
        let program = expand_home(cli);
        ensure!(
            executable(&program),
            "the configured Lody CLI {} isn't an executable file",
            program.display()
        );
        return Ok(vec![Cli {
            program,
            prefix: Vec::new(),
            electron_as_node: false,
            origin: "agentLody.cli",
        }]);
    }
    let mut found = Vec::new();
    let name = if cfg!(windows) { "lody.cmd" } else { "lody" };
    if let Some(program) = places
        .path
        .iter()
        .map(|dir| dir.join(name))
        .find(|path| executable(path))
    {
        found.push(Cli {
            program,
            prefix: Vec::new(),
            electron_as_node: false,
            origin: "PATH",
        });
    }
    if executable(&places.app_node) && places.app_cli.is_file() {
        found.push(Cli {
            program: places.app_node.clone(),
            prefix: vec![places.app_cli.clone().into_os_string()],
            electron_as_node: true,
            origin: "Lody.app",
        });
    }
    ensure!(
        !found.is_empty(),
        "Lody's CLI wasn't found (not on PATH, not in Lody.app)"
    );
    Ok(found)
}

/// A value the CLI would read as an option of its own.
fn plain(what: &str, value: &str) -> Result<()> {
    ensure!(
        !value.starts_with('-') && !value.chars().any(char::is_control),
        "{what} {value:?} isn't a name, ID, or path"
    );
    Ok(())
}

fn role_of(config: &LodyConfig) -> Result<&str> {
    let role = config
        .role()
        .context("no Lody Agent Role is set (capture.routing.agentLody.agentRole)")?;
    plain("the Agent Role", role)?;
    Ok(role)
}

/// `--workspace <selector>`, when the login has more than one workspace.
fn workspace_args(config: &LodyConfig) -> Result<Vec<OsString>> {
    let Some(workspace) = config.workspace() else {
        return Ok(Vec::new());
    };
    plain("the workspace", workspace)?;
    Ok(vec!["--workspace".into(), workspace.into()])
}

/// The arguments of `lody session create` for one capture.
fn create_args(
    config: &LodyConfig,
    prompt_file: &Path,
    attachments: &[PathBuf],
) -> Result<Vec<OsString>> {
    let role = role_of(config)?;
    ensure!(
        attachments.len() <= MAX_ATTACHMENTS,
        "Lody takes {MAX_ATTACHMENTS} attachments at most"
    );
    let mut args: Vec<OsString> = ["session", "create", "--json", "--agent-role", role]
        .into_iter()
        .map(OsString::from)
        .collect();
    if let Some(project) = config.project() {
        plain("the local project", project)?;
        args.push("--local-project".into());
        args.push(project.into());
    }
    args.extend(workspace_args(config)?);
    args.push("--prompt-file".into());
    args.push(prompt_file.into());
    for file in attachments {
        args.push("--attach".into());
        args.push(file.into());
    }
    Ok(args)
}

/// A private file holding the prompt, removed when dropped.
struct PromptFile(PathBuf);

impl PromptFile {
    fn write(text: &str) -> Result<Self> {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!(
            "hex-lody-prompt-{}.md",
            crate::util::make_bundle_id()
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .with_context(|| format!("Could not write {}", path.display()))?;
        let prompt = Self(path);
        file.write_all(text.as_bytes())
            .with_context(|| format!("Could not write {}", prompt.0.display()))?;
        Ok(prompt)
    }
}

impl Drop for PromptFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// The prompt for a capture and the files that go with it: the summary, then the contact sheet
/// as the one attachment.
fn prompt_of(bundle: &ScrawlBundle, directory: &Path) -> (String, Vec<PathBuf>) {
    let sheet = directory.join(&bundle.contact_sheet);
    let sheet_is_inside = Path::new(&bundle.contact_sheet)
        .components()
        .all(|part| matches!(part, std::path::Component::Normal(_)));
    let attached = !bundle.keyframes.is_empty()
        && !bundle.contact_sheet.is_empty()
        && sheet_is_inside
        && fs::metadata(&sheet).is_ok_and(|m| m.is_file() && m.len() <= MAX_ATTACHMENT_BYTES);
    let note = if attached {
        "Attached: the contact sheet (every keyframe in order). Full-size keyframes are in frames/ and frames-full/ in the recording folder, on the Mac that recorded it."
    } else {
        "No images."
    };
    let text = format!(
        "{}\n\n{note}\nRecorded at {}.\nThis capture is {} ms long.",
        crate::opencode::summary_of(bundle, directory).trim_end(),
        directory.display(),
        bundle.duration_ms,
    );
    (text, if attached { vec![sheet] } else { Vec::new() })
}

struct Ran {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Runs `cli` with `args`, killing it past `timeout`.
fn run(cli: &Cli, args: &[OsString], timeout: Duration) -> Result<Ran> {
    let mut command = Command::new(&cli.program);
    command
        .args(&cli.prefix)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if cli.electron_as_node {
        command.env("ELECTRON_RUN_AS_NODE", "1");
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("Could not run Lody's CLI ({})", cli.origin))?;
    let keep = |stream: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            if let Some(mut stream) = stream {
                let _ = (&mut stream).take(OUTPUT_LIMIT).read_to_end(&mut kept);
                let _ = std::io::copy(&mut stream, &mut std::io::sink());
            }
            String::from_utf8_lossy(&kept).into_owned()
        })
    };
    let stdout = keep(child.stdout.take().map(|s| Box::new(s) as _));
    let stderr = keep(child.stderr.take().map(|s| Box::new(s) as _));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().context("Could not wait for Lody's CLI")? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Lody's CLI didn't answer in time ({} s)", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok(Ran {
        success: status.success(),
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// What a failed run said: the `error` of its JSON, else its first line that isn't Node's
/// warning noise.
fn said(ran: &Ran) -> String {
    let json_error = |text: &str| {
        let json = serde_json::from_str::<serde_json::Value>(text.trim()).ok()?;
        match json.get("error")? {
            serde_json::Value::String(message) => Some(message.clone()),
            other => Some(other.get("message")?.as_str()?.to_owned()),
        }
    };
    let line = |text: &str| {
        text.lines()
            .map(str::trim)
            .find(|line| {
                !line.is_empty() && !line.starts_with("(node:") && !line.starts_with("(Use `")
            })
            .map(str::to_owned)
    };
    [&ran.stdout, &ran.stderr]
        .into_iter()
        .find_map(|text| json_error(text))
        .or_else(|| {
            [&ran.stderr, &ran.stdout]
                .into_iter()
                .find_map(|text| line(text))
        })
        .unwrap_or_else(|| "no reason given".into())
}

/// An older CLI that doesn't know the option it was given.
fn unknown_option(ran: &Ran) -> bool {
    ran.stderr.contains("unknown option") || ran.stdout.contains("unknown option")
}

/// Runs `args` on the first CLI that takes them, going on to the next past one that is too old
/// or can't run. A failure the CLI itself reported is final.
fn run_first(clis: &[Cli], args: &[OsString], what: &str, timeout: Duration) -> Result<Ran> {
    let mut last = None;
    for cli in clis {
        match run(cli, args, timeout) {
            Ok(ran) if ran.success => return Ok(ran),
            Ok(ran) if unknown_option(&ran) => {
                last = Some(anyhow!(
                    "Lody's CLI from {} is too old for {what}: {}",
                    cli.origin,
                    said(&ran)
                ));
            }
            Ok(ran) => bail!("lody {what} failed: {}", said(&ran)),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("Lody's CLI wasn't found")))
}

/// Creates a Lody session from a capture with the CLI `places` leads to.
fn send_bundle_with(config: &LodyConfig, places: &Places, bundle_dir: &Path) -> Result<Created> {
    role_of(config)?;
    let clis = candidates(config, places)?;
    let (directory, bundle) = load_bundle(bundle_dir)?;
    let (text, attachments) = prompt_of(&bundle, &directory);
    let prompt = PromptFile::write(&text)?;
    let args = create_args(config, &prompt.0, &attachments)?;
    let ran = run_first(&clis, &args, "session create", CLI_TIMEOUT)?;
    let created: serde_json::Value = serde_json::from_str(ran.stdout.trim())
        .with_context(|| format!("lody printed no JSON: {}", said(&ran)))?;
    let session_id = created
        .get("sessionId")
        .and_then(|id| id.as_str())
        .filter(|id| !id.is_empty())
        .context("lody named no session")?;
    Ok(Created {
        session_id: session_id.to_owned(),
    })
}

/// Creates a Lody session for the capture in `bundle_dir`, with the host's `agentLody` settings.
pub fn send_bundle(bundle_dir: &Path) -> Result<Created> {
    let config = crate::env::get().config.agent_lody.clone();
    send_bundle_with(&config, &Places::system(), bundle_dir)
}

/// What a send of the capture in `bundle_dir` would do, without sending: the Agent Role, the
/// project, the CLI that would run, and the files that would go (`hex capture route`).
pub fn explain(bundle_dir: &Path) -> Result<String> {
    let config = crate::env::get().config.agent_lody.clone();
    explain_with(&config, &Places::system(), bundle_dir)
}

fn explain_with(config: &LodyConfig, places: &Places, bundle_dir: &Path) -> Result<String> {
    let (directory, bundle) = load_bundle(bundle_dir)?;
    let (_, attachments) = prompt_of(&bundle, &directory);
    let mut lines = vec![format!("Capture: {}", directory.display())];
    lines.push(match config.role() {
        Some(role) => format!("Lody: a new session for Agent Role {role}"),
        None => "Lody: no Agent Role is set (capture.routing.agentLody.agentRole), so captures are only copied".into(),
    });
    if let Some(project) = config.project() {
        lines.push(format!("Local project: {project}"));
    }
    lines.push(match candidates(config, places) {
        Ok(clis) => format!(
            "CLI: {}",
            clis.iter()
                .map(|cli| format!("{} ({})", cli.program.display(), cli.origin))
                .collect::<Vec<_>>()
                .join(", then ")
        ),
        Err(error) => format!("CLI: {error:#}"),
    });
    lines.push(match attachments.as_slice() {
        [] => "Attaches: nothing".into(),
        files => format!(
            "Attaches: {}",
            files
                .iter()
                .map(|file| file.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    });
    Ok(lines.join("\n"))
}

/// [`send_bundle`] as the outbox retries it: whatever the marker says, the settings now in force
/// decide where it goes.
pub fn resend(bundle_dir: &Path, _destination: &str) -> Result<()> {
    send_bundle(bundle_dir).map(|_| ())
}

fn withdraw_with(
    config: &LodyConfig,
    places: &Places,
    session_id: &str,
    timeout: Duration,
) -> Result<()> {
    plain("the session", session_id)?;
    let clis = candidates(config, places)?;
    for verb in ["cancel", "archive"] {
        let mut args: Vec<OsString> = ["session", verb, session_id, "--json"]
            .into_iter()
            .map(OsString::from)
            .collect();
        args.extend(workspace_args(config)?);
        run_first(&clis, &args, &format!("session {verb}"), timeout)?;
    }
    Ok(())
}

/// Takes a capture's session back: its running turn is cancelled and the session archived (Lody
/// can restore it). Each of the two CLI calls may take `timeout`.
pub fn withdraw(session_id: &str, timeout: Duration) -> Result<()> {
    let config = crate::env::get().config.agent_lody.clone();
    withdraw_with(&config, &Places::system(), session_id, timeout)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hex-lody-test-{name}-{}",
            crate::util::make_bundle_id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn config(role: Option<&str>, cli: Option<&Path>) -> LodyConfig {
        LodyConfig {
            agent_role: role.map(str::to_owned),
            cli: cli.map(|cli| cli.display().to_string()),
            ..LodyConfig::default()
        }
    }

    fn none() -> Places {
        Places {
            path: Vec::new(),
            app_node: PathBuf::from("/nonexistent/Lody"),
            app_cli: PathBuf::from("/nonexistent/index.js"),
        }
    }

    fn bundle(dir: &Path, sheet: &[u8]) -> PathBuf {
        let bundle = dir.join("1.scrawl");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(
            bundle.join("scrawl.json"),
            serde_json::json!({
                "version": 1, "id": "test", "createdAtMs": 0, "durationMs": 4200,
                "target": {
                    "mode": "display", "app": "Board", "displayId": 1,
                    "frame": {"x": 0.0, "y": 0.0, "width": 100.0, "height": 100.0, "scale": 2.0},
                },
                "transcript": {"source": "test", "text": "this card", "segments": []},
                "strokes": [], "marks": [], "events": [],
                "keyframes": [{
                    "index": 0, "t": 0, "reason": "initial", "file": "frames/0.png", "event": "start",
                }],
                "contactSheet": "contact-sheet.png",
            })
            .to_string(),
        )
        .unwrap();
        fs::write(bundle.join("summary.md"), "Fix the header\n").unwrap();
        fs::write(bundle.join("contact-sheet.png"), sheet).unwrap();
        bundle
    }

    #[test]
    fn the_command_line_names_the_role_the_prompt_file_and_every_attachment() {
        let mut config = config(Some(" 前端 "), None);
        config.local_project = Some("~/code/hex".into());
        config.workspace = Some("gabyran".into());
        let args = create_args(
            &config,
            Path::new("/tmp/p.md"),
            &[PathBuf::from("/c/sheet.png"), PathBuf::from("/c/k1.png")],
        )
        .unwrap();
        let args: Vec<_> = args.iter().map(|a| a.to_string_lossy()).collect();
        assert_eq!(
            args,
            [
                "session",
                "create",
                "--json",
                "--agent-role",
                "前端",
                "--local-project",
                "~/code/hex",
                "--workspace",
                "gabyran",
                "--prompt-file",
                "/tmp/p.md",
                "--attach",
                "/c/sheet.png",
                "--attach",
                "/c/k1.png",
            ]
        );
        let plain = create_args(&self::config(Some("role"), None), Path::new("p"), &[]).unwrap();
        assert!(
            !plain
                .iter()
                .any(|a| a == "--local-project" || a == "--workspace" || a == "--attach")
        );
    }

    #[test]
    fn an_unset_role_or_an_option_lookalike_never_builds_a_command() {
        for role in [None, Some("  "), Some("--wait"), Some("a\nb")] {
            let result = create_args(&config(role, None), Path::new("p"), &[]);
            assert!(result.is_err(), "{role:?}");
        }
        let mut config = config(Some("role"), None);
        config.local_project = Some("--worktree".into());
        assert!(create_args(&config, Path::new("p"), &[]).is_err());
        config.local_project = None;
        config.workspace = Some("--json".into());
        assert!(create_args(&config, Path::new("p"), &[]).is_err());
        let nine = vec![PathBuf::from("f"); MAX_ATTACHMENTS + 1];
        assert!(create_args(&self::config(Some("role"), None), Path::new("p"), &nine).is_err());
    }

    #[test]
    fn a_missing_cli_is_an_error_naming_where_it_looked() {
        let dir = temp_dir("missing");
        let error = candidates(&config(Some("r"), None), &none())
            .unwrap_err()
            .to_string();
        assert!(error.contains("wasn't found"), "{error}");
        let gone = dir.join("lody");
        let error = candidates(&config(Some("r"), Some(&gone)), &none())
            .unwrap_err()
            .to_string();
        assert!(error.contains("isn't an executable"), "{error}");
        let bundle = bundle(&dir, b"png");
        let error = send_bundle_with(&config(Some("r"), None), &none(), &bundle)
            .unwrap_err()
            .to_string();
        assert!(error.contains("wasn't found"), "{error}");
    }

    #[test]
    fn lody_is_looked_for_on_path_then_in_the_app_and_a_configured_cli_stands_alone() {
        let dir = temp_dir("order");
        let on_path = script(&dir, "lody", "exit 0");
        let app = script(&dir, "lody-app", "exit 0");
        let index = dir.join("index.js");
        fs::write(&index, "").unwrap();
        let places = Places {
            path: vec![dir.clone()],
            app_node: app.clone(),
            app_cli: index.clone(),
        };
        let found = candidates(&config(Some("r"), None), &places).unwrap();
        let origins: Vec<_> = found.iter().map(|cli| cli.origin).collect();
        assert_eq!(origins, ["PATH", "Lody.app"]);
        assert_eq!(found[0].program, on_path);
        assert!(found[1].electron_as_node);
        assert_eq!(found[1].prefix, [index.into_os_string()]);
        let alone = candidates(&config(Some("r"), Some(&on_path)), &places).unwrap();
        assert_eq!(alone.len(), 1);
        assert_eq!(alone[0].origin, "agentLody.cli");
    }

    #[test]
    fn a_send_makes_a_private_prompt_file_attaches_the_sheet_and_cleans_up() {
        let dir = temp_dir("send");
        let seen = dir.join("seen");
        let fake = script(
            &dir,
            "lody",
            &format!(
                r#"for a in "$@"; do printf '%s\n' "$a"; done > {seen}.args
while [ $# -gt 0 ]; do
  if [ "$1" = "--prompt-file" ]; then
    cp "$2" {seen}.prompt
    ls -l "$2" | cut -c1-10 > {seen}.mode
    echo "$2" > {seen}.path
  fi
  shift
done
echo '{{"ok":true,"sessionId":"ses_abc"}}'"#,
                seen = seen.display()
            ),
        );
        let bundle = bundle(&dir, b"png");
        let created =
            send_bundle_with(&config(Some("前端"), Some(&fake)), &none(), &bundle).unwrap();
        assert_eq!(created.session_id, "ses_abc");
        let read = |ext: &str| fs::read_to_string(format!("{}.{ext}", seen.display())).unwrap();
        let args = read("args");
        let args: Vec<_> = args.lines().collect();
        assert_eq!(
            &args[..5],
            ["session", "create", "--json", "--agent-role", "前端"]
        );
        assert_eq!(args[5], "--prompt-file");
        let sheet = bundle.canonicalize().unwrap().join("contact-sheet.png");
        assert_eq!(&args[7..], ["--attach", sheet.to_str().unwrap()]);
        assert_eq!(read("mode").trim(), "-rw-------");
        let prompt = read("prompt");
        assert!(prompt.starts_with("Fix the header\n"), "{prompt}");
        assert!(prompt.contains("Attached: the contact sheet"), "{prompt}");
        assert!(prompt.contains("This capture is 4200 ms long."), "{prompt}");
        assert!(!Path::new(read("path").trim()).exists());
    }

    #[test]
    fn a_failed_run_still_removes_the_prompt_file() {
        let dir = temp_dir("failed");
        let path_file = dir.join("path");
        let fake = script(
            &dir,
            "lody",
            &format!(
                r#"while [ $# -gt 0 ]; do [ "$1" = "--prompt-file" ] && echo "$2" > {}; shift; done
echo 'Local session runtime is not available: connect ECONNREFUSED' >&2
exit 1"#,
                path_file.display()
            ),
        );
        let error = send_bundle_with(
            &config(Some("r"), Some(&fake)),
            &none(),
            &bundle(&dir, b"png"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("Local session runtime is not available"),
            "{error}"
        );
        let prompt = fs::read_to_string(&path_file).unwrap();
        assert!(!Path::new(prompt.trim()).exists());
    }

    #[test]
    fn a_json_error_is_read_for_its_message() {
        let ran = Ran {
            success: false,
            stdout: r#"{"ok":false,"error":{"message":"Agent Role not found: x"}}"#.into(),
            stderr: String::new(),
        };
        assert_eq!(said(&ran), "Agent Role not found: x");
        let ran = Ran {
            success: false,
            stdout: String::new(),
            stderr: "\nError: boom\nstack\n".into(),
        };
        assert_eq!(said(&ran), "Error: boom");
        // Node's deprecation warning comes first on stderr; the CLI's own answer is on stdout.
        let ran = Ran {
            success: false,
            stdout: r#"{"ok":false,"error":"Multiple workspaces are available; pass --workspace."}"#
                .into(),
            stderr: "(node:1) [DEP0169] DeprecationWarning: `url.parse()`\n(Use `Lody --trace-deprecation ...`)\n"
                .into(),
        };
        assert_eq!(
            said(&ran),
            "Multiple workspaces are available; pass --workspace."
        );
        let ran = Ran {
            success: false,
            stdout: String::new(),
            stderr: "(node:1) [DEP0169] DeprecationWarning\nError: boom\n".into(),
        };
        assert_eq!(said(&ran), "Error: boom");
    }

    #[test]
    fn a_cli_too_old_for_the_options_hands_over_to_the_next() {
        let dir = temp_dir("old");
        script(
            &dir,
            "lody",
            "echo \"error: unknown option '--agent-role'\" >&2; exit 1",
        );
        let new = script(
            &dir,
            "lody-app",
            r#"echo '{"ok":true,"sessionId":"ses_new"}'"#,
        );
        let index = dir.join("index.js");
        fs::write(&index, "").unwrap();
        let places = Places {
            path: vec![dir.clone()],
            app_node: new,
            app_cli: index,
        };
        let bundle = bundle(&dir, b"png");
        let created = send_bundle_with(&config(Some("r"), None), &places, &bundle).unwrap();
        assert_eq!(created.session_id, "ses_new");

        let only_old = Places {
            path: vec![dir.clone()],
            ..none()
        };
        let error = send_bundle_with(&config(Some("r"), None), &only_old, &bundle)
            .unwrap_err()
            .to_string();
        assert!(error.contains("too old"), "{error}");
    }

    #[test]
    fn a_reply_without_a_session_is_a_failure() {
        let dir = temp_dir("noid");
        let fake = script(&dir, "lody", r#"echo '{"ok":true}'"#);
        let bundle = bundle(&dir, b"png");
        assert!(send_bundle_with(&config(Some("r"), Some(&fake)), &none(), &bundle).is_err());
    }

    #[test]
    fn a_capture_without_screen_or_with_a_huge_sheet_goes_without_images() {
        let dir = temp_dir("sheet");
        let path = bundle(&dir, b"png");
        let (directory, loaded) = load_bundle(&path).unwrap();
        let (_, files) = prompt_of(&loaded, &directory);
        assert_eq!(files, [directory.join("contact-sheet.png")]);
        let mut no_screen = loaded.clone();
        no_screen.keyframes.clear();
        let (text, files) = prompt_of(&no_screen, &directory);
        assert!(files.is_empty() && text.contains("No images."));
        let mut escaping = loaded;
        escaping.contact_sheet = "../x.png".into();
        assert!(prompt_of(&escaping, &directory).1.is_empty());
    }

    #[test]
    fn withdrawing_cancels_then_archives_the_session() {
        let dir = temp_dir("withdraw");
        let log = dir.join("log");
        let fake = script(
            &dir,
            "lody",
            &format!(r#"echo "$@" >> {}; echo '{{"ok":true}}'"#, log.display()),
        );
        let mut in_workspace = config(None, Some(&fake));
        in_workspace.workspace = Some("gabyran".into());
        withdraw_with(&in_workspace, &none(), "ses_abc", CLI_TIMEOUT).unwrap();
        assert_eq!(
            fs::read_to_string(&log).unwrap(),
            "session cancel ses_abc --json --workspace gabyran\nsession archive ses_abc --json --workspace gabyran\n"
        );
        assert!(withdraw_with(&config(None, Some(&fake)), &none(), "--all", CLI_TIMEOUT).is_err());
    }

    #[test]
    fn a_cli_that_hangs_is_killed() {
        let dir = temp_dir("hang");
        let fake = script(&dir, "lody", "sleep 30");
        let cli = candidates(&config(None, Some(&fake)), &none())
            .unwrap()
            .remove(0);
        let started = Instant::now();
        let error = run(&cli, &[], Duration::from_millis(200))
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("didn't answer in time"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
