// Copyright 2022 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;

use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::types::number::UInt64Type;
use common_expression::types::ArgType;
use common_expression::types::DataType;
use common_expression::types::NumberDataType;
use common_expression::Literal;
use common_functions_v2::aggregates::AggregateCountFunction;

use crate::binder::ColumnBinding;
use crate::binder::Visibility;
use crate::optimizer::RelExpr;
use crate::optimizer::SExpr;
use crate::plans::Aggregate;
use crate::plans::AggregateFunction;
use crate::plans::AggregateMode;
use crate::plans::AndExpr;
use crate::plans::BoundColumnRef;
use crate::plans::CastExpr;
use crate::plans::ComparisonExpr;
use crate::plans::ComparisonOp;
use crate::plans::ConstantExpr;
use crate::plans::Filter;
use crate::plans::FunctionCall;
use crate::plans::Join;
use crate::plans::JoinType;
use crate::plans::Limit;
use crate::plans::NotExpr;
use crate::plans::OrExpr;
use crate::plans::RelOperator;
use crate::plans::Scalar;
use crate::plans::ScalarItem;
use crate::plans::SubqueryExpr;
use crate::plans::SubqueryType;
use crate::IndexType;
use crate::MetadataRef;
use crate::ScalarExpr;

#[allow(clippy::enum_variant_names)]
pub enum UnnestResult {
    // Semi/Anti Join, Cross join for EXISTS
    SimpleJoin,
    MarkJoin { marker_index: IndexType },
    SingleJoin,
}

pub struct FlattenInfo {
    pub from_count_func: bool,
}

/// Rewrite subquery into `Apply` operator
pub struct SubqueryRewriter {
    pub(crate) metadata: MetadataRef,
    pub(crate) derived_columns: HashMap<IndexType, IndexType>,
}

impl SubqueryRewriter {
    pub fn new(metadata: MetadataRef) -> Self {
        Self {
            metadata,
            derived_columns: Default::default(),
        }
    }

    pub fn rewrite(&mut self, s_expr: &SExpr) -> Result<SExpr> {
        match s_expr.plan().clone() {
            RelOperator::EvalScalar(mut plan) => {
                let mut input = self.rewrite(s_expr.child(0)?)?;

                for item in plan.items.iter_mut() {
                    let res = self.try_rewrite_subquery(&item.scalar, &input, false)?;
                    input = res.1;
                    item.scalar = res.0;
                }

                Ok(SExpr::create_unary(plan.into(), input))
            }
            RelOperator::Filter(mut plan) => {
                let mut input = self.rewrite(s_expr.child(0)?)?;
                for pred in plan.predicates.iter_mut() {
                    let res = self.try_rewrite_subquery(pred, &input, true)?;
                    input = res.1;
                    *pred = res.0;
                }

                Ok(SExpr::create_unary(plan.into(), input))
            }
            RelOperator::Aggregate(mut plan) => {
                let mut input = self.rewrite(s_expr.child(0)?)?;

                for item in plan.group_items.iter_mut() {
                    let res = self.try_rewrite_subquery(&item.scalar, &input, false)?;
                    input = res.1;
                    item.scalar = res.0;
                }

                for item in plan.aggregate_functions.iter_mut() {
                    let res = self.try_rewrite_subquery(&item.scalar, &input, false)?;
                    input = res.1;
                    item.scalar = res.0;
                }

                Ok(SExpr::create_unary(plan.into(), input))
            }

            RelOperator::Join(_) | RelOperator::UnionAll(_) => Ok(SExpr::create_binary(
                s_expr.plan().clone(),
                self.rewrite(s_expr.child(0)?)?,
                self.rewrite(s_expr.child(1)?)?,
            )),

            RelOperator::Limit(_) | RelOperator::Sort(_) => Ok(SExpr::create_unary(
                s_expr.plan().clone(),
                self.rewrite(s_expr.child(0)?)?,
            )),

            RelOperator::DummyTableScan(_) | RelOperator::Scan(_) => Ok(s_expr.clone()),

            _ => Err(ErrorCode::Internal("Invalid plan type")),
        }
    }

    /// Try to extract subquery from a scalar expression. Returns replaced scalar expression
    /// and the subqueries.
    fn try_rewrite_subquery(
        &mut self,
        scalar: &Scalar,
        s_expr: &SExpr,
        is_conjunctive_predicate: bool,
    ) -> Result<(Scalar, SExpr)> {
        match scalar {
            Scalar::BoundColumnRef(_) => Ok((scalar.clone(), s_expr.clone())),

            Scalar::ConstantExpr(_) => Ok((scalar.clone(), s_expr.clone())),

            Scalar::AndExpr(expr) => {
                // Notice that the conjunctions has been flattened in binder, if we encounter
                // a AND here, we can't treat it as a conjunction.
                let (left, s_expr) = self.try_rewrite_subquery(&expr.left, s_expr, false)?;
                let (right, s_expr) = self.try_rewrite_subquery(&expr.right, &s_expr, false)?;
                Ok((
                    AndExpr {
                        left: Box::new(left),
                        right: Box::new(right),
                        return_type: expr.return_type.clone(),
                    }
                    .into(),
                    s_expr,
                ))
            }

            Scalar::OrExpr(expr) => {
                let (left, s_expr) = self.try_rewrite_subquery(&expr.left, s_expr, false)?;
                let (right, s_expr) = self.try_rewrite_subquery(&expr.right, &s_expr, false)?;
                Ok((
                    OrExpr {
                        left: Box::new(left),
                        right: Box::new(right),
                        return_type: expr.return_type.clone(),
                    }
                    .into(),
                    s_expr,
                ))
            }

            Scalar::NotExpr(expr) => {
                let (argument, s_expr) =
                    self.try_rewrite_subquery(&expr.argument, s_expr, false)?;
                Ok((
                    NotExpr {
                        argument: Box::new(argument),
                        return_type: expr.return_type.clone(),
                    }
                    .into(),
                    s_expr,
                ))
            }

            Scalar::ComparisonExpr(expr) => {
                let (left, s_expr) = self.try_rewrite_subquery(&expr.left, s_expr, false)?;
                let (right, s_expr) = self.try_rewrite_subquery(&expr.right, &s_expr, false)?;
                Ok((
                    ComparisonExpr {
                        op: expr.op.clone(),
                        left: Box::new(left),
                        right: Box::new(right),
                        return_type: expr.return_type.clone(),
                    }
                    .into(),
                    s_expr,
                ))
            }

            Scalar::AggregateFunction(_) => Ok((scalar.clone(), s_expr.clone())),

            Scalar::FunctionCall(func) => {
                let mut args = vec![];
                let mut s_expr = s_expr.clone();
                for arg in func.arguments.iter() {
                    let res = self.try_rewrite_subquery(arg, &s_expr, false)?;
                    s_expr = res.1;
                    args.push(res.0);
                }

                let expr: Scalar = FunctionCall {
                    arguments: args,
                    func_name: func.func_name.clone(),
                    return_type: func.return_type.clone(),
                }
                .into();

                Ok((expr, s_expr))
            }

            Scalar::CastExpr(cast) => {
                let (scalar, s_expr) = self.try_rewrite_subquery(&cast.argument, s_expr, false)?;
                Ok((
                    CastExpr {
                        argument: Box::new(scalar),
                        from_type: cast.from_type.clone(),
                        target_type: cast.target_type.clone(),
                    }
                    .into(),
                    s_expr,
                ))
            }

            Scalar::SubqueryExpr(subquery) => {
                // Rewrite subquery recursively
                let mut subquery = subquery.clone();
                subquery.subquery = Box::new(self.rewrite(&subquery.subquery)?);

                // Check if the subquery is a correlated subquery.
                // If it is, we'll try to flatten it and rewrite to join.
                // If it is not, we'll just rewrite it to join
                let rel_expr = RelExpr::with_s_expr(&subquery.subquery);
                let prop = rel_expr.derive_relational_prop()?;
                let mut flatten_info = FlattenInfo {
                    from_count_func: false,
                };
                let (s_expr, result) = if prop.outer_columns.is_empty() {
                    self.try_rewrite_uncorrelated_subquery(s_expr, &subquery)?
                } else {
                    self.try_decorrelate_subquery(
                        s_expr,
                        &subquery,
                        &mut flatten_info,
                        is_conjunctive_predicate,
                    )?
                };

                // If we unnest the subquery into a simple join, then we can replace the
                // original predicate with a `TRUE` literal to eliminate the conjunction.
                if matches!(result, UnnestResult::SimpleJoin) {
                    return Ok((
                        Scalar::ConstantExpr(ConstantExpr {
                            value: Literal::Boolean(true),
                            data_type: Box::new(DataType::Boolean),
                        }),
                        s_expr,
                    ));
                }
                let (index, name) = if let UnnestResult::MarkJoin { marker_index } = result {
                    (marker_index, marker_index.to_string())
                } else if let UnnestResult::SingleJoin = result {
                    let mut output_column = subquery.output_column;
                    if let Some(index) = self.derived_columns.get(&output_column) {
                        output_column = *index;
                    }
                    (output_column, format!("scalar_subquery_{output_column}"))
                } else {
                    let index = subquery.output_column;
                    (index, format!("subquery_{}", index))
                };

                let data_type = if subquery.typ == SubqueryType::Scalar {
                    Box::new(subquery.data_type.wrap_nullable())
                } else if matches! {result, UnnestResult::MarkJoin {..}} {
                    Box::new(DataType::Nullable(Box::new(DataType::Boolean)))
                } else {
                    subquery.data_type.clone()
                };

                let column_ref = Scalar::BoundColumnRef(BoundColumnRef {
                    column: ColumnBinding {
                        database_name: None,
                        table_name: None,
                        column_name: name,
                        index,
                        data_type,
                        visibility: Visibility::Visible,
                    },
                });

                let scalar = if flatten_info.from_count_func {
                    // convert count aggregate function to multi_if function, if count() is not null, then count() else 0
                    let is_null = Scalar::FunctionCall(FunctionCall {
                        arguments: vec![column_ref.clone()],
                        func_name: "is_not_null".to_string(),
                        return_type: Box::new(DataType::Boolean),
                    });
                    let zero = Scalar::ConstantExpr(ConstantExpr {
                        value: Literal::Int64(0),
                        data_type: Box::new(
                            DataType::Number(NumberDataType::Int64).wrap_nullable(),
                        ),
                    });
                    Scalar::CastExpr(CastExpr {
                        argument: Box::new(Scalar::FunctionCall(FunctionCall {
                            arguments: vec![is_null, column_ref.clone(), zero],
                            func_name: "if".to_string(),
                            return_type: Box::new(
                                DataType::Number(NumberDataType::UInt64).wrap_nullable(),
                            ),
                        })),
                        from_type: Box::new(column_ref.data_type()),
                        target_type: Box::new(
                            DataType::Number(NumberDataType::UInt64).wrap_nullable(),
                        ),
                    })
                } else if subquery.typ == SubqueryType::NotExists {
                    Scalar::FunctionCall(FunctionCall {
                        arguments: vec![column_ref],
                        func_name: "not".to_string(),
                        return_type: Box::new(DataType::Nullable(Box::new(DataType::Boolean))),
                    })
                } else {
                    column_ref
                };

                Ok((scalar, s_expr))
            }
        }
    }

    fn try_rewrite_uncorrelated_subquery(
        &mut self,
        left: &SExpr,
        subquery: &SubqueryExpr,
    ) -> Result<(SExpr, UnnestResult)> {
        match subquery.typ {
            SubqueryType::Scalar => {
                let join_plan = Join {
                    left_conditions: vec![],
                    right_conditions: vec![],
                    non_equi_conditions: vec![],
                    join_type: JoinType::Single,
                    marker_index: None,
                    from_correlated_subquery: false,
                }
                .into();
                let s_expr =
                    SExpr::create_binary(join_plan, left.clone(), *subquery.subquery.clone());
                Ok((s_expr, UnnestResult::SingleJoin))
            }
            SubqueryType::Exists | SubqueryType::NotExists => {
                let mut subquery_expr = *subquery.subquery.clone();
                // Wrap Limit to current subquery
                let limit = Limit {
                    limit: Some(1),
                    offset: 0,
                };
                subquery_expr = SExpr::create_unary(limit.into(), subquery_expr.clone());

                // We will rewrite EXISTS subquery into the form `COUNT(*) = 1`.
                // For example, `EXISTS(SELECT a FROM t WHERE a > 1)` will be rewritten into
                // `(SELECT COUNT(*) = 1 FROM t WHERE a > 1 LIMIT 1)`.
                let agg_func = AggregateCountFunction::try_create("", vec![], vec![])?;
                let agg_func_index = self
                    .metadata
                    .write()
                    .add_derived_column("count(*)".to_string(), agg_func.return_type()?);

                let agg = Aggregate {
                    group_items: vec![],
                    aggregate_functions: vec![ScalarItem {
                        scalar: AggregateFunction {
                            display_name: "count(*)".to_string(),
                            func_name: "count".to_string(),
                            distinct: false,
                            params: vec![],
                            args: vec![],
                            return_type: Box::new(agg_func.return_type()?),
                        }
                        .into(),
                        index: agg_func_index,
                    }],
                    from_distinct: false,
                    mode: AggregateMode::Initial,
                };

                let compare = ComparisonExpr {
                    op: ComparisonOp::Equal,
                    left: Box::new(
                        BoundColumnRef {
                            column: ColumnBinding {
                                database_name: None,
                                table_name: None,
                                column_name: "count(*)".to_string(),
                                index: agg_func_index,
                                data_type: Box::new(agg_func.return_type()?),
                                visibility: Visibility::Visible,
                            },
                        }
                        .into(),
                    ),
                    right: Box::new(
                        ConstantExpr {
                            value: common_expression::Literal::UInt64(1),
                            data_type: Box::new(UInt64Type::data_type().wrap_nullable()),
                        }
                        .into(),
                    ),
                    return_type: Box::new(DataType::Boolean.wrap_nullable()),
                };
                let compare = if subquery.typ == SubqueryType::Exists {
                    compare.into()
                } else {
                    NotExpr {
                        argument: Box::new(compare.into()),
                        return_type: Box::new(DataType::Boolean.wrap_nullable()),
                    }
                    .into()
                };
                let filter = Filter {
                    predicates: vec![compare],
                    is_having: false,
                };

                // Filter: COUNT(*) = 1 or COUNT(*) != 1
                //     Aggregate: COUNT(*)
                let rewritten_subquery = SExpr::create_unary(
                    filter.into(),
                    SExpr::create_unary(agg.into(), subquery_expr),
                );
                let cross_join = Join {
                    left_conditions: vec![],
                    right_conditions: vec![],
                    non_equi_conditions: vec![],
                    join_type: JoinType::Cross,
                    marker_index: None,
                    from_correlated_subquery: false,
                }
                .into();
                Ok((
                    SExpr::create_binary(cross_join, left.clone(), rewritten_subquery),
                    UnnestResult::SimpleJoin,
                ))
            }
            SubqueryType::Any => {
                let index = subquery.output_column;
                let column_name = format!("subquery_{}", index);
                let left_condition = Scalar::BoundColumnRef(BoundColumnRef {
                    column: ColumnBinding {
                        database_name: None,
                        table_name: None,
                        column_name,
                        index,
                        data_type: subquery.data_type.clone(),
                        visibility: Visibility::Visible,
                    },
                });
                let child_expr = *subquery.child_expr.as_ref().unwrap().clone();
                let op = subquery.compare_op.as_ref().unwrap().clone();
                let (right_condition, is_non_equi_condition) =
                    check_child_expr_in_subquery(&child_expr, &op)?;
                let (left_conditions, right_conditions, non_equi_conditions) =
                    if !is_non_equi_condition {
                        (vec![left_condition], vec![right_condition], vec![])
                    } else {
                        let other_condition = Scalar::ComparisonExpr(ComparisonExpr {
                            op,
                            left: Box::new(right_condition),
                            right: Box::new(left_condition),
                            return_type: Box::new(DataType::Nullable(Box::new(DataType::Boolean))),
                        });
                        (vec![], vec![], vec![other_condition])
                    };
                // Add a marker column to save comparison result.
                // The column is Nullable(Boolean), the data value is TRUE, FALSE, or NULL.
                // If subquery contains NULL, the comparison result is TRUE or NULL.
                // Such as t1.a => {1, 3, 4}, select t1.a in (1, 2, NULL) from t1; The sql will return {true, null, null}.
                // If subquery doesn't contain NULL, the comparison result is FALSE, TRUE, or NULL.
                let marker_index = if let Some(idx) = subquery.projection_index {
                    idx
                } else {
                    self.metadata.write().add_derived_column(
                        "marker".to_string(),
                        DataType::Nullable(Box::new(DataType::Boolean)),
                    )
                };
                // Consider the sql: select * from t1 where t1.a = any(select t2.a from t2);
                // Will be transferred to:select t1.a, t2.a, marker_index from t1, t2 where t2.a = t1.a;
                // Note that subquery is the right table, and it'll be the build side.
                let mark_join = Join {
                    left_conditions: right_conditions,
                    right_conditions: left_conditions,
                    non_equi_conditions,
                    join_type: JoinType::RightMark,
                    marker_index: Some(marker_index),
                    from_correlated_subquery: false,
                }
                .into();
                let s_expr =
                    SExpr::create_binary(mark_join, left.clone(), *subquery.subquery.clone());
                Ok((s_expr, UnnestResult::MarkJoin { marker_index }))
            }
            _ => unreachable!(),
        }
    }
}

pub fn check_child_expr_in_subquery(
    child_expr: &Scalar,
    op: &ComparisonOp,
) -> Result<(Scalar, bool)> {
    match child_expr {
        Scalar::BoundColumnRef(_) => Ok((child_expr.clone(), op != &ComparisonOp::Equal)),
        Scalar::ConstantExpr(_) => Ok((child_expr.clone(), true)),
        Scalar::CastExpr(cast) => {
            let arg = &cast.argument;
            let (_, is_non_equi_condition) = check_child_expr_in_subquery(arg, op)?;
            Ok((child_expr.clone(), is_non_equi_condition))
        }
        other => Err(ErrorCode::Internal(format!(
            "Invalid child expr in subquery: {:?}",
            other
        ))),
    }
}
