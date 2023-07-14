use std::{
    collections::{HashMap, HashSet},
    fmt,
    net::SocketAddr,
    ops::Deref,
    str::FromStr,
    sync::Arc,
};

use compact_str::{CompactString, ToCompactString};
use fallible_iterator::FallibleIterator;
use indexmap::IndexMap;
use parking_lot::RwLock;
use rusqlite::Connection;
use serde::{
    de::{self, Visitor},
    Deserialize, Serialize,
};
use speedy::{Context, Readable, Writable};
use sqlite3_parser::{
    ast::{
        As, Cmd, Expr, Id, JoinConstraint, Name, OneSelect, Operator, ResultColumn, Select,
        SelectTable, Stmt,
    },
    lexer::sql::Parser,
};
use tokio::{
    sync::{
        broadcast,
        mpsc::{self, UnboundedSender},
    },
    task::block_in_place,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, trace, warn};
use uhlc::Timestamp;
use uuid::Uuid;

use crate::{
    api::RowResult,
    change::SqliteValue,
    filters::{parse_expr, AggregateChange, OwnedAggregateChange, SupportedExpr},
    schema::{NormalizedSchema, NormalizedTable},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub enum SubscriberId {
    Local { addr: SocketAddr },
    Global,
}

impl fmt::Display for SubscriberId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SubscriberId::Local { addr } => addr.fmt(f),
            SubscriberId::Global => f.write_str("global"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubscriptionId(pub CompactString);

impl SubscriptionId {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for SubscriptionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<'a, C: Context> Readable<'a, C> for SubscriptionId {
    fn read_from<R: speedy::Reader<'a, C>>(reader: &mut R) -> Result<Self, <C as Context>::Error> {
        let s = <&str as Readable<'a, C>>::read_from(reader)?;
        Ok(Self(s.into()))
    }
}

impl<'a, C: Context> Writable<C> for SubscriptionId {
    fn write_to<T: ?Sized + speedy::Writer<C>>(
        &self,
        writer: &mut T,
    ) -> Result<(), <C as Context>::Error> {
        self.0.as_bytes().write_to(writer)
    }
}

#[derive(Debug)]
pub enum Subscriber {
    Local {
        subscriptions: HashMap<SubscriptionId, SubscriptionInfo>,
        sender: UnboundedSender<SubscriptionMessage>,
    },
    Global {
        subscriptions: HashMap<SubscriptionId, SubscriptionInfo>,
    },
}

impl Subscriber {
    pub fn insert(&mut self, id: SubscriptionId, info: SubscriptionInfo) {
        match self {
            Subscriber::Local { subscriptions, .. } => subscriptions,
            Subscriber::Global { subscriptions } => subscriptions,
        }
        .insert(id, info);
    }

    pub fn remove(&mut self, id: &SubscriptionId) -> Option<SubscriptionInfo> {
        match self {
            Subscriber::Local { subscriptions, .. } => subscriptions,
            Subscriber::Global { subscriptions } => subscriptions,
        }
        .remove(id)
    }

    pub fn as_local(
        &self,
    ) -> Option<(
        &HashMap<SubscriptionId, SubscriptionInfo>,
        &UnboundedSender<SubscriptionMessage>,
    )> {
        match self {
            Subscriber::Local {
                subscriptions,
                sender,
            } => Some((subscriptions, sender)),
            Subscriber::Global { .. } => None,
        }
    }
}

#[derive(Debug)]
pub struct SubscriptionInfo {
    pub filter: Option<SubscriptionFilter>,
    pub updated_at: Timestamp,
}

pub type Subscriptions = Arc<RwLock<Subscriber>>;
pub type Subscribers = Arc<RwLock<HashMap<SubscriberId, Subscriptions>>>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionMessage {
    Event {
        id: SubscriptionId,
        event: SubscriptionEvent,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum SubscriptionEvent {
    Change(OwnedAggregateChange),
    Error { error: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Subscription {
    Add {
        id: SubscriptionId,
        where_clause: Option<String>,
        #[serde(default)]
        from_db_version: Option<i64>,
    },
    Remove {
        id: SubscriptionId,
    },
}

#[derive(Debug, Clone)]
pub struct SubscriptionFilter(Arc<String>, Arc<SupportedExpr>);

impl Deref for SubscriptionFilter {
    type Target = SupportedExpr;

    fn deref(&self) -> &Self::Target {
        &self.1
    }
}

impl SubscriptionFilter {
    pub fn new(input: String, expr: SupportedExpr) -> Self {
        Self(Arc::new(input), Arc::new(expr))
    }

    pub fn input(&self) -> &str {
        &self.0
    }
    pub fn expr(&self) -> &SupportedExpr {
        &self.1
    }
}

impl PartialEq for SubscriptionFilter {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for SubscriptionFilter {}

impl Serialize for SubscriptionFilter {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SubscriptionFilter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_string(SubscriptionFilterVisitor)
    }
}

struct SubscriptionFilterVisitor;

impl<'de> Visitor<'de> for SubscriptionFilterVisitor {
    type Value = SubscriptionFilter;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "a string")
    }

    fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_string(s.to_owned())
    }

    fn visit_string<E>(self, s: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        s.parse().map_err(de::Error::custom)
    }
}

impl FromStr for SubscriptionFilter {
    type Err = crate::filters::ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let expr = parse_expr(s)?;
        Ok(SubscriptionFilter::new(s.to_owned(), expr))
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeType {
    Upsert,
    Delete,
}

pub enum MatcherCmd {
    ProcessChange(MatcherStmt, Vec<SqliteValue>),
    Unsubscribe,
}

#[derive(Debug, Clone)]
pub struct Matcher(pub Arc<InnerMatcher>);

#[derive(Debug, Clone)]
pub struct MatcherStmt {
    new_query: String,
    temp_query: String,
}

#[derive(Debug)]
pub struct InnerMatcher {
    pub id: Uuid,
    pub query: Stmt,
    pub statements: HashMap<String, MatcherStmt>,
    pub pks: IndexMap<String, Vec<String>>,
    pub parsed: ParsedSelect,
    pub query_table: String,
    pub qualified_table_name: String,
    pub change_tx: broadcast::Sender<RowResult>,
    pub cmd_tx: mpsc::Sender<MatcherCmd>,
    pub col_names: Vec<CompactString>,
    pub cancel: CancellationToken,
}

impl Matcher {
    pub fn new(
        id: Uuid,
        schema: &NormalizedSchema,
        mut conn: Connection,
        init_tx: mpsc::Sender<RowResult>,
        change_tx: broadcast::Sender<RowResult>,
        sql: &str,
        cancel: CancellationToken,
    ) -> Result<Self, MatcherError> {
        let col_names: Vec<CompactString> = {
            conn.prepare(sql)?
                .column_names()
                .into_iter()
                .map(|s| s.to_compact_string())
                .collect()
        };

        let mut parser = Parser::new(sql.as_bytes());

        let (stmt, parsed) = match parser.next()?.ok_or(MatcherError::StatementRequired)? {
            Cmd::Stmt(stmt) => {
                let parsed = match stmt {
                    Stmt::Select(ref select) => extract_select_columns(select, schema)?,
                    _ => return Err(MatcherError::UnsupportedStatement),
                };

                (stmt, parsed)
            }
            _ => return Err(MatcherError::StatementRequired),
        };

        // println!("{stmt:#?}");
        // println!("parsed: {parsed:#?}");

        if parsed.table_columns.is_empty() {
            return Err(MatcherError::TableRequired);
        }

        let mut statements = HashMap::new();

        let mut pks = IndexMap::default();

        let mut stmt = stmt.clone();
        match &mut stmt {
            Stmt::Select(select) => match &mut select.body.select {
                OneSelect::Select { columns, .. } => {
                    let mut new_cols = parsed
                        .table_columns
                        .iter()
                        .filter_map(|(tbl_name, _cols)| {
                            schema.tables.get(tbl_name).map(|table| {
                                let tbl_name = parsed
                                    .aliases
                                    .iter()
                                    .find_map(|(alias, actual)| {
                                        (actual == tbl_name).then_some(alias)
                                    })
                                    .unwrap_or(tbl_name);
                                table
                                    .pk
                                    .iter()
                                    .map(|pk| {
                                        let alias = format!("__corro_pk_{tbl_name}_{pk}");
                                        let entry: &mut Vec<String> =
                                            pks.entry(table.name.clone()).or_default();
                                        entry.push(alias.clone());

                                        ResultColumn::Expr(
                                            Expr::Qualified(
                                                Name(tbl_name.clone()),
                                                Name(pk.clone()),
                                            ),
                                            Some(As::As(Name(alias))),
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            })
                        })
                        .flatten()
                        .collect::<Vec<_>>();

                    new_cols.append(&mut parsed.columns.clone());
                    *columns = new_cols;
                }
                _ => unreachable!(),
            },
            _ => unreachable!(),
        }

        let query_table = format!("query_{}", id.as_simple());

        for (tbl_name, _cols) in parsed.table_columns.iter() {
            let expr = table_to_expr(
                &parsed.aliases,
                schema
                    .tables
                    .get(tbl_name)
                    .expect("this should not happen, missing table in schema"),
                &tbl_name,
            )?;

            let mut stmt = stmt.clone();

            match &mut stmt {
                Stmt::Select(select) => match &mut select.body.select {
                    OneSelect::Select { where_clause, .. } => {
                        *where_clause = if let Some(prev) = where_clause.take() {
                            Some(Expr::Binary(Box::new(expr), Operator::And, Box::new(prev)))
                        } else {
                            Some(expr)
                        };
                    }
                    _ => {}
                },
                _ => {}
            }

            let mut new_query = Cmd::Stmt(stmt).to_string();
            new_query.pop();

            let mut tmp_cols = pks.values().cloned().flatten().collect::<Vec<String>>();
            for i in 0..(parsed.columns.len()) {
                tmp_cols.push(format!("col_{i}"));
            }

            statements.insert(
                tbl_name.clone(),
                MatcherStmt {
                    new_query,
                    temp_query: format!(
                        "SELECT {} FROM {} WHERE {}",
                        tmp_cols.join(","),
                        query_table,
                        pks.get(tbl_name)
                            .cloned()
                            .ok_or(MatcherError::MissingPrimaryKeys)?
                            .iter()
                            .map(|k| format!("{k} IS ?"))
                            .collect::<Vec<_>>()
                            .join(" AND ")
                    ),
                },
            );
        }

        let (cmd_tx, mut cmd_rx) = mpsc::channel(512);

        let matcher = Self(Arc::new(InnerMatcher {
            id,
            query: stmt,
            statements: statements,
            pks,
            parsed,
            qualified_table_name: format!("watches.{query_table}"),
            query_table,
            change_tx,
            cmd_tx,
            col_names: col_names.clone(),
            cancel: cancel.clone(),
        }));

        let mut tmp_cols = matcher
            .0
            .pks
            .values()
            .flatten()
            .cloned()
            .collect::<Vec<String>>();

        for i in 0..(matcher.0.parsed.columns.len()) {
            tmp_cols.push(format!("col_{i}"));
        }

        let create_temp_table = format!(
            "CREATE TABLE {} (__corro_rowid INTEGER PRIMARY KEY AUTOINCREMENT, {});
            CREATE UNIQUE INDEX watches.index_{}_pk ON {} ({});",
            matcher.0.qualified_table_name,
            tmp_cols.join(","),
            matcher.0.id.as_simple(),
            matcher.0.query_table,
            matcher
                .0
                .pks
                .values()
                .flatten()
                .cloned()
                .collect::<Vec<_>>()
                .join(","),
        );

        conn.execute_batch(&create_temp_table)?;

        tokio::spawn({
            let matcher = matcher.clone();
            async move {
                let _drop_guard = cancel.clone().drop_guard();
                if let Err(e) = init_tx.send(RowResult::Columns(col_names)).await {
                    error!("could not send back columns, probably means no receivers! {e}");
                    return;
                }

                let mut query_cols = vec![];
                for i in 0..(matcher.0.parsed.columns.len()) {
                    query_cols.push(format!("col_{i}"));
                }

                let res = block_in_place(|| {
                    let tx = conn.transaction()?;

                    let mut stmt_str = Cmd::Stmt(matcher.0.query.clone()).to_string();
                    stmt_str.pop();

                    let insert_into = format!(
                        "INSERT INTO {} ({}) {} RETURNING __corro_rowid,{}",
                        matcher.0.qualified_table_name,
                        tmp_cols.join(","),
                        stmt_str,
                        query_cols.join(","),
                    );

                    {
                        let mut prepped = tx.prepare(&insert_into)?;

                        let mut rows = prepped.query(())?;

                        loop {
                            match rows.next() {
                                Ok(Some(row)) => {
                                    let rowid: i64 = row.get(0)?;
                                    let cells = (1..=query_cols.len())
                                        .map(|i| row.get::<_, SqliteValue>(i))
                                        .collect::<rusqlite::Result<Vec<_>>>()?;

                                    if let Err(e) = init_tx.blocking_send(RowResult::Row {
                                        change_type: ChangeType::Upsert,
                                        rowid,
                                        cells,
                                    }) {
                                        error!("could not send back row: {e}");
                                        return Err(MatcherError::ChangeReceiverClosed);
                                    }
                                }
                                Ok(None) => {
                                    // done!
                                    break;
                                }
                                Err(e) => {
                                    return Err(e.into());
                                }
                            }
                        }
                    }

                    tx.commit()?;

                    Ok::<_, MatcherError>(())
                });

                if let Err(e) = res {
                    _ = init_tx.send(RowResult::Error(e.to_compact_string())).await;
                    return;
                }

                if let Err(e) = init_tx.send(RowResult::EndOfQuery).await {
                    error!("could not send back end-of-query message: {e}");
                    return;
                }

                loop {
                    let req = tokio::select! {
                        Some(req) = cmd_rx.recv() => req,
                        _ = cancel.cancelled() => return,
                        else => return,
                    };

                    match req {
                        MatcherCmd::ProcessChange(stmt, pks) => {
                            if let Err(e) =
                                block_in_place(|| matcher.handle_change(&mut conn, stmt, pks))
                            {
                                if matches!(e, MatcherError::ChangeReceiverClosed) {
                                    // break here...
                                    break;
                                }
                                error!("could not handle change: {e}");
                            }
                        }
                        MatcherCmd::Unsubscribe => {
                            if matcher.0.change_tx.receiver_count() == 0 {
                                info!(
                                    "matcher {} has no more subscribers, we're done!",
                                    matcher.0.id
                                );
                                break;
                            }
                        }
                    }
                }
                if let Err(e) =
                    conn.execute_batch(&format!("DROP TABLE {}", matcher.0.qualified_table_name))
                {
                    warn!(
                        "could not clean up temporary table {} => {e}",
                        matcher.0.qualified_table_name
                    );
                }
            }
        });

        Ok(matcher)
    }

    pub fn cmd_tx(&self) -> &mpsc::Sender<MatcherCmd> {
        &self.0.cmd_tx
    }

    pub fn has_match(&self, agg: &AggregateChange) -> bool {
        self.0.statements.contains_key(agg.table)
    }

    pub fn process_change<'a>(&self, agg: &AggregateChange<'a>) -> Result<(), MatcherError> {
        let stmt = if let Some(stmt) = self.0.statements.get(agg.table) {
            stmt
        } else {
            trace!("irrelevant table!");
            return Ok(());
        };

        self.0
            .cmd_tx
            .try_send(MatcherCmd::ProcessChange(
                stmt.clone(),
                agg.pk.values().map(|v| v.to_owned()).collect(),
            ))
            .map_err(|_| MatcherError::ChangeQueueClosedOrFull)?;

        Ok(())
    }

    pub fn table_name(&self) -> &str {
        &self.0.qualified_table_name
    }

    pub fn handle_change(
        &self,
        conn: &mut Connection,
        stmt: MatcherStmt,
        pks: Vec<SqliteValue>,
    ) -> Result<(), MatcherError> {
        let mut actual_cols = vec![];
        let mut tmp_cols = self
            .0
            .pks
            .values()
            .cloned()
            .flatten()
            .collect::<Vec<String>>();
        for i in 0..(self.0.parsed.columns.len()) {
            let col_name = format!("col_{i}");
            tmp_cols.push(col_name.clone());
            actual_cols.push(col_name);
        }

        let tx = conn.transaction()?;

        let sql = format!(
            "INSERT INTO {} ({})
                            SELECT * FROM (
                                {}
                                EXCEPT
                                {}
                            ) WHERE 1
                        ON CONFLICT({})
                            DO UPDATE SET
                                {}
                        RETURNING __corro_rowid,{}",
            // insert into
            self.0.qualified_table_name,
            tmp_cols.join(","),
            stmt.new_query,
            stmt.temp_query,
            self.0
                .pks
                .values()
                .cloned()
                .flatten()
                .collect::<Vec<String>>()
                .join(","),
            (0..(self.0.parsed.columns.len()))
                .map(|i| format!("col_{i} = excluded.col_{i}"))
                .collect::<Vec<_>>()
                .join(","),
            actual_cols.join(",")
        );

        // println!("sql: {sql}");

        let mut insert_prepped = tx.prepare_cached(&sql)?;

        let mut i = 1;

        // do this 2 times
        for _ in 0..2 {
            for pk in pks.iter() {
                insert_prepped.raw_bind_parameter(i, pk.to_owned())?;
                i += 1;
            }
        }

        let sql = format!(
            "
        DELETE FROM {} WHERE ({}) in (SELECT {} FROM (
            {}
            EXCEPT
            {}
        )) RETURNING __corro_rowid,{}",
            // delete from
            self.0.qualified_table_name,
            self.0
                .pks
                .values()
                .cloned()
                .flatten()
                .collect::<Vec<String>>()
                .join(","),
            self.0
                .pks
                .values()
                .cloned()
                .flatten()
                .collect::<Vec<String>>()
                .join(","),
            stmt.temp_query,
            stmt.new_query,
            actual_cols.join(",")
        );

        let mut delete_prepped = tx.prepare_cached(&sql)?;

        let mut i = 1;

        // do this 2 times
        for _ in 0..2 {
            for pk in pks.iter() {
                delete_prepped.raw_bind_parameter(i, pk.to_owned())?;
                i += 1;
            }
        }

        for (change_type, mut prepped) in [
            (ChangeType::Upsert, insert_prepped),
            (ChangeType::Delete, delete_prepped),
        ] {
            let col_count = prepped.column_count();

            let mut rows = prepped.raw_query();

            while let Ok(Some(row)) = rows.next() {
                let rowid: i64 = row.get(0)?;

                match (1..col_count)
                    .map(|i| row.get::<_, SqliteValue>(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
                {
                    Ok(cells) => {
                        if let Err(e) = self.0.change_tx.send(RowResult::Row {
                            rowid,
                            change_type,
                            cells,
                        }) {
                            error!("could not send back row to matcher sub sender: {e}");
                            return Err(MatcherError::ChangeReceiverClosed);
                        }
                    }
                    Err(e) => {
                        error!("could not deserialize row's cells: {e}");
                        return Ok(());
                    }
                }
            }
        }

        tx.commit()?;

        Ok(())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RowResult> {
        self.0.change_tx.subscribe()
    }

    pub fn cancel(&self) -> CancellationToken {
        self.0.cancel.clone()
    }
}

#[derive(Debug, Default)]
pub struct ParsedSelect {
    table_columns: IndexMap<String, HashSet<String>>,
    aliases: HashMap<String, String>,
    pub columns: Vec<ResultColumn>,
    children: Vec<Box<ParsedSelect>>,
}

fn extract_select_columns(
    select: &Select,
    schema: &NormalizedSchema,
) -> Result<ParsedSelect, MatcherError> {
    let mut parsed = ParsedSelect::default();

    match select.body.select {
        OneSelect::Select {
            ref from,
            ref columns,
            ref where_clause,
            ..
        } => {
            let from_table = match from {
                Some(from) => {
                    let from_table = match &from.select {
                        Some(table) => match table.as_ref() {
                            SelectTable::Table(name, alias, _) => {
                                if schema.tables.contains_key(name.name.0.as_str()) {
                                    if let Some(As::As(alias) | As::Elided(alias)) = alias {
                                        parsed.aliases.insert(alias.0.clone(), name.name.0.clone());
                                    } else if let Some(ref alias) = name.alias {
                                        parsed.aliases.insert(alias.0.clone(), name.name.0.clone());
                                    }
                                    parsed.table_columns.entry(name.name.0.clone()).or_default();
                                    Some(&name.name)
                                } else {
                                    return Err(MatcherError::TableNotFound(name.name.0.clone()));
                                }
                            }
                            // TODO: add support for:
                            // TableCall(QualifiedName, Option<Vec<Expr>>, Option<As>),
                            // Select(Select, Option<As>),
                            // Sub(FromClause, Option<As>),
                            t => {
                                warn!("ignoring {t:?}");
                                None
                            }
                        },
                        _ => {
                            // according to the sqlite3-parser docs, this can't really happen
                            // ignore!
                            unreachable!()
                        }
                    };
                    if let Some(ref joins) = from.joins {
                        for join in joins.iter() {
                            // let mut tbl_name = None;
                            let tbl_name = match &join.table {
                                SelectTable::Table(name, alias, _) => {
                                    if let Some(As::As(alias) | As::Elided(alias)) = alias {
                                        parsed.aliases.insert(alias.0.clone(), name.name.0.clone());
                                    } else if let Some(ref alias) = name.alias {
                                        parsed.aliases.insert(alias.0.clone(), name.name.0.clone());
                                    }
                                    parsed.table_columns.entry(name.name.0.clone()).or_default();
                                    &name.name
                                }
                                // TODO: add support for:
                                // TableCall(QualifiedName, Option<Vec<Expr>>, Option<As>),
                                // Select(Select, Option<As>),
                                // Sub(FromClause, Option<As>),
                                t => {
                                    warn!("ignoring JOIN's non-SelectTable::Table:  {t:?}");
                                    continue;
                                }
                            };
                            // ON or USING
                            if let Some(constraint) = &join.constraint {
                                match constraint {
                                    JoinConstraint::On(expr) => {
                                        extract_expr_columns(expr, schema, &mut parsed)?;
                                    }
                                    JoinConstraint::Using(names) => {
                                        let entry = parsed
                                            .table_columns
                                            .entry(tbl_name.0.clone())
                                            .or_default();
                                        for name in names.iter() {
                                            entry.insert(name.0.clone());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if let Some(expr) = where_clause {
                        extract_expr_columns(expr, schema, &mut parsed)?;
                    }
                    from_table
                }
                _ => None,
            };

            extract_columns(columns.as_slice(), from_table, schema, &mut parsed)?;
        }
        _ => {}
    }

    Ok(parsed)
}

fn extract_expr_columns(
    expr: &Expr,
    schema: &NormalizedSchema,
    parsed: &mut ParsedSelect,
) -> Result<(), MatcherError> {
    match expr {
        // simplest case
        Expr::Qualified(tblname, colname) => {
            let resolved_name = parsed.aliases.get(&tblname.0).unwrap_or(&tblname.0);
            parsed
                .table_columns
                .entry(resolved_name.clone())
                .or_default()
                .insert(colname.0.clone());
        }
        // simplest case but also mentioning the schema
        Expr::DoublyQualified(schema_name, tblname, colname) if schema_name.0 == "main" => {
            let resolved_name = parsed.aliases.get(&tblname.0).unwrap_or(&tblname.0);
            parsed
                .table_columns
                .entry(resolved_name.clone())
                .or_default()
                .insert(colname.0.clone());
        }

        Expr::Name(_) => {
            // figure out which table this is for...
            todo!()
        }

        Expr::Between { lhs, .. } => extract_expr_columns(lhs, schema, parsed)?,
        Expr::Binary(lhs, _, rhs) => {
            extract_expr_columns(lhs, schema, parsed)?;
            extract_expr_columns(rhs, schema, parsed)?;
        }
        Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } => {
            if let Some(expr) = base {
                extract_expr_columns(expr, schema, parsed)?;
            }
            for (when_expr, _then_expr) in when_then_pairs.iter() {
                // NOTE: should we also parse the then expr?
                extract_expr_columns(when_expr, schema, parsed)?;
            }
            if let Some(expr) = else_expr {
                extract_expr_columns(expr, schema, parsed)?;
            }
        }
        Expr::Cast { expr, .. } => extract_expr_columns(expr, schema, parsed)?,
        Expr::Collate(expr, _) => extract_expr_columns(expr, schema, parsed)?,
        Expr::Exists(select) => {
            parsed
                .children
                .push(Box::new(extract_select_columns(select, schema)?));
        }
        Expr::FunctionCall { args, .. } => {
            if let Some(args) = args {
                for expr in args.iter() {
                    extract_expr_columns(expr, schema, parsed)?;
                }
            }
        }
        Expr::InList { lhs, rhs, .. } => {
            extract_expr_columns(lhs, schema, parsed)?;
            if let Some(rhs) = rhs {
                for expr in rhs.iter() {
                    extract_expr_columns(expr, schema, parsed)?;
                }
            }
        }
        Expr::InSelect { lhs, rhs, .. } => {
            extract_expr_columns(lhs, schema, parsed)?;
            parsed
                .children
                .push(Box::new(extract_select_columns(rhs, schema)?));
        }
        expr @ Expr::InTable { .. } => {
            return Err(MatcherError::UnsupportedExpr { expr: expr.clone() })
        }
        Expr::IsNull(expr) => {
            extract_expr_columns(expr, schema, parsed)?;
        }
        Expr::Like { lhs, rhs, .. } => {
            extract_expr_columns(lhs, schema, parsed)?;
            extract_expr_columns(rhs, schema, parsed)?;
        }

        Expr::NotNull(expr) => {
            extract_expr_columns(expr, schema, parsed)?;
        }
        Expr::Parenthesized(parens) => {
            for expr in parens.iter() {
                extract_expr_columns(expr, schema, parsed)?;
            }
        }
        Expr::Subquery(select) => {
            parsed
                .children
                .push(Box::new(extract_select_columns(select, schema)?));
        }
        Expr::Unary(_, expr) => {
            extract_expr_columns(expr, schema, parsed)?;
        }

        // no column names in there...
        // Expr::FunctionCallStar { name, filter_over } => todo!(),
        // Expr::Id(_) => todo!(),
        // Expr::Literal(_) => todo!(),
        // Expr::Raise(_, _) => todo!(),
        // Expr::Variable(_) => todo!(),
        _ => {}
    }

    Ok(())
}

fn extract_columns(
    columns: &[ResultColumn],
    from: Option<&Name>,
    schema: &NormalizedSchema,
    parsed: &mut ParsedSelect,
) -> Result<(), MatcherError> {
    let mut i = 0;
    for col in columns.iter() {
        match col {
            ResultColumn::Expr(expr, _) => {
                extract_expr_columns(expr, schema, parsed)?;
                parsed.columns.push(ResultColumn::Expr(
                    expr.clone(),
                    Some(As::As(Name(format!("col_{i}")))),
                ));
                i += 1;
            }
            ResultColumn::Star => {
                if let Some(tbl_name) = from {
                    if let Some(table) = schema.tables.get(&tbl_name.0) {
                        let entry = parsed.table_columns.entry(table.name.clone()).or_default();
                        for col in table.columns.keys() {
                            entry.insert(col.clone());
                            parsed.columns.push(ResultColumn::Expr(
                                Expr::Name(Name(col.clone())),
                                Some(As::As(Name(format!("col_{i}")))),
                            ));
                            i += 1;
                        }
                    } else {
                        return Err(MatcherError::TableStarNotFound {
                            tbl_name: tbl_name.0.clone(),
                        });
                    }
                } else {
                    unreachable!()
                }
            }
            ResultColumn::TableStar(tbl_name) => {
                let name = parsed
                    .aliases
                    .get(tbl_name.0.as_str())
                    .unwrap_or(&tbl_name.0);
                if let Some(table) = schema.tables.get(name) {
                    let entry = parsed.table_columns.entry(table.name.clone()).or_default();
                    for col in table.columns.keys() {
                        entry.insert(col.clone());
                        parsed.columns.push(ResultColumn::Expr(
                            Expr::Qualified(tbl_name.clone(), Name(col.clone())),
                            Some(As::As(Name(format!("col_{i}")))),
                        ));
                        i += 1;
                    }
                } else {
                    return Err(MatcherError::TableStarNotFound {
                        tbl_name: name.clone(),
                    });
                }
            }
        }
    }
    Ok(())
}

fn table_to_expr(
    aliases: &HashMap<String, String>,
    tbl: &NormalizedTable,
    table: &str,
) -> Result<Expr, MatcherError> {
    let tbl_name = aliases
        .iter()
        .find_map(|(alias, actual)| (actual == table).then_some(alias))
        .cloned()
        .unwrap_or_else(|| table.to_owned());

    let mut pk_iter = tbl.pk.iter();

    let first = pk_iter
        .next()
        .ok_or_else(|| MatcherError::NoPrimaryKey(tbl_name.clone()))?;

    let mut expr = expr_from_pk(tbl_name.as_str(), first.as_str())
        .ok_or_else(|| MatcherError::AggPrimaryKeyMissing(tbl_name.clone(), first.clone()))?;

    for pk in pk_iter {
        expr = Expr::Binary(
            Box::new(expr),
            Operator::And,
            Box::new(expr_from_pk(tbl_name.as_str(), pk.as_str()).ok_or_else(|| {
                MatcherError::AggPrimaryKeyMissing(tbl_name.clone(), first.clone())
            })?),
        );
    }

    Ok(expr)
}

#[derive(Debug, thiserror::Error)]
pub enum MatcherError {
    #[error(transparent)]
    Lexer(#[from] sqlite3_parser::lexer::sql::Error),
    #[error("one statement is required for matching")]
    StatementRequired,
    #[error("unsupported statement")]
    UnsupportedStatement,
    #[error("at least 1 table is required in FROM / JOIN clause")]
    TableRequired,
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("table not found in schema: {0}")]
    TableNotFound(String),
    #[error("no primary key for table: {0}")]
    NoPrimaryKey(String),
    #[error("aggregate missing primary key {0}.{1}")]
    AggPrimaryKeyMissing(String, String),
    #[error("JOIN .. ON expression is not supported for join on table '{table}': {expr:?}")]
    JoinOnExprUnsupported { table: String, expr: Expr },
    #[error("expression is not supported: {expr:?}")]
    UnsupportedExpr { expr: Expr },
    #[error("could not find table for {tbl_name}.* in corrosion's schema")]
    TableStarNotFound { tbl_name: String },
    #[error("missing primary keys, this shouldn't happen")]
    MissingPrimaryKeys,
    #[error("change queue has been closed or is full")]
    ChangeQueueClosedOrFull,
    #[error("change receiver is closed")]
    ChangeReceiverClosed,
}

fn expr_from_pk(table: &str, pk: &str) -> Option<Expr> {
    Some(Expr::Binary(
        Box::new(Expr::Qualified(Name(table.to_owned()), Name(pk.to_owned()))),
        Operator::Is,
        Box::new(Expr::Id(Id("?".into()))),
    ))
}

#[cfg(test)]
mod tests {
    use crate::{
        actor::ActorId,
        change::{SqliteValue, SqliteValueRef},
        filters::ChangeEvent,
        schema::parse_sql,
        sqlite::setup_conn,
    };

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_diff() {
        let sql = "SELECT json_object(
            'targets', json_array(cs.address||':'||cs.port),
            'labels',  json_object(
              '__metrics_path__', JSON_EXTRACT(cs.meta, '$.path'),
              'app',            cs.app_name,
              'vm_account_id',  cs.organization_id,
              'instance',       cs.instance_id
            )
          )
          FROM consul_services cs
            LEFT JOIN machines m                   ON m.id = cs.instance_id
            LEFT JOIN machine_versions mv          ON m.id = mv.machine_id  AND m.machine_version_id = mv.id
            LEFT JOIN machine_version_statuses mvs ON m.id = mvs.machine_id AND m.machine_version_id = mvs.id
          WHERE cs.node = 'test-hostname'
            AND (mvs.status IS NULL OR mvs.status = 'started')
            AND cs.name == 'app-prometheus'";

        let schema_sql = "
          CREATE TABLE consul_services (
              node TEXT NOT NULL,
              id TEXT NOT NULL,
              name TEXT NOT NULL DEFAULT '',
              tags TEXT NOT NULL DEFAULT '[]',
              meta TEXT NOT NULL DEFAULT '{}',
              port INTEGER NOT NULL DEFAULT 0,
              address TEXT NOT NULL DEFAULT '',
              updated_at INTEGER NOT NULL DEFAULT 0,
              app_id INTEGER AS (CAST(JSON_EXTRACT(meta, '$.app_id') AS INTEGER)), network_id INTEGER AS (
                  CAST(JSON_EXTRACT(meta, '$.network_id') AS INTEGER)
              ), app_name TEXT AS (JSON_EXTRACT(meta, '$.app_name')), instance_id TEXT AS (
                  COALESCE(
                      JSON_EXTRACT(meta, '$.machine_id'),
                      SUBSTR(JSON_EXTRACT(meta, '$.alloc_id'), 1, 8),
                      CASE
                          WHEN INSTR(id, '_nomad-task-') = 1 THEN SUBSTR(id, 13, 8)
                          ELSE NULL
                      END
                  )
              ), organization_id INTEGER AS (
                  CAST(
                      JSON_EXTRACT(meta, '$.organization_id') AS INTEGER
                  )
              ), protocol TEXT
          AS (JSON_EXTRACT(meta, '$.protocol')),
              PRIMARY KEY (node, id)
          );
  
          CREATE TABLE machines (
              id TEXT NOT NULL PRIMARY KEY,
              node TEXT NOT NULL DEFAULT '',
              name TEXT NOT NULL DEFAULT '',
              machine_version_id TEXT NOT NULL DEFAULT '',
              app_id INTEGER NOT NULL DEFAULT 0,
              organization_id INTEGER NOT NULL DEFAULT 0,
              network_id INTEGER NOT NULL DEFAULT 0,
              updated_at INTEGER NOT NULL DEFAULT 0
          );
  
          CREATE TABLE machine_versions (
              machine_id TEXT NOT NULL,
              id TEXT NOT NULL DEFAULT '',
              config TEXT NOT NULL DEFAULT '{}',
              updated_at INTEGER NOT NULL DEFAULT 0,
              PRIMARY KEY (machine_id, id)
          );
  
          CREATE TABLE machine_version_statuses (
              machine_id TEXT NOT NULL,
              id TEXT NOT NULL,
              status TEXT NOT NULL DEFAULT '',
              updated_at INTEGER NOT NULL DEFAULT 0,
              PRIMARY KEY (machine_id, id)
          );
          ";

        let schema = parse_sql(schema_sql).unwrap();

        let tmpdir = tempfile::tempdir().unwrap();
        let db_path = tmpdir.path().join("test.db");

        let mut conn = rusqlite::Connection::open(&db_path).expect("could not open conn");

        setup_conn(
            &mut conn,
            &[(
                tmpdir
                    .path()
                    .join("watches.db")
                    .display()
                    .to_string()
                    .into(),
                "watches".into(),
            )]
            .into(),
        )
        .unwrap();

        conn.execute_batch(schema_sql)
            .expect("could not exec schema");

        // let's seed some data in there
        {
            let tx = conn.transaction().unwrap();
            tx.execute_batch(r#"
                        INSERT INTO consul_services (node, id, name, address, port, meta) VALUES ('test-hostname', 'service-1', 'app-prometheus', '127.0.0.1', 1, '{"path": "/1", "machine_id": "m-1"}');
        
                        INSERT INTO machines (id, machine_version_id) VALUES ('m-1', 'mv-1');
        
                        INSERT INTO machine_versions (machine_id, id) VALUES ('m-1', 'mv-1');
        
                        INSERT INTO machine_version_statuses (machine_id, id, status) VALUES ('m-1', 'mv-1', 'started');

                        INSERT INTO consul_services (node, id, name, address, port, meta) VALUES ('test-hostname', 'service-2', 'not-app-prometheus', '127.0.0.1', 1, '{"path": "/1", "machine_id": "m-2"}');

                INSERT INTO machines (id, machine_version_id) VALUES ('m-2', 'mv-2');

                INSERT INTO machine_versions (machine_id, id) VALUES ('m-2', 'mv-2');

                INSERT INTO machine_version_statuses (machine_id, id, status) VALUES ('m-2', 'mv-2', 'started');
                    "#).unwrap();
            tx.commit().unwrap();
        }

        let cancel = CancellationToken::new();
        let id = Uuid::new_v4();

        let mut matcher_conn = rusqlite::Connection::open(&db_path).expect("could not open conn");

        setup_conn(
            &mut matcher_conn,
            &[(
                tmpdir
                    .path()
                    .join("watches.db")
                    .display()
                    .to_string()
                    .into(),
                "watches".into(),
            )]
            .into(),
        )
        .unwrap();

        {
            let (tx, mut rx) = mpsc::channel(1);
            let (change_tx, mut change_rx) = broadcast::channel(1);
            let matcher =
                Matcher::new(id, &schema, matcher_conn, tx, change_tx, sql, cancel).unwrap();

            assert!(matches!(rx.recv().await.unwrap(), RowResult::Columns(_)));

            let cells = vec![SqliteValue::Text("{\"targets\":[\"127.0.0.1:1\"],\"labels\":{\"__metrics_path__\":\"/1\",\"app\":null,\"vm_account_id\":null,\"instance\":\"m-1\"}}".into())];

            assert_eq!(
                rx.recv().await.unwrap(),
                RowResult::Row {
                    rowid: 1,
                    change_type: ChangeType::Upsert,
                    cells
                }
            );
            assert!(matches!(rx.recv().await.unwrap(), RowResult::EndOfQuery));

            matcher
                .process_change(&AggregateChange {
                    actor_id: ActorId::default(),
                    version: 1,
                    table: "consul_services",
                    pk: vec![
                        ("node", SqliteValueRef::Text("test-hostname")),
                        ("id", SqliteValueRef::Text("service-1")),
                    ]
                    .into_iter()
                    .collect(),
                    evt_type: ChangeEvent::Insert,
                    data: vec![("name", SqliteValueRef::Text("app-prometheus"))]
                        .into_iter()
                        .collect(),
                })
                .unwrap();

            // insert the second row
            {
                let tx = conn.transaction().unwrap();
                tx.execute_batch(r#"
                INSERT INTO consul_services (node, id, name, address, port, meta) VALUES ('test-hostname', 'service-3', 'app-prometheus', '127.0.0.1', 1, '{"path": "/1", "machine_id": "m-3"}');

                INSERT INTO machines (id, machine_version_id) VALUES ('m-3', 'mv-3');

                INSERT INTO machine_versions (machine_id, id) VALUES ('m-3', 'mv-3');

                INSERT INTO machine_version_statuses (machine_id, id, status) VALUES ('m-3', 'mv-3', 'started');
            "#).unwrap();
                tx.commit().unwrap();
            }

            matcher
                .process_change(&AggregateChange {
                    actor_id: ActorId::default(),
                    version: 2,
                    table: "consul_services",
                    pk: vec![
                        ("node", SqliteValueRef::Text("test-hostname")),
                        ("id", SqliteValueRef::Text("service-3")),
                    ]
                    .into_iter()
                    .collect(),
                    evt_type: ChangeEvent::Insert,
                    data: vec![("name", SqliteValueRef::Text("app-prometheus"))]
                        .into_iter()
                        .collect(),
                })
                .unwrap();

            let cells = vec![SqliteValue::Text("{\"targets\":[\"127.0.0.1:1\"],\"labels\":{\"__metrics_path__\":\"/1\",\"app\":null,\"vm_account_id\":null,\"instance\":\"m-3\"}}".into())];

            assert_eq!(
                change_rx.recv().await.unwrap(),
                RowResult::Row {
                    rowid: 2,
                    change_type: ChangeType::Upsert,
                    cells
                }
            );

            // delete the first row
            {
                let tx = conn.transaction().unwrap();
                tx.execute_batch(r#"
                        DELETE FROM consul_services where node = 'test-hostname' AND id = 'service-1';
                    "#).unwrap();
                tx.commit().unwrap();
            }

            matcher
                .process_change(&AggregateChange {
                    actor_id: ActorId::default(),
                    version: 3,
                    table: "consul_services",
                    pk: vec![
                        ("node", SqliteValueRef::Text("test-hostname")),
                        ("id", SqliteValueRef::Text("service-1")),
                    ]
                    .into_iter()
                    .collect(),
                    evt_type: ChangeEvent::Delete,
                    data: Default::default(),
                })
                .unwrap();

            let cells = vec![SqliteValue::Text("{\"targets\":[\"127.0.0.1:1\"],\"labels\":{\"__metrics_path__\":\"/1\",\"app\":null,\"vm_account_id\":null,\"instance\":\"m-1\"}}".into())];

            assert_eq!(
                change_rx.recv().await.unwrap(),
                RowResult::Row {
                    rowid: 1,
                    change_type: ChangeType::Delete,
                    cells
                }
            );
        }
    }
}
