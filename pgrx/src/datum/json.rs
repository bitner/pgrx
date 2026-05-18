//LICENSE Portions Copyright 2019-2021 ZomboDB, LLC.
//LICENSE
//LICENSE Portions Copyright 2021-2023 Technology Concepts & Design, Inc.
//LICENSE
//LICENSE Portions Copyright 2023-2023 PgCentral Foundation, Inc. <contact@pgcentral.org>
//LICENSE
//LICENSE All rights reserved.
//LICENSE
//LICENSE Use of this source code is governed by the MIT license that can be found in the LICENSE file.
use crate::{
    FromDatum, IntoDatum, direct_function_call, direct_function_call_as_datum, pg_sys,
    set_varsize_4b, vardata_any, varsize_any_exhdr, void_mut_ptr,
};
use core::ptr::addr_of_mut;
use pgrx_sql_entity_graph::metadata::{
    ArgumentError, ReturnsError, ReturnsRef, SqlMappingRef, SqlTranslatable,
};
use serde::{Serialize, Serializer};
use serde_json::Value;

/// A `json` type from PostgreSQL
#[derive(Debug)]
pub struct Json(pub Value);

/// A `jsonb` type from PostgreSQL
#[derive(Debug)]
pub struct JsonB(pub Value);

/// A wholly Rust-[`String`] owned copy of a `json` type from PostgreSQL
#[derive(Debug)]
pub struct JsonString(pub String);

/// for json
impl FromDatum for Json {
    #[inline]
    unsafe fn from_polymorphic_datum(
        datum: pg_sys::Datum,
        is_null: bool,
        _: pg_sys::Oid,
    ) -> Option<Json> {
        if is_null {
            None
        } else {
            let varlena = pg_sys::pg_detoast_datum(datum.cast_mut_ptr());
            let len = varsize_any_exhdr(varlena);
            let data = vardata_any(varlena);
            let slice = std::slice::from_raw_parts(data as *const u8, len);
            let value =
                serde_json::from_slice(slice).expect("datum must refer to a valid json varlena");
            Some(Json(value))
        }
    }
}

/// for jsonb
impl FromDatum for JsonB {
    unsafe fn from_polymorphic_datum(
        datum: pg_sys::Datum,
        is_null: bool,
        _: pg_sys::Oid,
    ) -> Option<JsonB> {
        if is_null {
            None
        } else {
            let varlena = datum.cast_mut_ptr();
            let detoasted = pg_sys::pg_detoast_datum_packed(varlena);
            let len = varsize_any_exhdr(detoasted);
            let data = vardata_any(detoasted);
            let slice = std::slice::from_raw_parts(data as *const u8, len);
            let raw_jsonb = jsonb::RawJsonb::new(slice);

            let value = jsonb::from_raw_jsonb::<Value>(&raw_jsonb).unwrap_or_else(|err| {
                crate::warning!("jsonb binary parse failed, falling back to text parse: {:?}", err);
                jsonb_from_text(detoasted)
            });

            // free the detoasted datum if it turned out to be a copy
            if detoasted != varlena {
                pg_sys::pfree(detoasted as void_mut_ptr);
            }

            // return the parsed serde_json::Value
            Some(JsonB(value))
        }
    }
}

/// for `json` types to be represented as a wholly-owned Rust String copy
///
/// This returns a **copy**, allocated and managed by Rust, of the underlying `varlena` Datum
impl FromDatum for JsonString {
    #[inline]
    unsafe fn from_polymorphic_datum(
        datum: pg_sys::Datum,
        is_null: bool,
        _: pg_sys::Oid,
    ) -> Option<JsonString> {
        if is_null {
            None
        } else {
            let varlena = datum.cast_mut_ptr();
            let detoasted = pg_sys::pg_detoast_datum_packed(varlena);
            let len = varsize_any_exhdr(detoasted);
            let data = vardata_any(detoasted);

            let result =
                std::str::from_utf8_unchecked(std::slice::from_raw_parts(data as *mut u8, len))
                    .to_owned();

            if detoasted != varlena {
                pg_sys::pfree(detoasted as void_mut_ptr);
            }

            Some(JsonString(result))
        }
    }
}

/// for json
impl IntoDatum for Json {
    fn into_datum(self) -> Option<pg_sys::Datum> {
        let string = serde_json::to_string(&self.0).unwrap();
        string.into_datum()
    }

    fn type_oid() -> pg_sys::Oid {
        pg_sys::JSONOID
    }
}

/// for jsonb
impl IntoDatum for JsonB {
    fn into_datum(self) -> Option<pg_sys::Datum> {
        let value = jsonb::Value::from(&self.0);
        let bytes = value.to_vec();
        unsafe {
            jsonb_from_binary(&bytes).or_else(|| {
                let string = serde_json::to_string(&self.0).unwrap();
                let cstring = alloc::ffi::CString::new(string)
                    .expect("a text version of jsonb must contain no null terminator");
                direct_function_call_as_datum(pg_sys::jsonb_in, &[Some(cstring.as_ptr().into())])
            })
        }
    }

    fn type_oid() -> pg_sys::Oid {
        pg_sys::JSONBOID
    }
}

unsafe fn jsonb_from_text(detoasted: *mut pg_sys::varlena) -> Value {
    let cstr =
        direct_function_call::<&core::ffi::CStr>(pg_sys::jsonb_out, &[Some(detoasted.into())])
            .expect("datum must refer to a valid jsonb varlena");

    let value = serde_json::from_str(
        cstr.to_str().expect("a text version of the jsonb must be valid utf-8"),
    )
    .expect("a text version of jsonb must be a valid json");

    // free the cstring returned from direct_function_call -- we don't need it anymore
    pg_sys::pfree(cstr.as_ptr() as void_mut_ptr);

    value
}

unsafe fn jsonb_from_binary(bytes: &[u8]) -> Option<pg_sys::Datum> {
    let len = bytes.len().checked_add(pg_sys::VARHDRSZ)?;
    // Postgres varlena uses a 30-bit effective length field (the top 2 bits are flag bits).
    // This is the same upper-bound check used by existing bytea/text datum conversions in pgrx.
    if len >= (u32::MAX as usize >> 2) {
        return None;
    }

    // SAFETY: palloc gives us a valid pointer and if there's not enough memory it'll raise an error
    let varlena = pg_sys::palloc(len) as *mut pg_sys::varlena;

    // SAFETY: `varlena` can properly cast into a `varattrib_4b` and all of what it contains is properly
    // allocated thanks to our call to `palloc` above
    let varattrib_ref = varlena.cast::<pg_sys::varattrib_4b>().as_mut()?;
    let varattrib_4b = &mut varattrib_ref.va_4byte;

    set_varsize_4b(varlena, len as i32);
    std::ptr::copy_nonoverlapping(
        bytes.as_ptr(),
        addr_of_mut!(varattrib_4b.va_data).cast::<u8>(),
        bytes.len(),
    );

    Some(pg_sys::Datum::from(varlena))
}

/// for jsonstring
impl IntoDatum for JsonString {
    fn into_datum(self) -> Option<pg_sys::Datum> {
        self.0.as_str().into_datum()
    }

    fn type_oid() -> pg_sys::Oid {
        pg_sys::JSONOID
    }
}

impl Serialize for Json {
    fn serialize<S>(&self, serializer: S) -> Result<<S as Serializer>::Ok, <S as Serializer>::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl Serialize for JsonB {
    fn serialize<S>(&self, serializer: S) -> Result<<S as Serializer>::Ok, <S as Serializer>::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl Serialize for JsonString {
    fn serialize<S>(&self, serializer: S) -> Result<<S as Serializer>::Ok, <S as Serializer>::Error>
    where
        S: Serializer,
    {
        serde_json::to_value(self.0.as_str())
            .expect("JsonString is not valid JSON")
            .serialize(serializer)
    }
}

unsafe impl SqlTranslatable for Json {
    const TYPE_IDENT: &'static str = crate::pgrx_resolved_type!(Json);
    const TYPE_ORIGIN: pgrx_sql_entity_graph::metadata::TypeOrigin =
        pgrx_sql_entity_graph::metadata::TypeOrigin::External;
    const ARGUMENT_SQL: Result<SqlMappingRef, ArgumentError> = Ok(SqlMappingRef::literal("json"));
    const RETURN_SQL: Result<ReturnsRef, ReturnsError> =
        Ok(ReturnsRef::One(SqlMappingRef::literal("json")));
}

unsafe impl SqlTranslatable for JsonB {
    const TYPE_IDENT: &'static str = crate::pgrx_resolved_type!(JsonB);
    const TYPE_ORIGIN: pgrx_sql_entity_graph::metadata::TypeOrigin =
        pgrx_sql_entity_graph::metadata::TypeOrigin::External;
    const ARGUMENT_SQL: Result<SqlMappingRef, ArgumentError> = Ok(SqlMappingRef::literal("jsonb"));
    const RETURN_SQL: Result<ReturnsRef, ReturnsError> =
        Ok(ReturnsRef::One(SqlMappingRef::literal("jsonb")));
}
