//! Parsing the listing query string.
//!
//! `?filter[sku]=X` cannot be expressed as a serde struct, because the field names are not known
//! until a schema declares them. So the query string is parsed by hand — which also means the rule
//! that matters most can be enforced here: a parameter the endpoint does not define is a 400, never
//! something quietly dropped. A sender that misspells a filter must not be told everything is fine.

use crate::error::ApiError;

/// What a listing request asked for, before the filters are validated against the schema.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ListParams {
    /// Page size, if one was asked for.
    pub limit: Option<u16>,
    /// Feed position to continue after.
    pub cursor: Option<u64>,
    /// Raw `field = value` filter pairs, in the order they appeared.
    pub filters: Vec<(String, String)>,
}

/// Parses a listing query string.
///
/// # Errors
///
/// Returns a 400 for an unknown parameter, a malformed `filter[...]` key, a repeated `limit` or
/// `cursor`, or a numeric parameter that is not a number.
pub fn parse_list_params(raw: Option<&str>) -> Result<ListParams, ApiError> {
    let mut params = ListParams::default();
    let Some(raw) = raw else { return Ok(params) };

    for (key, value) in form_urlencoded::parse(raw.as_bytes()) {
        match key.as_ref() {
            "limit" => {
                reject_repeat(params.limit.is_some(), "limit")?;
                params.limit = Some(number(&key, &value)?);
            }
            "cursor" => {
                reject_repeat(params.cursor.is_some(), "cursor")?;
                params.cursor = Some(number(&key, &value)?);
            }
            key if key.starts_with("filter[") => {
                let field = key
                    .strip_prefix("filter[")
                    .and_then(|rest| rest.strip_suffix(']'))
                    .filter(|field| !field.is_empty())
                    .ok_or_else(|| ApiError::BadRequest {
                        code: "invalid_query",
                        message: format!("`{key}` is not a filter; write filter[field]=value"),
                    })?;
                params.filters.push((field.to_owned(), value.into_owned()));
            }
            other => {
                return Err(ApiError::BadRequest {
                    code: "invalid_query",
                    message: format!("`{other}` is not a parameter of this endpoint"),
                });
            }
        }
    }

    Ok(params)
}

fn reject_repeat(already_seen: bool, key: &str) -> Result<(), ApiError> {
    if already_seen {
        // Last-one-wins would mean the response answers a question the caller did not ask.
        return Err(ApiError::BadRequest {
            code: "invalid_query",
            message: format!("`{key}` is given more than once"),
        });
    }
    Ok(())
}

fn number<T: std::str::FromStr>(key: &str, value: &str) -> Result<T, ApiError> {
    value.parse().map_err(|_| ApiError::BadRequest {
        code: "invalid_query",
        message: format!("`{key}` must be a number, got `{value}`"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(raw: &str) -> ListParams {
        parse_list_params(Some(raw)).expect("valid query")
    }

    #[test]
    fn an_absent_query_asks_for_nothing() {
        assert_eq!(
            parse_list_params(None).expect("valid"),
            ListParams::default()
        );
    }

    #[test]
    fn paging_parameters_are_read() {
        let parsed = params("limit=50&cursor=12");
        assert_eq!(parsed.limit, Some(50));
        assert_eq!(parsed.cursor, Some(12));
    }

    #[test]
    fn filters_keep_the_order_they_arrived_in() {
        let parsed = params("filter[sku]=SW-1&filter[colour]=red");
        assert_eq!(
            parsed.filters,
            vec![
                ("sku".to_owned(), "SW-1".to_owned()),
                ("colour".to_owned(), "red".to_owned())
            ]
        );
    }

    #[test]
    fn values_are_percent_decoded() {
        let parsed = params("filter[sku]=SW%201%2F2");
        assert_eq!(
            parsed.filters,
            vec![("sku".to_owned(), "SW 1/2".to_owned())]
        );
    }

    #[test]
    fn an_unknown_parameter_is_refused() {
        let error = parse_list_params(Some("offset=20")).expect_err("must be refused");
        assert!(matches!(
            error,
            ApiError::BadRequest {
                code: "invalid_query",
                ..
            }
        ));
    }

    #[test]
    fn a_malformed_filter_key_is_refused() {
        for raw in ["filter[=x", "filter[]=x", "filter=x"] {
            assert!(
                parse_list_params(Some(raw)).is_err(),
                "{raw} should be refused"
            );
        }
    }

    #[test]
    fn a_repeated_paging_parameter_is_refused() {
        assert!(parse_list_params(Some("limit=1&limit=2")).is_err());
        assert!(parse_list_params(Some("cursor=1&cursor=2")).is_err());
    }

    #[test]
    fn a_non_numeric_paging_parameter_is_refused() {
        assert!(parse_list_params(Some("limit=all")).is_err());
        assert!(parse_list_params(Some("cursor=start")).is_err());
    }

    #[test]
    fn the_same_filter_twice_is_left_to_schema_validation() {
        // Two values for one field is a filter question, not a syntax one - the schema layer says
        // so with the field name in hand.
        let parsed = params("filter[sku]=a&filter[sku]=b");
        assert_eq!(parsed.filters.len(), 2);
    }
}
