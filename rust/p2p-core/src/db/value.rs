//! A SQLite value as it travels between devices.

use rusqlite::types::{Value as RValue, ValueRef};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const T_NULL: i64 = 0;
pub const T_INT: i64 = 1;
pub const T_REAL: i64 = 2;
pub const T_TEXT: i64 = 3;
pub const T_BLOB: i64 = 4;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SqlValue {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl SqlValue {
    /// Storage form: a type tag plus a canonical byte encoding.
    ///
    /// Numbers are stored little-endian rather than as SQLite natives so that
    /// "did this column actually change?" is a plain byte comparison, with no
    /// float equality subtleties.
    pub fn to_storage(&self) -> (i64, Option<Vec<u8>>) {
        match self {
            SqlValue::Null => (T_NULL, None),
            SqlValue::Int(i) => (T_INT, Some(i.to_le_bytes().to_vec())),
            SqlValue::Real(f) => (T_REAL, Some(f.to_le_bytes().to_vec())),
            SqlValue::Text(s) => (T_TEXT, Some(s.as_bytes().to_vec())),
            SqlValue::Blob(b) => (T_BLOB, Some(b.clone())),
        }
    }

    pub fn from_storage(vtype: i64, raw: Option<Vec<u8>>) -> Result<Self> {
        let bad = |what: &str| Error::Codec(format!("corrupt {what} in change log"));
        Ok(match vtype {
            T_NULL => SqlValue::Null,
            T_INT => {
                let b = raw.ok_or_else(|| bad("integer"))?;
                SqlValue::Int(i64::from_le_bytes(b.as_slice().try_into().map_err(|_| bad("integer"))?))
            }
            T_REAL => {
                let b = raw.ok_or_else(|| bad("real"))?;
                SqlValue::Real(f64::from_le_bytes(b.as_slice().try_into().map_err(|_| bad("real"))?))
            }
            T_TEXT => {
                let b = raw.ok_or_else(|| bad("text"))?;
                SqlValue::Text(String::from_utf8(b).map_err(|_| bad("text"))?)
            }
            T_BLOB => SqlValue::Blob(raw.unwrap_or_default()),
            other => return Err(Error::Codec(format!("unknown value type tag {other}"))),
        })
    }

    /// Text rendering used for primary keys, which are compared as strings so
    /// that integer and text keys can share one code path.
    pub fn to_pk_string(&self) -> String {
        match self {
            SqlValue::Null => String::new(),
            SqlValue::Int(i) => i.to_string(),
            SqlValue::Real(f) => f.to_string(),
            SqlValue::Text(s) => s.clone(),
            SqlValue::Blob(b) => hex::encode(b),
        }
    }
}

impl From<&SqlValue> for RValue {
    fn from(v: &SqlValue) -> RValue {
        match v {
            SqlValue::Null => RValue::Null,
            SqlValue::Int(i) => RValue::Integer(*i),
            SqlValue::Real(f) => RValue::Real(*f),
            SqlValue::Text(s) => RValue::Text(s.clone()),
            SqlValue::Blob(b) => RValue::Blob(b.clone()),
        }
    }
}

impl From<ValueRef<'_>> for SqlValue {
    fn from(v: ValueRef<'_>) -> Self {
        match v {
            ValueRef::Null => SqlValue::Null,
            ValueRef::Integer(i) => SqlValue::Int(i),
            ValueRef::Real(f) => SqlValue::Real(f),
            ValueRef::Text(t) => SqlValue::Text(String::from_utf8_lossy(t).into_owned()),
            ValueRef::Blob(b) => SqlValue::Blob(b.to_vec()),
        }
    }
}

impl From<RValue> for SqlValue {
    fn from(v: RValue) -> Self {
        match v {
            RValue::Null => SqlValue::Null,
            RValue::Integer(i) => SqlValue::Int(i),
            RValue::Real(f) => SqlValue::Real(f),
            RValue::Text(t) => SqlValue::Text(t),
            RValue::Blob(b) => SqlValue::Blob(b),
        }
    }
}
