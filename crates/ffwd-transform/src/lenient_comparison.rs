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
/// `TRY_CAST`, so unparseable values yield `NULL` instead of a query error.
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
            let target = match (
                type_of(&between.expr),
                type_of(&between.low),
                type_of(&between.high),
            ) {
                (Some(e), Some(lo), Some(hi))
                    if is_string(&e) && lo.is_numeric() && hi.is_numeric() =>
                {
                    comparison_coercion(&lo, &hi)
                }
                _ => None,
            };
            match target {
                Some(t) => {
                    let Between {
                        expr,
                        negated,
                        low,
                        high,
                    } = between;
                    Transformed::yes(Expr::Between(Between::new(
                        Box::new(try_cast(*expr, t)),
                        negated,
                        low,
                        high,
                    )))
                }
                None => Transformed::no(Expr::Between(between)),
            }
        }
        Expr::InList(in_list) => {
            let target = match type_of(&in_list.expr) {
                Some(e) if is_string(&e) && !in_list.list.is_empty() => {
                    in_list.list.iter().try_fold(None::<DataType>, |acc, item| {
                        let t = type_of(item).filter(DataType::is_numeric)?;
                        match acc {
                            None => Some(Some(t)),
                            Some(prev) => comparison_coercion(&prev, &t).map(Some),
                        }
                    })
                }
                _ => None,
            }
            .flatten();
            match target {
                Some(t) => {
                    let InList {
                        expr,
                        list,
                        negated,
                    } = in_list;
                    Transformed::yes(Expr::InList(InList::new(
                        Box::new(try_cast(*expr, t)),
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
