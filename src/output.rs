use anyhow::Result;
use serde::Serialize;

pub fn print_json<T: Serialize + ?Sized>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|header| display_width(header)).collect();
    for row in rows {
        for (column, value) in row.iter().enumerate() {
            if column < widths.len() {
                widths[column] = widths[column].max(display_width(value));
            }
        }
    }

    println!(
        "{}",
        render_row(
            &headers.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
            &widths
        )
    );
    println!(
        "{}",
        widths
            .iter()
            .map(|width| "-".repeat(*width))
            .collect::<Vec<_>>()
            .join("  ")
    );
    for row in rows {
        println!("{}", render_row(row, &widths));
    }
}

fn render_row(row: &[String], widths: &[usize]) -> String {
    row.iter()
        .enumerate()
        .map(|(column, value)| {
            let padding = widths[column].saturating_sub(display_width(value));
            format!("{value}{}", " ".repeat(padding))
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn display_width(value: &str) -> usize {
    value.chars().count()
}
