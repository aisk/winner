//! Command output: human-readable tables by default, JSON for machines.

/// `--format` of the listing commands.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Format {
    Table,
    Json,
}

/// Print a table with columns sized to their content.
pub fn table<const N: usize>(headers: [&str; N], rows: &[[String; N]]) {
    let mut widths = headers.map(str::len);
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: [&str; N]| {
        let mut out = String::new();
        for (i, (cell, w)) in cells.iter().zip(widths).enumerate() {
            if i + 1 == N {
                out.push_str(cell); // no trailing padding on the last column
            } else {
                out.push_str(&format!("{cell:<w$}   "));
            }
        }
        println!("{out}");
    };
    line(headers);
    for row in rows {
        line(row.each_ref().map(String::as_str));
    }
}
