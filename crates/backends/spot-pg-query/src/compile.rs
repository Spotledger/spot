//! Query-level compilation: clauses, ordering, paging and row locks.

use std::collections::BTreeMap;

use spot_storage_contract::{
    Catalog, EntityDef, ErrorKind, Expr, FieldType, JoinKind, LinkTarget, Name, Nulls, OrderDir,
    Query, RowLock, StorageError, StorageResult, Wait, schema::system,
};

use crate::schema_map::{SchemaMap, collation_name};
use crate::types::{ColumnType, CompiledQuery, Param};

/// Compiles `query` to parameterised PostgreSQL.
///
/// Runs [`Query::validate`] first, so this rejects the same queries as every other backend with
/// the same error kinds. It additionally rejects, with `InvalidQuery`, what only a typed
/// compiler can see: comparing or combining incompatible types, arithmetic or `Sum` on
/// non-numbers, `Min`/`Max` on JSON, a non-boolean condition, and an empty `Coalesce`.
///
/// The output is deterministic: the same query always gives byte-identical SQL, so prepared
/// statements can be cached. See the crate docs for the semantics.
pub fn compile(
    query: &Query,
    catalog: &Catalog,
    map: &dyn SchemaMap,
) -> StorageResult<CompiledQuery> {
    query.validate(catalog)?;
    build(query, catalog, map)
}

/// Compiles an already validated query. The compiler owns its own refusals for the ADR-087
/// constructs (same text as the shared validation), so it is also reachable without
/// `Query::validate` from this crate's tests.
pub(crate) fn build(
    query: &Query,
    catalog: &Catalog,
    map: &dyn SchemaMap,
) -> StorageResult<CompiledQuery> {
    let collation = collation_name(&catalog.collation);
    if collation.len() > Name::MAX_LEN {
        return Err(StorageError::new(
            ErrorKind::InvalidSchema,
            format!("collation name `{collation}` is longer than PostgreSQL allows"),
        ));
    }
    let mut aliases = BTreeMap::new();
    for s in std::iter::once(&query.from).chain(query.joins.iter().map(|j| &j.source)) {
        aliases.insert(s.alias.clone(), catalog.entity(&s.entity)?);
    }
    let mut b = Builder {
        map,
        aliases,
        collation: quote_raw(&collation),
        grouped: &[],
        params: Vec::new(),
        from_alias: query.from.alias.clone(),
        from_entity: query.from.entity.clone(),
    };
    b.query(query)
}

/// Quotes an identifier whose content is already known to be valid.
pub(crate) fn quote_raw(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Quotes an identifier from a [`SchemaMap`], refusing what PostgreSQL would mangle.
pub(crate) fn quote(ident: &str) -> StorageResult<String> {
    if ident.is_empty() || ident.len() > Name::MAX_LEN || ident.contains('\0') {
        return Err(StorageError::new(
            ErrorKind::InvalidName,
            format!("`{ident}` is not a usable PostgreSQL identifier"),
        ));
    }
    Ok(quote_raw(ident))
}

pub(crate) fn invalid(msg: impl Into<String>) -> StorageError {
    StorageError::new(ErrorKind::InvalidQuery, msg)
}

/// Compilation state for one query.
pub(crate) struct Builder<'q> {
    pub(crate) map: &'q dyn SchemaMap,
    pub(crate) aliases: BTreeMap<Name, &'q EntityDef>,
    /// The tenant collation's quoted SQL name.
    pub(crate) collation: String,
    /// Grouping keys of the clause being compiled (empty outside select, having and order by).
    pub(crate) grouped: &'q [Expr],
    pub(crate) params: Vec<Param>,
    /// The `from` alias and entity: a search term is evaluated against that entity's rows.
    pub(crate) from_alias: Name,
    pub(crate) from_entity: Name,
}

impl<'q> Builder<'q> {
    /// Registers a parameter and returns its placeholder.
    pub(crate) fn param(&mut self, p: Param) -> String {
        self.params.push(p);
        format!("${}", self.params.len())
    }

    /// `alias.column` for a field or system column.
    pub(crate) fn column(&self, alias: &Name, field: &Name) -> StorageResult<String> {
        let entity = self
            .aliases
            .get(alias)
            .ok_or_else(|| invalid(format!("unknown alias `{alias}`")))?;
        Ok(format!(
            "{}.{}",
            quote_raw(alias.as_str()),
            quote(&self.map.column(&entity.name, field))?
        ))
    }

    /// The type of `alias.field`, including system columns.
    pub(crate) fn field_type(&self, alias: &Name, field: &Name) -> StorageResult<ColumnType> {
        let entity = self
            .aliases
            .get(alias)
            .ok_or_else(|| invalid(format!("unknown alias `{alias}`")))?;
        let is_child = matches!(entity.kind, spot_storage_contract::EntityKind::Child { .. });
        Ok(match field.as_str() {
            system::ID => ColumnType::Id,
            system::PARENT if is_child => ColumnType::Id,
            system::VERSION if !is_child => ColumnType::Int,
            system::IDX if is_child => ColumnType::Int,
            system::PARENT_FIELD if is_child => ColumnType::Text,
            _ => column_type(&entity.field(field)?.ty)?,
        })
    }

    fn query(&mut self, q: &'q Query) -> StorageResult<CompiledQuery> {
        let mut columns = Vec::new();
        let mut decode = Vec::new();

        // Select, having and order by see the grouping keys.
        self.grouped = &q.group_by;
        let mut select = Vec::new();
        for s in &q.select {
            let c = self.expr(&s.expr)?;
            select.push(format!("{} AS {}", c.sql, quote_raw(s.alias.as_str())));
            columns.push(s.alias.clone());
            decode.push(c.ty.unwrap_or(ColumnType::Text));
        }

        self.grouped = &[];
        let mut from = format!(
            "{} AS {}",
            quote(&self.map.table(&q.from.entity))?,
            quote_raw(q.from.alias.as_str())
        );
        for j in &q.joins {
            let kind = match j.kind {
                JoinKind::Inner => "INNER JOIN",
                JoinKind::Left => "LEFT JOIN",
            };
            let on = self.condition(&j.on, "join condition")?;
            from.push_str(&format!(
                " {kind} {} AS {} ON {on}",
                quote(&self.map.table(&j.source.entity))?,
                quote_raw(j.source.alias.as_str())
            ));
        }

        let mut sql = format!("SELECT {} FROM {from}", select.join(", "));
        if let Some(f) = &q.filter {
            sql.push_str(&format!(" WHERE {}", self.condition(f, "filter")?));
        }
        if !q.group_by.is_empty() {
            let mut keys = Vec::new();
            for g in &q.group_by {
                keys.push(self.group_key(g)?);
            }
            sql.push_str(&format!(" GROUP BY {}", keys.join(", ")));
        }

        self.grouped = &q.group_by;
        if let Some(h) = &q.having {
            sql.push_str(&format!(" HAVING {}", self.condition(h, "having")?));
        }
        let order = self.order_by(q)?;
        if !order.is_empty() {
            sql.push_str(&format!(" ORDER BY {}", order.join(", ")));
        }
        self.grouped = &[];

        if let Some(limit) = q.limit {
            let p = self.param(Param::Int8(i64::from(limit)));
            sql.push_str(&format!(" LIMIT {p}"));
        }
        let offset = i64::try_from(q.offset)
            .map_err(|_| invalid(format!("offset {} is too large", q.offset)))?;
        let p = self.param(Param::Int8(offset));
        sql.push_str(&format!(" OFFSET {p}"));

        sql.push_str(&self.lock(q)?);

        Ok(CompiledQuery {
            sql,
            params: std::mem::take(&mut self.params),
            columns,
            decode,
            path: q.path,
        })
    }

    /// A grouping key: text is grouped exactly, whatever the column's collation.
    fn group_key(&mut self, g: &'q Expr) -> StorageResult<String> {
        let c = self.expr_inner(g)?;
        Ok(if c.ty == Some(ColumnType::Text) {
            self.exact(&c)
        } else {
            c.sql
        })
    }

    fn order_by(&mut self, q: &'q Query) -> StorageResult<Vec<String>> {
        let mut keys = Vec::new();
        for o in &q.order_by {
            // A rank is an ordering key and nothing else; it has no type and is never selected.
            if let Expr::SearchRank(t) = &o.expr {
                let rank = self.search_rank(t)?;
                keys.push(format!("{rank} {}", order_tail(o.dir, o.nulls)));
                continue;
            }
            self.refuse_geo_order(&o.expr)?;
            let c = self.expr(&o.expr)?;
            let tail = order_tail(o.dir, o.nulls);
            if c.ty == Some(ColumnType::Text) {
                // Tenant collation first, then code point, so the order is total.
                keys.push(format!("{} {tail}", self.collated(&c)));
                keys.push(format!("{} {tail}", self.exact(&c)));
            } else {
                keys.push(format!("{} {tail}", c.sql));
            }
        }
        if !q.is_aggregate() {
            let id = Name::new(system::ID)?;
            keys.push(format!("{} ASC", self.column(&q.from.alias, &id)?));
        }
        Ok(keys)
    }

    fn lock(&self, q: &Query) -> StorageResult<String> {
        let (strength, wait) = match q.lock {
            RowLock::None => return Ok(String::new()),
            RowLock::Update(w) => ("FOR UPDATE", w),
            RowLock::Share(w) => ("FOR SHARE", w),
        };
        let wait = match wait {
            Wait::Block => "",
            Wait::NoWait => " NOWAIT",
            Wait::SkipLocked => " SKIP LOCKED",
        };
        // Only the `from` rows are locked, never joined ones.
        Ok(format!(
            " {strength} OF {}{wait}",
            quote_raw(q.from.alias.as_str())
        ))
    }
}

fn order_tail(dir: OrderDir, nulls: Nulls) -> String {
    format!(
        "{} NULLS {}",
        match dir {
            OrderDir::Asc => "ASC",
            OrderDir::Desc => "DESC",
        },
        match nulls {
            Nulls::First => "FIRST",
            Nulls::Last => "LAST",
        }
    )
}

/// The result type of a declared field.
pub(crate) fn column_type(ty: &FieldType) -> StorageResult<ColumnType> {
    Ok(match ty {
        FieldType::Bool => ColumnType::Bool,
        FieldType::Int => ColumnType::Int,
        FieldType::Decimal { scale, .. } => ColumnType::Decimal {
            scale: Some(u32::from(*scale)),
        },
        FieldType::Text { .. } => ColumnType::Text,
        FieldType::Date => ColumnType::Date,
        FieldType::Time => ColumnType::Time,
        FieldType::DateTime => ColumnType::DateTime,
        FieldType::Link { target, .. } => match target {
            LinkTarget::Entity(_) | LinkTarget::Dynamic { .. } => ColumnType::Id,
        },
        FieldType::Json => ColumnType::Json,
        FieldType::Bytes => ColumnType::Bytes,
        // ADR-087: compiled by WP-STO-02.T9; until then a geo or range column is refused.
        FieldType::Geo { .. } | FieldType::Range { .. } => {
            return Err(StorageError::new(
                ErrorKind::Unsupported,
                "this compiler does not read geo or range fields yet (ADR-087)",
            ));
        }
    })
}
