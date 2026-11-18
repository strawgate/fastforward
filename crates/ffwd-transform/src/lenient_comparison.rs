//! Lenient string-vs-number comparisons.
//!
//! DataFusion coerces `string_col >= 400` by casting the string side to the
//! numeric type with a strict `CAST`. One non-numeric value (`"N/A"`, `"-"`)
//! then fails the whole query, and the pipeline drops the entire batch.
//!
//! Log fields routinely mix numbers and placeholders, so this analyzer rule
//! runs before DataFusion's `TypeCoercion` and wraps the string side of such
//! comparisons in `TRY_CAST`. Numeric strings compare numerically; anything
//! else becomes `NULL`, so the row simply does not match. This is the same
//! result as writing `int(status) >= 400` by hand. Explicit `CAST` written by
//! the user is left untouched.

use std::sync::Arc;

use arrow::datatypes::DataType;
use datafusion::common::config::ConfigOptions;
use datafusion::common::tree_node::{Transformed, TreeNode};
use datafusion::common::{DFSchema, Result};
use datafusion::logical_expr::expr::InList;
use datafusion::logical_expr::expr_rewriter::NamePreserver;
use datafusion::logical_expr::type_coercion::binary::comparison_coercion;
use datafusion::logical_expr::utils::merge_schema;
use datafusion::logical_expr::{
    Between, BinaryExpr, Expr, ExprSchemable, LogicalPlan, Operator, try_cast,
};
use datafusion::optimizer::{Analyzer, AnalyzerRule};

/// Analyzer rule that turns implicit string-to-number comparison casts into
/// `TRY_CAST`, so unparsable values yield `NULL` instead of a query error.
#[derive(Debug, Default)]
pub(crate) struct LenientStringNumericComparison;

impl AnalyzerRule for LenientStringNumericComparison {
    fn name(&self) -> &'static str {
        "ffwd_lenient_string_numeric_comparison"
    }

    fn analyze(&self, plan: LogicalPlan, _config: &ConfigOptions) -> Result<LogicalPlan> {
        Ok(plan.transform_up_with_subqueries(rewrite_plan)?.data)
    }
}

/// DataFusion's default analyzer rules with [`LenientStringNumericComparison`]
/// placed first, so it sees comparisons before `TypeCoercion` adds strict casts.
pub(crate) fn analyzer_rules() -> Vec<Arc<dyn AnalyzerRule + Send + Sync>> {
    let mut rules = Analyzer::new().rules;
    rules.insert(0, Arc::new(LenientStringNumericComparison));
    rules
}

fn rewrite_plan(plan: LogicalPlan) -> Result<Transformed<LogicalPlan>> {
    let mut schema = merge_schema(&plan.inputs());
    if let LogicalPlan::TableScan(scan) = &plan {
        let source =
            DFSchema::try_from_qualified_schema(scan.table_name.clone(), &scan.source.schema())?;
        schema.merge(&source);
    }

    let name_preserver = NamePreserver::new(&plan);
    plan.map_expressions(|expr| {
        let original_name = name_preserver.save(&expr);
        expr.transform_up(|e| Ok(rewrite_expr(e, &schema)))
            .map(|t| t.update_data(|e| original_name.restore(e)))
    })
}

fn rewrite_expr(expr: Expr, schema: &DFSchema) -> Transformed<Expr> {
    let type_of = |e: &Expr| e.get_type(schema).ok();

    match expr {
        Expr::BinaryExpr(BinaryExpr { left, op, right }) if is_comparison(op) => {
            let (lt, rt) = (type_of(&left), type_of(&right));
            match (lt, rt) {
                (Some(l), Some(r)) if is_string(&l) && r.is_numeric() => Transformed::yes(
                    Expr::BinaryExpr(BinaryExpr::new(Box::new(try_cast(*left, r)), op, right)),
                ),
                (Some(l), Some(r)) if l.is_numeric() && is_string(&r) => Transformed::yes(
                    Expr::BinaryExpr(BinaryExpr::new(left, op, Box::new(try_cast(*right, l)))),
                ),
                _ => Transformed::no(Expr::BinaryExpr(BinaryExpr { left, op, right })),
            }
        }
        Expr::Between(between) => {
            let types = [
                type_of(&between.expr),
                type_of(&between.low),
                type_of(&between.high),
            ];
            match numeric_target(&types) {
                Some(t) => {
                    let Between {
                        expr,
                        negated,
                        low,
                        high,
                    } = between;
                    let [e, lo, hi] = types;
                    Transformed::yes(Expr::Between(Between::new(
                        Box::new(lenient(*expr, e.as_ref(), &t)),
                        negated,
                        Box::new(lenient(*low, lo.as_ref(), &t)),
                        Box::new(lenient(*high, hi.as_ref(), &t)),
                    )))
                }
                None => Transformed::no(Expr::Between(between)),
            }
        }
        Expr::InList(in_list) => {
            let expr_type = type_of(&in_list.expr);
            let list_types: Vec<Option<DataType>> = in_list.list.iter().map(type_of).collect();
            let all_types: Vec<Option<DataType>> = std::iter::once(expr_type.clone())
                .chain(list_types.iter().cloned())
                .collect();
            match numeric_target(&all_types) {
                Some(t) => {
                    let InList {
                        expr,
                        list,
                        negated,
                    } = in_list;
                    let list = list
                        .into_iter()
                        .zip(list_types.iter())
                        .map(|(item, ty)| lenient(item, ty.as_ref(), &t))
                        .collect();
                    Transformed::yes(Expr::InList(InList::new(
                        Box::new(lenient(*expr, expr_type.as_ref(), &t)),
                        list,
                        negated,
                    )))
                }
                None => Transformed::no(Expr::InList(in_list)),
            }
        }
        other => Transformed::no(other),
    }
}

/// The numeric type a multi-operand comparison (`BETWEEN`, `IN`) should use
/// when it mixes string and numeric operands. `NULL` operands are neutral.
/// Returns `None` when there is nothing to make lenient, or when an operand
/// has a type outside strings, numbers and `NULL`.
fn numeric_target(types: &[Option<DataType>]) -> Option<DataType> {
    let mut has_string = false;
    let mut target: Option<DataType> = None;
    for ty in types {
        match ty.as_ref()? {
            DataType::Null => {}
            t if is_string(t) => has_string = true,
            t if t.is_numeric() => {
                target = match target {
                    None => Some(t.clone()),
                    Some(prev) => Some(comparison_coercion(&prev, t)?),
                };
            }
            _ => return None,
        }
    }
    if has_string { target } else { None }
}

/// `TRY_CAST` a string operand to `target`; leave other operands alone.
fn lenient(expr: Expr, ty: Option<&DataType>, target: &DataType) -> Expr {
    match ty {
        Some(t) if is_string(t) => try_cast(expr, target.clone()),
        _ => expr,
    }
}

fn is_comparison(op: Operator) -> bool {
    matches!(
        op,
        Operator::Eq
            | Operator::NotEq
            | Operator::Lt
            | Operator::LtEq
            | Operator::Gt
            | Operator::GtEq
            | Operator::IsDistinctFrom
            | Operator::IsNotDistinctFrom
    )
}

fn is_string(data_type: &DataType) -> bool {
    match data_type {
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => true,
        DataType::Dictionary(_, value) => is_string(value),
        _ => false,
    }
}
