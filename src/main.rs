use std::time::{Duration, Instant};

use tokio::task::JoinSet;

#[derive(Clone)]
struct Options {
    iterations: usize,
    concurrency: usize,
    sq_size: usize,
    cq_size: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            iterations: 1000000,
            concurrency: 1,
            sq_size: 128,
            cq_size: 256,
        }
    }
}

fn run_no_ops(opts: &Options, count: u64) -> Duration {
    let mut ring_opts = tokio_uring::uring_builder();
    ring_opts.setup_cqsize(opts.cq_size as _);

    let mut m = Duration::ZERO;

    // Run the required number of iterations
    for _ in 0..count {
        m += tokio_uring::builder()
            .entries(opts.sq_size as _)
            .uring_builder(&ring_opts)
            .start(async move {
                let mut js = JoinSet::new();

                for _ in 0..opts.iterations {
                    
                    js.spawn_local(tokio_uring::no_op());
                }

                let start = Instant::now();

                while let Some(res) = js.join_next().await {
                    res.unwrap().unwrap();
                }

                start.elapsed()
            })
    }
    m
}

fn main() {
    run_no_ops(&Options::default(), 1);
}
