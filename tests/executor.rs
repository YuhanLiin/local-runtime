use std::{
    future::pending,
    net::{TcpListener, TcpStream},
    rc::Rc,
    time::{Duration, Instant},
};

use futures_lite::{AsyncReadExt, AsyncWriteExt, StreamExt};
use local_runtime::{io::Async, time::sleep, Executor};

#[test]
fn spawn_one() {
    let n = 10;
    let ex = Executor::new();
    let out = ex.run(async {
        let handle = ex.spawn(async { &n });
        handle.await
    });
    assert_eq!(*out, 10);
}

#[test]
fn spawn_parallel() {
    let start = Instant::now();
    let ex = Executor::new();
    ex.run(async {
        let task1 = ex.spawn(sleep(Duration::from_millis(100)));
        let task2 = ex.spawn(async {
            sleep(Duration::from_millis(50)).await;
            sleep(Duration::from_millis(70)).await;
        });
        sleep(Duration::from_millis(100)).await;
        task1.await;
        task2.await;
    });
    let elapsed = start.elapsed();
    assert!(elapsed > Duration::from_millis(120));
    assert!(elapsed < Duration::from_millis(150));
}

#[test]
fn spawn_recursive() {
    let start = Instant::now();
    let ex = Rc::new(Executor::new());
    ex.run(async {
        #[allow(clippy::async_yields_async)]
        let task = ex.clone().spawn_rc(|ex| async move {
            sleep(Duration::from_millis(50)).await;
            ex.spawn(sleep(Duration::from_millis(20)))
        });

        ex.spawn(async move {
            let inner_task = task.await;
            inner_task.await
        })
        .await;
    });
    assert!(start.elapsed() > Duration::from_millis(70));
    assert_eq!(Rc::strong_count(&ex), 1);
}

#[test]
fn spawn_dropped() {
    let ex = Executor::new();
    ex.run(async {
        // Even though this task will never return, it doesn't matter because we don't await on it
        ex.spawn(pending::<()>());
    });
}

#[test]
fn client_server() {
    let _ = env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Trace)
        .try_init();

    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    let addr = listener.get_ref().local_addr().unwrap();

    let client = std::thread::spawn(move || {
        let ex = Executor::new();
        ex.run(async {
            for i in 1..=10 {
                let mut buf = [0u8; 11];
                let mut stream = Async::<TcpStream>::connect(addr).await.unwrap();
                stream.write_all(&[i; 5]).await.unwrap();
                stream.read(&mut buf).await.unwrap();
                assert_eq!(&buf, b"hello world");
            }
        })
    });

    let ex = Executor::new();
    ex.run(async {
        let mut incoming = listener.incoming();
        let mut tasks = vec![];
        for i in 1..=10 {
            let mut stream = incoming.next().await.unwrap().unwrap();
            log::info!("Server: connection {i}");
            let task = ex.spawn(async move {
                let mut buf = [0u8; 5];
                stream.read_exact(&mut buf).await.unwrap();
                stream.write_all(b"hello world").await.unwrap();
                assert!(buf.iter().all(|&b| b == i));
            });
            tasks.push(task);
        }

        for task in tasks {
            task.await;
        }
    });

    client.join().unwrap();
}
