//! Status, health, and explicit repair planning for the `/lhc` surface.
//!
//! When LHC is off / capture inactive, reports are cheap with no SQLite I/O.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use lhc::sdk::OpResult;
use lhc::shared_tech::thread_migrate::is_supported_thread_schema_version;
use lhc::shared_tech::{get_schema_version, open_database};

use crate::compact::CompactMode;
use crate::equivalence::{EquivalenceSnapshot, equivalence_armed, equivalence_snapshot};
use crate::gating::{is_enabled, lhc_root};
use crate::inference::inference_sampler_registered;
use crate::runtime_config::{ConfigSource, applied_config, config_parse_error};
use crate::serving::last_serve_outcome;
use crate::session::{encode_session_id_for_path, thread_file_path};
use crate::tee::capture_active;
use crate::{any_capture_active, lookup_session};

/// How the active request-context engine is labeled for the user.
///
/// Derived from the **last serve turn**, not from capture registration (Y2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextEngine {
    /// Last serve turn substituted an LHC-built conversation body.
    Lhc,
    /// Last serve turn used native (fail-open or capture inactive), or LHC is off.
    Native,
    /// Capture may be registered, but no serve turn has consulted LHC yet.
    NoServeTurnYet,
}

impl ContextEngine {
    pub fn as_str(self) -> &'static str {
        match self {
            ContextEngine::Lhc => "LHC",
            ContextEngine::Native => "native",
            ContextEngine::NoServeTurnYet => "(no serve turn yet)",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LhcStatusReport {
    pub enabled: bool,
    pub enabled_source: ConfigSource,
    pub compact_mode: CompactMode,
    pub compact_source: ConfigSource,
    pub root: PathBuf,
    pub root_source: ConfigSource,
    pub session_id: String,
    pub capture_active: bool,
    pub context_engine: ContextEngine,
    /// Fail-open reason from the last serve turn, when engine is native after a consult.
    pub last_serve_reason: Option<&'static str>,
    /// Whether ModelCall compaction has a registered inference sampler (Z2).
    pub inference_compact_available: bool,
    pub thread_path: Option<PathBuf>,
    pub storage_bytes: Option<u64>,
    pub event_count: Option<usize>,
    pub last_event_summary: Option<String>,
    pub view_status_line: Option<String>,
    pub last_compact_line: Option<String>,
    pub equivalence: Option<EquivalenceSnapshot>,
    pub equivalence_armed: bool,
    pub config_parse_error: Option<String>,
    pub health: LhcHealthReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LhcHealthReport {
    pub ok: bool,
    pub storage_reachable: bool,
    pub schema_present: bool,
    pub worker_alive: bool,
    pub failed_derivations: Option<i64>,
    pub queue_backlog: Option<u64>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct LhcRepairPlan {
    pub session_id: String,
    pub actions: Vec<LhcRepairAction>,
}

#[derive(Debug, Clone)]
pub struct LhcRepairAction {
    pub id: &'static str,
    pub description: String,
    pub delete_paths: Vec<PathBuf>,
}

fn pending_repair_plans() -> &'static Mutex<HashMap<String, LhcRepairPlan>> {
    static PLANS: OnceLock<Mutex<HashMap<String, LhcRepairPlan>>> = OnceLock::new();
    PLANS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn provenance_enabled() -> (bool, ConfigSource) {
    let enabled = is_enabled();
    let source = applied_config().map(|a| a.enabled.source).unwrap_or(
        if std::env::var_os("GROK_LHC").is_some() {
            ConfigSource::Env
        } else {
            ConfigSource::Default
        },
    );
    (enabled, source)
}

fn engine_from_last_serve(
    session_id: &str,
    capture: bool,
) -> (ContextEngine, Option<&'static str>) {
    // Without capture, never claim LHC — even if a stale outcome slipped through (Z1).
    if !capture {
        return (ContextEngine::Native, None);
    }
    match last_serve_outcome(session_id) {
        Some(o) if o.substituted => (ContextEngine::Lhc, None),
        Some(o) => (ContextEngine::Native, o.reason),
        None => (ContextEngine::NoServeTurnYet, None),
    }
}

/// SQLite main-DB header magic (16 bytes). Do not read the whole file (Z4).
const SQLITE_HEADER_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Validate that `path` is a real LHC thread SQLite with a supported schema.
///
/// Does not infer schema from filename presence (Y5). Vendored `open_database`
/// panics on some corrupt files (PRAGMA after a soft open), so we magic-check
/// first and catch_unwind around the open path.
fn validate_thread_schema(path: &Path) -> Result<(), String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| format!("open failed: {e}"))?;
    let mut header = [0u8; 16];
    let n = file
        .read(&mut header)
        .map_err(|e| format!("header read failed: {e}"))?;
    if n < SQLITE_HEADER_MAGIC.len() || header != *SQLITE_HEADER_MAGIC {
        return Err("file is not a SQLite database".into());
    }
    let path_str = path.to_string_lossy().into_owned();
    let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match open_database(&path_str) {
            OpResult::Ok { value: db } => match get_schema_version(&db) {
                OpResult::Ok { value: version } => Ok(version),
                OpResult::Err { error } => {
                    Err(format!("schema version read failed: {}", error.reason))
                }
            },
            OpResult::Err { error } => Err(format!("open failed: {}", error.reason)),
        }
    }));
    let version = match opened {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err("sqlite open panicked (corrupt database)".into()),
    };
    if !is_supported_thread_schema_version(version) {
        return Err(format!("unsupported schema version {version}"));
    }
    Ok(())
}

/// Build a status report. Off-path does no storage I/O.
pub async fn status_report(session_id: &str) -> LhcStatusReport {
    let (enabled, enabled_source) = provenance_enabled();
    let applied = applied_config();
    let compact_mode = crate::compact::compact_mode();
    let compact_source = applied
        .map(|a| a.compact.source)
        .unwrap_or(ConfigSource::Default);
    // Live path always follows current env / default (`lhc_root`), not the
    // first-applied OnceLock snapshot — otherwise a stale apply masks
    // `GROK_LHC_ROOT` changes in tests and mid-process overrides.
    let root = lhc_root();
    // Provenance: env only when the variable is actually set. Defaults must
    // not have been written into env (Y3); fall back to the applied snapshot.
    let root_source = if std::env::var_os("GROK_LHC_ROOT").is_some() {
        ConfigSource::Env
    } else {
        applied
            .map(|a| a.root.source)
            .unwrap_or(ConfigSource::Default)
    };

    let active = capture_active(session_id);
    let (context_engine, last_serve_reason) = engine_from_last_serve(session_id, active);
    let inference_compact_available = inference_sampler_registered(session_id);
    let parse_err = config_parse_error();

    if !enabled && !any_capture_active() {
        return LhcStatusReport {
            enabled: false,
            enabled_source,
            compact_mode: CompactMode::Off,
            compact_source,
            root,
            root_source,
            session_id: session_id.to_string(),
            capture_active: false,
            context_engine: ContextEngine::Native,
            last_serve_reason: None,
            inference_compact_available: false,
            thread_path: None,
            storage_bytes: None,
            event_count: None,
            last_event_summary: None,
            view_status_line: None,
            last_compact_line: None,
            equivalence: None,
            equivalence_armed: equivalence_armed(),
            config_parse_error: parse_err,
            health: LhcHealthReport {
                ok: true,
                storage_reachable: true,
                schema_present: true,
                worker_alive: false,
                failed_derivations: None,
                queue_backlog: None,
                notes: vec!["LHC is off — no capture worker, no storage I/O.".into()],
            },
        };
    }

    let thread_path = thread_file_path(&root, session_id);
    let storage_bytes = std::fs::metadata(&thread_path).ok().map(|m| m.len());

    let handle = lookup_session(session_id);
    let worker_alive = handle.is_some();
    let queue_backlog = handle.as_ref().map(|h| h.queue_depth() as u64);

    let mut event_count = None;
    let mut last_event_summary = None;
    let mut view_status_line = None;
    let mut failed_derivations = None;
    // Do not infer schema from filename — validate (Y5).
    let mut schema_present = false;
    let mut notes = Vec::new();
    // Worker inspection timed out or errored — must fail health (Z3).
    let mut inspection_degraded = false;
    if let Some(ref err) = parse_err {
        notes.push(format!("[lhc] config parse error: {err}"));
    }
    if active && !inference_compact_available {
        notes.push(
            "ModelCall compaction sampler not registered — compact may be unavailable \
             until a new session spawn (or re-run /lhc on with a live sampler)."
                .into(),
        );
    }

    if let Some(h) = handle.as_ref() {
        // Bound worker RPCs — a stuck capture worker must not hang `/lhc`.
        const STATUS_RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
        match tokio::time::timeout(STATUS_RPC_TIMEOUT, h.list_events()).await {
            Ok(Ok(events)) => {
                schema_present = true;
                event_count = Some(events.len());
                if let Some(last) = events.last() {
                    last_event_summary = Some(format!(
                        "order={} kind={}",
                        last.event_order(),
                        last.event_kind().as_str()
                    ));
                }
            }
            Ok(Err(e)) => {
                notes.push(format!("list_events failed: {e}"));
                schema_present = false;
                inspection_degraded = true;
            }
            Err(_) => {
                notes.push("list_events timed out — worker may be stuck".into());
                // Prefer not to open the DB while the worker holds it.
                schema_present = true;
                inspection_degraded = true;
            }
        }
        match tokio::time::timeout(STATUS_RPC_TIMEOUT, h.get_view_status()).await {
            Ok(Ok(vs)) => {
                failed_derivations = Some(vs.derivation.failed);
                view_status_line = Some(format!(
                    "pending={} failed={} blocked={} compact_recommended={} tail_tokens={}",
                    vs.derivation.pending,
                    vs.derivation.failed,
                    vs.derivation.blocked,
                    vs.compact_recommended,
                    vs.tail_tokens
                ));
                if vs.derivation.failed > 0 {
                    notes.push(format!(
                        "{} failed derivation(s) — see /lhc repair",
                        vs.derivation.failed
                    ));
                }
            }
            Ok(Err(e)) => {
                notes.push(format!("view status unavailable: {e}"));
                inspection_degraded = true;
            }
            Err(_) => {
                notes.push("view status timed out — worker may be stuck".into());
                inspection_degraded = true;
            }
        }
    } else if thread_path.exists() {
        match validate_thread_schema(&thread_path) {
            Ok(()) => schema_present = true,
            Err(e) => {
                schema_present = false;
                notes.push(format!("thread schema invalid: {e}"));
            }
        }
        if enabled {
            notes.push(
                "Process enabled but no capture worker for this session (spawn skipped or /lhc off)."
                    .into(),
            );
        }
    } else if enabled {
        notes.push(
            "Process enabled but no capture worker for this session (spawn skipped or /lhc off)."
                .into(),
        );
    }

    let storage_reachable = root.exists() || storage_bytes.is_some();
    if !storage_reachable {
        notes.push(format!("storage root missing: {}", root.display()));
    }

    let backlog_unbounded =
        queue_backlog.is_some_and(|d| d > (crate::CAPTURE_QUEUE_CAP as u64 * 3 / 4));
    if backlog_unbounded {
        notes.push(format!(
            "capture queue depth high ({:?} / {})",
            queue_backlog,
            crate::CAPTURE_QUEUE_CAP
        ));
    }

    let ok = storage_reachable
        && (schema_present || !thread_path.exists())
        && failed_derivations.unwrap_or(0) == 0
        && !backlog_unbounded
        && (!active || worker_alive)
        && !inspection_degraded;

    LhcStatusReport {
        enabled,
        enabled_source,
        compact_mode,
        compact_source,
        root,
        root_source,
        session_id: session_id.to_string(),
        capture_active: active,
        context_engine,
        last_serve_reason,
        inference_compact_available,
        thread_path: Some(thread_path),
        storage_bytes,
        event_count,
        last_event_summary,
        view_status_line,
        last_compact_line: None,
        equivalence: if active && equivalence_armed() {
            Some(equivalence_snapshot())
        } else {
            None
        },
        equivalence_armed: equivalence_armed(),
        config_parse_error: parse_err,
        health: LhcHealthReport {
            ok,
            storage_reachable,
            schema_present,
            worker_alive,
            failed_derivations,
            queue_backlog,
            notes,
        },
    }
}

pub async fn health_check(session_id: &str) -> LhcHealthReport {
    status_report(session_id).await.health
}

/// Plan repair actions and bind them for a later [`execute_repair`] (Y4).
///
/// Never deletes until confirm. Confirm must match an action id from this
/// displayed plan — a fresh re-plan is not enough.
pub async fn plan_repair(session_id: &str) -> LhcRepairPlan {
    let root = lhc_root();
    let thread = thread_file_path(&root, session_id);
    let encoded = encode_session_id_for_path(session_id);
    let orphan_dir = root.join("threads").join(format!("orphan-{encoded}"));

    let mut actions = Vec::new();
    let health = health_check(session_id).await;

    if health.failed_derivations.unwrap_or(0) > 0 {
        actions.push(LhcRepairAction {
            id: "rebuild-view",
            description: "Ask LHC to rebuild the view on next read (event log kept). \
                 Does not delete files or touch native conversation."
                .into(),
            delete_paths: vec![],
        });
    }

    if thread.exists() && (!health.schema_present || !health.ok) {
        actions.push(LhcRepairAction {
            id: "delete-thread-db",
            description: format!(
                "DELETE the LHC thread SQLite at {} and re-bootstrap from native \
                 history on next /lhc on. Native session files are NOT deleted. \
                 Irreversible for LHC-only history.",
                thread.display()
            ),
            delete_paths: vec![thread.clone()],
        });
    }

    if orphan_dir.exists() && !capture_active(session_id) {
        actions.push(LhcRepairAction {
            id: "remove-orphan-session-dir",
            description: format!("Remove orphaned directory {}.", orphan_dir.display()),
            delete_paths: vec![orphan_dir],
        });
    }

    if actions.is_empty() {
        actions.push(LhcRepairAction {
            id: "noop",
            description: "No repair actions indicated — store looks healthy or LHC is off.".into(),
            delete_paths: Vec::new(),
        });
    }

    let plan = LhcRepairPlan {
        session_id: session_id.to_string(),
        actions,
    };
    if let Ok(mut g) = pending_repair_plans().lock() {
        g.insert(session_id.to_string(), plan.clone());
    }
    plan
}

/// Execute a repair action from the plan last displayed by [`plan_repair`].
///
/// Requires a prior `/lhc repair` for this session whose action id matches.
/// Destructive paths use the stored plan's enumerated delete list — never a
/// freshly constructed plan (Y4).
pub async fn execute_repair(session_id: &str, action_id: &str) -> Result<String, String> {
    let plan = pending_repair_plans()
        .lock()
        .ok()
        .and_then(|mut g| g.remove(session_id))
        .ok_or_else(|| {
            "no displayed repair plan for this session; run `/lhc repair` first".to_string()
        })?;

    let action = plan
        .actions
        .iter()
        .find(|a| a.id == action_id)
        .ok_or_else(|| {
            format!(
                "action {action_id:?} was not in the displayed plan; run `/lhc repair` again \
                 (shown: {})",
                plan.actions
                    .iter()
                    .map(|a| a.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

    if action.id == "noop" || action.id == "rebuild-view" {
        return Ok(format!(
            "Repair '{id}': {desc} (no files deleted).",
            id = action.id,
            desc = action.description
        ));
    }

    let mut deleted = Vec::new();
    for path in &action.delete_paths {
        match std::fs::remove_file(path).or_else(|_| remove_dir_all_if_dir(path)) {
            Ok(()) => deleted.push(path.display().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("failed to delete {}: {e}", path.display())),
        }
    }
    Ok(format!(
        "Repair '{id}' executed. Deleted: {deleted:?}. Native conversation untouched.",
        id = action.id
    ))
}

fn remove_dir_all_if_dir(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "not a directory",
        ))
    }
}

pub fn format_status_report(r: &LhcStatusReport) -> String {
    if !r.enabled && !r.capture_active {
        let mut out = format!(
            "**LHC:** off (source: {})\n\n\
             **Active context engine:** {}\n\n\
             No capture worker. Request context is built by the native host path.\n\
             Enable via `GROK_LHC=1` or `[lhc] enabled = true` in config.toml \
             (env wins over config).",
            r.enabled_source.as_str(),
            r.context_engine.as_str(),
        );
        if let Some(ref err) = r.config_parse_error {
            out.push_str(&format!("\n\n**Config error:** {err}\n"));
        }
        return out;
    }

    let mut out = String::new();
    out.push_str(&format!(
        "**LHC:** {} (source: {})\n\n\
         **Active context engine:** {}\n",
        if r.enabled { "on" } else { "off" },
        r.enabled_source.as_str(),
        r.context_engine.as_str(),
    ));
    match r.context_engine {
        ContextEngine::Lhc => {
            out.push_str("\n**Last serve turn:** substituted (LHC body)\n");
        }
        ContextEngine::Native => {
            if let Some(reason) = r.last_serve_reason {
                out.push_str(&format!(
                    "\n**Last serve turn:** native fail-open ({reason})\n"
                ));
            } else {
                out.push_str("\n**Last serve turn:** native (capture inactive or not consulted)\n");
            }
        }
        ContextEngine::NoServeTurnYet => {
            out.push_str(
                "\n**Last serve turn:** none yet — engine label waits for the next model turn\n",
            );
        }
    }
    out.push_str(&format!(
        "\n**Compaction mode:** {:?} (source: {})\n\n\
         **Storage root:** {} (source: {})\n\n\
         **Session:** {}\n\n\
         **Capture active:** {}\n\n\
         **ModelCall compact:** {}\n",
        r.compact_mode,
        r.compact_source.as_str(),
        r.root.display(),
        r.root_source.as_str(),
        r.session_id,
        r.capture_active,
        if r.inference_compact_available {
            "available"
        } else if r.capture_active {
            "unavailable — no inference sampler (start a new session to restore)"
        } else {
            "n/a (capture inactive)"
        },
    ));
    if let Some(ref p) = r.thread_path {
        out.push_str(&format!("\n**Thread file:** {}\n", p.display()));
    }
    if let Some(bytes) = r.storage_bytes {
        out.push_str(&format!("\n**Storage size:** {bytes} bytes\n"));
    }
    if let Some(n) = r.event_count {
        out.push_str(&format!("\n**Events:** {n}\n"));
    }
    if let Some(ref last) = r.last_event_summary {
        out.push_str(&format!("\n**Last event:** {last}\n"));
    }
    if let Some(ref vs) = r.view_status_line {
        out.push_str(&format!("\n**View status:** {vs}\n"));
    }
    if let Some(ref lc) = r.last_compact_line {
        out.push_str(&format!("\n**Last compact:** {lc}\n"));
    }
    out.push_str(&format!(
        "\n**Equivalence instrument:** {}\n",
        if r.equivalence_armed {
            "armed"
        } else {
            "disarmed"
        }
    ));
    if let Some(eq) = r.equivalence {
        out.push_str(&format!(
            "  compared={} fallback={} structural={} informational={}\n",
            eq.turns_served_and_compared,
            eq.turns_fallen_back,
            eq.structural_divergences,
            eq.informational_divergences,
        ));
    }
    if let Some(ref err) = r.config_parse_error {
        out.push_str(&format!("\n**Config error:** {err}\n"));
    }
    out.push_str(&format!(
        "\n**Health:** {}\n",
        if r.health.ok { "ok" } else { "degraded" }
    ));
    for n in &r.health.notes {
        out.push_str(&format!("  - {n}\n"));
    }
    out.push_str(
        "\nCommands: `/lhc` status · `/lhc health` · `/lhc repair` · \
         `/lhc repair confirm <id>` · `/lhc off` · `/lhc on`",
    );
    out
}

pub fn format_health_report(h: &LhcHealthReport) -> String {
    let mut out = format!(
        "**LHC health:** {}\n\
         storage_reachable={} schema_present={} worker_alive={}\n",
        if h.ok { "ok" } else { "degraded" },
        h.storage_reachable,
        h.schema_present,
        h.worker_alive,
    );
    if let Some(f) = h.failed_derivations {
        out.push_str(&format!("failed_derivations={f}\n"));
    }
    if let Some(q) = h.queue_backlog {
        out.push_str(&format!("queue_depth={q}\n"));
    }
    for n in &h.notes {
        out.push_str(&format!("- {n}\n"));
    }
    out
}

pub fn format_repair_plan(p: &LhcRepairPlan) -> String {
    let mut out = format!("**LHC repair plan** for session {}:\n\n", p.session_id);
    for a in &p.actions {
        out.push_str(&format!("- `{}`: {}\n", a.id, a.description));
        for path in &a.delete_paths {
            out.push_str(&format!("    would delete: {}\n", path.display()));
        }
    }
    out.push_str(
        "\nNothing has been deleted. To execute: `/lhc repair confirm <id>`\n\
         Native conversation / `updates.jsonl` are never touched by these actions.\n\
         Confirm only executes an action id from this displayed plan.",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gating::env_lock;

    #[test]
    fn status_when_off_is_cheap_and_native() {
        let _g = env_lock();
        let prev = std::env::var_os("GROK_LHC");
        unsafe { std::env::remove_var("GROK_LHC") };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let r = rt.block_on(status_report("status-off-sess"));
        assert!(!r.enabled);
        assert_eq!(r.context_engine, ContextEngine::Native);
        assert!(r.event_count.is_none());
        assert!(format_status_report(&r).contains("Active context engine:** native"));
        match prev {
            Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
            None => unsafe { std::env::remove_var("GROK_LHC") },
        }
    }

    #[test]
    fn garbage_thread_file_is_not_healthy_schema() {
        let _g = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("GROK_LHC");
        let prev_root = std::env::var_os("GROK_LHC_ROOT");
        unsafe {
            std::env::set_var("GROK_LHC", "1");
            std::env::set_var("GROK_LHC_ROOT", dir.path());
        }
        let sid = "garbage-schema-sess";
        let thread = thread_file_path(dir.path(), sid);
        std::fs::create_dir_all(thread.parent().unwrap()).unwrap();
        std::fs::write(&thread, b"not a sqlite database").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let h = rt.block_on(health_check(sid));
        assert!(!h.schema_present, "garbage file must not claim schema");
        assert!(!h.ok, "corrupt store must be degraded");
        match prev {
            Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
            None => unsafe { std::env::remove_var("GROK_LHC") },
        }
        match prev_root {
            Some(v) => unsafe { std::env::set_var("GROK_LHC_ROOT", v) },
            None => unsafe { std::env::remove_var("GROK_LHC_ROOT") },
        }
    }

    #[test]
    fn schema_header_check_reads_only_fixed_prefix() {
        // Z4: validation must accept a file that is huge after a valid header
        // without needing to load the whole body into memory. We only assert
        // the magic path rejects short/non-magic files and accepts magic+junk
        // far enough to open (open may still fail version check).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.sqlite");
        let mut bytes = SQLITE_HEADER_MAGIC.to_vec();
        bytes.extend(std::iter::repeat_n(0u8, 64 * 1024));
        std::fs::write(&path, &bytes).unwrap();
        // Not a real DB — version check / open should fail, but must not OOM
        // or require reading the whole buffer into a second allocation of the
        // file contents for the magic check itself.
        let err = validate_thread_schema(&path).unwrap_err();
        assert!(
            err.contains("open failed")
                || err.contains("schema")
                || err.contains("unsupported")
                || err.contains("panicked"),
            "expected open/version failure after header ok, got {err}"
        );
        let short = dir.path().join("short.sqlite");
        std::fs::write(&short, b"SQLite").unwrap();
        let err2 = validate_thread_schema(&short).unwrap_err();
        assert!(
            err2.contains("not a SQLite"),
            "short header must be rejected: {err2}"
        );
    }

    #[test]
    fn repair_confirm_requires_displayed_plan() {
        let _g = env_lock();
        let prev = std::env::var_os("GROK_LHC");
        unsafe { std::env::remove_var("GROK_LHC") };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(execute_repair(
                "repair-unbound",
                "remove-orphan-session-dir",
            ))
            .unwrap_err();
        assert!(
            err.contains("displayed repair plan"),
            "confirm without plan must refuse: {err}"
        );
        match prev {
            Some(v) => unsafe { std::env::set_var("GROK_LHC", v) },
            None => unsafe { std::env::remove_var("GROK_LHC") },
        }
    }
}
