use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::process::exit;
use vam_telemetry::{verify_ledger_chain, AuditRecord};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: verify_chain <audit_records.jsonl>");
        exit(2);
    }

    let file_path = &args[1];
    let file = match File::open(file_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error opening file '{}': {}", file_path, e);
            exit(1);
        }
    };

    let reader = BufReader::new(file);
    let mut records = Vec::new();

    for (line_num, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Error reading line {}: {}", line_num + 1, e);
                exit(1);
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        match serde_json::from_str::<AuditRecord>(trimmed) {
            Ok(record) => records.push(record),
            Err(e) => {
                eprintln!("Error parsing JSON on line {}: {}", line_num + 1, e);
                exit(1);
            }
        }
    }

    match verify_ledger_chain(&records, None) {
        Ok(()) => {
            println!(
                "✅ Audit ledger chain verified successfully: {} records intact.",
                records.len()
            );
        }
        Err(err) => {
            eprintln!("❌ Audit ledger chain verification FAILED: {}", err);
            exit(1);
        }
    }
}
