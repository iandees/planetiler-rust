//! Port of `BenchmarkVarInt.java`: encodes then decodes 100M var-longs, 3 times, printing throughput to stderr.

use planetiler_core::stats::timer::Timer;
use planetiler_core::util::format;
use planetiler_core::util::var_int;

fn main() {
    let num: i64 = 100_000_000;

    for _ in 0..3 {
        let mut stream: Vec<u8> = Vec::new();
        let timer = Timer::start();

        let mut sum: i64 = 0;
        for l in 0..num {
            var_int::put_var_long(l, &mut stream);
            sum = sum.wrapping_add(l);
        }

        let mut buf: &[u8] = &stream;
        let mut acc: i64 = 0;
        for _ in 0..num {
            acc = acc.wrapping_add(var_int::get_var_long(&mut buf));
        }

        if sum != acc {
            eprintln!("Sums do not match");
        }

        let wall = timer.stop().elapsed().wall;
        eprintln!(
            "{num} varints took {} ({}/s)",
            format::duration(wall),
            format::numeric(num as f64 / wall.as_secs_f64(), false)
        );
    }
}
