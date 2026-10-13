use anyhow::{Result, ensure};
use baza_core::sql::{literal, qualified};
use baza_core::{Cell, Dialect, QueryResult, TableRef};
#[derive(Clone, Copy, PartialEq)]
pub enum Format {
    Csv,
    Tsv,
    Json,
    Xml,
    Sql,
}
impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Self::Csv => "CSV",
            Self::Tsv => "TSV",
            Self::Json => "JSON",
            Self::Xml => "XML",
            Self::Sql => "SQL",
        }
    }
    pub fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Tsv => "tsv",
            Self::Json => "json",
            Self::Xml => "xml",
            Self::Sql => "sql",
        }
    }
}
pub fn export(
    result: &QueryResult,
    format: Format,
    dialect: &Dialect,
    table: Option<&TableRef>,
) -> Result<Vec<u8>> {
    ensure!(result.rows.len() <= 20_000, "export_too_large");
    let bytes = match format {
        Format::Csv | Format::Tsv => {
            let delimiter = if format == Format::Tsv { '\t' } else { ',' };
            let mut output = String::new();
            output.push_str(
                &result
                    .columns
                    .iter()
                    .map(|c| csv_field(&c.name, delimiter, false))
                    .collect::<Vec<_>>()
                    .join(&delimiter.to_string()),
            );
            output.push('\n');
            for row in &result.rows {
                output.push_str(
                    &row.iter()
                        .map(|cell| {
                            if matches!(cell, Cell::Null) {
                                "\\N".to_owned()
                            } else {
                                let value = cell.display();
                                csv_field(&value, delimiter, value == "\\N" || value.is_empty())
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(&delimiter.to_string()),
                );
                output.push('\n');
            }
            output.into_bytes()
        }
        Format::Json => serde_json::to_vec_pretty(
            &serde_json::json!({"format":"baza-studio-table","version":1,"columns":result.columns,"rows":result.rows}),
        )?,
        Format::Xml => {
            let escape = |s: &str| {
                s.replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
                    .replace('"', "&quot;")
                    .replace('\'', "&apos;")
            };
            let mut s = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<table>\n");
            for r in &result.rows {
                s.push_str("  <row>\n");
                for (c, v) in result.columns.iter().zip(r) {
                    s.push_str(&format!(
                        "    <cell name=\"{}\" null=\"{}\">{}</cell>\n",
                        escape(&c.name),
                        matches!(v, Cell::Null),
                        escape(&v.display())
                    ));
                }
                s.push_str("  </row>\n");
            }
            s.push_str("</table>\n");
            s.into_bytes()
        }
        Format::Sql => {
            let t = table.ok_or_else(|| anyhow::anyhow!("sql_export_requires_table"))?;
            let target = qualified(dialect, t)?;
            let cols = result
                .columns
                .iter()
                .map(|c| baza_core::sql::quote_identifier(dialect, &c.name))
                .collect::<Result<Vec<_>>>()?
                .join(", ");
            let mut s = String::from("-- Baza Studio data export\n-- Review before execution.\n");
            for r in &result.rows {
                s.push_str(&format!(
                    "INSERT INTO {target} ({cols}) VALUES ({});\n",
                    r.iter()
                        .map(|c| literal(dialect, c))
                        .collect::<Result<Vec<_>>>()?
                        .join(", ")
                ));
            }
            s.into_bytes()
        }
    };
    ensure!(bytes.len() <= 64 * 1024 * 1024, "export_too_large");
    Ok(bytes)
}
pub fn import_rows(data: &[u8], format: Format) -> Result<(Vec<String>, Vec<Vec<Cell>>)> {
    ensure!(data.len() <= 5 * 1024 * 1024, "import_file_too_large");
    match format {
        Format::Csv | Format::Tsv => {
            let quoted = csv_quoted_fields(data, if format == Format::Tsv { b'\t' } else { b',' })?;
            let mut r = csv::ReaderBuilder::new()
                .delimiter(if format == Format::Tsv { b'\t' } else { b',' })
                .from_reader(data);
            let headers = r.headers()?.iter().map(str::to_owned).collect::<Vec<_>>();
            ensure!(
                !headers.is_empty() && headers.len() <= 128,
                "import_columns_invalid"
            );
            let mut rows = vec![];
            for record in r.records() {
                ensure!(rows.len() < 20_000, "import_rows_too_many");
                let flags = quoted
                    .get(rows.len() + 1)
                    .ok_or_else(|| anyhow::anyhow!("csv_record_metadata_invalid"))?;
                let record = record?;
                ensure!(flags.len() == record.len(), "csv_record_metadata_invalid");
                rows.push(
                    record
                        .iter()
                        .enumerate()
                        .map(|(column, v)| {
                            if v == "\\N" && !flags[column] {
                                Cell::Null
                            } else {
                                Cell::Text(v.into())
                            }
                        })
                        .collect(),
                );
            }
            Ok((headers, rows))
        }
        Format::Json => {
            let v: serde_json::Value = serde_json::from_slice(data)?;
            let headers: Vec<baza_core::ResultColumn> =
                serde_json::from_value(v["columns"].clone())?;
            let rows: Vec<Vec<Cell>> = serde_json::from_value(v["rows"].clone())?;
            ensure!(
                rows.len() <= 20_000
                    && headers.len() <= 128
                    && rows.iter().all(|r| r.len() == headers.len()),
                "import_shape_invalid"
            );
            Ok((headers.into_iter().map(|c| c.name).collect(), rows))
        }
        _ => anyhow::bail!("import_format_unsupported"),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn csv_null_and_injection_roundtrip() {
        let q = QueryResult {
            columns: vec![baza_core::ResultColumn {
                name: "name".into(),
                data_type: "TEXT".into(),
            }],
            rows: vec![vec![Cell::Text("a,b\"\n=x".into())], vec![Cell::Null]],
            ..Default::default()
        };
        let b = export(&q, Format::Csv, &Dialect::ANSI, None).unwrap();
        let (_, r) = import_rows(&b, Format::Csv).unwrap();
        assert_eq!(r, q.rows);
    }
    #[test]
    fn xml_escaping() {
        let q = QueryResult {
            columns: vec![baza_core::ResultColumn {
                name: "a\"".into(),
                data_type: "TEXT".into(),
            }],
            rows: vec![vec![Cell::Text("<&".into())]],
            ..Default::default()
        };
        let s = String::from_utf8(export(&q, Format::Xml, &Dialect::ANSI, None).unwrap()).unwrap();
        assert!(s.contains("a&quot;"));
        assert!(s.contains("&lt;&amp;"));
    }
}

fn csv_field(value: &str, delimiter: char, force: bool) -> String {
    if force
        || value.contains(delimiter)
        || value.contains('"')
        || value.contains('\n')
        || value.contains('\r')
    {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}
fn csv_quoted_fields(data: &[u8], delimiter: u8) -> Result<Vec<Vec<bool>>> {
    let mut rows = vec![];
    let mut fields = vec![];
    let mut i = if data.starts_with(&[0xef, 0xbb, 0xbf]) {
        3
    } else {
        0
    };
    let mut start = i;
    let mut quoted = false;
    let mut inside = false;
    let mut row_has_data = false;
    while i < data.len() {
        let byte = data[i];
        if inside {
            if byte == b'"' {
                if data.get(i + 1) == Some(&b'"') {
                    i += 2;
                    continue;
                }
                inside = false;
            }
            i += 1;
            continue;
        }
        if i == start && byte == b'"' {
            quoted = true;
            inside = true;
            row_has_data = true;
            i += 1;
            continue;
        }
        if byte == delimiter {
            fields.push(quoted);
            quoted = false;
            row_has_data = true;
            i += 1;
            start = i;
            continue;
        }
        if byte == b'\r' || byte == b'\n' {
            if row_has_data || i > start {
                fields.push(quoted);
                rows.push(std::mem::take(&mut fields));
            }
            quoted = false;
            row_has_data = false;
            i += 1;
            if byte == b'\r' && data.get(i) == Some(&b'\n') {
                i += 1;
            }
            start = i;
            continue;
        }
        row_has_data = true;
        i += 1;
    }
    ensure!(!inside, "csv_unterminated_quote");
    if row_has_data || i > start {
        fields.push(quoted);
        rows.push(fields);
    }
    ensure!(
        rows.len() <= 20_001 && rows.iter().all(|row| row.len() <= 128),
        "import_shape_invalid"
    );
    Ok(rows)
}
#[cfg(test)]
mod csv_lossless_tests {
    use super::*;
    #[test]
    fn sentinel_text_empty_backslash_and_null_are_distinct() {
        for format in [Format::Csv, Format::Tsv] {
            let query = QueryResult {
                columns: vec![baza_core::ResultColumn {
                    name: "value".into(),
                    data_type: "TEXT".into(),
                }],
                rows: vec![
                    vec![Cell::Null],
                    vec![Cell::Text("\\N".into())],
                    vec![Cell::Text("".into())],
                    vec![Cell::Text("C:\\Users\\name".into())],
                    vec![Cell::Text("comma,tab\tquote\"newline\nend".into())],
                ],
                ..QueryResult::default()
            };
            let bytes = export(&query, format, &Dialect::ANSI, None).unwrap();
            let (_, rows) = import_rows(&bytes, format).unwrap();
            assert_eq!(rows, query.rows);
        }
    }
    #[test]
    fn quoted_external_sentinel_is_literal_and_no_backslash_unescape() {
        let (_, rows) =
            import_rows(b"value\r\n\\N\r\n\"\\N\"\r\n\"a\\b\"\r\n", Format::Csv).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Cell::Null],
                vec![Cell::Text("\\N".into())],
                vec![Cell::Text("a\\b".into())]
            ]
        );
        assert!(import_rows(b"value\n\"unterminated", Format::Csv).is_err());
    }
}
