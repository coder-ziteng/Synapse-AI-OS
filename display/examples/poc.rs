//! 混合模式最小视觉 POC —— 命令行入口
//!
//! ```text
//! cargo run --example poc -- --output output/poc.png --expr happy
//! cargo run --example poc -- --output /tmp/poc.png --expr thinking
//! ```
//!
//! 默认参数：无 --output 时写到 `./output/poc_<expr>.png`（输出目录不存在则自动创建）。

use std::path::PathBuf;

use synapse_display::avatar::Expression;
use synapse_display::poc::{run, PocOptions};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (output, expression) = parse_args(&args);

    if let Some(parent) = PathBuf::from(&output).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }

    let opts = PocOptions {
        output,
        viewport: synapse_display::camera::Viewport::FHD,
        expression,
    };

    match run(&opts) {
        Ok(elapsed) => {
            println!(
                "[poc] rendered: expression={:?}, {} ms",
                opts.expression, elapsed as u64
            );
            println!("[poc] output: {}", opts.output);
        }
        Err(e) => {
            eprintln!("[poc] FAILED: {}", e);
            std::process::exit(1);
        }
    }
}

fn parse_args(args: &[String]) -> (String, Expression) {
    let mut output: Option<String> = None;
    let mut expr = Expression::default();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--output" | "-o" => {
                if i + 1 < args.len() {
                    output = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--expr" | "-e" => {
                if i + 1 < args.len() {
                    expr = match args[i + 1].to_lowercase().as_str() {
                        "neutral" => Expression::Neutral,
                        "happy" => Expression::Happy,
                        "thinking" => Expression::Thinking,
                        "confused" => Expression::Confused,
                        "alert" => Expression::Alert,
                        "asleep" => Expression::Asleep,
                        _ => {
                            eprintln!(
                                "[poc] unknown expression: {}",
                                args[i + 1]
                            );
                            Expression::Neutral
                        }
                    };
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            _ => i += 1,
        }
    }

    let output = output.unwrap_or_else(|| {
        format!(
            "./output/poc_{}.png",
            format!("{:?}", expr).to_lowercase()
        )
    });
    (output, expr)
}

fn print_help() {
    println!("Synapse S6 显示栈 PoC —— 混合模式最小视觉");
    println!();
    println!("USAGE:");
    println!(
        "  cargo run --example poc -- [--output <path>] [--expr <kind>]"
    );
    println!();
    println!("ARGS:");
    println!("  --output, -o <path>    PNG 输出路径（默认 ./output/poc_<expr>.png）");
    println!("  --expr, -e <kind>      虚拟人表情：neutral|happy|thinking|confused|alert|asleep");
    println!();
    println!("EXAMPLE:");
    println!("  cargo run --example poc -- --output output/poc_happy.png --expr happy");
}