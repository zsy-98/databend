// Copyright 2021 Datafuse Labs.
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

use common_base::base::tokio::io::AsyncWrite;
use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::types::number::NumberScalar;
use common_expression::types::DataType;
use common_expression::types::NumberDataType;
use common_expression::Column as ExprColumn;
use common_expression::DataField;
use common_expression::DataSchemaRef;
use common_expression::ScalarRef;
use common_expression::SchemaDataType;
use common_expression::SendableChunkStream;
use common_formats::field_encoder::FieldEncoderRowBased;
use common_formats::field_encoder::FieldEncoderValues;
use common_io::prelude::FormatSettings;
use futures_util::StreamExt;
use opensrv_mysql::*;
use tracing::error;

/// Reports progress information as string, intend to be put into the mysql Ok packet.
/// Mainly for decoupling with concrete type like `QueryContext`
///
/// Something like
/// "Read x rows, y MiB in z sec., A million rows/sec., B MiB/sec."
pub trait ProgressReporter {
    fn progress_info(&self) -> String;
}

pub struct QueryResult {
    chunks: SendableChunkStream,
    extra_info: Option<Box<dyn ProgressReporter + Send>>,
    has_result_set: bool,
    schema: DataSchemaRef,
}

impl QueryResult {
    pub fn create(
        chunks: SendableChunkStream,
        extra_info: Option<Box<dyn ProgressReporter + Send>>,
        has_result_set: bool,
        schema: DataSchemaRef,
    ) -> QueryResult {
        QueryResult {
            chunks,
            extra_info,
            has_result_set,
            schema,
        }
    }
}

pub struct DFQueryResultWriter<'a, W: AsyncWrite + Send + Unpin> {
    inner: Option<QueryResultWriter<'a, W>>,
}

fn write_field<'a, W: AsyncWrite + Unpin>(
    row_writer: &mut RowWriter<'a, W>,
    column: &ExprColumn,
    encoder: &FieldEncoderValues,
    buf: &mut Vec<u8>,
    row_index: usize,
) -> Result<()> {
    buf.clear();
    encoder.write_field(column, row_index, buf, true);
    row_writer.write_col(&buf[..])?;
    Ok(())
}

impl<'a, W: AsyncWrite + Send + Unpin> DFQueryResultWriter<'a, W> {
    pub fn create(inner: QueryResultWriter<'a, W>) -> DFQueryResultWriter<'a, W> {
        DFQueryResultWriter::<'a, W> { inner: Some(inner) }
    }

    pub async fn write(
        &mut self,
        query_result: Result<QueryResult>,
        format: &FormatSettings,
    ) -> Result<()> {
        if let Some(writer) = self.inner.take() {
            match query_result {
                Ok(query_result) => Self::ok(query_result, writer, format).await?,
                Err(error) => Self::err(&error, writer).await?,
            }
        }
        Ok(())
    }

    async fn ok(
        mut query_result: QueryResult,
        dataset_writer: QueryResultWriter<'a, W>,
        format: &FormatSettings,
    ) -> Result<()> {
        // XXX: num_columns == 0 may is error?
        if !query_result.has_result_set {
            // For statements without result sets, we still need to pull the stream because errors may occur in the stream.
            let chunks = &mut query_result.chunks;
            while let Some(chunk) = chunks.next().await {
                if let Err(e) = chunk {
                    error!("dataset write failed: {:?}", e);
                    dataset_writer
                        .error(
                            ErrorKind::ER_UNKNOWN_ERROR,
                            format!("dataset write failed: {}", e).as_bytes(),
                        )
                        .await?;

                    return Ok(());
                }
            }

            dataset_writer.completed(OkResponse::default()).await?;
            return Ok(());
        }

        fn convert_field_type(field: &DataField) -> Result<ColumnType> {
            match field.data_type().remove_nullable() {
                SchemaDataType::Null => Ok(ColumnType::MYSQL_TYPE_NULL),
                SchemaDataType::EmptyArray => Ok(ColumnType::MYSQL_TYPE_VARCHAR),
                SchemaDataType::Boolean => Ok(ColumnType::MYSQL_TYPE_SHORT),
                SchemaDataType::String => Ok(ColumnType::MYSQL_TYPE_VARCHAR),
                SchemaDataType::Number(num_ty) => match num_ty {
                    NumberDataType::Int8 => Ok(ColumnType::MYSQL_TYPE_TINY),
                    NumberDataType::Int16 => Ok(ColumnType::MYSQL_TYPE_SHORT),
                    NumberDataType::Int32 => Ok(ColumnType::MYSQL_TYPE_LONG),
                    NumberDataType::Int64 => Ok(ColumnType::MYSQL_TYPE_LONGLONG),
                    NumberDataType::UInt8 => Ok(ColumnType::MYSQL_TYPE_TINY),
                    NumberDataType::UInt16 => Ok(ColumnType::MYSQL_TYPE_SHORT),
                    NumberDataType::UInt32 => Ok(ColumnType::MYSQL_TYPE_LONG),
                    NumberDataType::UInt64 => Ok(ColumnType::MYSQL_TYPE_LONGLONG),
                    NumberDataType::Float32 => Ok(ColumnType::MYSQL_TYPE_FLOAT),
                    NumberDataType::Float64 => Ok(ColumnType::MYSQL_TYPE_DOUBLE),
                },
                SchemaDataType::Date => Ok(ColumnType::MYSQL_TYPE_DATE),
                SchemaDataType::Timestamp => Ok(ColumnType::MYSQL_TYPE_DATETIME),
                SchemaDataType::Array(_) => Ok(ColumnType::MYSQL_TYPE_VARCHAR),
                SchemaDataType::Map(_) => Ok(ColumnType::MYSQL_TYPE_VARCHAR),
                SchemaDataType::Tuple { .. } => Ok(ColumnType::MYSQL_TYPE_VARCHAR),
                SchemaDataType::Variant => Ok(ColumnType::MYSQL_TYPE_VARCHAR),
                _ => Err(ErrorCode::Unimplemented(format!(
                    "Unsupported column type:{:?}",
                    field.data_type()
                ))),
            }
        }

        fn make_column_from_field(field: &DataField) -> Result<Column> {
            convert_field_type(field).map(|column_type| Column {
                table: "".to_string(),
                column: field.name().to_string(),
                coltype: column_type,
                colflags: ColumnFlags::empty(),
            })
        }

        fn convert_schema(schema: &DataSchemaRef) -> Result<Vec<Column>> {
            schema.fields().iter().map(make_column_from_field).collect()
        }

        let tz = format.timezone;
        match convert_schema(&query_result.schema) {
            Err(error) => Self::err(&error, dataset_writer).await,
            Ok(columns) => {
                let mut row_writer = dataset_writer.start(&columns).await?;
                let chunks = &mut query_result.chunks;
                
                todo!("expression");
                Ok(())
            }
        }
    }

    async fn err(error: &ErrorCode, writer: QueryResultWriter<'a, W>) -> Result<()> {
        if error.code() != ErrorCode::ABORTED_QUERY && error.code() != ErrorCode::ABORTED_SESSION {
            error!("OnQuery Error: {:?}", error);
            writer
                .error(ErrorKind::ER_UNKNOWN_ERROR, error.to_string().as_bytes())
                .await?;
        } else {
            writer
                .error(
                    ErrorKind::ER_ABORTING_CONNECTION,
                    error.to_string().as_bytes(),
                )
                .await?;
        }

        Ok(())
    }
}
