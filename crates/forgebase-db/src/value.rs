//! Binding [`Value`]s as query parameters and decoding result columns back into them.

use bytes::BytesMut;
use forgebase_core::meta::FieldType;
use forgebase_core::value::Value;
use time::{Date, OffsetDateTime, UtcOffset};
use tokio_postgres::Row;
use tokio_postgres::types::{IsNull, ToSql, Type, to_sql_checked};

/// A [`Value`] as a query parameter.
///
/// A newtype because neither `Value` nor `ToSql` is defined in this crate. Generated SQL gives
/// every placeholder an explicit cast (see [`cast_type`]), so the parameter type comes from the
/// metadata and never from inference. A value of the wrong kind for that type is an error from
/// the inner `ToSql` check, not wrong bytes on the wire.
///
/// Datetimes have microsecond precision (PostgreSQL `timestamptz`): finer digits of a bound
/// value are truncated by the driver when it is written, not rounded or rejected.
#[derive(Debug)]
pub(crate) struct SqlValue<'a>(pub(crate) &'a Value);

type BoxError = Box<dyn std::error::Error + Sync + Send>;

impl ToSql for SqlValue<'_> {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        match self.0 {
            Value::Text(v) => v.to_sql_checked(ty, out),
            Value::Int(v) => v.to_sql_checked(ty, out),
            Value::Decimal(v) => v.to_sql_checked(ty, out),
            Value::Bool(v) => v.to_sql_checked(ty, out),
            Value::Date(v) => v.to_sql_checked(ty, out),
            // `to_offset` panics when the UTC instant is outside the range of `time`
            // (for example 9999-12-31T23:00:00-05:00), and the `ToSql` impl of
            // `OffsetDateTime` calls it, so convert first and report the range error.
            Value::Datetime(v) => v
                .checked_to_offset(UtcOffset::UTC)
                .ok_or("datetime is out of range once converted to UTC")?
                .to_sql_checked(ty, out),
            Value::Null => Ok(IsNull::Yes),
        }
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

/// The SQL type a placeholder for a field of this type is cast to.
pub(crate) fn cast_type(field_type: &FieldType) -> &'static str {
    match field_type {
        FieldType::Data { .. } => "varchar",
        FieldType::Text | FieldType::Select { .. } => "text",
        FieldType::Int => "bigint",
        FieldType::Decimal { .. } => "numeric",
        FieldType::Check => "boolean",
        FieldType::Date => "date",
        FieldType::Datetime => "timestamptz",
    }
}

/// Reads column `index` of `row` as a value of the given field type (SQL NULL becomes
/// [`Value::Null`]). A column of another type is an error, not a panic. A value the `time`
/// crate cannot represent (years beyond ±9999, `infinity`) is an error too; the caller adds the
/// field name.
pub(crate) fn decode(
    row: &Row,
    index: usize,
    field_type: &FieldType,
) -> Result<Value, tokio_postgres::Error> {
    Ok(match field_type {
        FieldType::Data { .. } | FieldType::Text | FieldType::Select { .. } => row
            .try_get::<_, Option<String>>(index)?
            .map_or(Value::Null, Value::Text),
        FieldType::Int => row
            .try_get::<_, Option<i64>>(index)?
            .map_or(Value::Null, Value::Int),
        FieldType::Decimal { .. } => row
            .try_get::<_, Option<rust_decimal::Decimal>>(index)?
            .map_or(Value::Null, Value::Decimal),
        FieldType::Check => row
            .try_get::<_, Option<bool>>(index)?
            .map_or(Value::Null, Value::Bool),
        FieldType::Date => row
            .try_get::<_, Option<Date>>(index)?
            .map_or(Value::Null, Value::Date),
        FieldType::Datetime => row
            .try_get::<_, Option<OffsetDateTime>>(index)?
            .map_or(Value::Null, Value::Datetime),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_binds_as_sql_null_for_any_type() {
        let mut out = BytesMut::new();
        let null = SqlValue(&Value::Null);
        assert!(matches!(
            null.to_sql_checked(&Type::INT8, &mut out),
            Ok(IsNull::Yes)
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn wrong_kind_for_the_parameter_type_is_an_error() {
        let mut out = BytesMut::new();
        let text = Value::Text("x".to_owned());
        assert!(
            SqlValue(&text)
                .to_sql_checked(&Type::INT8, &mut out)
                .is_err()
        );
        let int = Value::Int(1);
        assert!(
            SqlValue(&int)
                .to_sql_checked(&Type::TEXT, &mut out)
                .is_err()
        );
        assert!(SqlValue(&int).to_sql_checked(&Type::INT8, &mut out).is_ok());
    }

    #[test]
    fn datetime_out_of_range_in_utc_is_an_error_not_a_panic() {
        let edge = Date::from_calendar_date(9999, time::Month::December, 31)
            .expect("date")
            .with_hms(23, 0, 0)
            .expect("time")
            .assume_offset(UtcOffset::from_hms(-5, 0, 0).expect("offset"));
        let value = Value::Datetime(edge);
        let mut out = BytesMut::new();
        assert!(
            SqlValue(&value)
                .to_sql_checked(&Type::TIMESTAMPTZ, &mut out)
                .is_err()
        );
        let fine = Value::Datetime(edge.replace_hour(12).expect("hour"));
        assert!(
            SqlValue(&fine)
                .to_sql_checked(&Type::TIMESTAMPTZ, &mut out)
                .is_ok()
        );
    }

    #[test]
    fn cast_types() {
        assert_eq!(cast_type(&FieldType::Data { max_length: 5 }), "varchar");
        assert_eq!(cast_type(&FieldType::Int), "bigint");
        assert_eq!(cast_type(&FieldType::Datetime), "timestamptz");
    }
}
