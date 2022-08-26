use std::env::current_dir;

use anyhow::Result;
use clap::Parser;
use turbopack_create_test_app::test_app_builder::TestAppBuilder;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// The number of modules to generate
    #[clap(short, long, value_parser, default_value_t = 1000)]
    modules: usize,

    /// The number of directories to generate
    #[clap(short, long, value_parser, default_value_t = 50)]
    directories: usize,

    /// How flat should be the component tree
    #[clap(short, long, value_parser, default_value_t = 4)]
    flatness: usize,

    /// Generate a package.json with required dependencies
    #[clap(long)]
    package_json: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    println!(
        "{}",
        TestAppBuilder {
            target: Some(current_dir()?),
            module_count: args.modules,
            directories_count: args.directories,
            flatness: args.flatness,
            package_json: args.package_json
        }
        .build()?
        .display()
    );

    Ok(())
}