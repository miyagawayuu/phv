//! `std.orm`: entities (classes marked `#[Table]`) read from and written to
//! the rows of a table through `std.db`.
//!
//! The type checker describes the entity class where a program writes
//! `new Repo<User>($db)` (see `phv_types`, `check/orm.rs`); `__orm_meta` reads
//! that description once per `Repo` and keeps it in `vm.resources`. The other
//! builtins build the statements (as `Sql` values, which `Database` then runs
//! with every limit of std.db) and turn rows into objects and objects into
//! column values.
//!
//! Table and column names come only from the class and its annotations: the
//! checker and `parse` allow letters, digits and `_`, and they are quoted for
//! the database (`IDENT_START` / `IDENT_END`, rendered as `"` or `` ` ``).
//! Names a script passes at run time (`orderBy`, `in()`, `column()`) only pick
//! one of the entity's properties; values are always bound parameters.

use crate::db::{parts, Dialect, IDENT_END, IDENT_START, PARAM};
use crate::jsonas::{Builder, Prop, Shape};
use crate::{int, list, map, s};
use phv_vm::value::Object;
use phv_vm::vm::const_value;
use phv_vm::{BuiltinFn, Key, Map, Value, Vm, VmError};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

type R = Result<Value, VmError>;

/// Values `Repo::in()` takes at most (each is a bound parameter).
pub(crate) const MAX_IN: usize = 1000;
/// Terms of an `orderBy` (`"name, createdAt desc"`) at most, and its length.
pub(crate) const MAX_ORDER_TERMS: usize = 16;
pub(crate) const MAX_ORDER_LEN: usize = 1024;
/// The longest table or column name (as the checker allows).
const MAX_NAME: usize = 63;

thread_local! {
    /// Transactions of `Database::transaction()` rolled back (or ended by an
    /// error), and rollbacks written as SQL, on this thread so far. A `Repo`
    /// forgets the values it saw when this changes, since the rows may no
    /// longer hold them.
    static ROLLBACKS: Cell<i64> = const { Cell::new(0) };
    /// Writes of Repos on this thread so far, by table. A `Repo` forgets the
    /// values it saw when another Repo wrote to its table (found by the 19th
    /// audit: a Repo that had not seen another Repo's write skipped a column
    /// whose row no longer held the entity's value).
    static WRITES: std::cell::RefCell<HashMap<String, i64>> = std::cell::RefCell::new(HashMap::new());
}

/// Tables whose writes are counted apart, at most; a write to another table
/// counts as a rollback (every Repo forgets).
const MAX_TABLES: usize = 4096;

/// Notes that a transaction was rolled back (or failed to commit).
pub(crate) fn rolled_back() {
    ROLLBACKS.with(|r| r.set(r.get().wrapping_add(1)));
}

/// What a Repo of `table` compares to know whether it must forget: both
/// counts only grow, so their sum changes whenever either does.
fn epoch(table: &str) -> i64 {
    let writes = WRITES.with(|w| w.borrow().get(table).copied().unwrap_or(0));
    ROLLBACKS.with(|r| r.get()).wrapping_add(writes)
}

/// Notes a Repo's write to `table`, and returns the epoch after it.
fn wrote(table: &str) -> i64 {
    WRITES.with(|w| {
        let mut w = w.borrow_mut();
        let full = w.len() >= MAX_TABLES;
        match w.get_mut(table) {
            Some(n) => *n = n.wrapping_add(1),
            None if !full => {
                w.insert(table.to_string(), 1);
            }
            None => rolled_back(),
        }
    });
    epoch(table)
}

#[derive(Debug, PartialEq)]
enum Kind {
    Int,
    Float,
    Str,
    Bool,
    Enum { class: u32, by_name: bool },
    Json(ShapeText),
}

/// A `json_decode_as` shape, kept as text for `Debug` and parsed when used.
#[derive(Debug, PartialEq)]
struct ShapeText(String);

impl ShapeText {
    fn shape(&self) -> Option<Shape> {
        crate::jsonas::shape(&mut self.0.chars())
    }
}

struct Column {
    prop: String,
    name: String,
    /// `name` as a key of a row.
    key: Key,
    kind: Kind,
    nullable: bool,
    /// The property's type, for messages.
    ty: String,
    /// The property's slot in the entity class.
    slot: usize,
}

struct Meta {
    class: u32,
    /// `Repo<User>`, for messages.
    what: String,
    /// `User`.
    entity: String,
    table: String,
    key: usize,
    columns: Vec<Column>,
    json: HashMap<u32, Vec<Prop>>,
    defaults: Vec<Value>,
    dialect: Dialect,
    /// MySQL: whether the key column is the table's `AUTO_INCREMENT` column,
    /// once looked up (`made_query`).
    auto_key: Cell<Option<bool>>,
}

/// The description of an entity as the checker writes it, before it is
/// matched with the program's classes.
#[derive(Debug, PartialEq)]
struct Desc<'a> {
    class: u32,
    table: &'a str,
    key: usize,
    generated: bool,
    /// Property, column, nullable, kind, type.
    columns: Vec<(&'a str, &'a str, bool, Kind, &'a str)>,
    json_lines: Vec<&'a str>,
}

fn sql_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= MAX_NAME
        && matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn parse_kind(k: &str) -> Option<(bool, Kind)> {
    let (nullable, k) = match k.strip_prefix('?') {
        Some(rest) => (true, rest),
        None => (false, k),
    };
    let kind = match k {
        "i" => Kind::Int,
        "f" => Kind::Float,
        "s" => Kind::Str,
        "b" => Kind::Bool,
        _ => match k.split_at_checked(1)? {
            ("e", n) => Kind::Enum { class: n.parse().ok()?, by_name: false },
            ("n", n) => Kind::Enum { class: n.parse().ok()?, by_name: true },
            ("j", shape) => {
                let text = ShapeText(shape.to_string());
                text.shape()?;
                Kind::Json(text)
            }
            _ => return None,
        },
    };
    Some((nullable, kind))
}

/// Reads a description (see `phv_types`, `check/orm.rs`).
fn parse_desc(desc: &str) -> Option<Desc<'_>> {
    let mut lines = desc.split('\n');
    let head: Vec<&str> = lines.next()?.split('\t').collect();
    let ["E", class, table, key, generated] = head[..] else { return None };
    let mut d = Desc {
        class: class.parse().ok()?,
        table,
        key: key.parse().ok()?,
        generated: generated == "1",
        columns: Vec::new(),
        json_lines: Vec::new(),
    };
    if !sql_name(table) {
        return None;
    }
    for line in lines {
        if let Some(rest) = line.strip_prefix("P\t") {
            let f: Vec<&str> = rest.split('\t').collect();
            let [prop, column, kind, ty] = f[..] else { return None };
            let (nullable, kind) = parse_kind(kind)?;
            if !sql_name(column) {
                return None;
            }
            d.columns.push((prop, column, nullable, kind, ty));
        } else if line.starts_with('c') {
            d.json_lines.push(line);
        } else {
            return None;
        }
    }
    if d.key >= d.columns.len() || !matches!(d.columns[d.key].3, Kind::Int | Kind::Str) {
        return None;
    }
    Some(d)
}

fn bad_description(vm: &Vm) -> VmError {
    vm.throw(
        "RuntimeException",
        "std.orm: this Repo has no entity description (create it with `new Repo<Entity>($db)`)",
    )
}

/// `__orm_meta($db, $description, $defaults)`: the description, read and
/// matched with the program's classes, as a handle.
fn meta_new(vm: &mut Vm, a: &[Value]) -> R {
    let handle = database_handle(vm, &a[0])?;
    let dialect = crate::db::dialect_of(vm, &handle)
        .ok_or_else(|| vm.throw("DatabaseError", "the database connection is closed"))?;
    let Some(d) = parse_desc(s(&a[1])) else { return Err(bad_description(vm)) };
    let prog = vm.prog.clone();
    let Some(cls) = prog.classes.get(d.class as usize) else { return Err(bad_description(vm)) };
    let slots: HashMap<&str, usize> = cls.fields.iter().enumerate().map(|(i, f)| (f.name.as_str(), i)).collect();
    let mut columns = Vec::with_capacity(d.columns.len());
    for (prop, name, nullable, kind, ty) in d.columns {
        let Some(&slot) = slots.get(prop) else { return Err(bad_description(vm)) };
        if let Kind::Enum { class, .. } = kind {
            if prog.classes.get(class as usize).is_none() {
                return Err(bad_description(vm));
            }
        }
        let key = Key::Str(Rc::new(name.to_string()));
        columns.push(Column { prop: prop.into(), name: name.into(), key, kind, nullable, ty: ty.into(), slot });
    }
    let Some(json) = crate::jsonas::parse_classes(d.json_lines.into_iter()) else { return Err(bad_description(vm)) };
    let meta = Meta {
        class: d.class,
        what: format!("Repo<{}>", cls.name),
        entity: cls.name.clone(),
        table: d.table.to_string(),
        key: d.key,
        columns,
        json,
        defaults: list(&a[2]).to_vec(),
        auto_key: Cell::new(None),
        dialect,
    };
    vm.resources.push(Some(Box::new(Rc::new(meta))));
    Ok(Value::Int(vm.resources.len() as i64 - 1))
}

/// The handle inside a `Database` object (its private `handle` property).
fn database_handle(vm: &Vm, db: &Value) -> Result<Value, VmError> {
    if let Value::Object(o) = db {
        let o = o.borrow();
        let cls = &vm.prog.classes[o.class as usize];
        if cls.name == "Database" {
            if let Some(i) = cls.fields.iter().position(|f| f.name == "handle") {
                return Ok(o.fields[i].clone());
            }
        }
    }
    Err(vm.throw("RuntimeException", "std.orm: a Repo needs a Database"))
}

fn meta_of(vm: &Vm, h: &Value) -> Result<Rc<Meta>, VmError> {
    vm.resources
        .get(int(h) as usize)
        .and_then(|r| r.as_ref())
        .and_then(|b| b.downcast_ref::<Rc<Meta>>())
        .cloned()
        .ok_or_else(|| bad_description(vm))
}

fn sql(text: String, params: Vec<Value>) -> Value {
    Value::list(vec![Value::str(text), Value::list(params)])
}

/// A name a script gave, for a message: quoted, and cut when long.
fn quoted(t: &str) -> String {
    let short: String = t.chars().take(64).collect();
    let more = if short.len() < t.len() { "..." } else { "" };
    format!("\"{}{more}\"", short.escape_debug())
}

impl Meta {
    /// A table or column name, quoted (a plain SQL name: see `sql_name`). On
    /// SQLite in backquotes: a name in double quotes that names no column is
    /// read there as a string (`"emial" = 'x'` compares two strings), while
    /// in backquotes it is an error.
    fn ident(&self, name: &str) -> String {
        match self.dialect {
            Dialect::Sqlite => format!("`{name}`"),
            _ => format!("{IDENT_START}{name}{IDENT_END}"),
        }
    }

    fn select_list(&self) -> String {
        self.columns.iter().map(|c| self.ident(&c.name)).collect::<Vec<_>>().join(", ")
    }

    fn key_column(&self) -> &Column {
        &self.columns[self.key]
    }

    /// The column of property `prop` (a name the script gave).
    fn column(&self, vm: &Vm, prop: &str) -> Result<&Column, VmError> {
        self.columns.iter().find(|c| c.prop == prop).ok_or_else(|| {
            let names: Vec<&str> = self.columns.iter().map(|c| c.prop.as_str()).collect();
            vm.throw(
                "ValueError",
                format!(
                    "{}: {} has no property {} (it has {})",
                    self.what,
                    self.entity,
                    quoted(prop),
                    names.join(", ")
                ),
            )
        })
    }

    /// The value of the column's property in `e` (an object of the entity class or a subclass).
    fn field(&self, vm: &Vm, e: &Value, c: &Column) -> Result<Value, VmError> {
        let Value::Object(o) = e else { return Err(vm.throw("RuntimeException", "std.orm: not an entity")) };
        let o = o.borrow();
        if o.class == self.class {
            return Ok(o.fields[c.slot].clone());
        }
        let cls = &vm.prog.classes[o.class as usize];
        if cls.supers.binary_search(&self.class).is_err() {
            return Err(vm.throw("RuntimeException", "std.orm: not an entity of this Repo"));
        }
        match cls.fields.iter().position(|f| f.name == c.prop) {
            Some(i) => Ok(o.fields[i].clone()),
            None => Err(vm.throw("RuntimeException", "std.orm: not an entity of this Repo")),
        }
    }

    /// What the column holds for the property value `v`.
    fn column_value(&self, vm: &Vm, c: &Column, v: &Value) -> R {
        match (&c.kind, v) {
            (_, Value::Null) => Ok(Value::Null),
            (Kind::Enum { by_name, .. }, Value::Object(o)) => {
                let o = o.borrow();
                let field = if *by_name { "name" } else { "value" };
                let cls = &vm.prog.classes[o.class as usize];
                match cls.fields.iter().position(|f| f.name == field) {
                    Some(i) => Ok(o.fields[i].clone()),
                    None => Err(vm.throw("RuntimeException", "std.orm: not an enum case")),
                }
            }
            (Kind::Json(_), v) => {
                let mut out = String::new();
                crate::json(vm, v, &mut out, 0)?;
                Ok(Value::str(out))
            }
            (_, v) => Ok(v.clone()),
        }
    }

    /// The column values of entity `e`.
    fn values(&self, vm: &Vm, e: &Value) -> Result<Vec<Value>, VmError> {
        let mut out = Vec::with_capacity(self.columns.len());
        for c in &self.columns {
            let v = self.field(vm, e, c)?;
            out.push(self.column_value(vm, c, &v)?);
        }
        Ok(out)
    }

    /// The property value for what column `c` holds (`v`), checked against
    /// the property's type.
    fn property_value(&self, vm: &mut Vm, c: &Column, v: &Value) -> R {
        if matches!(v, Value::Null) {
            return if c.nullable { Ok(Value::Null) } else { Err(self.mismatch(vm, c, v)) };
        }
        match (&c.kind, v) {
            (Kind::Int, Value::Int(_)) | (Kind::Float, Value::Float(_)) | (Kind::Str, Value::Str(_)) => Ok(v.clone()),
            (Kind::Bool, Value::Bool(_)) => Ok(v.clone()),
            (Kind::Float, Value::Int(i)) => Ok(Value::Float(*i as f64)),
            // SQLite and MySQL keep booleans as 0 and 1.
            (Kind::Bool, Value::Int(i @ (0 | 1))) => Ok(Value::Bool(*i == 1)),
            (Kind::Enum { class, by_name }, v) => {
                let prog = vm.prog.clone();
                let cls = &prog.classes[*class as usize];
                let found = cls.cases.iter().position(|(name, value)| match (v, by_name) {
                    (Value::Str(s), true) => s.as_str() == name,
                    (_, true) => false,
                    (v, false) => match (v, &const_value(value)) {
                        (Value::Int(a), Value::Int(b)) => a == b,
                        (Value::Str(a), Value::Str(b)) => a == b,
                        _ => false,
                    },
                });
                match found {
                    Some(i) => Ok(vm.enum_case(*class, i as u32)),
                    None => {
                        let what = if *by_name { "a case name" } else { "a value" };
                        let held = match v {
                            Value::Str(t) => quoted(t),
                            Value::Int(i) => i.to_string(),
                            v => kind_of(v).to_string(),
                        };
                        Err(vm.throw(
                            "DatabaseError",
                            format!(
                                "{}: the column \"{}\" of \"{}\" holds {held}, which is not {what} of {} (`{}::${}`)",
                                self.what, c.name, self.table, cls.name, self.entity, c.prop
                            ),
                        ))
                    }
                }
            }
            (Kind::Json(shape), Value::Str(text)) => {
                let Some(shape) = shape.shape() else { return Err(bad_description(vm)) };
                let doc = crate::web::json_decode_in(vm, text)?.map_err(|e| {
                    vm.throw(
                        "DatabaseError",
                        format!(
                            "{}: the column \"{}\" of \"{}\" does not hold JSON ({e})",
                            self.what, c.name, self.table
                        ),
                    )
                })?;
                let start = format!("{}: the JSON in the column \"{}\" of \"{}\"", self.what, c.name, self.table);
                let mut b = Builder {
                    classes: &self.json,
                    defaults: &self.defaults,
                    error: ("DatabaseError", &start),
                    path: "$".into(),
                    built: 0,
                };
                b.build(vm, &doc, &shape)
            }
            _ => Err(self.mismatch(vm, c, v)),
        }
    }

    fn mismatch(&self, vm: &Vm, c: &Column, v: &Value) -> VmError {
        let held = match v {
            Value::Null => "NULL".to_string(),
            Value::Int(i) => format!("the integer {i}"),
            Value::Bool(b) => format!("the boolean {b}"),
            v => kind_of(v).to_string(),
        };
        // PostgreSQL `numeric` and MySQL `DECIMAL` are read as strings.
        let hint = match (&c.kind, v) {
            (Kind::Int | Kind::Float, Value::Str(t)) if t.trim().parse::<f64>().is_ok() => {
                " (a NUMERIC or DECIMAL column is read as a string: give the property the type `string`, or the \
                 column an integer or floating-point type)"
            }
            _ => "",
        };
        vm.throw(
            "DatabaseError",
            format!(
                "{}: the column \"{}\" of \"{}\" holds {held}, but `{}::${}` is `{}`{hint}",
                self.what, c.name, self.table, self.entity, c.prop, c.ty
            ),
        )
    }

    /// An entity built from a row (its columns by name; others are ignored).
    fn load(&self, vm: &mut Vm, row: &Map) -> R {
        let prog = vm.prog.clone();
        let cls = &prog.classes[self.class as usize];
        let mut fields: Vec<Value> = cls.fields.iter().map(|f| const_value(&f.default)).collect();
        for c in &self.columns {
            let Some(v) = row.get(&c.key) else {
                return Err(vm.throw(
                    "DatabaseError",
                    format!(
                        "{}: the row has no column \"{}\" (for `{}::${}`); select every column of \"{}\"",
                        self.what, c.name, self.entity, c.prop, self.table
                    ),
                ));
            };
            fields[c.slot] = self.property_value(vm, c, v)?;
        }
        let o = Rc::new(RefCell::new(Object { class: self.class, fields }));
        vm.track(&o);
        Ok(Value::Object(o))
    }

    /// The key of entity `e` as a parameter, or a `ValueError` when it has none.
    fn key_value(&self, vm: &Vm, e: &Value, doing: &str) -> R {
        let k = self.field(vm, e, self.key_column())?;
        if matches!(k, Value::Null) {
            return Err(vm.throw(
                "ValueError",
                format!(
                    "{}::{doing}(): the entity has no key (`{}::${}` is null); insert() it first",
                    self.what,
                    self.entity,
                    self.key_column().prop
                ),
            ));
        }
        Ok(k)
    }

    fn where_key(&self, text: &mut String) {
        text.push_str(" WHERE ");
        text.push_str(&self.ident(&self.key_column().name));
        text.push_str(" = ");
        text.push(PARAM);
    }
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "NULL",
        Value::Bool(_) => "a boolean",
        Value::Int(_) => "an integer",
        Value::Float(_) => "a float",
        Value::Str(_) | Value::Html(_) => "a string",
        _ => "a value",
    }
}

/// What a script passed, for a message: `kind_of`, or the class of an object.
fn given(vm: &Vm, v: &Value) -> String {
    match v {
        Value::Object(o) => format!("a {}", vm.prog.classes[o.borrow().class as usize].name),
        Value::Null => "null".into(),
        v => kind_of(v).into(),
    }
}

/// The terms of an `orderBy`: property names, each with `asc` or `desc`
/// (`"name, createdAt desc"`); `true` for descending.
fn order_terms(spec: &str) -> Result<Vec<(&str, bool)>, String> {
    if spec.trim().is_empty() {
        return Ok(Vec::new());
    }
    if spec.len() > MAX_ORDER_LEN {
        return Err(format!("orderBy is longer than {MAX_ORDER_LEN} characters"));
    }
    let terms: Vec<&str> = spec.split(',').collect();
    if terms.len() > MAX_ORDER_TERMS {
        return Err(format!("orderBy has more than {MAX_ORDER_TERMS} terms"));
    }
    let mut out = Vec::with_capacity(terms.len());
    for t in terms {
        let mut words = t.split_whitespace();
        let Some(prop) = words.next() else { return Err("orderBy has an empty term".into()) };
        let desc = match words.next() {
            None => false,
            Some(w) if w.eq_ignore_ascii_case("asc") => false,
            Some(w) if w.eq_ignore_ascii_case("desc") => true,
            Some(w) => {
                return Err(format!("orderBy: expected `asc` or `desc` after {}, found {}", quoted(prop), quoted(w)))
            }
        };
        if let Some(w) = words.next() {
            return Err(format!("orderBy: unexpected {} after {}", quoted(w), quoted(prop)));
        }
        out.push((prop, desc));
    }
    Ok(out)
}

/// `__orm_select($h, $where, $orderBy, $limit, $offset, $count)` (`$limit` null: all): the
/// `SELECT` of the entity's columns (or of `COUNT(*) AS "n"`).
fn select(vm: &mut Vm, a: &[Value]) -> R {
    let m = meta_of(vm, &a[0])?;
    let count = matches!(a[5], Value::Bool(true));
    let mut text = String::from("SELECT ");
    if count {
        text.push_str("COUNT(*) AS ");
        text.push_str(&m.ident("n"));
    } else {
        text.push_str(&m.select_list());
    }
    text.push_str(" FROM ");
    text.push_str(&m.ident(&m.table));
    let mut params = Vec::new();
    if let Value::List(_) = &a[1] {
        let (cond, p) = parts(&a[1]);
        if !cond.trim().is_empty() {
            text.push_str(" WHERE (");
            text.push_str(cond);
            // On a line of its own after a `--` comment, which ends there.
            text.push_str(if cond.contains("--") { "\n)" } else { ")" });
            params.extend(p.iter().cloned());
        }
    }
    if count {
        return Ok(sql(text, params));
    }
    let terms = order_terms(s(&a[2])).map_err(|e| vm.throw("ValueError", format!("{}: {e}", m.what)))?;
    for (i, (prop, desc)) in terms.iter().enumerate() {
        text.push_str(if i == 0 { " ORDER BY " } else { ", " });
        text.push_str(&m.ident(&m.column(vm, prop)?.name));
        if *desc {
            text.push_str(" DESC");
        }
    }
    let limit = match &a[3] {
        Value::Int(n) => Some(*n),
        _ => None,
    };
    let offset = int(&a[4]);
    if limit.is_some_and(|n| n < 0) || offset < 0 {
        return Err(vm.throw("ValueError", format!("{}::findBy(): limit and offset cannot be negative", m.what)));
    }
    // SQLite and MySQL need a LIMIT before an OFFSET.
    if limit.is_some() || offset > 0 {
        text.push_str(&format!(" LIMIT {}", limit.unwrap_or(i64::MAX)));
    }
    if offset > 0 {
        text.push_str(&format!(" OFFSET {offset}"));
    }
    Ok(sql(text, params))
}

/// `__orm_find($h, $key)`: the `SELECT` of the row with key `$key`.
fn find(vm: &mut Vm, a: &[Value]) -> R {
    let m = meta_of(vm, &a[0])?;
    let k = m.key_column();
    let fits = matches!((&k.kind, &a[1]), (Kind::Int, Value::Int(_)) | (Kind::Str, Value::Str(_)));
    if !fits {
        let want = if k.kind == Kind::Int { "an int" } else { "a string" };
        return Err(vm.throw(
            "ValueError",
            format!("{}::find(): the key `{}::${}` is {want}, found {}", m.what, m.entity, k.prop, given(vm, &a[1])),
        ));
    }
    let mut text = format!("SELECT {} FROM {}", m.select_list(), m.ident(&m.table));
    m.where_key(&mut text);
    Ok(sql(text, vec![a[1].clone()]))
}

/// `__orm_insert($h, $entity)`: the `INSERT` of the entity's columns (all
/// but a key that is null, which the database makes and `RETURNING` gives
/// back on SQLite and PostgreSQL).
fn insert(vm: &mut Vm, a: &[Value]) -> R {
    let m = meta_of(vm, &a[0])?;
    let values = m.values(vm, &a[1])?;
    let made = matches!(values[m.key], Value::Null);
    if made && m.key_column().kind == Kind::Str && matches!(m.dialect, Dialect::MySql) {
        return Err(vm.throw(
            "ValueError",
            format!(
                "{}::insert(): MySQL cannot give back a key it made that is not an integer; give `{}::${}` a value first",
                m.what,
                m.entity,
                m.key_column().prop
            ),
        ));
    }
    let mut names = Vec::new();
    let mut params = Vec::new();
    for (i, (c, v)) in m.columns.iter().zip(values).enumerate() {
        if i == m.key && made {
            continue;
        }
        names.push(m.ident(&c.name));
        params.push(v);
    }
    let mut text = format!("INSERT INTO {}", m.ident(&m.table));
    if names.is_empty() {
        text.push_str(if matches!(m.dialect, Dialect::MySql) { " () VALUES ()" } else { " DEFAULT VALUES" });
    } else {
        let marks = vec![PARAM.to_string(); names.len()].join(", ");
        text.push_str(&format!(" ({}) VALUES ({marks})", names.join(", ")));
    }
    if made && !matches!(m.dialect, Dialect::MySql) {
        text.push_str(" RETURNING ");
        text.push_str(&m.ident(&m.key_column().name));
    }
    Ok(sql(text, params))
}

/// `__orm_insert_mode($h, $entity)`: 0 when the entity has its key, 1 when
/// the `INSERT` returns the key the database made (run it with `first()`),
/// 2 when it is read with `lastInsertId()` afterwards (MySQL).
fn insert_mode(vm: &mut Vm, a: &[Value]) -> R {
    let m = meta_of(vm, &a[0])?;
    let k = m.field(vm, &a[1], m.key_column())?;
    Ok(Value::Int(match (k, m.dialect) {
        (Value::Null, Dialect::MySql) => 2,
        (Value::Null, _) => 1,
        _ => 0,
    }))
}

/// The error of an `insert()` on MySQL whose key column is not the table's
/// `AUTO_INCREMENT` column: `lastInsertId()` would be another column's value
/// (another row's key), or 0.
fn not_auto_key(vm: &Vm, m: &Meta) -> VmError {
    let k = m.key_column();
    vm.throw(
        "ValueError",
        format!(
            "{}::insert(): MySQL gives back only a key its AUTO_INCREMENT column made, and the column \"{}\" of \"{}\" \
             is not that column; give `{}::${}` a value first",
            m.what, k.name, m.table, m.entity, k.prop
        ),
    )
}

/// `__orm_made_query($h)`, before an `insert()` that reads its key with
/// `lastInsertId()` (MySQL): null when the key column is known to be the
/// table's `AUTO_INCREMENT` column, the query that tells when it is not known
/// yet (run once per Repo, the answer given to `__orm_made`), and
/// `ValueError` when it is known not to be (found by the 19th audit: the key
/// was taken from `lastInsertId()`, which is any column's AUTO_INCREMENT
/// value, or 0).
fn made_query(vm: &mut Vm, a: &[Value]) -> R {
    let m = meta_of(vm, &a[0])?;
    match m.auto_key.get() {
        Some(true) => Ok(Value::Null),
        Some(false) => Err(not_auto_key(vm, &m)),
        None => Ok(sql(
            format!(
                "SELECT EXTRA AS extra FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = {PARAM} AND COLUMN_NAME = {PARAM}"
            ),
            vec![Value::str(m.table.clone()), Value::str(m.key_column().name.clone())],
        )),
    }
}

/// `__orm_made($h, $row)`: the answer to `__orm_made_query` (its row, or null).
fn made(vm: &mut Vm, a: &[Value]) -> R {
    let m = meta_of(vm, &a[0])?;
    let extra = match &a[1] {
        Value::Map(row) => row.get(&Key::Str(Rc::new("extra".into()))).map(Value::to_display_string),
        _ => None,
    };
    let auto = extra.is_some_and(|e| e.to_ascii_lowercase().split_ascii_whitespace().any(|w| w == "auto_increment"));
    m.auto_key.set(Some(auto));
    if !auto {
        return Err(not_auto_key(vm, &m));
    }
    Ok(Value::Null)
}

/// `__orm_set_key($h, $entity, $key)`: sets the key the database made, from
/// `lastInsertId()` or from the row `RETURNING` gave.
fn set_key(vm: &mut Vm, a: &[Value]) -> R {
    let m = meta_of(vm, &a[0])?;
    let k = m.key_column();
    let v = match &a[2] {
        Value::Map(row) => row.get(&k.key).cloned().unwrap_or(Value::Null),
        // `lastInsertId()` is 0 when the statement made no AUTO_INCREMENT value.
        Value::Int(0) => Value::Null,
        v => v.clone(),
    };
    if matches!(v, Value::Null) {
        return Err(vm.throw(
            "DatabaseError",
            format!("{}::insert(): the database gave back no key for \"{}\"", m.what, m.table),
        ));
    }
    let v = m.property_value(vm, k, &v)?;
    let Value::Object(o) = &a[1] else { return Err(vm.throw("RuntimeException", "std.orm: not an entity")) };
    let mut o = o.borrow_mut();
    let slot = if o.class == m.class {
        k.slot
    } else {
        match vm.prog.classes[o.class as usize].fields.iter().position(|f| f.name == k.prop) {
            Some(i) => i,
            None => return Err(vm.throw("RuntimeException", "std.orm: not an entity of this Repo")),
        }
    };
    o.fields[slot] = v;
    Ok(Value::Null)
}

/// `__orm_key($h, $entity)`: the key as a text that tells keys apart (`i5`,
/// `sabc`), or null when it is null.
fn key_text(vm: &mut Vm, a: &[Value]) -> R {
    let m = meta_of(vm, &a[0])?;
    Ok(match &m.field(vm, &a[1], m.key_column())? {
        Value::Int(i) => Value::str(format!("i{i}")),
        Value::Str(t) => Value::str(format!("s{t}")),
        _ => Value::Null,
    })
}

/// `__orm_values($h, $entity)`: what each column holds for the entity.
fn values(vm: &mut Vm, a: &[Value]) -> R {
    let m = meta_of(vm, &a[0])?;
    let values = m.values(vm, &a[1])?;
    let mut out = Map::with_capacity(values.len());
    for (c, v) in m.columns.iter().zip(values) {
        out.insert(c.key.clone(), v);
    }
    Ok(Value::map(out))
}

/// `__orm_update($h, $entity, $seen, $before)`: the `UPDATE` of the columns
/// whose values differ from `$before` (what the database held when the Repo
/// last read or wrote the row) when `$seen` is this very entity, of every
/// column but the key otherwise; null when nothing changed.
fn update(vm: &mut Vm, a: &[Value]) -> R {
    let m = meta_of(vm, &a[0])?;
    let key = m.key_value(vm, &a[1], "update")?;
    let same = matches!((&a[1], &a[2]), (Value::Object(x), Value::Object(y)) if Rc::ptr_eq(x, y));
    let before = match &a[3] {
        Value::Map(b) if same => Some(b.clone()),
        _ => None,
    };
    let values = m.values(vm, &a[1])?;
    let mut sets = Vec::new();
    let mut params = Vec::new();
    for (i, (c, v)) in m.columns.iter().zip(values).enumerate() {
        if i == m.key {
            continue;
        }
        if let Some(old) = before.as_ref().and_then(|b| b.get(&c.key)) {
            if phv_vm::value::values_equal(old, &v) {
                continue;
            }
        }
        sets.push(format!("{} = {PARAM}", m.ident(&c.name)));
        params.push(v);
    }
    if sets.is_empty() {
        return Ok(Value::Null);
    }
    let mut text = format!("UPDATE {} SET {}", m.ident(&m.table), sets.join(", "));
    m.where_key(&mut text);
    params.push(key);
    Ok(sql(text, params))
}

/// `__orm_delete($h, $entity)`: the `DELETE` of the entity's row.
fn delete(vm: &mut Vm, a: &[Value]) -> R {
    let m = meta_of(vm, &a[0])?;
    let key = m.key_value(vm, &a[1], "delete")?;
    let mut text = format!("DELETE FROM {}", m.ident(&m.table));
    m.where_key(&mut text);
    Ok(sql(text, vec![key]))
}

/// `__orm_in($h, $property, $values)`: `"column" IN (?, ?, ...)`.
fn in_list(vm: &mut Vm, a: &[Value]) -> R {
    let m = meta_of(vm, &a[0])?;
    let c = m.column(vm, s(&a[1]))?;
    let items = list(&a[2]);
    if let Kind::Json(_) = c.kind {
        return Err(vm.throw(
            "ValueError",
            format!("{}::in(): `{}::${}` is kept as JSON and cannot be compared", m.what, m.entity, c.prop),
        ));
    }
    if items.len() > MAX_IN {
        return Err(vm.throw(
            "ValueError",
            format!("{}::in(): {} values, more than {MAX_IN}; split the list", m.what, items.len()),
        ));
    }
    if items.is_empty() {
        // Nothing is in an empty list (`IN ()` is not SQL).
        return Ok(sql("1 = 0".into(), Vec::new()));
    }
    let mut params = Vec::with_capacity(items.len());
    for v in items.iter() {
        let fits = match (&c.kind, v) {
            (Kind::Int, Value::Int(_)) | (Kind::Str, Value::Str(_)) | (Kind::Bool, Value::Bool(_)) => true,
            (Kind::Float, Value::Int(_) | Value::Float(_)) => true,
            (Kind::Enum { class, .. }, Value::Object(o)) => o.borrow().class == *class,
            _ => false,
        };
        if !fits {
            return Err(vm.throw(
                "ValueError",
                format!("{}::in(): `{}::${}` is `{}`, found {}", m.what, m.entity, c.prop, c.ty, given(vm, v)),
            ));
        }
        params.push(m.column_value(vm, c, v)?);
    }
    let marks = vec![PARAM.to_string(); params.len()].join(", ");
    Ok(sql(format!("{} IN ({marks})", m.ident(&c.name)), params))
}

pub fn lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "__orm_meta" => meta_new,
        "__orm_epoch" => |vm, a| Ok(Value::Int(epoch(&meta_of(vm, &a[0])?.table))),
        "__orm_wrote" => |vm, a| Ok(Value::Int(wrote(&meta_of(vm, &a[0])?.table))),
        "__orm_made_query" => made_query,
        "__orm_made" => made,
        "__orm_select" => select,
        "__orm_find" => find,
        "__orm_load" => |vm, a| {
            let m = meta_of(vm, &a[0])?;
            let row = map(&a[1]).clone();
            m.load(vm, &row)
        },
        "__orm_key" => key_text,
        "__orm_values" => values,
        "__orm_insert" => insert,
        "__orm_insert_mode" => insert_mode,
        "__orm_set_key" => set_key,
        "__orm_update" => update,
        "__orm_delete" => delete,
        "__orm_in" => in_list,
        "__orm_column" => |vm, a| {
            let m = meta_of(vm, &a[0])?;
            let c = m.column(vm, s(&a[1]))?;
            Ok(sql(m.ident(&c.name), Vec::new()))
        },
        _ => return None,
    };
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_are_read() {
        let d = parse_desc(
            "E\t3\tusers\t0\t1\nP\tid\tid\t?i\t?int\nP\tname\tuser_name\ts\tstring\nP\tstatus\tstatus\te4\tStatus\n\
             P\ttags\ttags\tjls\tlist<string>\nP\tmeta\tmeta\t?jc5\t?Meta\nc5 a!:i",
        )
        .unwrap();
        assert_eq!((d.class, d.table, d.key, d.generated), (3, "users", 0, true));
        assert_eq!(d.columns.len(), 5);
        assert_eq!(d.columns[1], ("name", "user_name", false, Kind::Str, "string"));
        assert_eq!(d.columns[2].3, Kind::Enum { class: 4, by_name: false });
        assert_eq!(d.columns[3].3, Kind::Json(ShapeText("ls".into())));
        assert!(d.columns[4].2);
        assert_eq!(d.json_lines, vec!["c5 a!:i"]);
    }

    /// A description that names something other than plain SQL names, or
    /// whose key is not an int or a string, is refused (the checker makes
    /// none; this is the second line of defence for the names in SQL).
    #[test]
    fn bad_descriptions_are_refused() {
        for bad in [
            "",
            "E\t3\tusers\t0\t1",
            "E\t3\tus\"ers\t0\t1\nP\tid\tid\ti\tint",
            "E\t3\tusers\t0\t1\nP\tid\tid`x\ti\tint",
            "E\t3\tusers\t0\t1\nP\tid\tid; DROP\ti\tint",
            "E\t3\tusers\t0\t1\nP\tid\t1id\ti\tint",
            "E\t3\tusers\t0\t1\nP\tid\tid\tjz\tint",
            "E\t3\tusers\t0\t1\nP\tid\tid\tf\tfloat",
            "E\t3\tusers\t1\t1\nP\tid\tid\ti\tint",
            "E\t3\tusers\t0\t1\nP\tid\tid\ti\tint\nX",
            &format!("E\t3\t{}\t0\t1\nP\tid\tid\ti\tint", "t".repeat(64)),
        ] {
            assert!(parse_desc(bad).is_none(), "{bad:?}");
        }
        assert!(parse_desc(&format!("E\t3\t{}\t0\t0\nP\tid\tid\ti\tint", "t".repeat(63))).is_some());
    }

    #[test]
    fn order_by_names_properties_with_a_direction() {
        assert_eq!(order_terms("").unwrap(), vec![]);
        assert_eq!(order_terms("  ").unwrap(), vec![]);
        assert_eq!(order_terms("name").unwrap(), vec![("name", false)]);
        assert_eq!(order_terms("name ASC, createdAt desc").unwrap(), vec![("name", false), ("createdAt", true)]);
        assert!(order_terms("name,").is_err());
        assert!(order_terms("name sideways").is_err());
        assert!(order_terms("name desc nulls").is_err());
        assert!(order_terms(&vec!["a"; MAX_ORDER_TERMS + 1].join(",")).is_err());
        assert!(order_terms(&vec!["a"; MAX_ORDER_TERMS].join(",")).is_ok());
        assert!(order_terms(&"a".repeat(MAX_ORDER_LEN + 1)).is_err());
    }

    /// Names are quoted for each database: SQLite in backquotes (a name in
    /// double quotes that names no column would be read as a string there).
    #[test]
    fn names_are_quoted_for_each_database() {
        let column = |name: &str| Column {
            prop: name.into(),
            name: name.into(),
            key: Key::Str(Rc::new(name.into())),
            kind: Kind::Int,
            nullable: false,
            ty: "int".into(),
            slot: 0,
        };
        let meta = |dialect| Meta {
            class: 0,
            what: "Repo<Order>".into(),
            entity: "Order".into(),
            table: "order".into(),
            key: 0,
            columns: vec![column("id"), column("select")],
            json: HashMap::new(),
            defaults: Vec::new(),
            auto_key: Cell::new(None),
            dialect,
        };
        let sqlite = meta(Dialect::Sqlite);
        assert_eq!(sqlite.select_list(), "`id`, `select`");
        let pg = meta(Dialect::Postgres);
        assert_eq!(crate::db::render_sql(&pg.select_list(), Dialect::Postgres), "\"id\", \"select\"");
        let my = meta(Dialect::MySql);
        assert_eq!(crate::db::render_sql(&my.ident(&my.table), Dialect::MySql), "`order`");
        let mut text = String::new();
        pg.where_key(&mut text);
        assert_eq!(crate::db::render_sql(&text, Dialect::Postgres), " WHERE \"id\" = $1");
    }

    #[test]
    fn names_in_messages_are_quoted_and_cut() {
        assert_eq!(quoted("a\"b"), "\"a\\\"b\"");
        assert_eq!(quoted(&"x".repeat(100)), format!("\"{}...\"", "x".repeat(64)));
    }
}
