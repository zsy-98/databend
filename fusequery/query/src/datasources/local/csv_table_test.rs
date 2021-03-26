// Copyright 2020-2021 The FuseQuery Authors.
//
// SPDX-License-Identifier: Apache-2.0.

#[tokio::test]
async fn test_csv_table() -> crate::error::FuseQueryResult<()> {
    use std::env;

    use arrow::datatypes::{Field, Schema};
    use common_datavalues::DataType;
    use common_planners::*;
    use futures::TryStreamExt;

    use crate::datasources::local::*;

    let options: TableOptions = [(
        "location".to_string(),
        env::current_dir()?
            .join("../../tests/data/sample.csv")
            .display()
            .to_string(),
    )]
    .iter()
    .cloned()
    .collect();

    let ctx = crate::tests::try_create_context()?;
    let table = CsvTable::try_create(
        ctx.clone(),
        "default".into(),
        "test_csv".into(),
        Schema::new(vec![Field::new("a", DataType::UInt64, false)]).into(),
        options,
    )?;
    table.read_plan(ctx.clone(), PlanBuilder::empty().build()?)?;

    let stream = table.read(ctx).await?;
    let blocks = stream.try_collect::<Vec<_>>().await?;
    let rows: usize = blocks.iter().map(|block| block.num_rows()).sum();

    assert_eq!(rows, 4);
    Ok(())
}
