# async-cancellation-token

[![Crates.io](https://img.shields.io/crates/v/async-cancellation-token)](https://crates.io/crates/async-cancellation-token)
[![Docs](https://docs.rs/async-cancellation-token/badge.svg)](https://docs.rs/async-cancellation-token)

`async-cancellation-token` is a lightweight Rust library that provides **cancellation tokens** for cooperative cancellation of asynchronous tasks and callbacks.  
It works in `!no_std` environments with `alloc`, and is designed to integrate seamlessly with Rust `Future`s and `async/await`.

## Features

- Create a `CancellationTokenSource` to control cancellation.
- Generate `CancellationToken`s to be passed to tasks.
- Async-aware: `token.cancelled().await` completes when the token is cancelled.
- Supports registering callbacks that execute on cancellation.
- Fully `Clone`able and `Send`/`Sync` friendly in `Rc`/`Arc` contexts.

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
async-cancellation-token = "0.1"
```

## Usage

```rust
use async_cancellation_token::{CancellationTokenSource, Cancelled};
use futures::{executor::LocalPool, pin_mut, select, FutureExt};
use futures_timer::Delay;
use std::time::Duration;

let cts = CancellationTokenSource::new();
let token = cts.token();

let mut pool = LocalPool::new();
let spawner = pool.spawner();

spawner.spawn_local(async move {
    for i in 1..=10 {
        let delay = Delay::new(Duration::from_millis(200)).fuse();
        let cancelled = token.cancelled().fuse();
        pin_mut!(delay, cancelled);

        select! {
            _ = delay => println!("Step {i}"),
            _ = cancelled => {
                println!("Cancelled!");
                break;
            }
        }
    }
}.map(|_| ())).unwrap();

// Cancel after 1 second
spawner.spawn_local(async move {
    Delay::new(Duration::from_secs(1)).await;
    cts.cancel();
}.map(|_| ())).unwrap();

pool.run();
```

## API

- CancellationTokenSource::new() – create a new source.
- token() – get a CancellationToken.
- cancel() – cancel all tasks associated with this source.
- is_cancelled() – check if already cancelled.
- CancellationToken::cancelled() – returns a Future that resolves on cancellation.
- CancellationToken::register(f) – register a callback for cancellation.

## License
Apache-2.0
