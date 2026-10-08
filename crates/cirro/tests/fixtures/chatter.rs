//! Writes numbered lines to the console as fast as it can, forever, so a
//! console log outgrows one `GET /vms/{name}/logs` answer and keeps growing
//! while `cirro logs` reads it.
use std::io::Write;

fn main() {
    let mut out = std::io::stdout().lock();
    for n in 0u64.. {
        let _ = writeln!(out, "CHATTER {n:012}");
    }
}
