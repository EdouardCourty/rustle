//! TCP discard server (sink target): accepts connections and drops all
//! bytes. One `current_thread` runtime per core, one `SO_REUSEPORT`
//! listener each; every connection reads into its thread's single 256 KB
//! scratch buffer (`try_read` between awaits only), so an idle tunnel costs
//! no buffer at all and a bulk flow is not capped by a small read size.

use std::cell::RefCell;

use tokio::net::{TcpListener, TcpStream};

use crate::rt::{bind_listener, run_threads};

const SCRATCH: usize = 256 * 1024;

thread_local! {
    static SCRATCH_BUF: RefCell<Vec<u8>> = RefCell::new(vec![0u8; SCRATCH]);
}

async fn discard(s: TcpStream) {
    loop {
        if s.readable().await.is_err() {
            return;
        }
        // No await while the scratch is borrowed: tasks of the thread take
        // turns on it.
        let r = SCRATCH_BUF.with(|b| s.try_read(&mut b.borrow_mut()));
        match r {
            Ok(0) => return,
            Ok(_) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return,
        }
    }
}

pub(crate) fn run_sink(listen: &str, threads: usize) -> ! {
    let listen = listen.to_string();
    // Bind before spawning: a bind error must stop the process.
    let first = match bind_listener(&listen, threads > 1) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("sink: bind {listen} failed: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("sink: listening on {listen} ({threads} threads)");
    let first = std::sync::Mutex::new(Some(first));
    run_threads(threads, move |i| {
        let l = if i == 0 {
            first.lock().unwrap().take()
        } else {
            None
        };
        let listen = listen.clone();
        async move {
            let std_l = match l {
                Some(l) => l,
                None => bind_listener(&listen, true).expect("sink: bind"),
            };
            let listener = TcpListener::from_std(std_l).expect("sink: listener");
            loop {
                match listener.accept().await {
                    Ok((s, _)) => {
                        tokio::spawn(discard(s));
                    }
                    Err(e) => eprintln!("sink: accept error: {e}"),
                }
            }
        }
    });
    unreachable!()
}
