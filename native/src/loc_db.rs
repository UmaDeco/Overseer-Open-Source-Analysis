//! Wave 4 — SQLite master.mdb text localization hooks (ported from the upstream
//! localization framework's LibNative Sqlite3 hook tree).
//!
//! Five instance hooks + one resolved-not-hooked `GetInt`, driven by a global `SELECT_QUERIES` map
//! keyed by the QUERY object pointer (the value returned by `Connection.Query`/`PreparedQuery`, which
//! is also the `this` of `BindInt`/`GetText`/`Dispose`):
//!   * `Connection.Query` / `PreparedQuery` (argc 1) → parse the SELECT and register its state.
//!   * `PreparedQuery.BindInt` (argc 2) → record the bound key ints.
//!   * `Query.GetText` (argc 1) → return the pack translation for the row, else the original.
//!   * `Query.Dispose` (argc 0) → drop the state (frees the pointer key before it can be reused).
//! All are INSTANCE managed methods → detours take `this` first and forward the trailing MethodInfo*.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use fnv::FnvHashMap;
use once_cell::sync::Lazy;
use retour::RawDetour;
use sqlparser::{ast::BinaryOperator, dialect::SQLiteDialect, keywords::Keyword, parser::Parser};

use crate::il2cpp::{self, Method, Object};
use crate::sql::{self, ExprExt, SelectExt, SelectItemExt};

fn log(m: &str) {
    crate::tools::log(m);
}

/// query object ptr → its parsed SELECT state. Removed on Dispose so a reused pointer can't
/// mis-dispatch.
static SELECT_QUERIES: Lazy<Mutex<FnvHashMap<usize, Box<dyn sql::SelectQueryState + Send + Sync>>>> =
    Lazy::new(|| Mutex::new(FnvHashMap::default()));

static TR_QUERY: AtomicUsize = AtomicUsize::new(0);
static D_QUERY: OnceLock<RawDetour> = OnceLock::new();
static TR_PREP: AtomicUsize = AtomicUsize::new(0);
static D_PREP: OnceLock<RawDetour> = OnceLock::new();
static TR_BIND: AtomicUsize = AtomicUsize::new(0);
static D_BIND: OnceLock<RawDetour> = OnceLock::new();
static TR_GETTEXT: AtomicUsize = AtomicUsize::new(0);
static D_GETTEXT: OnceLock<RawDetour> = OnceLock::new();
static TR_DISPOSE: AtomicUsize = AtomicUsize::new(0);
static D_DISPOSE: OnceLock<RawDetour> = OnceLock::new();
static M_GETINT: AtomicUsize = AtomicUsize::new(0);

/// `Query.GetInt(this, idx)` — resolved (not hooked); read a bound int back from the live query.
pub fn get_int(this: Object, idx: i32) -> i32 {
    let m = M_GETINT.load(Ordering::Relaxed) as Method;
    if m.is_null() {
        return 0;
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return 0;
    }
    let f: extern "C" fn(Object, i32, Method) -> i32 = unsafe { std::mem::transmute(p) };
    f(this, idx, m)
}

/// Parse the SELECT `sql` and register a query state keyed by the returned `query` object.
fn parse_query(query: Object, sql: Object) {
    if query.is_null() {
        return;
    }
    let sql_str = il2cpp::read_string(sql);
    if !sql_str.starts_with("SELECT") {
        return; // quick escape
    }
    let dialect = SQLiteDialect {};
    let Ok(mut parser) = Parser::new(&dialect).try_with_sql(&sql_str) else {
        return;
    };
    if !parser.parse_keyword(Keyword::SELECT) {
        return;
    }
    let Ok(select) = parser.parse_select() else {
        return;
    };
    let Some(table_name) = select.get_first_table_name() else {
        return;
    };
    let mut query_state: Box<dyn sql::SelectQueryState + Send + Sync> = match table_name.as_str() {
        "text_data" => Box::new(sql::TextDataQuery::default()),
        "character_system_text" => Box::new(sql::CharacterSystemTextQuery::default()),
        "race_jikkyo_comment" => Box::new(sql::RaceJikkyoCommentQuery::default()),
        "race_jikkyo_message" => Box::new(sql::RaceJikkyoMessageQuery::default()),
        _ => return,
    };

    // Columns (0-based projection order).
    let mut i = 0;
    for item in select.projection.iter() {
        if let Some(name) = item.get_unnamed_expr_ident() {
            query_state.add_column(i, name);
            i += 1;
        }
    }
    // Params (1-based bind index; visited in `col1 = ? AND col2 = ? ...` order).
    let mut p = 1;
    if let Some(selection) = &select.selection {
        for expr in selection.binary_op_iter() {
            if *expr.op != BinaryOperator::Eq {
                continue;
            }
            if let Some(name) = expr.left.get_ident_value() {
                if expr.right.is_placeholder_value() {
                    query_state.add_param(p, name);
                    p += 1;
                }
            }
        }
    }

    if let Ok(mut m) = SELECT_QUERIES.lock() {
        m.insert(query as usize, query_state);
    }
}

// (1) Connection.Query(this, sql) -> Query*
type QueryFn = unsafe extern "C" fn(Object, Object, Method) -> Object;
unsafe extern "C" fn h_query(this: Object, sql: Object, mi: Method) -> Object {
    let tr = TR_QUERY.load(Ordering::Relaxed);
    let query = if tr != 0 {
        let f: QueryFn = std::mem::transmute(tr);
        f(this, sql, mi)
    } else {
        std::ptr::null_mut()
    };
    parse_query(query, sql);
    query
}

// (2) Connection.PreparedQuery(this, sql) -> PreparedQuery*
unsafe extern "C" fn h_prepared_query(this: Object, sql: Object, mi: Method) -> Object {
    let tr = TR_PREP.load(Ordering::Relaxed);
    let query = if tr != 0 {
        let f: QueryFn = std::mem::transmute(tr);
        f(this, sql, mi)
    } else {
        std::ptr::null_mut()
    };
    parse_query(query, sql);
    query
}

// (3) PreparedQuery.BindInt(this, idx, value) -> bool
type BindIntFn = unsafe extern "C" fn(Object, i32, i32, Method) -> bool;
unsafe extern "C" fn h_bind_int(this: Object, idx: i32, value: i32, mi: Method) -> bool {
    if let Ok(mut m) = SELECT_QUERIES.lock() {
        if let Some(qs) = m.get_mut(&(this as usize)) {
            qs.bind_int(idx, value);
        }
    }
    let tr = TR_BIND.load(Ordering::Relaxed);
    if tr != 0 {
        let f: BindIntFn = std::mem::transmute(tr);
        f(this, idx, value, mi)
    } else {
        false
    }
}

// (4) Query.GetText(this, idx) -> string
type GetTextFn = unsafe extern "C" fn(Object, i32, Method) -> Object;
unsafe extern "C" fn h_get_text(this: Object, idx: i32, mi: Method) -> Object {
    // master.mdb text substitution is a TRANSLATION surface — it follows the translation switch, so
    // turning translation off really does hand the game back its own strings.
    if crate::runtime::active(crate::runtime::Subsystem::Translation) {
        // Hold the lock across the (self-only) GetInt re-entry inside get_text; GetInt isn't hooked,
        // so there's no re-entrant lock. Miss → fall through to the original GetText.
        let hit = SELECT_QUERIES
            .lock()
            .ok()
            .and_then(|m| m.get(&(this as usize)).and_then(|qs| qs.get_text(this, idx)));
        if let Some(s) = hit {
            return s;
        }
    }
    let tr = TR_GETTEXT.load(Ordering::Relaxed);
    if tr != 0 {
        let f: GetTextFn = std::mem::transmute(tr);
        f(this, idx, mi)
    } else {
        std::ptr::null_mut()
    }
}

// (5) Query.Dispose(this)
type DisposeFn = unsafe extern "C" fn(Object, Method);
unsafe extern "C" fn h_dispose(this: Object, mi: Method) {
    if let Ok(mut m) = SELECT_QUERIES.lock() {
        m.remove(&(this as usize));
    }
    let tr = TR_DISPOSE.load(Ordering::Relaxed);
    if tr != 0 {
        let f: DisposeFn = std::mem::transmute(tr);
        f(this, mi);
    }
}

/// Resolve GetInt + install the 5 hooks. Non-fatal on any miss (returns Err → wave disabled).
pub fn install() -> Result<(), String> {
    let conn = il2cpp::class("LibNative.Sqlite3.Connection");
    let query_cls = il2cpp::class("LibNative.Sqlite3.Query");
    let prep_cls = il2cpp::class("LibNative.Sqlite3.PreparedQuery");
    if conn.is_null() || query_cls.is_null() || prep_cls.is_null() {
        return Err("LibNative.Sqlite3 classes not found".into());
    }
    let getint = il2cpp::method(query_cls, "GetInt", 1);
    if getint.is_null() {
        return Err("Query.GetInt not found".into());
    }
    M_GETINT.store(getint as usize, Ordering::Relaxed);
    unsafe {
        il2cpp::hook_method(conn, "Query", 1, h_query as *const (), &TR_QUERY, &D_QUERY)?;
        il2cpp::hook_method(conn, "PreparedQuery", 1, h_prepared_query as *const (), &TR_PREP, &D_PREP)?;
        il2cpp::hook_method(prep_cls, "BindInt", 2, h_bind_int as *const (), &TR_BIND, &D_BIND)?;
        il2cpp::hook_method(query_cls, "GetText", 1, h_get_text as *const (), &TR_GETTEXT, &D_GETTEXT)?;
        il2cpp::hook_method(query_cls, "Dispose", 0, h_dispose as *const (), &TR_DISPOSE, &D_DISPOSE)?;
    }
    Ok(())
}
