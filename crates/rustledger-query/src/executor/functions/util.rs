//! Utility function implementations for the BQL executor.
//!
//! This module includes metadata, conversion, casting, and helper functions.

use rust_decimal::Decimal;
use rustledger_core::{MetaValue, Metadata};

use crate::ast::FunctionCall;
use crate::error::QueryError;

use super::super::Executor;
use super::super::types::{PostingContext, SourceLocation, Value};

// Daytech fork: beancount-compatible rendering for positions and inventories,
// used by STR. Ported from `beancount.core.position.to_string` and
// `beancount.core.inventory.Inventory.to_string`.
//
// Numbers render at their natural Decimal scale -- beancount's STR goes through
// DEFAULT_FORMATTER, NOT the inferred DisplayContext, so `4875.160000 HKD` keeps
// the six decimals it was written with. (The bare `position` *column* does use
// the DisplayContext and renders differently; that is a separate path.)

/// beancount's `CURRENCY_ORDER` — common currencies sort first.
fn currency_order(currency: &str) -> usize {
    match currency {
        "USD" => 0,
        "EUR" => 1,
        "JPY" => 2,
        "CAD" => 3,
        "GBP" => 4,
        "AUD" => 5,
        "NZD" => 6,
        "CHF" => 7,
        // NCURRENCIES + len(currency)
        other => 8 + other.len(),
    }
}

/// `Position.sortkey()`: `(order_units, cost_number, cost_currency, units.number)`.
/// Note the currency itself is NOT a tiebreak beyond `order_units`, so two
/// same-length currencies fall through to the *number* — which is why beancount
/// renders `(4875.16 HKD, 21973.613 XAU)` in that order.
fn position_sort_key(p: &rustledger_core::Position) -> (usize, Decimal, String, Decimal) {
    let (cost_number, cost_currency) = p.cost.as_ref().map_or_else(
        || (Decimal::ZERO, String::new()),
        |c| (c.number, c.currency.to_string()),
    );
    (
        currency_order(&p.units.currency),
        cost_number,
        cost_currency,
        p.units.number,
    )
}

/// `position.to_string`: `<number> <currency>` plus `{<cost>}` when held at cost.
pub(crate) fn render_position(p: &rustledger_core::Position) -> String {
    let mut s = format!("{} {}", p.units.number, p.units.currency);
    if let Some(c) = &p.cost {
        s.push_str(" {");
        s.push_str(&format!("{} {}", c.number, c.currency));
        if let Some(d) = c.date {
            s.push_str(&format!(", {d}"));
        }
        if let Some(l) = &c.label {
            s.push_str(&format!(", \"{l}\""));
        }
        s.push('}');
    }
    s
}

/// `Inventory.to_string`: sorted positions, comma-joined, wrapped in parens.
/// An empty inventory renders as `()` -- not as a zero amount.
pub(crate) fn render_inventory(inv: &rustledger_core::Inventory) -> String {
    let mut ps: Vec<&rustledger_core::Position> = inv.positions().collect();
    ps.sort_by(|a, b| position_sort_key(a).cmp(&position_sort_key(b)));
    let body: Vec<String> = ps.into_iter().map(render_position).collect();
    format!("({})", body.join(", "))
}

impl Executor<'_> {
    /// Evaluate metadata functions: `META`, `ENTRY_META`, `ANY_META`.
    ///
    /// - `META(key)` - Get metadata value from the posting
    /// - `ENTRY_META(key)` - Get metadata value from the transaction
    /// - `ANY_META(key)` - Get metadata value from posting, falling back to transaction
    pub(crate) fn eval_meta_function(
        &self,
        name: &str,
        func: &FunctionCall,
        ctx: &PostingContext,
    ) -> Result<Value, QueryError> {
        Self::require_args(name, func, 1)?;

        let key = match self.evaluate_expr(&func.args[0], ctx)? {
            Value::String(s) => s,
            _ => {
                return Err(QueryError::Type(format!(
                    "{name}: argument must be a string key"
                )));
            }
        };

        let posting = &ctx.transaction.postings[ctx.posting_index];

        // beanquery exposes `filename`/`lineno` as members of a posting's /
        // entry's metadata. Resolve them per scope: posting metadata carries
        // the POSTING's location, entry metadata the enclosing directive's.
        let posting_loc = self.resolved_source_location(ctx);
        let entry_loc = ctx
            .directive_index
            .and_then(|i| self.get_source_location(i).cloned());

        let meta_value = match name {
            "META" | "POSTING_META" => Self::meta_lookup(&posting.meta, posting_loc.as_ref(), &key),
            "ENTRY_META" => Self::meta_lookup(&ctx.transaction.meta, entry_loc.as_ref(), &key),
            "ANY_META" => Self::meta_lookup(&posting.meta, posting_loc.as_ref(), &key)
                .or_else(|| Self::meta_lookup(&ctx.transaction.meta, entry_loc.as_ref(), &key)),
            _ => unreachable!(),
        };

        Ok(Self::meta_value_to_value(meta_value.as_ref()))
    }

    /// beanquery's parser injects `filename` and `lineno` into every posting's
    /// and entry's metadata dict. rledger keeps source location in spans, not in
    /// the meta map, so synthesize those two keys at the BQL boundary from the
    /// resolved location. A user-defined key of the same name wins (callers
    /// consult the raw map first).
    fn source_location_meta_key(loc: Option<&SourceLocation>, key: &str) -> Option<MetaValue> {
        let loc = loc?;
        match key {
            "filename" => Some(MetaValue::String(loc.filename.clone())),
            // beanquery's lineno is an integer; emit a true `Int` (the `lineno`
            // column is also `Integer`). Falls back to `Number` only on the
            // practically-impossible case of a line number exceeding i64.
            "lineno" => Some(i64::try_from(loc.lineno).map_or_else(
                |_| MetaValue::Number(Decimal::from(loc.lineno as u64)),
                MetaValue::Int,
            )),
            _ => None,
        }
    }

    /// The synthetic `filename` source-location column value.
    pub(crate) fn source_filename_value(loc: Option<&SourceLocation>) -> Value {
        loc.map_or(Value::Null, |l| Value::String(l.filename.clone()))
    }

    /// The line number as an `i64`, saturating to `i64::MAX` only on the
    /// practically-impossible case of a line number exceeding `i64` — the single,
    /// overflow-checked replacement for the unchecked `loc.lineno as i64` casts
    /// (bug #4). Shared by the `lineno` and `location` columns so they can never
    /// disagree on the same posting.
    fn lineno_i64(loc: &SourceLocation) -> i64 {
        i64::try_from(loc.lineno).unwrap_or(i64::MAX)
    }

    /// The synthetic `lineno` source-location column value, as an `Integer`.
    pub(crate) fn source_lineno_value(loc: Option<&SourceLocation>) -> Value {
        loc.map_or(Value::Null, |l| Value::Integer(Self::lineno_i64(l)))
    }

    /// The synthetic `location` source-location column value (`filename:lineno`),
    /// using the same saturated line number as [`Self::source_lineno_value`].
    pub(crate) fn source_location_value(loc: Option<&SourceLocation>) -> Value {
        loc.map_or(Value::Null, |l| {
            Value::String(format!("{}:{}", l.filename, Self::lineno_i64(l)))
        })
    }

    /// Look up a single metadata key, falling back to the synthetic
    /// source-location keys (`filename`/`lineno`) when absent from `raw`.
    fn meta_lookup(raw: &Metadata, loc: Option<&SourceLocation>, key: &str) -> Option<MetaValue> {
        raw.get(key)
            .cloned()
            .or_else(|| Self::source_location_meta_key(loc, key))
    }

    /// Return `raw` extended with beanquery's synthetic `filename`/`lineno`
    /// metadata keys resolved from `loc` (existing user keys win). Used to
    /// materialize the full `meta` column value.
    pub(crate) fn augmented_meta(raw: &Metadata, loc: Option<&SourceLocation>) -> Metadata {
        if loc.is_none() {
            return raw.clone();
        }
        let mut meta = raw.clone();
        for key in ["filename", "lineno"] {
            if !meta.contains_key(key)
                && let Some(value) = Self::source_location_meta_key(loc, key)
            {
                meta.insert(key.to_string(), value);
            }
        }
        meta
    }

    /// Convert a `MetaValue` to a `Value`.
    pub(crate) fn meta_value_to_value(mv: Option<&MetaValue>) -> Value {
        match mv {
            None => Value::Null,
            Some(MetaValue::String(s)) => Value::String(s.clone()),
            Some(MetaValue::Number(n)) => Value::Number(*n),
            Some(MetaValue::Int(i)) => Value::Integer(*i),
            Some(MetaValue::Date(d)) => Value::Date(*d),
            Some(MetaValue::Bool(b)) => Value::Boolean(*b),
            Some(MetaValue::Amount(a)) => Value::Amount(a.clone()),
            // Lower typed meta values to BQL String at the query boundary
            // (matches bean-query semantics — no first-class Account/Currency
            // type in the SQL surface).
            Some(MetaValue::Account(a)) => Value::String(a.to_string()),
            Some(MetaValue::Currency(c)) => Value::String(c.to_string()),
            Some(MetaValue::Tag(t)) => Value::String(t.to_string()),
            Some(MetaValue::Link(l)) => Value::String(l.to_string()),
            Some(MetaValue::None) => Value::Null,
        }
    }

    // =========================================================================
    // Value conversion helpers (shared between eval_* and evaluate_function_on_values)
    // =========================================================================

    /// Convert a Value to string.
    pub(crate) fn value_to_str(val: &Value) -> Result<Value, QueryError> {
        match val {
            Value::String(s) => Ok(Value::String(s.clone())),
            Value::Integer(i) => Ok(Value::String(i.to_string())),
            Value::Number(n) => Ok(Value::String(n.to_string())),
            Value::Boolean(b) => Ok(Value::String(if *b { "TRUE" } else { "FALSE" }.to_string())),
            Value::Date(d) => Ok(Value::String(d.to_string())),
            Value::Amount(a) => Ok(Value::String(format!("{} {}", a.number, a.currency))),
            // Daytech fork: beancount's STR accepts positions and inventories.
            // `str(position)` / `str(balance)` / `str(sum(position))` are the ERP's
            // bread and butter -- without these, 19 of its 31 query sites fail.
            Value::Position(p) => Ok(Value::String(render_position(p))),
            Value::Inventory(inv) => Ok(Value::String(render_inventory(inv))),
            Value::Null => Ok(Value::Null),
            _ => Err(QueryError::Type(
                "STR expects a string, integer, number, boolean, date, amount, position, or inventory"
                    .to_string(),
            )),
        }
    }

    /// Convert a Value to integer.
    pub(crate) fn value_to_int(val: &Value) -> Result<Value, QueryError> {
        use rust_decimal::prelude::ToPrimitive;
        match val {
            Value::Integer(i) => Ok(Value::Integer(*i)),
            Value::Number(n) => {
                let truncated = n.trunc();
                truncated.to_i64().map(Value::Integer).ok_or_else(|| {
                    QueryError::Type(format!("INT: cannot convert '{n}' to integer"))
                })
            }
            Value::Boolean(b) => Ok(Value::Integer(i64::from(*b))),
            Value::String(s) => s
                .parse::<i64>()
                .map(Value::Integer)
                .map_err(|_| QueryError::Type(format!("INT: cannot parse '{s}' as integer"))),
            Value::Null => Ok(Value::Null),
            _ => Err(QueryError::Type(
                "INT expects a number, integer, boolean, or string".to_string(),
            )),
        }
    }

    /// Convert a Value to decimal.
    pub(crate) fn value_to_decimal(val: &Value) -> Result<Value, QueryError> {
        match val {
            Value::Number(n) => Ok(Value::Number(*n)),
            Value::Integer(i) => Ok(Value::Number(Decimal::from(*i))),
            Value::Boolean(b) => Ok(Value::Number(if *b { Decimal::ONE } else { Decimal::ZERO })),
            Value::String(s) => s
                .parse::<Decimal>()
                .map(Value::Number)
                .map_err(|_| QueryError::Type(format!("DECIMAL: cannot parse '{s}' as decimal"))),
            Value::Null => Ok(Value::Null),
            _ => Err(QueryError::Type(
                "DECIMAL expects a number, integer, boolean, or string".to_string(),
            )),
        }
    }

    /// Convert a Value to boolean.
    pub(crate) fn value_to_bool(val: &Value) -> Result<Value, QueryError> {
        match val {
            Value::Boolean(b) => Ok(Value::Boolean(*b)),
            Value::Integer(i) => Ok(Value::Boolean(*i != 0)),
            Value::Number(n) => Ok(Value::Boolean(!n.is_zero())),
            Value::String(s) => {
                let s_upper = s.to_uppercase();
                match s_upper.as_str() {
                    "TRUE" | "YES" | "1" | "T" | "Y" => Ok(Value::Boolean(true)),
                    "FALSE" | "NO" | "0" | "F" | "N" | "" => Ok(Value::Boolean(false)),
                    _ => Err(QueryError::Type(format!(
                        "BOOL: cannot parse '{s}' as boolean"
                    ))),
                }
            }
            Value::Null => Ok(Value::Null),
            _ => Err(QueryError::Type(
                "BOOL expects a boolean, number, integer, or string".to_string(),
            )),
        }
    }

    /// Evaluate COALESCE function.
    pub(crate) fn eval_coalesce(
        &self,
        func: &FunctionCall,
        ctx: &PostingContext,
    ) -> Result<Value, QueryError> {
        for arg in &func.args {
            let val = self.evaluate_expr(arg, ctx)?;
            if !matches!(val, Value::Null) {
                return Ok(val);
            }
        }
        Ok(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::Executor;
    use super::super::super::types::{SourceLocation, Value};

    fn loc(lineno: usize) -> SourceLocation {
        SourceLocation {
            filename: "f.bean".to_string(),
            lineno,
        }
    }

    #[test]
    fn source_lineno_value_basics() {
        assert_eq!(
            Executor::source_lineno_value(Some(&loc(42))),
            Value::Integer(42)
        );
        assert_eq!(Executor::source_lineno_value(None), Value::Null);
    }

    // bug #4: a line number exceeding `i64` must saturate to `i64::MAX`, not wrap
    // negative as the old unchecked `loc.lineno as i64` cast did. Only reachable
    // where `usize` is wider than `i64`.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn source_lineno_value_saturates_on_overflow() {
        assert_eq!(
            Executor::source_lineno_value(Some(&loc(usize::MAX))),
            Value::Integer(i64::MAX)
        );
        // `location` must use the same saturated value so the two columns agree.
        assert_eq!(
            Executor::source_location_value(Some(&loc(usize::MAX))),
            Value::String(format!("f.bean:{}", i64::MAX))
        );
    }

    #[test]
    fn source_filename_and_location_values() {
        assert_eq!(
            Executor::source_filename_value(Some(&loc(7))),
            Value::String("f.bean".to_string())
        );
        assert_eq!(
            Executor::source_location_value(Some(&loc(7))),
            Value::String("f.bean:7".to_string())
        );
        assert_eq!(Executor::source_filename_value(None), Value::Null);
        assert_eq!(Executor::source_location_value(None), Value::Null);
    }
}
