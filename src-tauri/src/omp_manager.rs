use std::collections::HashMap;
use std::io::Write;
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

struct OmpProcess {
    child: Child,
    stdin: ChildStdin,
}

pub struct OmpManager {
    processes: Arc<Mutex<HashMap<u16, OmpProcess>>>,
    /// Maps session_file -> port for dedicated per-session processes.
    session_ports: Arc<Mutex<HashMap<String, u16>>>,
    /// Maps workspace_port -> [dedicated session ports] for cleanup on window close.
    workspace_dedicated: Arc<Mutex<HashMap<u16, Vec<u16>>>>,
    static_dir: PathBuf,
}

struct EmbeddedExtensionResolution {
    path: String,
    /// Tag describing which candidate matched, for diagnostic logging.
    /// Examples: "bundled", "dev:source", "env:OMCOT_EXTENSION".
    source: &'static str,
}

/// Resolve the system omp version by running `omp --version`.
/// Result is cached lazily on first call.
pub fn locked_omp_version() -> &'static str {
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED.get_or_init(|| {
        let bin_name = if cfg!(target_os = "windows") {
            "omp.exe"
        } else {
            "omp"
        };
        let bin = std::env::var("OMP_BIN")
            .ok()
            .map(PathBuf::from)
            .or_else(|| which::which(bin_name).ok());
        match bin {
            Some(bin) => {
                match Command::new(&bin).arg("--version").output() {
                    Ok(output) => {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        stdout.trim().to_string()
                    }
                    Err(e) => {
                        log::warn!("[ompcot] failed to run omp --version: {}", e);
                        "unknown".to_string()
                    }
                }
            }
            None => "unknown (omp not found on PATH)".to_string(),
        }
    })
}

#[cfg(target_os = "windows")]
fn configure_child_process_for_windows(command: &mut Command) {
    // Prevent child `omp.exe` processes from creating a visible console window
    // when Ompcot runs as a GUI app on Windows.
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn configure_child_process_for_windows(_command: &mut Command) {}

/// Build an augmented PATH for child processes.
///
/// `fix_path_env::fix()` is called at app startup and already merges the
/// user's login-shell PATH into this process.  This function is a second
/// safety net: it appends any well-known tool directories that might still
/// be absent (e.g. nvm-managed node versions, Volta, Bun, Mise shims) so
/// that `npm`, `npx`, and friends are always reachable.
///
/// Directories already present in PATH are not duplicated.
fn build_augmented_path() -> String {
    use std::path::{Path, PathBuf};

    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_default();

    #[cfg(not(target_os = "windows"))]
    {
        let mut extras: Vec<PathBuf> = vec![
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/opt/homebrew/sbin"),
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/usr/local/sbin"),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
        ];

        if let Ok(home) = std::env::var("HOME") {
            let h = Path::new(&home);
            extras.push(omp_extension_npm_bin_dir(h));
            extras.push(h.join(".local/bin"));
            extras.push(h.join(".bun/bin"));
            extras.push(h.join(".volta/bin"));
            extras.push(h.join(".cargo/bin"));
            extras.push(h.join(".local/share/mise/shims"));
            // nvm: enumerate all installed node versions
            let nvm_root = h.join(".nvm/versions/node");
            if let Ok(entries) = std::fs::read_dir(nvm_root) {
                for entry in entries.flatten() {
                    let bin = entry.path().join("bin");
                    if bin.is_dir() {
                        extras.push(bin);
                    }
                }
            }
        }

        for extra in extras {
            if !dirs.iter().any(|d| d == &extra) {
                dirs.push(extra);
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        let mut extras: Vec<PathBuf> = Vec::new();
        if let Ok(appdata) = std::env::var("APPDATA") {
            extras.push(Path::new(&appdata).join("npm"));
        }
        if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
            let h = Path::new(&home);
            extras.push(omp_extension_npm_bin_dir(h));
            extras.push(h.join(".cargo").join("bin"));
            extras.push(h.join(".bun").join("bin"));
            extras.push(h.join("scoop").join("shims"));
        }
        for extra in extras {
            if !dirs.iter().any(|d| d == &extra) {
                dirs.push(extra);
            }
        }
    }

    std::env::join_paths(dirs)
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| std::env::var("PATH").unwrap_or_default())
}

fn omp_extension_npm_bin_dir(home: &Path) -> PathBuf {
    home.join(".omp")
        .join("agent")
        .join("npm")
        .join("node_modules")
        .join(".bin")
}

fn log_child_path_diagnostics(context: &str, path: &str) {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok();
    let Some(home) = home else {
        log::info!(
            "[ompcot] child PATH diagnostics: context={} home=<unset> path={}",
            context,
            path
        );
        return;
    };

    let omp_extension_bin = omp_extension_npm_bin_dir(Path::new(&home));
    let hypa_bin = omp_extension_bin.join(if cfg!(target_os = "windows") {
        "hypa.cmd"
    } else {
        "hypa"
    });
    let dirs: Vec<PathBuf> = std::env::split_paths(path).collect();
    let contains_omp_extension_bin = dirs.iter().any(|dir| dir == &omp_extension_bin);

    log::info!(
        "[ompcot] child PATH diagnostics: context={} omp_extension_bin={} exists={} hypa_bin={} hypa_exists={} contains_omp_extension_bin={} path={}",
        context,
        omp_extension_bin.display(),
        omp_extension_bin.is_dir(),
        hypa_bin.display(),
        hypa_bin.is_file(),
        contains_omp_extension_bin,
        path
    );
}

/// Strip a Windows verbatim / extended-length path prefix (`\\?\` or
/// `\\?\UNC\`) from a path string.
///
/// Tauri's `resource_dir()` returns extended-length paths (e.g.
/// `\\?\C:\Users\...\Ompcot\pi\omp.exe`). The embedded omp (Bun 1.3.10,
/// Windows arm64, compiled standalone) segfaults (`Segmentation fault at
/// address 0x18`) when it is launched with — or asked to load an
/// `--extension` from — a `\\?\`-prefixed path. Passing the plain
/// `C:\Users\...` form avoids the crash. This is a no-op on non-Windows
/// platforms and for paths without the prefix.
fn strip_verbatim_prefix(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        // `\\?\UNC\server\share` -> `\\server\share`
        format!(r"\\{}", rest)
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        path.to_string()
    }
}

/// Return a path to the embedded-server extension that is safe to pass as a
/// `--extension` argument to the embedded omp binary.
///
/// On Windows the Bun-compiled omp binary truncates `--extension` values at the
/// first space (e.g. `C:\...\Ompcot\...\embedded-server.mjs` is loaded as
/// `C:\...\OMP`), which then fails to load and segfaults the process. Since OMP
/// Studio always installs under a space-containing path, we mirror the
/// extension file into a space-free directory under the system temp dir and
/// return that path instead. The copy is idempotent (skipped when an existing
/// mirror already matches by length + mtime), so repeated spawns are cheap.
///
/// On non-Windows platforms, or when the path has no space, the original path
/// is returned unchanged.
#[cfg(not(target_os = "windows"))]
fn sanitize_extension_path_for_omp(original: &str) -> String {
    original.to_string()
}

#[cfg(target_os = "windows")]
fn sanitize_extension_path_for_omp(original: &str) -> String {
    if !original.contains(' ') {
        return original.to_string();
    }

    match mirror_to_space_free_dir(Path::new(original)) {
        Ok(mirrored) => {
            log::info!(
                "[ompcot] extension path contains spaces; mirrored to space-free path: {} -> {}",
                original,
                mirrored.display()
            );
            mirrored.to_string_lossy().to_string()
        }
        Err(e) => {
            // Non-fatal: fall back to the original path. Worst case is the
            // pre-existing crash, but we don't want the mirroring step itself
            // to be a new hard failure mode.
            log::warn!(
                "[ompcot] failed to mirror extension to space-free path ({}); using original: {}",
                e,
                original
            );
            original.to_string()
        }
    }
}

/// Copy `src` into `<temp>/ompcot-ext/<filename>` (a space-free directory),
/// skipping the copy when an up-to-date mirror already exists. Returns the
/// mirrored path.
#[cfg(target_os = "windows")]
fn mirror_to_space_free_dir(src: &Path) -> std::io::Result<PathBuf> {
    let file_name = src.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "extension path has no file name",
        )
    })?;

    let mut dest_dir = std::env::temp_dir();
    // Guard: if the temp dir itself contains a space, fall back to a
    // well-known space-free root so the workaround actually helps.
    if dest_dir.to_string_lossy().contains(' ') {
        dest_dir = PathBuf::from("C:\\ProgramData\\ompcot");
    }
    dest_dir.push("ompcot-ext");
    std::fs::create_dir_all(&dest_dir)?;

    let dest = dest_dir.join(file_name);

    if mirror_is_up_to_date(src, &dest) {
        return Ok(dest);
    }

    std::fs::copy(src, &dest)?;
    Ok(dest)
}

/// Cheap freshness check: the mirror is considered current when it exists and
/// matches the source by byte length and modified time. This avoids re-copying
/// the extension on every spawn while still picking up updated builds.
#[cfg(target_os = "windows")]
fn mirror_is_up_to_date(src: &Path, dest: &Path) -> bool {
    let (Ok(src_meta), Ok(dest_meta)) = (std::fs::metadata(src), std::fs::metadata(dest)) else {
        return false;
    };
    if src_meta.len() != dest_meta.len() {
        return false;
    }
    match (src_meta.modified(), dest_meta.modified()) {
        (Ok(src_mtime), Ok(dest_mtime)) => dest_mtime >= src_mtime,
        _ => false,
    }
}

impl OmpManager {
    pub fn new(static_dir: PathBuf) -> Self {
        Self {
            processes: Arc::new(Mutex::new(HashMap::new())),
            session_ports: Arc::new(Mutex::new(HashMap::new())),
            workspace_dedicated: Arc::new(Mutex::new(HashMap::new())),
            static_dir,
        }
    }

    /// Locate the omp binary.
    ///
    /// Lookup order:
    /// 1. `OMP_BIN` env var (escape hatch for testing a different binary).
    /// 2. `omp` on PATH — the user's system-installed omp (Homebrew, etc.).
    ///    This means `brew upgrade omp` automatically picks up the latest.
    ///
    /// Returns `Err` if no binary is found at all.
    fn resolve_omp(&self) -> Result<PathBuf, String> {
        let bin_name = if cfg!(target_os = "windows") {
            "omp.exe"
        } else {
            "omp"
        };

        // 1. Explicit override (rare; useful when smoke-testing a hand-built omp).
        if let Ok(explicit) = std::env::var("OMP_BIN") {
            let candidate = PathBuf::from(explicit.trim());
            if candidate.is_file() {
                return Ok(candidate);
            }
        }

        // 2. System omp on PATH — the common case. `brew upgrade omp` updates this.
        if let Ok(path) = which::which(bin_name) {
            log::info!("[ompcot] using system omp: {}", path.display());
            return Ok(path);
        }

        Err(format!(
            "Could not find omp binary on PATH.\n\n\
             Install omp via:\n  brew install omp\n\n\
             Or set OMP_BIN to the path of your omp binary."
        ))
    }
    /// Locate the embedded-server extension shipped with this build.
    ///
    /// Returns the first existing candidate from this priority order:
    ///
    /// 1. `OMCOT_EXTENSION` env var (explicit override; useful for tests).
    /// 2. Bundled `extensions/embedded-server.mjs` next to `static_dir`. This
    ///    is what shipped Ompcot installs use; the bundle is produced by
    ///    `scripts/build-extensions.js` and is fully self-contained (no
    ///    `node_modules` lookup at runtime).
    /// 3. Source `extensions/embedded-server.ts` next to `static_dir`. Used
    ///    by `tauri dev` where omp loads the raw `.ts` via jiti against the
    ///    repo's `node_modules/`.
    /// 4. *Debug builds only:* repo-relative paths via `CARGO_MANIFEST_DIR`
    ///    and `cwd`. These are gated to debug builds because
    ///    `CARGO_MANIFEST_DIR` is a compile-time string and would otherwise
    ///    silently "work" only on the build machine.
    ///
    /// Returns `Err` (not `Ok(None)`) when nothing is found, so callers can
    /// fail-fast and surface the error to the user instead of silently
    /// spawning a pi that has no `/api` surface.
    fn resolve_embedded_extension_path(&self) -> Result<EmbeddedExtensionResolution, String> {
        if let Ok(explicit) = std::env::var("OMCOT_EXTENSION") {
            let candidate = explicit.trim();
            if !candidate.is_empty() && Path::new(candidate).exists() {
                return Ok(EmbeddedExtensionResolution {
                    path: candidate.to_string(),
                    source: "env:OMCOT_EXTENSION",
                });
            }
        }

        let mut candidates: Vec<(PathBuf, &'static str)> = Vec::new();

        // Compile-time path fallbacks: only useful while developing locally.
        // Prefer the live source in debug builds before any target/debug bundle:
        // that bundle can be stale after frontend/extension edits and causes the
        // dev app to serve old API behavior until a full resource copy happens.
        if cfg!(debug_assertions) {
            candidates.push((
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("..")
                    .join("extensions")
                    .join("embedded-server.ts"),
                "dev:cargo-manifest-dir",
            ));
            if let Ok(cwd) = std::env::current_dir() {
                candidates.push((cwd.join("extensions").join("embedded-server.ts"), "dev:cwd"));
            }
        }

        if let Some(parent) = self.static_dir.parent() {
            candidates.push((
                parent.join("extensions").join("embedded-server.mjs"),
                "bundled",
            ));
            candidates.push((
                parent.join("extensions").join("embedded-server.ts"),
                "dev:source",
            ));
        }

        for (candidate, source) in &candidates {
            if candidate.exists() {
                return Ok(EmbeddedExtensionResolution {
                    path: candidate.to_string_lossy().to_string(),
                    source,
                });
            }
        }

        let tried = candidates
            .into_iter()
            .map(|(p, source)| format!("  - [{}] {}", source, p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        Err(format!(
            "Could not find embedded-server extension. Tried:\n{}\n\n\
             For release builds, this means the .app bundle is missing \
             `extensions/embedded-server.mjs` (run `bun run build:extensions` \
             before `tauri build`). For dev, make sure `extensions/embedded-server.ts` \
             exists in this repo.",
            tried
        ))
    }

    pub fn spawn(&self, cwd: &str, port: u16, session_path: Option<&str>) -> Result<(), String> {
        let pi_bin = self.resolve_omp()?;
        // Tauri resolves resource paths as `\\?\`-prefixed extended-length
        // paths. Bun (the embedded omp runtime) segfaults on Windows arm64 when
        // launched from such a path, so normalize the binary path and every
        // path-shaped argument/env we hand to it back to the plain form.
        let pi_bin_str = strip_verbatim_prefix(&pi_bin.to_string_lossy());
        let static_dir = strip_verbatim_prefix(&self.static_dir.to_string_lossy());
        let cwd = strip_verbatim_prefix(cwd);

        // We treat a missing embedded-server extension as a hard error
        // rather than continuing to spawn omp without `--extension`. Without
        // the extension, omp runs as a plain RPC process with no
        // `/api/sessions` or `/ws`, which the web UI then renders as
        // "Failed to load sessions" / "Disconnected" — a confusing soft
        // failure that hides the real bundling bug.
        let extension = self.resolve_embedded_extension_path()?;
        log::info!(
            "[ompcot] embedded-server resolved: source={} path={}",
            extension.source,
            extension.path
        );

        // The embedded omp (Bun-compiled standalone) mis-parses `--extension`
        // paths that contain spaces on Windows: it truncates at the first
        // space, so `...\Ompcot\extensions\embedded-server.mjs` is loaded
        // as `...\OMP`, which then fails to load and crashes the process
        // (segfault) during extension-load error handling. The primary fix is
        // the space-free `productName` ("Ompcot") so the install dir has no
        // space; this mirroring remains as a defensive fallback for paths that
        // can still contain spaces out of our control (e.g. a Windows username
        // like `C:\Users\Shi Xin\...`). Work around it by mirroring the
        // extension into a space-free directory and passing that path instead.
        //
        // Also strip the `\\?\` verbatim prefix first: Bun on Windows arm64
        // segfaults when loading an extension from an extended-length path.
        let extension_path =
            sanitize_extension_path_for_omp(&strip_verbatim_prefix(&extension.path));

        let mut args: Vec<String> = vec![
            "--extension".to_string(),
            extension_path,
            "--mode".to_string(),
            "rpc".to_string(),
        ];
        if let Some(session) = session_path {
            args.push("--session".to_string());
            args.push(session.to_string());
        }

        log::info!(
            "[ompcot] spawning pi: bin={} args={:?} cwd={} port={} static_dir={}",
            pi_bin_str,
            args,
            cwd,
            port,
            static_dir
        );

        let augmented_path = build_augmented_path();
        log_child_path_diagnostics("spawn", &augmented_path);

        let mut child = Command::new(&pi_bin_str);
        configure_child_process_for_windows(&mut child);
        child
            .args(&args)
            .current_dir(&cwd)
            .env("PATH", augmented_path)
            .env("OMCOT_STATIC_DIR", &static_dir)
            .env("OMCOT_PORT", port.to_string())
            .env("OMCOT_OMP_VERSION", locked_omp_version())
            .stdin(Stdio::piped())
            // Drop stdout: omp emits RPC frames on it that we don't consume here, and
            // letting it fill an unread pipe would eventually block the child.
            .stdout(Stdio::null())
            // Inherit stderr so omp's startup/runtime errors are visible in the same
            // terminal running `bun run dev` — critical for diagnosing failures of
            // new_session / open_workspace that would otherwise be silent.
            .stderr(Stdio::inherit());

        let spawn_started_at = Instant::now();
        let mut child = child.spawn().map_err(|e| {
            format!(
                "Failed to spawn omp ({}): {}. \
                 Make sure omp is installed (brew install omp) and on your PATH.",
                pi_bin.display(),
                e,
            )
        })?;
        log::info!(
            "[ompcot] omp process spawned: port={} pid={} elapsed_ms={}",
            port,
            child.id(),
            spawn_started_at.elapsed().as_millis()
        );
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Failed to get pi stdin".to_string())?;

        let mut lock = self.processes.lock().unwrap();
        lock.insert(port, OmpProcess { child, stdin });

        Ok(())
    }

    /// Send an RPC command to a omp instance (JSON line on stdin)
    pub fn send_rpc(&self, port: u16, cmd: serde_json::Value) -> Result<(), String> {
        let mut lock = self.processes.lock().unwrap();
        let proc = lock
            .get_mut(&port)
            .ok_or_else(|| format!("No omp instance on port {}", port))?;
        let mut line = cmd.to_string();
        line.push('\n');
        proc.stdin
            .write_all(line.as_bytes())
            .map_err(|e| e.to_string())
    }

    /// Returns `Some(exit_status_string)` if the process has already exited, `None` if still running.
    pub fn check_exited(&self, port: u16) -> Option<String> {
        let mut lock = self.processes.lock().unwrap();
        let proc = lock.get_mut(&port)?;
        match proc.child.try_wait() {
            Ok(Some(status)) => Some(format!("{}", status)),
            _ => None,
        }
    }

    pub fn kill(&self, port: u16) {
        let mut lock = self.processes.lock().unwrap();
        if let Some(mut proc) = lock.remove(&port) {
            let _ = proc.child.kill();
        }
    }

    pub fn kill_all(&self) {
        let mut lock = self.processes.lock().unwrap();
        for (_, mut proc) in lock.drain() {
            let _ = proc.child.kill();
        }
    }

    /// Spawn (or reuse) a dedicated omp process for a specific session file,
    /// so it can run concurrently with the workspace's primary process.
    /// Returns the port the dedicated process is listening on.
    pub fn spawn_session_dedicated(
        &self,
        workspace_port: u16,
        session_file: String,
        cwd: &str,
    ) -> Result<u16, String> {
        {
            let sp = self.session_ports.lock().unwrap();
            if let Some(&port) = sp.get(&session_file) {
                return Ok(port);
            }
        }
        let port = self.next_port();
        self.spawn(cwd, port, Some(&session_file))?;
        {
            let mut sp = self.session_ports.lock().unwrap();
            sp.insert(session_file, port);
        }
        {
            let mut wd = self.workspace_dedicated.lock().unwrap();
            wd.entry(workspace_port).or_default().push(port);
        }
        Ok(port)
    }

    /// Kill all dedicated session processes spawned for a workspace port.
    /// Called when the workspace window is destroyed.
    pub fn kill_workspace_dedicated(&self, workspace_port: u16) {
        let dedicated_ports = {
            let mut wd = self.workspace_dedicated.lock().unwrap();
            wd.remove(&workspace_port).unwrap_or_default()
        };
        for port in &dedicated_ports {
            self.kill(*port);
        }
        if !dedicated_ports.is_empty() {
            let port_set: std::collections::HashSet<u16> = dedicated_ports.into_iter().collect();
            let mut sp = self.session_ports.lock().unwrap();
            sp.retain(|_, v| !port_set.contains(v));
        }
    }

    pub fn next_port(&self) -> u16 {
        let lock = self.processes.lock().unwrap();
        let mut port = 47821u16;
        while lock.contains_key(&port) || is_port_in_use(port) {
            port += 1;
        }
        port
    }

    /// Run `pi <args...>` with the embedded binary and return stdout.
    /// Used by Settings UI package management operations (install/remove/list).
    pub fn run_pi_command(&self, args: &[String]) -> Result<String, String> {
        let pi_bin = self.resolve_omp()?;
        let pi_bin_str = strip_verbatim_prefix(&pi_bin.to_string_lossy());
        let augmented_path = build_augmented_path();
        log_child_path_diagnostics("run_pi_command", &augmented_path);
        let mut command = Command::new(&pi_bin_str);
        configure_child_process_for_windows(&mut command);
        command
            .args(args)
            .env("PATH", augmented_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = command.output().map_err(|e| {
            format!(
                "Failed to run omp command ({} {:?}): {}",
                pi_bin_str, args, e
            )
        })?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let details = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("exit status {}", output.status)
        };
        Err(format!(
            "Omp command failed: {} {:?}: {}",
            pi_bin_str, args, details
        ))
    }

    /// List installed package sources via `omp plugin list --json`.
    ///
    /// omp 17 removed the top-level `omp list` command, so the previous
    /// text-scraping implementation always failed with
    /// ``error: `omp list` is not a top-level command``. The JSON payload is:
    ///
    /// ```json
    /// { "npm": [{ "name": "pkg", "version": "1.0.0", ... }],
    ///   "marketplace": [{ "id": "pkg", "scope": "user", ... }] }
    /// ```
    ///
    /// npm entries are reported as `npm:<name>` so they match the source
    /// strings the browse UI builds in `browseSourceFor`; marketplace
    /// entries are reported by bare id.
    pub fn list_configured_package_sources(&self) -> Result<Vec<String>, String> {
        let args = vec![
            "plugin".to_string(),
            "list".to_string(),
            "--json".to_string(),
        ];
        let output = self.run_pi_command(&args)?;
        let parsed: serde_json::Value = serde_json::from_str(output.trim())
            .map_err(|e| format!("Failed to parse `omp plugin list --json` output: {}", e))?;

        let mut sources = Vec::new();
        if let Some(entries) = parsed.get("npm").and_then(serde_json::Value::as_array) {
            for entry in entries {
                if let Some(name) = entry.get("name").and_then(serde_json::Value::as_str) {
                    sources.push(format!("npm:{}", name));
                }
            }
        }
        if let Some(entries) = parsed
            .get("marketplace")
            .and_then(serde_json::Value::as_array)
        {
            for entry in entries {
                if let Some(id) = entry.get("id").and_then(serde_json::Value::as_str) {
                    sources.push(id.to_string());
                }
            }
        }
        Ok(sources)
    }

    pub fn install_package_source(&self, source: &str) -> Result<(), String> {
        let args = vec!["install".to_string(), strip_source_scheme(source).to_string()];
        let _ = self.run_pi_command(&args)?;
        Ok(())
    }

    /// omp has no top-level `remove` command — unknown arguments are treated
    /// as a prompt, so the previous implementation started an agent turn
    /// instead of uninstalling anything.
    pub fn remove_package_source(&self, source: &str) -> Result<(), String> {
        let args = vec![
            "plugin".to_string(),
            "uninstall".to_string(),
            strip_source_scheme(source).to_string(),
        ];
        let _ = self.run_pi_command(&args)?;
        Ok(())
    }
}

/// The UI addresses npm packages as `npm:<name>` (see `browseSourceFor` in
/// `public/app.js`); omp's CLI expects the bare spec.
fn strip_source_scheme(source: &str) -> &str {
    source.strip_prefix("npm:").unwrap_or(source)
}

pub fn is_port_in_use(port: u16) -> bool {
    std::net::TcpListener::bind(format!("0.0.0.0:{}", port)).is_err()
}

pub async fn wait_for_health(port: u16, timeout_secs: u64) -> Result<(), String> {
    wait_for_endpoint(port, "/api/health", timeout_secs).await
}

/// Wait for a specific HTTP endpoint on the omp instance to respond with a non-5xx status.
/// Useful when we need to confirm the API surface the frontend will hit first (e.g. /api/sessions)
/// is ready before navigating, avoiding cold-start races where /api/health is up but route
/// handlers are still warming.
pub async fn wait_for_endpoint(port: u16, path: &str, timeout_secs: u64) -> Result<(), String> {
    let url = format!("http://localhost:{}{}", port, path);
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if std::time::Instant::now() > deadline {
            return Err(format!("Timed out waiting for {} on port {}", path, port));
        }
        if let Ok(resp) = reqwest::get(&url).await {
            if resp.status().as_u16() < 500 {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};

    #[test]
    fn augmented_path_includes_omp_extension_npm_bin() {
        let home = std::env::var("HOME").expect("HOME must be set for this test");
        let expected = Path::new(&home)
            .join(".omp")
            .join("agent")
            .join("npm")
            .join("node_modules")
            .join(".bin");

        let path = build_augmented_path();
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();

        assert!(
            dirs.iter().any(|dir| dir == &expected),
            "expected augmented PATH to include {}",
            expected.display()
        );
    }

    #[test]
    fn port_in_use_detects_unspecified_ipv4_listener() {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
            .expect("bind ephemeral port");
        let port = listener.local_addr().expect("listener addr").port();

        assert!(is_port_in_use(port));
    }
}
