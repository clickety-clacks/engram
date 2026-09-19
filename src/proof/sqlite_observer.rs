//! Opt-in, in-process counters from the actual query statements, never a replay.
//! This does not instrument the hash-pinned historical baseline executable.
use std::collections::BTreeMap;
use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use super::t1772::{PROOF_ROOT, ProofResult, canonical_json_lf};
use rusqlite::{Connection, ffi};
use serde_json::json;

static OUTPUT: OnceLock<Mutex<File>> = OnceLock::new();
static FAILED: AtomicBool = AtomicBool::new(false);
static ROWS: OnceLock<Mutex<BTreeMap<usize, u64>>> = OnceLock::new();

pub fn install(conn: &Connection) -> rusqlite::Result<()> {
    let Some(path) = std::env::var_os("T1772_SQLITE_STATEMENTS") else {
        return Ok(());
    };
    let path = Path::new(&path);
    let parent = path.parent().ok_or(rusqlite::Error::InvalidQuery)?;
    if !parent
        .canonicalize()
        .map_err(|_| rusqlite::Error::InvalidQuery)?
        .starts_with(PROOF_ROOT)
    {
        return Err(rusqlite::Error::InvalidQuery);
    }
    if OUTPUT.get().is_none() {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        file.write_all(&canonical_json_lf(&json!({"event":"begin","pid":std::process::id(),
            "method":"sqlite3_trace_v2 PROFILE; sqlite3_stmt_status counters reset after each completed execution"}))
            .map_err(|_|rusqlite::Error::InvalidQuery)?).map_err(|_|rusqlite::Error::InvalidQuery)?;
        OUTPUT
            .set(Mutex::new(file))
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
    }
    // SQLite invokes PROFILE while the statement pointer is valid. The context
    // has process lifetime; no borrowed connection or Rust object crosses FFI.
    let code = unsafe {
        ffi::sqlite3_trace_v2(
            conn.handle(),
            ffi::SQLITE_TRACE_PROFILE | ffi::SQLITE_TRACE_ROW,
            Some(profile),
            std::ptr::null_mut(),
        )
    };
    if code != ffi::SQLITE_OK {
        return Err(rusqlite::Error::SqliteFailure(ffi::Error::new(code), None));
    }
    Ok(())
}

unsafe extern "C" fn profile(
    mask: u32,
    _: *mut libc::c_void,
    statement: *mut libc::c_void,
    elapsed: *mut libc::c_void,
) -> i32 {
    // No unwinding across the C boundary. Failure is checked before CLI success.
    let result = std::panic::catch_unwind(|| -> ProofResult<()> {
        let mut row_counts = ROWS
            .get_or_init(|| Mutex::new(BTreeMap::new()))
            .lock()
            .map_err(|_| "row counter lock poisoned")?;
        if mask == ffi::SQLITE_TRACE_ROW {
            *row_counts.entry(statement as usize).or_default() += 1;
            return Ok(());
        }
        let rows = row_counts.remove(&(statement as usize)).unwrap_or(0);
        drop(row_counts);
        let stmt = statement.cast::<ffi::sqlite3_stmt>();
        let sql_ptr = unsafe { ffi::sqlite3_sql(stmt) };
        if sql_ptr.is_null() {
            return Err("profile omitted statement SQL".into());
        }
        let sql = unsafe { CStr::from_ptr(sql_ptr) }.to_str()?;
        let counter = |kind| unsafe { ffi::sqlite3_stmt_status(stmt, kind, 1) };
        let row = json!({"event":"statement", "sql":sql,"rows_visited":rows,
            "sqlite_nanoseconds":unsafe { *elapsed.cast::<u64>() },
            "sort":counter(ffi::SQLITE_STMTSTATUS_SORT),
            "autoindex":counter(ffi::SQLITE_STMTSTATUS_AUTOINDEX),
            "fullscan_steps":counter(ffi::SQLITE_STMTSTATUS_FULLSCAN_STEP),
            "vm_steps":counter(ffi::SQLITE_STMTSTATUS_VM_STEP)});
        let mut file = OUTPUT
            .get()
            .ok_or("trace output absent")?
            .lock()
            .map_err(|_| "trace lock poisoned")?;
        file.write_all(&canonical_json_lf(&row)?)?;
        Ok(())
    });
    if !matches!(result, Ok(Ok(()))) {
        FAILED.store(true, Ordering::Relaxed);
    }
    0
}

pub fn finish() -> ProofResult<()> {
    if std::env::var_os("T1772_SQLITE_STATEMENTS").is_none() {
        return Ok(());
    }
    if FAILED.load(Ordering::Relaxed) {
        return Err("statement counter observation failed".into());
    }
    let mut file = OUTPUT
        .get()
        .ok_or("no measured SQLite connection opened")?
        .lock()
        .map_err(|_| "trace lock poisoned")?;
    file.write_all(&canonical_json_lf(
        &json!({"event":"end","pid":std::process::id(),"complete":true}),
    )?)?;
    file.sync_all()?;
    Ok(())
}
