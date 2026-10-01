//! Keeps one CPU busy forever, so `cirro top` has something to show.

fn main() {
    println!("SPIN_STARTED");
    let mut n: u64 = 0;
    loop {
        n = std::hint::black_box(n.wrapping_add(1));
    }
}
