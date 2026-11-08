mod nested_error_phase;

use std::io::{self, BufRead, Write};
use std::path::Path;

use nested_error_phase::NestedErrorFixture;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    let descriptor = std::fs::read(&args[1])?;
    let mut fixture = NestedErrorFixture::start(&descriptor, Some(Path::new(&args[2])));
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
