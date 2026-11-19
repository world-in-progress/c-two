mod nested_error_phase;

use std::io::{self, BufRead, Write};
use std::path::Path;

use nested_error_phase::NestedErrorFixture;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    let descriptor = std::fs::read(&args[1])?;
    let relay_url = args.get(3).map(String::as_str);
    let mut fixture =
        NestedErrorFixture::start_with_relay(&descriptor, Some(Path::new(&args[2])), relay_url);
    fixture.reject_b_before_callback();
    println!(
        "READY {} {} {}",
        fixture.address, fixture.route_a, fixture.route_b
    );
    io::stdout().flush()?;

    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let (a, b) = fixture.counts();
    println!("COUNTS {a} {b}");
    io::stdout().flush()?;
    Ok(())
}
